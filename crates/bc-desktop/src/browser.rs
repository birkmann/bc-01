//! Linux (and other non-macOS) front end: the UI in a chromeless app window of an installed
//! browser, with its own profile dir and window class `bc` (so the dock shows the bc icon): a
//! Chromium-family browser (`--app=URL`) when there is one, else Firefox (or a fork) with a
//! private profile whose `userChrome.css` hides the tab strip and toolbars, else the default
//! browser. Tray (StatusNotifierItem over D-Bus): show, play/pause, next, quit.

use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ksni::TrayMethods;

use crate::{Core, IDLE_QUIT_S};

const CHROMIUM_NAMES: [&str; 7] =
    ["google-chrome-stable", "chromium", "brave", "google-chrome", "brave-browser", "microsoft-edge-stable", "vivaldi-stable"];
const FIREFOX_NAMES: [&str; 5] = ["firefox", "firefox-esr", "librewolf", "floorp", "waterfox"];

/// The browser that hosts the app window.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Browser {
    /// Chromium family: a real app window (`--app=URL`).
    Chromium(PathBuf),
    /// Firefox or a fork: no app mode, so a private profile hides the browser chrome.
    Firefox(PathBuf),
}

impl Browser {
    /// `BC_DESKTOP_BROWSER` may name either kind; it is told apart by the binary's name.
    fn from_path(p: PathBuf) -> Self {
        let name = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        if FIREFOX_NAMES.iter().any(|f| name.starts_with(f)) { Self::Firefox(p) } else { Self::Chromium(p) }
    }
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// A Chromium-family browser when one is installed (the best app window), else Firefox.
pub(crate) fn find_browser() -> Option<Browser> {
    if let Some(b) = std::env::var_os("BC_DESKTOP_BROWSER") {
        return Some(Browser::from_path(PathBuf::from(b)));
    }
    CHROMIUM_NAMES
        .iter()
        .find_map(|n| on_path(n))
        .map(Browser::Chromium)
        .or_else(|| FIREFOX_NAMES.iter().find_map(|n| on_path(n)).map(Browser::Firefox))
}

pub(crate) fn profile_dir(name: &str) -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("bc-rust").join(name)
}

/// Turn off the browser's own chrome for this private profile: translate offers (the UI is
/// English, the user's browser language may not be), password prompts, the desktop-themed frame. Only rewritten when the
/// browser is not running on the profile (Chrome rewrites Preferences on exit).
pub(crate) fn quiet_profile(profile: &std::path::Path) {
    if profile.join("SingletonLock").exists() {
        return;
    }
    let path = profile.join("Default").join("Preferences");
    let mut prefs: serde_json::Value =
        std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_else(|| serde_json::json!({}));
    let Some(obj) = prefs.as_object_mut() else { return };
    obj.insert("translate".into(), serde_json::json!({ "enabled": false }));
    obj.insert("credentials_enable_service".into(), serde_json::json!(false));
    // Classic (non-GTK/Qt) frame drawn by the browser: it takes the page's `theme-color`, so the
    // title strip matches the app header instead of the desktop theme's headerbar.
    let browser = obj.entry("browser").or_insert_with(|| serde_json::json!({}));
    if let Some(b) = browser.as_object_mut() {
        b.insert("custom_chrome_frame".into(), serde_json::json!(true));
    }
    let ext = obj.entry("extensions").or_insert_with(|| serde_json::json!({}));
    if let Some(e) = ext.as_object_mut() {
        let theme = e.entry("theme").or_insert_with(|| serde_json::json!({}));
        if let Some(t) = theme.as_object_mut() {
            t.insert("system_theme".into(), serde_json::json!(0));
            t.insert("use_system".into(), serde_json::json!(false));
        }
    }
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(profile));
    let _ = std::fs::write(&path, prefs.to_string());
}

/// Firefox has no app mode. Its private profile gets prefs that keep it quiet (no first-run pages,
/// default-browser checks, translation offers, password prompts or session-restore prompts; audio
/// may start without a click, like Chromium's `--autoplay-policy`) and a `userChrome.css` that
/// hides the tab strip and toolbars while the app is the only tab. A link opened in a new tab
/// (Open on Bandcamp) brings them back until that tab is closed. Rewritten on every start:
/// Firefox reads `user.js` at startup and never writes it.
const FIREFOX_USER_JS: &str = r#"// Written by bc-desktop on every start; edits are overwritten.
user_pref("toolkit.legacyUserProfileCustomizations.stylesheets", true);
user_pref("browser.tabs.inTitlebar", 0);
user_pref("browser.toolbars.bookmarks.visibility", "never");
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
user_pref("media.autoplay.default", 0);
"#;

