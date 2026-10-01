//! Global keyboard shortcuts, scoped so they never fire while focus is in an
//! input, select, textarea, button or contenteditable (logic in `logic::shortcuts`).
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::app::{Panel, use_app};
use crate::logic::shortcuts::{Mods, Shortcut, focus_swallows_keys, map_key};
use crate::player::use_player;
use crate::util::window;

pub fn install() {
    let app = use_app();
    let player = use_player();
    let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |ev: web_sys::KeyboardEvent| {
        if ev.default_prevented() || ev.is_composing() {
            return;
        }
        let target = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok());
        let swallow = target
            .map(|el| {
                let ce = el
                    .dyn_ref::<web_sys::HtmlElement>()
                    .map(|h| h.is_content_editable())
                    .unwrap_or(false);
                let role = el.get_attribute("role");
                focus_swallows_keys(&el.tag_name(), ce, role.as_deref())
            })
            .unwrap_or(false);
        let mods = Mods { shift: ev.shift_key(), ctrl: ev.ctrl_key(), meta: ev.meta_key(), alt: ev.alt_key() };
        let Some(sc) = map_key(&ev.key(), mods, swallow) else { return };
        let consume = match sc {
            Shortcut::TogglePlay => {
                player.toggle();
                true
            }
            Shortcut::Next => {
                player.next();
                true
            }
            Shortcut::Previous => {
                player.previous();
                true
            }
            Shortcut::SeekForward => {
                player.seek_relative(5.0);
                true
            }
            Shortcut::SeekBack => {
                player.seek_relative(-5.0);
                true
            }
            Shortcut::TogglePlanner => {
                app.toggle_panel(Panel::Plan);
                true
            }
            Shortcut::ToggleSimilar => {
                app.toggle_panel(Panel::Similar);
                true
            }
            Shortcut::CutTransition => {
                player.cmd(bc_types::player::PlayerCommand::CutNow);
                true
            }
            Shortcut::ToggleDeck => {
                app.deck_open.update(|v| *v = !*v);
                true
            }
            Shortcut::FocusSearch => {
                app.focus_search.update(|n| *n += 1);
                true
            }
            Shortcut::Palette => {
                app.palette_open.update(|v| *v = !*v);
                true
            }
            Shortcut::Escape => {
                if app.palette_open.get_untracked() {
                    app.palette_open.set(false);
                    true
                } else if app.deck_open.get_untracked() {
                    app.deck_open.set(false);
                    true
                } else {
                    false
                }
            }
        };
        if consume {
            ev.prevent_default();
        }
    });
    let _ = window().add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
    // The shell lives for the whole session; leak the closure on purpose.
    cb.forget();
}
