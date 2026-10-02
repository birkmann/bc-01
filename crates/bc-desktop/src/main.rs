//! bc desktop app (PLAN §3.2).
//!
//! What it does:
//! * Runs `bc_server` in-process: library, jobs, analysis, and the native audio engine with MPRIS
//!   media keys.
//! * Shows the UI in a chromeless app window of an installed browser, with its own profile dir and
//!   window class `bc` (so the dock shows the bc icon): a Chromium-family browser (`--app=URL`)
//!   when there is one, else Firefox (or a fork) with a private profile whose `userChrome.css`
//!   hides the tab strip and toolbars, else the default browser.
//! * Is single-instance. Launching it again while it runs just opens another window on the running
//!   server. `bc-desktop --quit` stops it.
//! * Closing the window quits, unless music is playing; then it keeps playing in the background.
//!   Reopen via the dock icon or the tray.
//! * Tray (StatusNotifierItem over D-Bus): show, play/pause, next, quit. Shows notifications when
//!   downloads finish.
//!
//! Why not an embedded WebKitGTK view (Tauri, or plain webkit2gtk)? webkit2gtk 2.52 was not usable
//! on Wayland + NVIDIA. Symbolised core dumps on 2026-10-01 showed three failures:
//! * a UI-process segfault in `AcceleratedBackingStore::update` whenever DMA-BUF or compositing
//!   was disabled;
//! * "Error 71" Wayland protocol errors with the defaults;
//! * blank white windows under XWayland.
//!
//! Chromium and Firefox render the same UI reliably. Nothing in the UI needs a JS bridge: it talks
//! HTTP/WS.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bc_core::Config;
use bc_server::{RunningServer, ServerOptions};
use bc_types::player::PlayerStatus;
use ksni::TrayMethods;

/// Runtime handshake file for single-instance behaviour: `{"pid":…, "url":"http://…"}`.
fn runtime_file() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("bc-desktop.json")
}

fn pid_alive(pid: i32) -> bool {
    pid > 0 && PathBuf::from(format!("/proc/{pid}")).exists()
}

/// `(pid, url)` of a running instance whose server answers /api/health.
fn running_instance() -> Option<(i32, String)> {
    let raw = std::fs::read_to_string(runtime_file()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let pid = v.get("pid")?.as_i64()? as i32;
    let url = v.get("url")?.as_str()?.to_string();
    (pid_alive(pid) && health_ok(&url)).then_some((pid, url))
}

/// Minimal blocking HTTP GET of `/api/health` (no client dependency in the launcher path).
fn health_ok(url: &str) -> bool {
    use std::io::{Read, Write};
    let Some(hostport) = url.strip_prefix("http://") else { return false };
    let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(hostport) else { return false };
    for addr in addrs {
        if let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(800)) {
            let _ = s.set_read_timeout(Some(Duration::from_millis(1500)));
            let req = format!("GET /api/health HTTP/1.0\r\nHost: {hostport}\r\n\r\n");
            if s.write_all(req.as_bytes()).is_ok() {
                let mut buf = [0u8; 16];
                if let Ok(n) = s.read(&mut buf) {
                    return buf[..n].starts_with(b"HTTP/1.1 200") || buf[..n].starts_with(b"HTTP/1.0 200");
                }
            }
        }
    }
    false
}

const CHROMIUM_NAMES: [&str; 7] =
    ["google-chrome-stable", "chromium", "brave", "google-chrome", "brave-browser", "microsoft-edge-stable", "vivaldi-stable"];
const FIREFOX_NAMES: [&str; 5] = ["firefox", "firefox-esr", "librewolf", "floorp", "waterfox"];

/// The browser that hosts the app window.
#[derive(Debug, Clone, PartialEq)]
enum Browser {
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
fn find_browser() -> Option<Browser> {
    if let Some(b) = std::env::var_os("BC_DESKTOP_BROWSER") {
        return Some(Browser::from_path(PathBuf::from(b)));
    }
    CHROMIUM_NAMES
        .iter()
        .find_map(|n| on_path(n))
        .map(Browser::Chromium)
        .or_else(|| FIREFOX_NAMES.iter().find_map(|n| on_path(n)).map(Browser::Firefox))
}

fn profile_dir(name: &str) -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("bc-rust").join(name)
}

