//! Linux sign-in window for Bandcamp: the same browser that hosts the app window, on Bandcamp's
//! login page, in a throwaway profile (deleted before and after, so each sign-in starts fresh and
//! can switch accounts). Once the page has set the `identity` cookie it is stored and the window
//! closes; closing it first cancels.
//!
//! How the cookie is read without touching the browser's encrypted cookie store:
//! * Chromium: `--remote-debugging-pipe` gives this process (and only it) the DevTools protocol
//!   on fds 3 and 4; `Storage.getCookies` is polled every second.
//! * Firefox: its `cookies.sqlite` is not encrypted. A copy (with the WAL, so recent writes are
//!   in it) is read every second; the live file may be locked.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bc_types::bandcamp::BandcampLoginState;

use crate::browser::{Browser, chromium_app, find_browser, profile_dir, quiet_profile};
use crate::{BANDCAMP_LOGIN_URL, Core, bandcamp_cookie_header};

/// A sign-in window is open (the browser cannot be raised from outside, so a second request
/// leaves it be).
static OPEN: AtomicBool = AtomicBool::new(false);

const POLL: Duration = Duration::from_secs(1);

/// A browser to sign in with is installed (the `xdg-open` fallback has no way to hand back the
/// cookie).
pub fn available() -> bool {
    find_browser().is_some()
}

/// Open the sign-in window and wait for it on a thread of its own. Needs a tokio runtime context.
pub fn open(core: Arc<Core>) {
    if OPEN.swap(true, Ordering::SeqCst) {
        core.login_event(BandcampLoginState::Open, "");
        return;
    }
    let rt = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let outcome = match find_browser() {
            Some(Browser::Chromium(b)) => chromium(&core, &rt, &b),
            Some(Browser::Firefox(b)) => firefox(&core, &rt, &b),
            None => Err("no browser found to sign in with".into()),
        };
        match outcome {
            // `try_login_cookie` already reported it.
            Ok(true) => {}
            Ok(false) => core.login_event(BandcampLoginState::Closed, ""),
            Err(e) => {
                tracing::warn!("sign-in window: {e}");
                core.login_event(BandcampLoginState::Failed, format!("Could not open the sign-in window: {e}"));
            }
        }
        OPEN.store(false, Ordering::SeqCst);
    });
}

/// Hands each new cookie header to the server once; `true` when it was accepted.
struct Handover<'a> {
    core: &'a Core,
    rt: &'a tokio::runtime::Handle,
    tried: Option<String>,
}

impl Handover<'_> {
    fn offer(&mut self, header: Option<String>) -> bool {
        let Some(header) = header else { return false };
        if self.tried.as_ref() == Some(&header) {
            return false;
        }
        self.tried = Some(header.clone());
        self.rt.block_on(self.core.try_login_cookie(header))
    }
}

