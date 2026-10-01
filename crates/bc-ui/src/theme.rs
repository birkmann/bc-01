//! Theme state. The palette lives in CSS custom properties (style/tokens.css);
//! this store decides which `ThemeSpec` is active and writes its overrides inline
//! on `<html>`, so a switch never remounts anything. Persisted server-side in
//! `ui_state['themes']` (shared by desktop and phone), mirrored to localStorage
//! for a flash-free first paint.
use bc_types::theme::*;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::util::{document_element, entropy, ls_get, ls_set};

const LS_KEY: &str = "bc:theme:v3";
const MAX_USER_THEMES: usize = 50;

#[derive(Clone, Copy)]
pub struct ThemeCtx {
    pub store: RwSignal<ThemeStore>,
    /// In-progress edit, applied live, never persisted until saved.
    pub draft: RwSignal<Option<ThemeSpec>>,
}

pub fn find_theme(store: &ThemeStore, id: &str) -> Option<ThemeSpec> {
    find_builtin(id).or_else(|| store.themes.iter().find(|t| t.id == id).cloned())
}

pub fn builtin_for(mode: Mode) -> ThemeSpec {
    find_builtin(if mode == Mode::Dark { "builtin:dark" } else { "builtin:light" }).expect("builtin")
}

pub fn active_spec(store: &ThemeStore, draft: &Option<ThemeSpec>) -> ThemeSpec {
    if let Some(d) = draft {
        return d.clone();
    }
    let id = if store.mode == Mode::Dark { &store.dark } else { &store.light };
    find_theme(store, id).unwrap_or_else(|| builtin_for(store.mode))
}

impl ThemeCtx {
    pub fn active(&self) -> ThemeSpec {
        active_spec(&self.store.get(), &self.draft.get())
    }
    pub fn active_untracked(&self) -> ThemeSpec {
        active_spec(&self.store.get_untracked(), &self.draft.get_untracked())
    }
    pub fn toggle_mode(&self) {
        self.draft.set(None);
        self.store.update(|s| s.mode = if s.mode == Mode::Dark { Mode::Light } else { Mode::Dark });
    }
    pub fn select(&self, id: &str) {
        let Some(spec) = find_theme(&self.store.get_untracked(), id) else { return };
        self.draft.set(None);
        self.store.update(|s| {
            s.mode = spec.base;
            if spec.base == Mode::Dark { s.dark = spec.id.clone() } else { s.light = spec.id.clone() }
        });
    }
    pub fn save_draft(&self, as_new: bool) -> Option<String> {
        let d = self.draft.get_untracked()?;
        let fork = as_new || is_builtin_id(&d.id);
        let mut spec = d;
        if fork {
            spec.id = new_theme_id(entropy());
        }
        let id = spec.id.clone();
        self.store.update(|s| {
            s.themes.retain(|t| t.id != spec.id);
            s.themes.push(spec.clone());
            let n = s.themes.len();
            if n > MAX_USER_THEMES {
                s.themes.drain(0..n - MAX_USER_THEMES);
            }
            s.mode = spec.base;
            if spec.base == Mode::Dark { s.dark = spec.id.clone() } else { s.light = spec.id.clone() }
        });
        self.draft.set(None);
        Some(id)
    }
    pub fn delete(&self, id: &str) {
        if is_builtin_id(id) {
            return;
        }
        self.store.update(|s| {
            s.themes.retain(|t| t.id != id);
            if s.dark == id {
                s.dark = "builtin:dark".into();
            }
            if s.light == id {
                s.light = "builtin:light".into();
            }
        });
        if self.draft.get_untracked().map(|d| d.id == id).unwrap_or(false) {
            self.draft.set(None);
        }
    }
    pub fn import(&self, json: &str) -> Result<String, String> {
        let spec = parse_theme_json(json, entropy())?;
        let id = spec.id.clone();
        self.store.update(|s| {
            s.themes.push(spec.clone());
            s.mode = spec.base;
            if spec.base == Mode::Dark { s.dark = spec.id.clone() } else { s.light = spec.id.clone() }
        });
        self.draft.set(None);
        Ok(id)
    }
}

fn default_store() -> ThemeStore {
    ThemeStore { dark: "builtin:dark".into(), light: "builtin:light".into(), mode: Mode::Dark, themes: vec![] }
}

