//! macOS front end: the UI in a native window with WebKit's web view (WKWebView, through tao and
//! wry). It gives bc its own dock icon, About box and menu bar and needs no other browser.
//!
//! Mac behaviour:
//! * Closing the window (⌘W) hides it and music keeps playing; the dock icon brings it back. With
//!   nothing playing the app quits as on Linux: right away without a track, else after
//!   [`IDLE_QUIT_S`]. ⌘Q quits.
//! * The Edit menu is what makes ⌘C/⌘V/⌘A work in text fields: WKWebView only gets them through
//!   menu items.
//! * The Controls menu takes the place of the Linux tray menu (play/pause, next track); media keys
//!   and Now Playing come from the engine (souvlaki), which needs this window's run loop.
//! * Links that leave the app (Open on Bandcamp, track exports) open in the default browser.
//!   Downloads started by the page go to ~/Downloads.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use muda::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::{NewWindowResponse, WebViewBuilder};

use crate::{Core, IDLE_QUIT_S, shutdown};

/// Bundle identifier, as in `packaging/macos/Info.plist`.
const BUNDLE_ID: &str = "io.github.birkmann.bc";

enum UserEvent {
    Show,
    Quit,
    Tick,
    Title(String),
    Menu(MenuEvent),
}

/// Open `url` in the default browser.
fn open_external(url: &str) {
    if let Err(e) = Command::new("/usr/bin/open").arg(url).spawn() {
        tracing::warn!("could not open {url}: {e}");
    }
}

/// Apps started from Finder or the dock get launchd's PATH (`/usr/bin:/bin:/usr/sbin:/sbin`), so
/// Homebrew's `ffmpeg` and `bandcamp-dl` would not be found: add its prefixes. Called first thing
/// in `main`, before any other thread exists.
pub fn extend_path() {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<_> = std::env::split_paths(&path).collect();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
        if !dirs.iter().any(|d| d.as_os_str() == extra) {
            dirs.push(extra.into());
        }
    }
    if let Ok(joined) = std::env::join_paths(dirs) {
        // SAFETY: single-threaded at this point (see above).
        unsafe { std::env::set_var("PATH", joined) };
    }
}