/// A fresh, empty profile directory.
fn fresh_profile(name: &str) -> std::io::Result<std::path::PathBuf> {
    let dir = profile_dir(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn quiet(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
}

/// Wait up to `grace` for the browser to exit, then kill it.
fn reap(child: &mut Child, grace: Duration) {
    let until = Instant::now() + grace;
    while Instant::now() < until {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

// -- Chromium ---------------------------------------------------------------------------

/// A close-on-exec pipe whose ends sit above fd 4, so moving the child's ends onto 3 and 4
/// cannot overwrite one of them.
fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe2 fills two fds on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut out = [-1; 2];
    for (i, fd) in fds.into_iter().enumerate() {
        // SAFETY: fd is ours; the duplicate is close-on-exec, the original is closed right after.
        out[i] = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 5) };
        unsafe { libc::close(fd) };
    }
    if out.contains(&-1) {
        let err = std::io::Error::last_os_error();
        out.iter().filter(|fd| **fd >= 0).for_each(|fd| unsafe {
            libc::close(*fd);
        });
        return Err(err);
    }
    // SAFETY: both are open fds owned by nothing else.
    Ok(unsafe { (OwnedFd::from_raw_fd(out[0]), OwnedFd::from_raw_fd(out[1])) })
}

fn chromium(core: &Core, rt: &tokio::runtime::Handle, browser: &Path) -> Result<bool, String> {
    let profile = fresh_profile("bandcamp-login").map_err(|e| e.to_string())?;
    quiet_profile(&profile);
    let result = chromium_session(core, rt, browser, &profile);
    let _ = std::fs::remove_dir_all(&profile);
    result
}

fn chromium_session(core: &Core, rt: &tokio::runtime::Handle, browser: &Path, profile: &Path) -> Result<bool, String> {
    // Chromium reads commands from fd 3 and writes replies to fd 4, each message ending in NUL.
    let (cmd_read, cmd_write) = pipe().map_err(|e| e.to_string())?;
    let (reply_read, reply_write) = pipe().map_err(|e| e.to_string())?;
    let mut cmd = chromium_app(browser, profile, BANDCAMP_LOGIN_URL);
    cmd.args(["--window-size=520,760", "--remote-debugging-pipe"]);
    let (r, w) = (cmd_read.as_raw_fd(), reply_write.as_raw_fd());
    // SAFETY: only async-signal-safe dup2 between fork and exec. dup2 clears close-on-exec on
    // the new fds, so exactly 3 and 4 reach the browser.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(r, 3) < 0 || libc::dup2(w, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = quiet(&mut cmd).spawn().map_err(|e| format!("{}: {e}", browser.display()))?;
    drop((cmd_read, reply_write));
    core.login_event(BandcampLoginState::Open, "");

    // Replies arrive on a thread of their own; the channel closes when the browser exits.
    let (tx, rx) = mpsc::channel::<serde_json::Value>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(File::from(reply_read));
        let mut buf = Vec::new();
        while matches!(reader.read_until(0, &mut buf), Ok(n) if n > 0) {
            if buf.last() == Some(&0) {
                buf.pop();
            }
            if let Ok(v) = serde_json::from_slice(&buf)
                && tx.send(v).is_err()
            {
                break;
            }
            buf.clear();
        }
    });
    let mut commands = File::from(cmd_write);
    let mut send = |id: u64, method: &str| commands.write_all(format!(r#"{{"id":{id},"method":"{method}"}}"#).as_bytes()).and_then(|_| commands.write_all(&[0]));

    let mut handover = Handover { core, rt, tried: None };
    let mut id = 0u64;
    let signed_in = 'poll: loop {
        id += 1;
        if send(id, "Storage.getCookies").is_err() {
            break false;
        }
        let until = Instant::now() + POLL;
        let cookies = loop {
            match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(v) if v.get("id").and_then(|i| i.as_u64()) == Some(id) => break v,
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => continue 'poll,
                Err(mpsc::RecvTimeoutError::Disconnected) => break 'poll false,
            }
        };
        if handover.offer(chromium_cookie_header(&cookies)) {
            let _ = send(id + 1, "Browser.close");
            break true;
        }
        std::thread::sleep(POLL);
    };
    reap(&mut child, Duration::from_secs(3));
    Ok(signed_in)
}

/// The Bandcamp cookie header in a `Storage.getCookies` reply.
fn chromium_cookie_header(reply: &serde_json::Value) -> Option<String> {
    let cookies = reply.pointer("/result/cookies")?.as_array()?;
    let field = |c: &'_ serde_json::Value, k: &str| c.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let owned: Vec<(String, String, String)> = cookies.iter().map(|c| (field(c, "domain"), field(c, "name"), field(c, "value"))).collect();
    bandcamp_cookie_header(owned.iter().map(|(d, n, v)| (d.as_str(), n.as_str(), v.as_str())))
}

// -- Firefox ----------------------------------------------------------------------------

/// A quiet, normal-looking browser window (tabs and address bar stay, unlike the app window).
const FIREFOX_LOGIN_USER_JS: &str = r#"// Written by bc-desktop for the Bandcamp sign-in window.
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage_override.mstone", "ignore");
user_pref("browser.aboutwelcome.enabled", false);
user_pref("trailhead.firstrun.didSeeAboutWelcome", true);
user_pref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);
user_pref("toolkit.telemetry.reportingpolicy.firstRun", false);
user_pref("browser.translations.automaticallyPopup", false);
user_pref("signon.rememberSignons", false);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("browser.tabs.warnOnClose", false);
"#;

