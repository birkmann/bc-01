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
//! * Signing in to Bandcamp opens a second window on Bandcamp's login page. Its web view is
//!   private (nothing persists, so each sign-in starts fresh and can switch accounts); its cookies
//!   are checked after every page load and every couple of seconds, and the window closes once
//!   the `identity` cookie is stored.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use muda::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder};
use wry::{NewWindowResponse, PageLoadEvent, WebView, WebViewBuilder};

use bc_types::bandcamp::BandcampLoginState;

use crate::{BANDCAMP_LOGIN_URL, Core, IDLE_QUIT_S, bandcamp_cookie_header, shutdown};

/// Bundle identifier, as in `packaging/macos/Info.plist`.
const BUNDLE_ID: &str = "io.github.birkmann.bc";

enum UserEvent {
    Show,
    Quit,
    Tick,
    Title(String),
    Menu(MenuEvent),
    /// Settings asked for the sign-in window.
    Login,
    /// A page finished loading in the sign-in window: look for the cookie.
    LoginCheck,
    /// The cookie handed over was stored (`true`) or rejected (`false`).
    LoginStored(bool),
}

/// The Bandcamp sign-in window. Dropping it closes the window.
struct LoginWindow {
    // Field order: the web view goes before its window.
    webview: WebView,
    window: Window,
    /// The last cookie header handed over, so a rejected one is not retried.
    tried: Option<String>,
    /// A handed-over cookie is being checked with Bandcamp.
    checking: bool,
}

impl LoginWindow {
    fn open(target: &EventLoopWindowTarget<UserEvent>, proxy: &EventLoopProxy<UserEvent>) -> anyhow::Result<Self> {
        let window = WindowBuilder::new()
            .with_title("Sign in to Bandcamp")
            .with_inner_size(LogicalSize::new(520.0, 760.0))
            .with_min_inner_size(LogicalSize::new(400.0, 480.0))
            .build(target)?;
        let p = proxy.clone();
        let webview = WebViewBuilder::new()
            .with_url(BANDCAMP_LOGIN_URL)
            .with_incognito(true)
            .with_accept_first_mouse(true)
            .with_on_page_load_handler(move |ev, _| {
                if matches!(ev, PageLoadEvent::Finished) {
                    let _ = p.send_event(UserEvent::LoginCheck);
                }
            })
            // "Forgot password" and help links: the default browser.
            .with_new_window_req_handler(|url, _| {
                open_external(&url);
                NewWindowResponse::Deny
            })
            .build(&window)?;
        Ok(Self { webview, window, tried: None, checking: false })
    }

    /// A Bandcamp cookie header with `identity` that has not been handed over yet.
    fn new_cookie(&mut self) -> Option<String> {
        if self.checking {
            return None;
        }
        let cookies = self.webview.cookies().ok()?;
        let header = bandcamp_cookie_header(cookies.iter().map(|c| (c.domain().unwrap_or(""), c.name(), c.value())))?;
        if self.tried.as_ref() == Some(&header) {
            return None;
        }
        self.tried = Some(header.clone());
        self.checking = true;
        Some(header)
    }
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

    {
        let _guard = rt.enter();
        let p = proxy.clone();
        crate::on_login_request(&core, move || {
            let _ = p.send_event(UserEvent::Login);
        });
    }

    // Seconds spent with the window closed and nothing playing; quit after IDLE_QUIT_S.
    let mut idle_s = 0u64;
    let mut login: Option<LoginWindow> = None;
    event_loop.run(move |event, target, control_flow| {
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
            Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. } if login.as_ref().is_some_and(|l| l.window.id() == window_id) => {
                login = None;
                core.login_event(BandcampLoginState::Closed, "");
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                idle_s = 0;
                window.set_visible(false);
            }
            Event::UserEvent(UserEvent::Login) => match &login {
                Some(l) => l.window.set_focus(),
                None => match LoginWindow::open(target, &proxy) {
                    Ok(l) => {
                        login = Some(l);
                        core.login_event(BandcampLoginState::Open, "");
                    }
                    Err(e) => core.login_event(BandcampLoginState::Failed, format!("Could not open the sign-in window: {e}")),
                },
            },
            Event::UserEvent(UserEvent::LoginCheck) => {
                if let Some(header) = login.as_mut().and_then(LoginWindow::new_cookie) {
                    let (core, p) = (core.clone(), proxy.clone());
                    rt.spawn(async move {
                        let ok = core.try_login_cookie(header).await;
                        let _ = p.send_event(UserEvent::LoginStored(ok));
                    });
                }
            }
            Event::UserEvent(UserEvent::LoginStored(ok)) => {
                if ok {
                    login = None;
                } else if let Some(l) = login.as_mut() {
                    l.checking = false;
                }
            }
            Event::Reopen { .. } | Event::UserEvent(UserEvent::Show) => show(&mut idle_s),
            // LoopDestroyed: quit from the dock, a logout or `osascript -e 'quit app "bc"'`.
            Event::UserEvent(UserEvent::Quit) | Event::LoopDestroyed => shutdown(),
            Event::UserEvent(UserEvent::Title(title)) => window.set_title(&title),
            Event::UserEvent(UserEvent::Tick) => {
                // Page loads cover a normal sign-in; this catches cookies set by scripts.
                if login.is_some() {
                    let _ = proxy.send_event(UserEvent::LoginCheck);
                }
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