/// Run the window on the main thread until the app quits. Never returns on success.
pub fn run(rt: tokio::runtime::Runtime, core: Arc<Core>) -> anyhow::Result<()> {
    // Notifications show bc's name and icon only when they are sent as the app bundle.
    let _ = notify_rust::set_application(BUNDLE_ID);

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Signals and the idle clock run on the runtime and reach the window through the proxy.
    let p = proxy.clone();
    rt.spawn(async move {
        let (Ok(mut term), Ok(mut usr1)) = (
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()),
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()),
        ) else {
            return;
        };
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            let ev = tokio::select! {
                _ = term.recv() => UserEvent::Quit,
                _ = tokio::signal::ctrl_c() => UserEvent::Quit,
                _ = usr1.recv() => UserEvent::Show,
                _ = tick.tick() => UserEvent::Tick,
            };
            if p.send_event(ev).is_err() {
                break;
            }
        }
    });
    let p = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = p.send_event(UserEvent::Menu(e));
    }));

    let about = AboutMetadata {
        name: Some("bc".into()),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        comments: Some("Bandcamp library & DJ tool".into()),
        copyright: Some("The bc-01 contributors · MIT".into()),
        website: Some("https://github.com/birkmann/bc-01".into()),
        ..Default::default()
    };
    let quit = MenuItem::new("Quit bc", true, "CmdOrCtrl+Q".parse().ok());
    let reload = MenuItem::new("Reload", true, "CmdOrCtrl+R".parse().ok());
    // No accelerators: Space and the arrow keys belong to the page (and to text fields).
    let toggle = MenuItem::new("Play / Pause", true, None);
    let next = MenuItem::new("Next Track", true, None);
    let window_menu = Submenu::with_items(
        "Window",
        true,
        &[&PredefinedMenuItem::minimize(None), &PredefinedMenuItem::maximize(None), &PredefinedMenuItem::separator(), &PredefinedMenuItem::close_window(None)],
    )?;
    let menu = Menu::with_items(&[
        &Submenu::with_items(
            "bc",
            true,
            &[
                &PredefinedMenuItem::about(None, Some(about)),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::services(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &PredefinedMenuItem::separator(),
                &quit,
            ],
        )?,
        &Submenu::with_items(
            "Edit",
            true,
            &[
                &PredefinedMenuItem::undo(None),
                &PredefinedMenuItem::redo(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::cut(None),
                &PredefinedMenuItem::copy(None),
                &PredefinedMenuItem::paste(None),
                &PredefinedMenuItem::select_all(None),
            ],
        )?,
        &Submenu::with_items("View", true, &[&reload, &PredefinedMenuItem::separator(), &PredefinedMenuItem::fullscreen(None)])?,
        &Submenu::with_items("Controls", true, &[&toggle, &next])?,
        &window_menu,
    ])?;

    let window = WindowBuilder::new()
        .with_title("bc")
        .with_inner_size(LogicalSize::new(1360.0, 860.0))
        .with_min_inner_size(LogicalSize::new(720.0, 480.0))
        .build(&event_loop)?;

    let origin = core.url.clone();
    let p = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_url(&core.url)
        // The UI's background (`theme-color`), so the window does not flash white while loading.
        .with_background_color((8, 8, 8, 255))
        .with_autoplay(true)
        .with_accept_first_mouse(true)
        .with_navigation_handler(move |url| {
            if url.starts_with(&origin) || url.starts_with("blob:") || url.starts_with("about:") {
                return true;
            }
            open_external(&url);
            false
        })
        // target=_blank links and window.open: Bandcamp pages and the track exports, which the
        // browser downloads from the local server.
        .with_new_window_req_handler(|url, _| {
            open_external(&url);
            NewWindowResponse::Deny
        })
        .with_document_title_changed_handler(move |title| {
            let _ = p.send_event(UserEvent::Title(title));
        })
        .build(&window)?;

    // Seconds spent with the window closed and nothing playing; quit after IDLE_QUIT_S.
    let mut idle_s = 0u64;
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        // Owned by the loop so they live as long as the app.
        let _ = (&rt, &webview, &menu);
        let show = |idle_s: &mut u64| {
            *idle_s = 0;
            window.set_visible(true);
            window.set_minimized(false);
            window.set_focus();
        };
        match event {
            Event::NewEvents(StartCause::Init) => {
                menu.init_for_nsapp();
                window_menu.set_as_windows_menu_for_nsapp();
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                idle_s = 0;
                window.set_visible(false);
            }
            Event::Reopen { .. } | Event::UserEvent(UserEvent::Show) => show(&mut idle_s),
            // LoopDestroyed: quit from the dock, a logout or `osascript -e 'quit app "bc"'`.
            Event::UserEvent(UserEvent::Quit) | Event::LoopDestroyed => shutdown(),
            Event::UserEvent(UserEvent::Title(title)) => window.set_title(&title),
            Event::UserEvent(UserEvent::Tick) => {
                // Window closed: keep playing in the background; once nothing has played for a
                // while (or right away if nothing was playing when it closed), quit.
                if window.is_visible() || core.playing() {
                    idle_s = 0;
                } else {
                    idle_s += 2;
                    if idle_s >= IDLE_QUIT_S || !core.has_track() {
                        shutdown();
                    }
                }
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                if e.id == quit.id() {
                    shutdown();
                } else if e.id == reload.id() {
                    let _ = webview.reload();
                } else if e.id == toggle.id() {
                    let _guard = rt.enter();
                    core.player_cmd(serde_json::json!({ "cmd": "toggle" }));
                } else if e.id == next.id() {
                    let _guard = rt.enter();
                    core.player_cmd(serde_json::json!({ "cmd": "next" }));
                }
            }
            _ => {}
        }
    })
}