fn firefox(core: &Core, rt: &tokio::runtime::Handle, browser: &Path) -> Result<bool, String> {
    let profile = fresh_profile("bandcamp-login-firefox").map_err(|e| e.to_string())?;
    let result = firefox_session(core, rt, browser, &profile);
    let _ = std::fs::remove_dir_all(&profile);
    result
}

fn firefox_session(core: &Core, rt: &tokio::runtime::Handle, browser: &Path, profile: &Path) -> Result<bool, String> {
    std::fs::write(profile.join("user.js"), FIREFOX_LOGIN_USER_JS).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(browser);
    cmd.arg("--profile").arg(profile).args(["--no-remote", "--new-instance", "--class", "bc", "--width", "520", "--height", "760"]);
    cmd.arg(BANDCAMP_LOGIN_URL);
    let mut child = quiet(&mut cmd).spawn().map_err(|e| format!("{}: {e}", browser.display()))?;
    core.login_event(BandcampLoginState::Open, "");

    let mut handover = Handover { core, rt, tried: None };
    let signed_in = loop {
        std::thread::sleep(POLL);
        let exited = !matches!(child.try_wait(), Ok(None));
        // One last look after the window closed: Firefox writes the WAL back on exit.
        if handover.offer(firefox_cookie_header(profile)) {
            break true;
        }
        if exited {
            break false;
        }
    };
    if signed_in {
        // SIGTERM lets Firefox shut down cleanly (no "restore session" next time).
        // SAFETY: plain kill(2) on our own child.
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    }
    reap(&mut child, Duration::from_secs(5));
    Ok(signed_in)
}

/// The Bandcamp cookie header in a Firefox profile, read from a copy of its cookie database.
fn firefox_cookie_header(profile: &Path) -> Option<String> {
    let src = profile.join("cookies.sqlite");
    if !src.is_file() {
        return None;
    }
    let peek = profile.join("bc-peek");
    let _ = std::fs::remove_dir_all(&peek);
    std::fs::create_dir_all(&peek).ok()?;
    std::fs::copy(&src, peek.join("cookies.sqlite")).ok()?;
    let _ = std::fs::copy(profile.join("cookies.sqlite-wal"), peek.join("cookies.sqlite-wal"));
    let rows = (|| -> rusqlite::Result<Vec<(String, String, String)>> {
        let db = rusqlite::Connection::open(peek.join("cookies.sqlite"))?;
        let mut st = db.prepare("SELECT host, name, value FROM moz_cookies WHERE host = 'bandcamp.com' OR host LIKE '%.bandcamp.com'")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    })();
    let _ = std::fs::remove_dir_all(&peek);
    let rows = rows.ok()?;
    bandcamp_cookie_header(rows.iter().map(|(d, n, v)| (d.as_str(), n.as_str(), v.as_str())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devtools_reply_gives_the_cookie_header() {
        let reply = serde_json::json!({ "id": 3, "result": { "cookies": [
            { "name": "identity", "value": "7%09abc", "domain": ".bandcamp.com" },
            { "name": "NID", "value": "x", "domain": ".google.com" },
        ] } });
        assert_eq!(chromium_cookie_header(&reply).as_deref(), Some("identity=7%09abc"));
        assert_eq!(chromium_cookie_header(&serde_json::json!({ "id": 3, "result": { "cookies": [] } })), None);
    }

    #[test]
    fn firefox_cookies_are_read_from_a_copy() {
        let dir = std::env::temp_dir().join(format!("bc-desktop-login-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(firefox_cookie_header(&dir), None);
        let db = rusqlite::Connection::open(dir.join("cookies.sqlite")).unwrap();
        db.execute_batch(
            "CREATE TABLE moz_cookies (host TEXT, name TEXT, value TEXT);
             INSERT INTO moz_cookies VALUES ('.bandcamp.com', 'client_id', '1'), ('.bandcamp.com', 'identity', 'abc'), ('.example.com', 'identity', 'no');",
        )
        .unwrap();
        drop(db);
        assert_eq!(firefox_cookie_header(&dir).as_deref(), Some("client_id=1; identity=abc"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