/// Re-validate whatever came out of storage: a stale or hand-edited entry must
/// not be able to inject arbitrary CSS values.
fn sanitize(raw: ThemeStore) -> ThemeStore {
    let mut s = default_store();
    s.mode = raw.mode;
    if !raw.dark.is_empty() {
        s.dark = raw.dark;
    }
    if !raw.light.is_empty() {
        s.light = raw.light;
    }
    for t in raw.themes {
        if let Ok(v) = serde_json::to_value(&t) {
            if let Ok(ok) = parse_theme_value(&v, true, entropy()) {
                if !is_builtin_id(&ok.id) {
                    s.themes.push(ok);
                }
            }
        }
    }
    s
}

/// Write the spec to the document.
pub fn apply(spec: &ThemeSpec) {
    let root = document_element();
    let style = root.style();
    let _ = root.set_attribute("data-theme", if spec.base == Mode::Light { "light" } else { "dark" });
    let _ = root.set_attribute("data-density", spec.sizes.density.label());
    for (name, val) in css_vars(spec) {
        match val {
            Some(v) => {
                let _ = style.set_property(&name, &v);
            }
            None => {
                let _ = style.remove_property(&name);
            }
        }
    }
    sync_theme_color();
}

/// Tint the window frame (Chromium `--app` windows honour `theme-color`) with the header surface,
/// so frame, header and sidebar read as one strip in every theme.
fn sync_theme_color() {
    let Ok(Some(cs)) = crate::util::window().get_computed_style(&document_element()) else { return };
    let Ok(c) = cs.get_property_value("--color-surface-0") else { return };
    let c = c.trim();
    if c.is_empty() {
        return;
    }
    if let Ok(Some(meta)) = crate::util::document().query_selector("meta[name=theme-color]") {
        let _ = meta.set_attribute("content", c);
    }
}

/// Provide the theme context, apply the stored theme synchronously, then sync with the server.
pub fn provide_theme() -> ThemeCtx {
    let initial = ls_get(LS_KEY)
        .and_then(|s| serde_json::from_str::<ThemeStore>(&s).ok())
        .map(sanitize)
        .unwrap_or_else(default_store);
    let ctx = ThemeCtx { store: RwSignal::new(initial), draft: RwSignal::new(None) };
    provide_context(ctx);
    apply(&ctx.active_untracked());

    let loaded = RwSignal::new(false);
    // Live apply + persist.
    Effect::new(move |_| {
        let spec = ctx.active();
        apply(&spec);
    });
    Effect::new(move |_| {
        let store = ctx.store.get();
        if let Ok(s) = serde_json::to_string(&store) {
            ls_set(LS_KEY, &s);
            if loaded.get_untracked() {
                schedule_save(s);
            }
        }
    });
    // Server copy wins when it exists.
    spawn_local(async move {
        if let Ok(v) = api::get::<serde_json::Value>(&format!("/ui-state/{}", bc_types::ui::KEY_THEMES)).await {
            if let Ok(raw) = serde_json::from_value::<ThemeStore>(v) {
                ctx.store.set(sanitize(raw));
            }
        }
        loaded.set(true);
    });
    crate::data::ws::use_topic::<serde_json::Value>("ui_state.changed", move |v| {
        if v.get("key").and_then(|k| k.as_str()) == Some(bc_types::ui::KEY_THEMES) {
            spawn_local(async move {
                if let Ok(v) = api::get::<serde_json::Value>(&format!("/ui-state/{}", bc_types::ui::KEY_THEMES)).await {
                    if let Ok(raw) = serde_json::from_value::<ThemeStore>(v) {
                        let new = sanitize(raw);
                        if new != ctx.store.get_untracked() {
                            ctx.store.set(new);
                        }
                    }
                }
            });
        }
    });
    ctx
}

thread_local! {
    static SAVE_TIMER: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

fn schedule_save(json: String) {
    use wasm_bindgen::prelude::*;
    let w = crate::util::window();
    SAVE_TIMER.with(|t| {
        if let Some(id) = t.get() {
            w.clear_timeout_with_handle(id);
        }
        let cb = Closure::once_into_js(move || {
            spawn_local(async move {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
                    let _ = api::call_json("PUT", &format!("/ui-state/{}", bc_types::ui::KEY_THEMES), &v).await;
                }
            });
        });
        let id = w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), 600).ok();
        t.set(id);
    });
}

pub fn use_theme() -> ThemeCtx {
    expect_context::<ThemeCtx>()
}
