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
#[cfg(target_os = "macos")]
mod macos;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bc_core::Config;
use bc_server::{RunningServer, ServerOptions};
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
