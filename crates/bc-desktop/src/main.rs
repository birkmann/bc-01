//! bc desktop app (PLAN §3.2).
//!
//! What it does:
//! * Runs `bc_server` in-process: library, jobs, analysis, and the native audio engine with MPRIS
//!   media keys (Now Playing on macOS).
//! * Shows the UI in a window:
//!   * Linux: a chromeless app window of an installed browser (`browser.rs`), with a tray
//!     (StatusNotifierItem over D-Bus): show, play/pause, next, quit.
//!   * macOS: a native window with WebKit's web view (`macos.rs`) and a menu bar, so bc has its own
//!     dock icon and needs no other browser.
//! * Is single-instance. Launching it again while it runs just opens another window on the running
//!   server. `bc-desktop --quit` stops it.
//! * Closing the window quits, unless music is playing; then it keeps playing in the background.
//!   Reopen via the dock icon or the tray.
//! * Shows notifications when downloads finish.
//! * Signs in to Bandcamp: Settings asks for a window on Bandcamp's own login page (a WKWebView
//!   window on macOS, a throwaway browser profile on Linux, `login.rs`); once the page has set the
//!   `identity` cookie it is stored like a pasted one and the window closes. bc never sees the
//!   password.
//!
//! Why not an embedded WebKitGTK view on Linux (Tauri, or plain webkit2gtk)? webkit2gtk 2.52 was
//! not usable on Wayland + NVIDIA. Symbolised core dumps on 2026-10-01 showed three failures:
//! * a UI-process segfault in `AcceleratedBackingStore::update` whenever DMA-BUF or compositing
//!   was disabled;
//! * "Error 71" Wayland protocol errors with the defaults;
//! * blank white windows under XWayland.
//!
//! Chromium and Firefox render the same UI reliably. macOS's own WebKit has none of these problems.
//! Nothing in the UI needs a JS bridge: it talks HTTP/WS.

#[cfg(not(target_os = "macos"))]
mod browser;
#[cfg(not(target_os = "macos"))]
mod login;
#[cfg(target_os = "macos")]
mod macos;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bc_core::Config;
use bc_server::{RunningServer, ServerOptions};
use bc_types::bandcamp::{BandcampLoginEvent, BandcampLoginState, TOPIC_BANDCAMP_LOGIN, TOPIC_BANDCAMP_LOGIN_REQUEST};
use bc_types::player::PlayerStatus;

/// With the window closed and nothing playing for this long, the app quits.
const IDLE_QUIT_S: u64 = 300;

/// Runtime handshake file for single-instance behaviour: `{"pid":…, "url":"http://…"}`.
fn runtime_file() -> PathBuf {
    // macOS has no XDG_RUNTIME_DIR; its temp dir is already per user.
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("bc-desktop.json")
}

fn pid_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists and may be signalled.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
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

/// What every front end shares: the in-process server and a mirror of the player state.
struct Core {
    url: String,
    server: Arc<RunningServer>,
    /// Mirrors the last `player.state` event: (status, has a current track).
    player: Mutex<(PlayerStatus, bool)>,
}

impl Core {
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

/// Where the sign-in window starts.
const BANDCAMP_LOGIN_URL: &str = "https://bandcamp.com/login";

/// The Cookie header for Bandcamp from a sign-in window's `(domain, name, value)` cookies, once
/// it holds `identity` (only a signed-in session has it). The companion cookies go along, as
/// with a pasted header.
fn bandcamp_cookie_header<'a>(cookies: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>) -> Option<String> {
    let pairs: Vec<String> = cookies
        .into_iter()
        .filter(|(domain, _, value)| {
            let d = domain.trim_start_matches('.');
            (d == "bandcamp.com" || d.ends_with(".bandcamp.com")) && !value.is_empty()
        })
        .map(|(_, name, value)| format!("{name}={value}"))
        .collect();
    pairs.iter().any(|p| p.starts_with("identity=")).then(|| pairs.join("; "))
}

