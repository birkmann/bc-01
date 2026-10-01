//! Anchored popover placement + Tooltip.
use leptos::portal::Portal;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::util::window;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn of(el: &web_sys::Element) -> Self {
        let r = el.get_bounding_client_rect();
        Rect { left: r.left(), top: r.top(), width: r.width(), height: r.height() }
    }
    pub fn point(x: f64, y: f64) -> Self {
        Rect { left: x, top: y, width: 0.0, height: 0.0 }
    }
}

/// Inline style placing a `position: fixed` popover under (or above) the anchor,
/// clamped to the viewport.
pub fn place(anchor: Rect, min_w: f64, est_w: f64, est_h: f64, vw: f64, vh: f64) -> String {
    let w = est_w.max(min_w);
    let mut left = anchor.left;
    if left + w > vw - 8.0 {
        left = (anchor.left + anchor.width - w).max(8.0);
    }
    let below = vh - (anchor.top + anchor.height) - 8.0;
    let above = anchor.top - 8.0;
    if below >= est_h || below >= above {
        let top = anchor.top + anchor.height + 4.0;
        format!("left:{left:.0}px;top:{top:.0}px;min-width:{min_w:.0}px;max-height:{:.0}px", (below - 4.0).max(120.0))
    } else {
        let bottom = vh - anchor.top + 4.0;
        format!("left:{left:.0}px;bottom:{bottom:.0}px;min-width:{min_w:.0}px;max-height:{:.0}px", (above - 4.0).max(120.0))
    }
}

pub fn viewport() -> (f64, f64) {
    let w = window();
    (
        w.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1024.0),
        w.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(768.0),
    )
}

/// Hover/focus tooltip around any inline content.
#[component]
pub fn Tip(#[prop(into)] text: String, children: Children) -> impl IntoView {
    let pos = RwSignal::new(None::<Rect>);
    let timer = StoredValue::new(None::<i32>);
    let anchor = NodeRef::<leptos::html::Span>::new();
    let show = move || {
        let w = window();
        if let Some(id) = timer.get_value() {
            w.clear_timeout_with_handle(id);
        }
        let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
            if let Some(Some(el)) = anchor.try_get_untracked() {
                pos.set(Some(Rect::of(el.unchecked_ref())));
            }
        });
        let id = w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), 450).ok();
        timer.set_value(id);
    };
    let hide = move || {
        if let Some(id) = timer.get_value() {
            window().clear_timeout_with_handle(id);
        }
        pos.set(None);
    };
    let t = StoredValue::new(text.clone());
    view! {
        <span node_ref=anchor class="tip-anchor" style="display:inline-flex"
            on:pointerenter=move |_| show() on:pointerleave=move |_| hide()
            on:focusin=move |_| show() on:focusout=move |_| hide()
            on:pointerdown=move |_| hide()>
            {children()}
        </span>
        {move || pos.get().map(|r| {
            let (vw, vh) = viewport();
            let style = place(r, 0.0, 200.0, 28.0, vw, vh);
            view! { <Portal><div class="tooltip" role="tooltip" style=style.clone()>{t.get_value()}</div></Portal> }
        })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_below_and_clamps_horizontally() {
        let a = Rect { left: 900.0, top: 100.0, width: 80.0, height: 30.0 };
        let s = place(a, 160.0, 220.0, 200.0, 1000.0, 800.0);
        assert!(s.contains("top:134px"), "{s}");
        // would overflow the right edge: right-aligned to the anchor
        assert!(s.contains("left:760px"), "{s}");
    }

    #[test]
    fn flips_above_when_no_room_below() {
        let a = Rect { left: 20.0, top: 700.0, width: 80.0, height: 30.0 };
        let s = place(a, 160.0, 200.0, 300.0, 1000.0, 800.0);
        assert!(s.contains("bottom:104px"), "{s}");
    }
}
