//! Synced UI preferences (`ui_state['ui.prefs']`, `bc_types::ui::UiPrefs`), mirrored to
//! localStorage for a flash-free start.
use bc_types::ui::{KEY_SETTINGS_UI, UiPrefs};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{ls_get, ls_set};

const LS_KEY: &str = "bc:ui:prefs";

#[derive(Clone, Copy)]
pub struct PrefsCtx {
    pub prefs: RwSignal<UiPrefs>,
}

pub fn provide_prefs() -> PrefsCtx {
    let initial = ls_get(LS_KEY).and_then(|s| serde_json::from_str::<UiPrefs>(&s).ok()).unwrap_or_default();
    let ctx = PrefsCtx { prefs: RwSignal::new(initial) };
    provide_context(ctx);
    let loaded = RwSignal::new(false);
    spawn_local(async move {
        if let Ok(p) = api::get::<UiPrefs>(&format!("/ui-state/{KEY_SETTINGS_UI}")).await {
            ctx.prefs.set(p);
        }
        loaded.set(true);
    });
    // tag colours are a CSS switch on <html>
    Effect::new(move |_| {
        let on = ctx.prefs.with(|p| p.tag_colors);
        let root = crate::util::document_element();
        if on { let _ = root.set_attribute("data-tag-colors", "1"); } else { let _ = root.remove_attribute("data-tag-colors"); }
    });
    let timer = StoredValue::new(None::<i32>);
    Effect::new(move |_| {
        let p = ctx.prefs.get();
        if let Ok(s) = serde_json::to_string(&p) {
            ls_set(LS_KEY, &s);
        }
        if loaded.get_untracked() {
            use wasm_bindgen::JsCast;
            let w = crate::util::window();
            if let Some(id) = timer.get_value() {
                w.clear_timeout_with_handle(id);
            }
            let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
                spawn_local(async move {
                    let _ = api::call_json("PUT", &format!("/ui-state/{KEY_SETTINGS_UI}"), &p).await;
                });
            });
            timer.set_value(w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), 600).ok());
        }
    });
    ctx
}

pub fn use_prefs() -> PrefsCtx {
    expect_context::<PrefsCtx>()
}

/// Two-way bridge between a `UiPrefs` string field and a local signal usable by
/// `SegmentedControl` & co: reads follow the (synced) prefs, writes update them.
pub fn bind_pref(get: fn(&UiPrefs) -> String, set: fn(&mut UiPrefs, String)) -> RwSignal<String> {
    let prefs = use_prefs().prefs;
    let sig = RwSignal::new(prefs.with_untracked(get));
    Effect::new(move |_| {
        let v = prefs.with(get);
        if sig.get_untracked() != v {
            sig.set(v);
        }
    });
    Effect::new(move |_| {
        let v = sig.get();
        if prefs.with_untracked(get) != v {
            prefs.update(|p| set(p, v));
        }
    });
    sig
}

pub fn wave_style_pref() -> RwSignal<String> {
    bind_pref(|p| if p.waveform_style.is_empty() { "rgb".into() } else { p.waveform_style.clone() }, |p, v| p.waveform_style = v)
}
