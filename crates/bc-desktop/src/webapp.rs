//! bc as an installed web app of the window profile (Chromium family only). A plain `--app=URL`
//! window always has the browser's title strip above the page; an installed app whose manifest
//! asks for `window-controls-overlay` can fold it away, and then bc's own header is the title bar
//! with the browser's minimise / maximise / close buttons drawn over its right end. Chromium has no
//! switch to install an app, so a headless browser does it once over the DevTools pipe
//! (`PWA.install`), and the window is then opened with `--app-id`.
//!
//! Chromium starts every app with the overlay off (the chevron in the title strip toggles it, and
//! there is no switch or DevTools call for it), so once after installing, bc turns it on in the
//! profile's web app database: field 34 of the app's record, `window_controls_overlay_enabled`.
//! Only once, so folding the strip back down with the chevron sticks.
//!
//! The app id is Chromium's own hash, recorded as the one new directory under
//! `Web Applications/Manifest Resources` after the install. `bc-webapp.json` in the profile keeps
//! it with the origin it was installed for. Anything that fails leaves the plain `--app` window.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::browser::{pipe, reap};

const MARKER: &str = "bc-webapp.json";
const INSTALL_TIMEOUT: Duration = Duration::from_secs(30);

fn resources_dir(profile: &Path) -> PathBuf {
    profile.join("Default").join("Web Applications").join("Manifest Resources")
}

fn installed_ids(profile: &Path) -> HashSet<String> {
    std::fs::read_dir(resources_dir(profile))
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default()
}

/// What the marker says about `origin`: `Some(Some(id))` installed, `Some(None)` installing failed
/// with this browser (not retried until the browser changes), `None` never tried.
fn recorded(profile: &Path, origin: &str, browser: &Path) -> Option<Option<String>> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(profile.join(MARKER)).ok()?).ok()?;
    if v.get("origin")?.as_str()? != origin {
        // Installed for the usual port; this run fell back to another one. Keep the plain window
        // rather than installing a second copy for a port that is gone next time.
        return Some(None);
    }
    match v.get("app_id").and_then(|i| i.as_str()) {
        Some(id) if installed_ids(profile).contains(id) => Some(Some(id.to_string())),
        Some(_) => None, // uninstalled from the browser's side: install again
        None => (v.get("browser").and_then(|b| b.as_str()) == Some(&*browser.to_string_lossy())).then_some(None),
    }
}

fn record(profile: &Path, origin: &str, browser: &Path, app_id: Option<&str>) {
    let v = serde_json::json!({ "origin": origin, "app_id": app_id, "browser": browser.to_string_lossy() });
    let _ = std::fs::write(profile.join(MARKER), v.to_string());
}

fn overlay_preset(profile: &Path) -> bool {
    std::fs::read_to_string(profile.join(MARKER))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("overlay")?.as_bool())
        == Some(true)
}

fn record_overlay_preset(profile: &Path) {
    let path = profile.join(MARKER);
    let Some(mut v) = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) else {
        return;
    };
    v["overlay"] = true.into();
    let _ = std::fs::write(path, v.to_string());
}

/// A browser runs on the profile. `SingletonLock` is a symlink to "host-pid" that dangles, so
/// `exists()` (which follows it) would say no.
fn in_use(profile: &Path) -> bool {
    profile.join("SingletonLock").symlink_metadata().is_ok()
}

/// The app id to open the window with (`--app-id`), installing bc first when it is not yet.
/// Installs, and turns the overlay on, only while no browser runs on the profile.
pub(crate) fn app_id(browser: &Path, profile: &Path, origin: &str) -> Option<String> {
    let id = match recorded(profile, origin, browser) {
        Some(r) => r?,
        None if in_use(profile) => return None,
        None => installed(browser, profile, origin)?,
    };
    if !overlay_preset(profile) && !in_use(profile) {
        match enable_overlay(profile, &id) {
            Ok(()) => tracing::info!(app_id = %id, "turned on the window controls overlay: bc's header is the title bar"),
            Err(e) => tracing::warn!("could not turn on the window controls overlay ({e}); the title strip's chevron does it"),
        }
        record_overlay_preset(profile);
    }
    Some(id)
}

