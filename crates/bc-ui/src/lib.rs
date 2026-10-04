//! bc-ui: Leptos CSR app (PLAN §10).
// Stylistic lints that fight view!-heavy UI code (nested `if let` in event handlers, signal handles that are Copy).
#![allow(clippy::collapsible_if, clippy::type_complexity, clippy::too_many_arguments, clippy::clone_on_copy, clippy::redundant_closure, clippy::unnecessary_to_owned, clippy::unnecessary_sort_by, clippy::manual_inspect)]
pub mod api;
pub mod app;
pub mod data;
pub mod ds;
pub mod history;
pub mod logic;
pub mod nav;
pub mod palette;
pub mod pages;
pub mod player;
pub mod prefs;
pub mod shell;
pub mod shortcuts;
pub mod theme;
pub mod util;
pub mod widgets;

/// Entry point called by `main` (and by trunk's generated glue).
pub fn mount() {
    install_panic_overlay();
    let Some(code) = pages::settings::lan::link_code() else {
        leptos::mount::mount_to_body(app::App);
        return;
    };
    // A pairing link (the host's QR code): redeem it before anything else talks to the API.
    wasm_bindgen_futures::spawn_local(async move {
        let res = pages::settings::lan::redeem(&code).await;
        let loc = util::window().location();
        let clean = format!("{}{}", loc.pathname().unwrap_or_default(), loc.hash().unwrap_or_default());
        match res {
            // The token cookie is set: a fresh load without the code comes up paired.
            Ok(()) => {
                let _ = loc.replace(&clean);
            }
            Err(e) => {
                if let Ok(h) = util::window().history() {
                    let _ = h.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&clean));
                }
                leptos::mount::mount_to_body(app::App);
                ds::toast_err(&format!("Pairing failed: {}", e.detail.clone().unwrap_or_else(|| e.message())));
            }
        }
    });
}

/// A wasm panic cannot be caught per route (no unwinding), so a panic shows a recoverable
/// overlay with the message and a reload button instead of a silently dead page.
fn install_panic_overlay() {
    std::panic::set_hook(Box::new(|info| {
        console_error_panic_hook::hook(info);
        let msg = info.to_string();
        let doc = util::document();
        if doc.get_element_by_id("bc-panic").is_some() {
            return;
        }
        if let (Ok(el), Some(body)) = (doc.create_element("div"), doc.body()) {
            el.set_id("bc-panic");
            let _ = el.set_attribute(
                "style",
                "position:fixed;inset:0;z-index:1000;background:rgba(0,0,0,.82);display:grid;place-items:center;padding:24px;color:#f0f0f0;font-family:system-ui,sans-serif",
            );
            let html = format!(
                "<div style=\"max-width:560px;background:#141414;border:1px solid #ff4444;border-radius:10px;padding:20px\"><h2 style=\"margin:0 0 8px\">bc hit an internal error</h2><p style=\"opacity:.8;margin:0 0 12px\">The UI stopped. Your library and playback are not affected.</p><pre style=\"white-space:pre-wrap;font-size:12px;max-height:30vh;overflow:auto;opacity:.7\">{}</pre><button onclick=\"location.reload()\" style=\"margin-top:12px;padding:8px 16px;border-radius:6px;border:0;background:#01ff95;color:#001a10;font-weight:600;cursor:pointer\">Reload</button></div>",
                msg.replace('&', "&amp;").replace('<', "&lt;")
            );
            el.set_inner_html(&html);
            let _ = body.append_child(&el);
        }
    }));
}