const FIREFOX_USER_CHROME: &str = r#"/* Written by bc-desktop on every start; edits are overwritten. */
#navigator-toolbox:not(:has(.tabbrowser-tab ~ .tabbrowser-tab)) :is(#TabsToolbar, #nav-bar, #PersonalToolbar) {
  visibility: collapse !important;
}
"#;

fn firefox_profile(profile: &std::path::Path) {
    let _ = std::fs::create_dir_all(profile.join("chrome"));
    let _ = std::fs::write(profile.join("user.js"), FIREFOX_USER_JS);
    let _ = std::fs::write(profile.join("chrome").join("userChrome.css"), FIREFOX_USER_CHROME);
}

/// Open an app window on `url`. Returns the browser process when this call started one.
fn open_window(url: &str) -> Option<Child> {
    let mut cmd = match find_browser() {
        None => {
            tracing::warn!("no Chromium-family browser or Firefox found; opening the default browser");
            let _ = Command::new("xdg-open").arg(url).spawn();
            return None;
        }
        Some(Browser::Firefox(browser)) => {
            let profile = profile_dir("window-profile-firefox");
            firefox_profile(&profile);
            let mut cmd = Command::new(browser);
            // `--name` is also Firefox's remoting name: a second launch with the same profile and
            // name opens a window in the running instance instead of failing, like Chromium does.
            cmd.arg("--profile").arg(&profile).args(["--class", "bc", "--name", "bc", "--new-window", url]);
            cmd
        }
        Some(Browser::Chromium(browser)) => chromium_cmd(browser, url),
    };
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    match cmd.spawn() {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::error!("could not start the window: {e}");
            None
        }
    }
}

/// The main window: bc installed as a web app of the profile when that works (its header can
/// then replace the title strip, see `webapp.rs`), else a plain app window on `url`.
fn chromium_cmd(browser: PathBuf, url: &str) -> Command {
    let profile = profile_dir("window-profile");
    let _ = std::fs::create_dir_all(&profile);
    quiet_profile(&profile);
    let mut cmd = match crate::webapp::app_id(&browser, &profile, url) {
        Some(id) => {
            let mut cmd = chromium(&browser, &profile);
            cmd.arg(format!("--app-id={id}"));
            cmd
        }
        None => chromium_app(&browser, &profile, url),
    };
    cmd.args(["--window-size=1360,860", "--autoplay-policy=no-user-gesture-required"]);
    cmd
}

/// A Chromium app window on `url` with its own profile, without the browser's prompts and with
/// the window class `bc`: the main window when bc is not an installed app, and the Bandcamp
/// sign-in window.
pub(crate) fn chromium_app(browser: &std::path::Path, profile: &std::path::Path, url: &str) -> Command {
    let mut cmd = chromium(browser, profile);
    cmd.arg(format!("--app={url}"));
    cmd
}

/// The browser on `profile` with the flags every bc window shares.
fn chromium(browser: &std::path::Path, profile: &std::path::Path) -> Command {
    let mut cmd = Command::new(browser);
    cmd.arg(format!("--user-data-dir={}", profile.display()))
        .args(["--class=bc", "--name=bc", "--no-first-run", "--no-default-browser-check"])
        .args(["--disable-features=Translate,TranslateUI,MediaRouter"])
        // Distro/AUR builds of Chrome cannot self-update and nag "Chrome can't be updated" in
        // every window; a far-future "outdated" date switches that bubble off for this app
        // window only. Updates still come from the package manager.
        .args(["--simulate-outdated-no-au=Tue, 31 Dec 2099 23:59:59 GMT", "--disable-component-update"]);
    // X11 (XWayland) so the window class `bc` is honoured and the dock matches bc.desktop.
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var("BC_DESKTOP_WAYLAND").as_deref() != Ok("1") {
        cmd.arg("--ozone-platform=x11");
    }
    cmd
}

