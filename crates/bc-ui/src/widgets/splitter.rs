//! Column resize handle: drag (or arrow keys) to size the neighbouring column, double-click to
//! reset. The width is a per-device convenience, kept in localStorage.
use leptos::prelude::*;

/// Where the resized column sits relative to the handle.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Bounds, default and storage key of one resizable column.
#[derive(Clone, Copy)]
pub struct ColSize {
    pub key: &'static str,
    pub default: f64,
    pub min: f64,
    pub max: f64,
}

impl ColSize {
    pub fn clamp(&self, w: f64) -> f64 {
        w.clamp(self.min, self.max).round()
    }

    /// The saved width (or the default) as a signal.
    pub fn signal(&self) -> RwSignal<f64> {
        let saved = crate::util::ls_get(self.key).and_then(|s| s.parse::<f64>().ok()).filter(|w| w.is_finite());
        RwSignal::new(saved.map(|w| self.clamp(w)).unwrap_or(self.default))
    }

    fn save(&self, w: f64) {
        crate::util::ls_set(self.key, &format!("{w:.0}"));
    }
}

const KEY_STEP: f64 = 16.0;

#[component]
pub fn Splitter(width: RwSignal<f64>, size: ColSize, side: Side, #[prop(into)] label: String, #[prop(optional, into)] class: String) -> impl IntoView {
    // pointer x and column width at grab
    let grab = StoredValue::new(None::<(f64, f64)>);
    let dragging = RwSignal::new(false);
    let dir = if side == Side::Left { 1.0 } else { -1.0 };
    let root = crate::util::document_element;
    let end = move || {
        if grab.get_value().is_some() {
            grab.set_value(None);
            dragging.set(false);
            let _ = root().class_list().remove_1("col-resizing");
            size.save(width.get_untracked());
        }
    };
    on_cleanup(move || {
        let _ = root().class_list().remove_1("col-resizing");
    });
    view! {
        <div class=format!("splitter {class}") class:dragging=move || dragging.get()
            role="separator" aria-orientation="vertical" aria-label=label tabindex="0"
            aria-valuemin=size.min.to_string() aria-valuemax=size.max.to_string() aria-valuenow=move || width.get().to_string()
            title="Drag to resize · double-click to reset"
            on:pointerdown=move |ev: web_sys::PointerEvent| {
                if ev.button() != 0 {
                    return;
                }
                ev.prevent_default();
                if let Some(el) = ev.current_target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()) {
                    let _ = el.set_pointer_capture(ev.pointer_id());
                }
                grab.set_value(Some((ev.client_x() as f64, width.get_untracked())));
                dragging.set(true);
                let _ = root().class_list().add_1("col-resizing");
            }
            on:pointermove=move |ev: web_sys::PointerEvent| {
                if let Some((x0, w0)) = grab.get_value() {
                    width.set(size.clamp(w0 + dir * (ev.client_x() as f64 - x0)));
                }
            }
            on:pointerup=move |_| end()
            on:pointercancel=move |_| end()
            on:dblclick=move |_| {
                width.set(size.default);
                size.save(size.default);
            }
            on:keydown=move |ev: web_sys::KeyboardEvent| {
                let delta = match ev.key().as_str() {
                    "ArrowLeft" => -KEY_STEP * dir,
                    "ArrowRight" => KEY_STEP * dir,
                    "Home" => f64::NEG_INFINITY,
                    "End" => f64::INFINITY,
                    _ => return,
                };
                ev.prevent_default();
                let w = size.clamp(width.get_untracked() + delta);
                width.set(w);
                size.save(w);
            }>
        </div>
    }
}