fn installed(browser: &Path, profile: &Path, origin: &str) -> Option<String> {
    let before = installed_ids(profile);
    let started = Instant::now();
    match install(browser, profile, origin) {
        Ok(()) => {
            let new: Vec<String> = installed_ids(profile).difference(&before).cloned().collect();
            if let [id] = new.as_slice() {
                tracing::info!(app_id = %id, ms = started.elapsed().as_millis() as u64, "installed bc as a web app of the window profile");
                record(profile, origin, browser, Some(id));
                return Some(id.clone());
            }
            tracing::warn!(?new, "web app install finished without one new app; using a plain app window");
        }
        Err(e) => tracing::warn!("could not install bc as a web app ({e}); using a plain app window"),
    }
    record(profile, origin, browser, None);
    None
}

/// `window_controls_overlay_enabled` in Chromium's `WebAppProto`.
const OVERLAY_FIELD: u64 = 34;

/// Set the app's overlay flag in the web app database (`Sync Data/LevelDB`, key
/// `web_apps-dt-<id>`, a `WebAppProto`). The browser must not be running.
fn enable_overlay(profile: &Path, id: &str) -> anyhow::Result<()> {
    let dir = profile.join("Default").join("Sync Data").join("LevelDB");
    let opts = rusty_leveldb::Options { create_if_missing: false, ..Default::default() };
    let mut db = rusty_leveldb::DB::open(&dir, opts)?;
    let key = format!("web_apps-dt-{id}");
    let record = db.get(key.as_bytes()).ok_or_else(|| anyhow::anyhow!("no record for {id}"))?;
    db.put(key.as_bytes(), &with_varint_field(&record, OVERLAY_FIELD, 1)?)?;
    db.close()?;
    Ok(())
}

/// The protobuf message `msg` with every `field` dropped and `field = value` (a varint) appended.
fn with_varint_field(msg: &[u8], field: u64, value: u64) -> anyhow::Result<Vec<u8>> {
    fn varint(b: &[u8], p: &mut usize) -> anyhow::Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *b.get(*p).ok_or_else(|| anyhow::anyhow!("truncated varint"))?;
            *p += 1;
            v |= u64::from(byte & 0x7f) << shift;
            if byte < 0x80 {
                return Ok(v);
            }
        }
        anyhow::bail!("overlong varint")
    }
    fn put_varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push(v as u8 | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    let mut out = Vec::with_capacity(msg.len() + 4);
    let mut p = 0;
    while p < msg.len() {
        let start = p;
        let tag = varint(msg, &mut p)?;
        match tag & 7 {
            0 => drop(varint(msg, &mut p)?),
            1 => p += 8,
            2 => p += varint(msg, &mut p)? as usize,
            5 => p += 4,
            t => anyhow::bail!("unexpected wire type {t}"),
        }
        anyhow::ensure!(p <= msg.len(), "truncated field");
        if tag >> 3 != field {
            out.extend_from_slice(&msg[start..p]);
        }
    }
    put_varint(&mut out, field << 3);
    put_varint(&mut out, value);
    Ok(out)
}