impl Core {
    /// Tell the Settings page how the sign-in window is doing.
    fn login_event(&self, state: BandcampLoginState, detail: impl Into<String>) {
        self.server.state.bus.publish(TOPIC_BANDCAMP_LOGIN, &BandcampLoginEvent { state, detail: detail.into() });
    }

    /// Store a cookie header from the sign-in window. `true` once it is stored and Bandcamp did
    /// not reject it (the window may close); `false` keeps the window waiting.
    async fn try_login_cookie(&self, header: String) -> bool {
        let Some(bc) = &self.server.state.services.bandcamp else { return false };
        match bc.store_cookie(&header).await {
            Ok(s) if s.valid == Some(false) => false,
            Ok(s) => {
                let detail = s.username.map(|u| format!("Signed in as {u}")).unwrap_or(s.detail);
                self.login_event(BandcampLoginState::SignedIn, detail);
                true
            }
            Err(e) => {
                tracing::warn!("sign-in cookie not stored: {e}");
                false
            }
        }
    }
}

/// Run `open` whenever Settings asks for the sign-in window. Needs a tokio runtime context.
fn on_login_request(core: &Core, open: impl Fn() + Send + 'static) {
    let mut rx = core.server.state.bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) if ev.topic == TOPIC_BANDCAMP_LOGIN_REQUEST => open(),
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
        }
    });
}

/// Keep `Core::player` in step with the engine's `player.state` events.
fn spawn_player_watch(core: Arc<Core>) {
    let mut rx = core.server.state.bus.subscribe();
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
                    *core.player.lock().unwrap_or_else(|e| e.into_inner()) = (status, has);
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

fn server_options() -> ServerOptions {
    #[cfg(target_os = "macos")]
    let bandcamp_login = true;
    #[cfg(not(target_os = "macos"))]
    let bandcamp_login = login::available();
    ServerOptions { bandcamp_login, ..ServerOptions::from_env() }
}

async fn start_server() -> anyhow::Result<RunningServer> {
    // Prefer a stable port (the window's origin keys its local storage); fall back to any port.
    let config = Config::from_env();
    match bc_server::start(config.clone(), server_options()).await {
        Ok(s) => Ok(s),
        Err(e) => {
            tracing::warn!("port {} unavailable ({e}); using a free port", config.port);
            let mut config = config;
            config.port = 0;
            bc_server::start(config, server_options()).await
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber_init();
    #[cfg(target_os = "macos")]
    macos::extend_path();
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
    let core = rt.block_on(async {
        let server = Arc::new(start_server().await?);
        let url = server.url();
        let _ = std::fs::write(runtime_file(), serde_json::json!({ "pid": std::process::id(), "url": url }).to_string());
        tracing::info!(%url, "bc desktop up");

        let core = Arc::new(Core { url, server: server.clone(), player: Mutex::new((PlayerStatus::Idle, false)) });
        spawn_player_watch(core.clone());
        spawn_notifier(server);
        anyhow::Ok(core)
    })?;

    #[cfg(target_os = "macos")]
    macos::run(rt, core)?;
    #[cfg(not(target_os = "macos"))]
    rt.block_on(browser::run(core))?;
    shutdown()
}

/// Remove the handshake file and exit. Background threads (file watcher, audio engine, blocking
/// pool) never finish on their own; dropping the runtime would wait for them forever. Every DB
/// write has already committed.
fn shutdown() -> ! {
    let _ = std::fs::remove_file(runtime_file());
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
    fn cookie_header_needs_identity_and_keeps_only_bandcamp() {
        assert_eq!(bandcamp_cookie_header([(".bandcamp.com", "client_id", "1")]), None);
        assert_eq!(
            bandcamp_cookie_header([
                (".bandcamp.com", "client_id", "1"),
                ("www.google.com", "NID", "x"),
                ("bandcamp.com", "identity", "7%09abc"),
                (".bandcamp.com", "session", ""),
            ])
            .as_deref(),
            Some("client_id=1; identity=7%09abc")
        );
        assert_eq!(bandcamp_cookie_header([("notbandcamp.com", "identity", "x")]), None);
    }
}