/// Wait up to `grace` for the browser to exit, then kill it.
pub(crate) fn reap(child: &mut Child, grace: Duration) {
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

/// A close-on-exec pipe whose ends sit above fd 4, so moving the child's ends onto 3 and 4
/// cannot overwrite one of them.
pub(crate) fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
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

struct App {
    core: Arc<Core>,
    window: Mutex<Option<Child>>,
    quit: tokio::sync::Notify,
}

impl App {
    fn window_open(&self) -> bool {
        let mut w = self.window.lock().unwrap_or_else(|e| e.into_inner());
        match w.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn show(&self) {
        if self.window_open() {
            // The browser cannot be raised from outside; a second app window of the same profile
            // opens in the running browser process and gets focus.
            let _ = open_window(&self.core.url);
            return;
        }
        let child = open_window(&self.core.url);
        *self.window.lock().unwrap_or_else(|e| e.into_inner()) = child;
    }
}

struct BcTray {
    app: Arc<App>,
}

impl ksni::Tray for BcTray {
    fn id(&self) -> String {
        "bc".into()
    }
    fn title(&self) -> String {
        "bc".into()
    }
    fn icon_name(&self) -> String {
        "bc".into()
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.app.show();
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem { label: "Show bc".into(), activate: Box::new(|t: &mut Self| t.app.show()), ..Default::default() }.into(),
            StandardItem {
                label: "Play / Pause".into(),
                activate: Box::new(|t: &mut Self| t.app.core.player_cmd(serde_json::json!({ "cmd": "toggle" }))),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Next track".into(),
                activate: Box::new(|t: &mut Self| t.app.core.player_cmd(serde_json::json!({ "cmd": "next" }))),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem { label: "Quit".into(), activate: Box::new(|t: &mut Self| t.app.quit.notify_one()), ..Default::default() }.into(),
        ]
    }
}


/// Show the window and the tray, then wait until the app should quit.
pub async fn run(core: Arc<Core>) -> anyhow::Result<()> {
    let app = Arc::new(App { core, window: Mutex::new(None), quit: tokio::sync::Notify::new() });
    app.show();
    let c = app.core.clone();
    crate::on_login_request(&app.core, move || crate::login::open(c.clone()));
    // Without a tray host (e.g. GNOME without the AppIndicator extension) this just fails.
    let _tray = BcTray { app: app.clone() }.spawn().await.map_err(|e| tracing::warn!("no tray: {e}")).ok();

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut usr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    // Seconds spent with no window and nothing playing; quit after IDLE_QUIT_S.
    let mut idle_s = 0u64;
    loop {
        tokio::select! {
            _ = app.quit.notified() => break,
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = usr1.recv() => { idle_s = 0; app.show(); }
            _ = tick.tick() => {
                let open = app.window_open();
                if !open {
                    *app.window.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
                // Window closed: keep playing in the background; once nothing has played for
                // a while (or right away if nothing was playing when it closed), quit.
                if open || app.core.playing() || find_browser().is_none() {
                    idle_s = 0;
                } else {
                    idle_s += 2;
                    if idle_s >= IDLE_QUIT_S || !app.core.has_track() {
                        break;
                    }
                }
            }
        }
    }
    if let Some(mut c) = app.window.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = c.kill();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_kind_follows_the_binary_name() {
        assert_eq!(Browser::from_path("/usr/bin/firefox".into()), Browser::Firefox("/usr/bin/firefox".into()));
        assert_eq!(Browser::from_path("/opt/librewolf/librewolf".into()), Browser::Firefox("/opt/librewolf/librewolf".into()));
        assert_eq!(Browser::from_path("/usr/bin/firefox-developer-edition".into()), Browser::Firefox("/usr/bin/firefox-developer-edition".into()));
        assert_eq!(Browser::from_path("/usr/bin/chromium".into()), Browser::Chromium("/usr/bin/chromium".into()));
        assert_eq!(Browser::from_path("/usr/bin/brave".into()), Browser::Chromium("/usr/bin/brave".into()));
    }

    #[test]
    fn firefox_profile_enables_user_chrome() {
        let dir = std::env::temp_dir().join(format!("bc-desktop-ff-{}", std::process::id()));
        firefox_profile(&dir);
        let js = std::fs::read_to_string(dir.join("user.js")).unwrap();
        assert!(js.contains(r#"user_pref("toolkit.legacyUserProfileCustomizations.stylesheets", true);"#));
        assert!(js.contains(r#"user_pref("media.autoplay.default", 0);"#));
        let css = std::fs::read_to_string(dir.join("chrome/userChrome.css")).unwrap();
        assert!(css.contains("#nav-bar"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