/// Install `origin` as a web app in `profile` with a headless browser driven over
/// `--remote-debugging-pipe` (Chromium reads commands on fd 3 and answers on fd 4).
fn install(browser: &Path, profile: &Path, origin: &str) -> anyhow::Result<()> {
    let (cmd_r, cmd_w) = pipe()?;
    let (res_r, res_w) = pipe()?;
    let (child_in, child_out) = (cmd_r.as_raw_fd(), res_w.as_raw_fd());
    let mut cmd = Command::new(browser);
    cmd.arg(format!("--user-data-dir={}", profile.display()))
        .args(["--headless", "--remote-debugging-pipe", "--no-first-run", "--no-default-browser-check", "about:blank"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe dup2 between fork and exec; dup2 clears close-on-exec on 3/4.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(child_in, 3) < 0 || libc::dup2(child_out, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop((cmd_r, res_w));

    let (tx, rx) = mpsc::channel::<serde_json::Value>();
    std::thread::spawn(move || {
        for msg in BufReader::new(File::from(res_r)).split(0).map_while(Result::ok) {
            if let Ok(v) = serde_json::from_slice(&msg)
                && tx.send(v).is_err()
            {
                break;
            }
        }
    });
    let mut out = File::from(cmd_w);
    let deadline = Instant::now() + INSTALL_TIMEOUT;
    let mut next_id = 0u64;
    let mut call = |method: &str, params: serde_json::Value| -> anyhow::Result<serde_json::Value> {
        next_id += 1;
        let mut msg = serde_json::to_vec(&serde_json::json!({ "id": next_id, "method": method, "params": params }))?;
        msg.push(0);
        out.write_all(&msg)?;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).ok_or_else(|| anyhow::anyhow!("{method} timed out"))?;
            let v = rx.recv_timeout(left).map_err(|_| anyhow::anyhow!("{method}: no answer"))?;
            if v.get("id").and_then(|i| i.as_u64()) != Some(next_id) {
                continue; // events
            }
            if let Some(e) = v.get("error") {
                anyhow::bail!("{method}: {e}");
            }
            return Ok(v);
        }
    };
    // The manifest's `id` is "/", so the manifest id is the origin's root.
    let manifest_id = format!("{}/", origin.trim_end_matches('/'));
    let result = (|| {
        call("PWA.install", serde_json::json!({ "manifestId": manifest_id, "installUrlOrBundleUrl": manifest_id }))?;
        // Installs from DevTools open in a browser tab by default.
        call("PWA.changeAppUserSettings", serde_json::json!({ "manifestId": manifest_id, "displayMode": "standalone" }))?;
        anyhow::Ok(())
    })();
    let _ = call("Browser.close", serde_json::json!({}));
    reap(&mut child, Duration::from_secs(10));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_profile(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bc-webapp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(resources_dir(&dir)).unwrap();
        dir
    }

    #[test]
    fn marker_decides_whether_to_install() {
        let p = temp_profile("marker");
        let chrome = Path::new("/usr/bin/chromium");
        assert_eq!(recorded(&p, "http://127.0.0.1:8420", chrome), None, "never tried");

        std::fs::create_dir_all(resources_dir(&p).join("abc")).unwrap();
        record(&p, "http://127.0.0.1:8420", chrome, Some("abc"));
        assert_eq!(recorded(&p, "http://127.0.0.1:8420", chrome), Some(Some("abc".into())));
        assert_eq!(recorded(&p, "http://127.0.0.1:39123", chrome), Some(None), "fallback port keeps the plain window");

        std::fs::remove_dir_all(resources_dir(&p).join("abc")).unwrap();
        assert_eq!(recorded(&p, "http://127.0.0.1:8420", chrome), None, "uninstalled: install again");

        record(&p, "http://127.0.0.1:8420", chrome, None);
        assert_eq!(recorded(&p, "http://127.0.0.1:8420", chrome), Some(None), "failed with this browser");
        assert_eq!(recorded(&p, "http://127.0.0.1:8420", Path::new("/usr/bin/brave")), None, "another browser may manage");
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn overlay_field_replaces_any_earlier_value() {
        // 2: "bc", 34: 0, 3: 300
        let msg = [0x12, 2, b'b', b'c', 0x90, 0x02, 0, 0x18, 0xac, 0x02];
        assert_eq!(with_varint_field(&msg, 34, 1).unwrap(), [0x12, 2, b'b', b'c', 0x18, 0xac, 0x02, 0x90, 0x02, 1]);
        assert_eq!(with_varint_field(&[0x12, 2, b'b', b'c'], 34, 1).unwrap(), [0x12, 2, b'b', b'c', 0x90, 0x02, 1]);
        assert!(with_varint_field(&[0x12, 5, b'b'], 34, 1).is_err(), "truncated");
    }

    #[test]
    fn overlay_is_turned_on_in_the_web_app_database() {
        let p = temp_profile("overlay");
        let dir = p.join("Default").join("Sync Data").join("LevelDB");
        let mut db = rusty_leveldb::DB::open(&dir, rusty_leveldb::Options::default()).unwrap();
        db.put(b"web_apps-dt-abc", &[0x12, 2, b'b', b'c', 0x90, 0x02, 0]).unwrap();
        db.close().unwrap();

        enable_overlay(&p, "abc").unwrap();
        let mut db = rusty_leveldb::DB::open(&dir, rusty_leveldb::Options::default()).unwrap();
        assert_eq!(db.get(b"web_apps-dt-abc").unwrap().as_ref(), [0x12, 2, b'b', b'c', 0x90, 0x02, 1]);
        assert!(enable_overlay(&p, "missing").is_err());

        record(&p, "http://127.0.0.1:8420", Path::new("/usr/bin/chromium"), Some("abc"));
        assert!(!overlay_preset(&p));
        record_overlay_preset(&p);
        assert!(overlay_preset(&p));
        let _ = std::fs::remove_dir_all(&p);
    }
}
