//! Library work that should not be walked away from (a move, a scan, an import): tracked from
//! `/library/tasks` and the `library.task.*` events, and guarded by a "leave this page?" prompt
//! when the window is closed or reloaded meanwhile. The server keeps going in the background
//! either way (the desktop shell stays up while it is busy); the prompt is the reminder.
use std::collections::BTreeSet;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;

use crate::api;
use crate::data::use_topic;

/// The fields of a library task this needs.
#[derive(Debug, Clone, Deserialize)]
struct Task {
    id: String,
    kind: String,
    state: String,
}

fn guarded(t: &Task) -> bool {
    matches!(t.kind.as_str(), "move" | "scan" | "import")
}

/// Ids of the running guarded tasks. Install once, from the app root.
pub fn install() -> RwSignal<BTreeSet<String>> {
    let running = RwSignal::new(BTreeSet::<String>::new());
    let resync = move || {
        spawn_local(async move {
            if let Ok(list) = api::get::<Vec<Task>>("/library/tasks").await {
                running.set(list.into_iter().filter(|t| guarded(t) && t.state == "running").map(|t| t.id).collect());
            }
        });
    };
    resync();
    use_topic::<Task>("library.task.started", move |t| {
        if guarded(&t) {
            running.update(|s| {
                s.insert(t.id);
            });
        }
    });
    use_topic::<Task>("library.task.done", move |t| {
        running.update(|s| {
            s.remove(&t.id);
        });
    });
    // after a reconnect (the server may have restarted, and resumed a move) the events in between are gone
    Effect::new(move |was: Option<bool>| {
        let now = crate::data::ws_connected().get();
        if now && was == Some(false) {
            resync();
        }
        now
    });

    let on_unload = Closure::<dyn FnMut(web_sys::Event)>::new(move |ev: web_sys::Event| {
        if !running.with_untracked(BTreeSet::is_empty) {
            // Chrome wants preventDefault, older engines a returnValue; the text is the browser's own.
            ev.prevent_default();
            let _ = js_sys::Reflect::set(&ev, &"returnValue".into(), &"".into());
        }
    });
    let _ = crate::util::window().add_event_listener_with_callback("beforeunload", on_unload.as_ref().unchecked_ref());
    on_unload.forget();
    running
}