/// Turn off the browser's own chrome for this private profile: translate offers (the UI is
/// English, the user's browser language may not be), password prompts, the desktop-themed frame. Only rewritten when the
/// browser is not running on the profile (Chrome rewrites Preferences on exit).
fn quiet_profile(profile: &std::path::Path) {
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

fn chromium_cmd(browser: PathBuf, url: &str) -> Command {
    let profile = profile_dir("window-profile");
    let _ = std::fs::create_dir_all(&profile);
    quiet_profile(&profile);
    let mut cmd = Command::new(browser);
    cmd.arg(format!("--app={url}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .args(["--class=bc", "--name=bc", "--no-first-run", "--no-default-browser-check", "--window-size=1360,860"])
        .args(["--disable-features=Translate,TranslateUI,MediaRouter", "--autoplay-policy=no-user-gesture-required"])
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

struct App {
    url: String,
    server: Arc<RunningServer>,
    window: Mutex<Option<Child>>,
    quit: tokio::sync::Notify,
    /// Mirrors the last `player.state` event: (status, has a current track).
    player: Mutex<(PlayerStatus, bool)>,
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
            let _ = open_window(&self.url);
            return;
        }
        let child = open_window(&self.url);
        *self.window.lock().unwrap_or_else(|e| e.into_inner()) = child;
    }

    fn playing(&self) -> bool {
        let (status, _) = *self.player.lock().unwrap_or_else(|e| e.into_inner());
        matches!(status, PlayerStatus::Playing | PlayerStatus::Loading)
    }

    fn has_track(&self) -> bool {
        self.player.lock().unwrap_or_else(|e| e.into_inner()).1
    }

    fn player_cmd(&self, cmd: serde_json::Value) {
        let server = self.server.clone();
        tokio::spawn(async move {
            if let Some(p) = &server.state.player {
                let _ = p.handle_command(cmd).await;
            }
        });
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
                activate: Box::new(|t: &mut Self| t.app.player_cmd(serde_json::json!({ "cmd": "toggle" }))),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Next track".into(),
                activate: Box::new(|t: &mut Self| t.app.player_cmd(serde_json::json!({ "cmd": "next" }))),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem { label: "Quit".into(), activate: Box::new(|t: &mut Self| t.app.quit.notify_one()), ..Default::default() }.into(),
        ]
    }
}

/// Keep `App::player` in step with the engine's `player.state` events.
fn spawn_player_watch(app: Arc<App>) {
    let mut rx = app.server.state.bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) if ev.topic == "player.state" => {
                    let status = ev
                        .payload
                        .get("status")
                        .cloned()
                        .and_then(|v| serde_json::from_value::<PlayerStatus>(v).ok())
                        .unwrap_or(PlayerStatus::Idle);
                    let has = ev.payload.get("current").is_some_and(|c| !c.is_null());
                    *app.player.lock().unwrap_or_else(|e| e.into_inner()) = (status, has);
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    });
}

/// Notify when a download job settles (watches the same event bus the UI uses).
fn spawn_notifier(server: Arc<RunningServer>) {
    let mut rx = server.state.bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) if ev.topic == "job.progress" => {
                    let status = ev.payload.get("status").and_then(|s| s.as_str()).unwrap_or("");
                    let total = ev.payload.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
                    let done = ev.payload.get("completed").and_then(|v| v.as_i64()).unwrap_or(0);
                    if status == "completed" && total > 0 {
                        let body = format!("{done} of {total} items downloaded");
                        tokio::task::spawn_blocking(move || {
                            let _ = notify_rust::Notification::new()
                                .appname("bc")
                                .summary("Download finished")
                                .body(&body)
                                .icon("bc")
                                .show();
                        });
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    });
}

async fn start_server() -> anyhow::Result<RunningServer> {
    // Prefer a stable port (the window's origin keys its local storage); fall back to any port.
    let config = Config::from_env();
    match bc_server::start(config.clone(), ServerOptions::from_env()).await {
        Ok(s) => Ok(s),
        Err(e) => {
            tracing::warn!("port {} unavailable ({e}); using a free port", config.port);
            let mut config = config;
            config.port = 0;
            bc_server::start(config, ServerOptions::from_env()).await
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber_init();
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--quit") {
        if let Some((pid, _)) = running_instance() {
            // SAFETY: plain kill(2) on a pid we read from our own runtime file.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        return Ok(());
    }
    // Single instance: a second launch asks the running app to show its window.
    if let Some((pid, _)) = running_instance() {
        // SAFETY: plain kill(2) on a pid we read from our own runtime file.
        unsafe { libc::kill(pid, libc::SIGUSR1) };
        return Ok(());
    }

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let server = Arc::new(start_server().await?);
        let url = server.url();
        let _ = std::fs::write(runtime_file(), serde_json::json!({ "pid": std::process::id(), "url": url }).to_string());
        tracing::info!(%url, "bc desktop up");

        let app = Arc::new(App {
            url: url.clone(),
            server: server.clone(),
            window: Mutex::new(None),
            quit: tokio::sync::Notify::new(),
            player: Mutex::new((PlayerStatus::Idle, false)),
        });
        spawn_player_watch(app.clone());
        app.show();
        spawn_notifier(server.clone());
        // Without a tray host (e.g. GNOME without the AppIndicator extension) this just fails.
        let _tray = BcTray { app: app.clone() }.spawn().await.map_err(|e| tracing::warn!("no tray: {e}")).ok();

        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut usr1 = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())?;
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        // Seconds spent with no window and nothing playing; quit after IDLE_QUIT_S.
        const IDLE_QUIT_S: u64 = 300;
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
                    if open || app.playing() || find_browser().is_none() {
                        idle_s = 0;
                    } else {
                        idle_s += 2;
                        if idle_s >= IDLE_QUIT_S || !app.has_track() {
                            break;
                        }
                    }
                }
            }
        }
        if let Some(mut c) = app.window.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
        }
        let _ = std::fs::remove_file(runtime_file());
        anyhow::Ok(())
    })?;
    // Background threads (file watcher, audio engine, blocking pool) never finish on their own;
    // dropping the runtime would wait for them forever. Every DB write has already committed.
    std::process::exit(0);
}

fn tracing_subscriber_init() {
    // bc-server installs its own subscriber when run via `bc serve`; the desktop app only needs
    // warnings on stderr, which the default (no subscriber) drops. Keep it dependency-free.
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
