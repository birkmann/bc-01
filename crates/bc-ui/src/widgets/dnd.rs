//! Pointer-event drag and drop (works on touch). Sources call [`begin_drag`] from
//! `pointerdown`; the drag starts after a small movement threshold (long-press on
//! touch so scrolling still works). Targets are elements carrying
//! `data-dnd-target="<id>"`, registered with [`register_target`].
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::util::{document, window};

#[derive(Clone, Debug, PartialEq, Default)]
pub struct DragPayload {
    /// What is being dragged: "track", "release", "queue-row", "job-row", ...
    pub kind: String,
    pub ids: Vec<i64>,
    /// Shown in the drag ghost.
    pub label: String,
    /// Source row index for reorders.
    pub index: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DropInfo {
    /// Pointer is in the upper half of the hovered element (insert before).
    pub before: bool,
    /// `data-dnd-index` of the hovered element, when present.
    pub index: Option<usize>,
}

type DropFn = Rc<dyn Fn(DragPayload, DropInfo)>;

struct Target {
    accept: Vec<String>,
    on_drop: DropFn,
}

thread_local! {
    static TARGETS: RefCell<HashMap<String, Target>> = RefCell::new(HashMap::new());
    static OVER: std::cell::Cell<Option<RwSignal<Option<(String, DropInfo)>>>> = const { std::cell::Cell::new(None) };
    static ACTIVE: std::cell::Cell<Option<RwSignal<Option<DragPayload>>>> = const { std::cell::Cell::new(None) };
    static GHOST: RefCell<Option<web_sys::HtmlElement>> = const { RefCell::new(None) };
}

/// The target currently hovered by an active drag (for styling).
pub fn over() -> RwSignal<Option<(String, DropInfo)>> {
    OVER.with(|o| match o.get() {
        Some(s) => s,
        None => {
            let s = crate::util::root_signal(None);
            o.set(Some(s));
            s
        }
    })
}

/// The payload of the drag in progress, if any.
pub fn active() -> RwSignal<Option<DragPayload>> {
    ACTIVE.with(|o| match o.get() {
        Some(s) => s,
        None => {
            let s = crate::util::root_signal(None);
            o.set(Some(s));
            s
        }
    })
}

pub fn register_target(id: &str, accept: &[&str], on_drop: impl Fn(DragPayload, DropInfo) + 'static) {
    let id = id.to_string();
    TARGETS.with(|t| {
        t.borrow_mut().insert(id.clone(), Target { accept: accept.iter().map(|s| s.to_string()).collect(), on_drop: Rc::new(on_drop) });
    });
    on_cleanup(move || {
        TARGETS.with(|t| {
            t.borrow_mut().remove(&id);
        });
    });
}

/// Pure hit decision used by tests and the runtime: may `payload_kind` drop on a target accepting `accept`?
pub fn accepts(accept: &[String], payload_kind: &str) -> bool {
    accept.is_empty() || accept.iter().any(|a| a == "*" || a == payload_kind)
}

/// Movement (px) before a mouse/pen drag starts; touch needs a long-press.
const THRESHOLD: f64 = 6.0;
const LONG_PRESS_MS: i32 = 350;

fn target_at(x: f64, y: f64, kind: &str) -> Option<(String, DropInfo)> {
    let el = document().element_from_point(x as f32, y as f32)?;
    let mut cur = Some(el);
    while let Some(e) = cur {
        if let Some(id) = e.get_attribute("data-dnd-target") {
            let ok = TARGETS.with(|t| t.borrow().get(&id).map(|t| accepts(&t.accept, kind)).unwrap_or(false));
            if ok {
                let r = e.get_bounding_client_rect();
                let before = y < r.top() + r.height() / 2.0;
                let index = e.get_attribute("data-dnd-index").and_then(|s| s.parse().ok());
                return Some((id, DropInfo { before, index }));
            }
        }
        cur = e.parent_element();
    }
    None
}

fn make_ghost(label: &str) -> Option<web_sys::HtmlElement> {
    let g: web_sys::HtmlElement = document().create_element("div").ok()?.dyn_into().ok()?;
    g.set_class_name("dnd-ghost");
    g.set_text_content(Some(label));
    document().body()?.append_child(&g).ok()?;
    Some(g)
}

fn move_ghost(x: f64, y: f64) {
    GHOST.with(|g| {
        if let Some(g) = g.borrow().as_ref() {
            let _ = g.style().set_property("transform", &format!("translate({}px,{}px)", x + 12.0, y + 12.0));
        }
    });
}

fn end_drag() {
    GHOST.with(|g| {
        if let Some(g) = g.borrow_mut().take() {
            g.remove();
        }
    });
    over().set(None);
    active().set(None);
    let _ = document().body().map(|b| b.class_list().remove_1("dnd-dragging"));
}

/// Call from `pointerdown` on a drag handle / row.
pub fn begin_drag(ev: &web_sys::PointerEvent, payload: DragPayload) {
    if ev.button() != 0 && ev.pointer_type() == "mouse" {
        return;
    }
    let touch = ev.pointer_type() == "touch";
    let (sx, sy) = (ev.client_x() as f64, ev.client_y() as f64);
    let started = Rc::new(std::cell::Cell::new(false));
    let armed = Rc::new(std::cell::Cell::new(!touch)); // touch arms after the long press
    let payload = Rc::new(payload);
    let pointer_id = ev.pointer_id();

    let timer = if touch {
        let armed = armed.clone();
        let cb = Closure::once_into_js(move || armed.set(true));
        window().set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), LONG_PRESS_MS).ok()
    } else {
        None
    };

    // closures stored so they can remove themselves
    let slots: Rc<RefCell<Vec<(&'static str, Closure<dyn FnMut(web_sys::PointerEvent)>)>>> = Rc::new(RefCell::new(vec![]));
    let cleanup = {
        let slots = slots.clone();
        Rc::new(move || {
            for (name, c) in slots.borrow_mut().drain(..) {
                let _ = window().remove_event_listener_with_callback(name, c.as_ref().unchecked_ref());
            }
        })
    };

    let on_move = {
        let (started, armed, payload, cleanup) = (started.clone(), armed.clone(), payload.clone(), cleanup.clone());
        Closure::<dyn FnMut(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
            if e.pointer_id() != pointer_id {
                return;
            }
            let (x, y) = (e.client_x() as f64, e.client_y() as f64);
            if !started.get() {
                let moved = ((x - sx).powi(2) + (y - sy).powi(2)).sqrt();
                if !armed.get() {
                    // touch: moving before the long press means scrolling; give up
                    if moved > THRESHOLD * 2.0 {
                        cleanup();
                    }
                    return;
                }
                if moved < THRESHOLD {
                    return;
                }
                started.set(true);
                active().set(Some((*payload).clone()));
                let label = if payload.ids.len() > 1 { format!("{} ({})", payload.label, payload.ids.len()) } else { payload.label.clone() };
                GHOST.with(|g| *g.borrow_mut() = make_ghost(&label));
                let _ = document().body().map(|b| b.class_list().add_1("dnd-dragging"));
            }
            e.prevent_default();
            move_ghost(x, y);
            over().set(target_at(x, y, &payload.kind));
        })
    };
    let on_up = {
        let (started, payload, cleanup) = (started.clone(), payload.clone(), cleanup.clone());
        Closure::<dyn FnMut(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
            if e.pointer_id() != pointer_id {
                return;
            }
            if let Some(t) = timer {
                window().clear_timeout_with_handle(t);
            }
            if started.get() {
                if let Some((id, info)) = target_at(e.client_x() as f64, e.client_y() as f64, &payload.kind) {
                    let f = TARGETS.with(|t| t.borrow().get(&id).map(|t| t.on_drop.clone()));
                    if let Some(f) = f {
                        f((*payload).clone(), info);
                    }
                }
                // swallow the click that follows a drag
                let swallow = Closure::once_into_js(|e: web_sys::Event| {
                    e.stop_propagation();
                    e.prevent_default();
                });
                let opts = web_sys::AddEventListenerOptions::new();
                opts.set_once(true);
                opts.set_capture(true);
                let _ = window().add_event_listener_with_callback_and_add_event_listener_options("click", swallow.unchecked_ref(), &opts);
                crate::util::after(0, || {});
                end_drag();
            }
            cleanup();
        })
    };
    let on_cancel = {
        let cleanup = cleanup.clone();
        Closure::<dyn FnMut(web_sys::PointerEvent)>::new(move |_e: web_sys::PointerEvent| {
            end_drag();
            cleanup();
        })
    };
    for (name, c) in [("pointermove", on_move), ("pointerup", on_up), ("pointercancel", on_cancel)] {
        let _ = window().add_event_listener_with_callback(name, c.as_ref().unchecked_ref());
        slots.borrow_mut().push((name, c));
    }
}

/// Insert position for a reorder drop: `index` of the hovered row, adjusted by the half hint.
pub fn drop_index(info: DropInfo, len: usize) -> usize {
    match info.index {
        Some(i) => (if info.before { i } else { i + 1 }).min(len),
        None => len,
    }
}

/// Final index of an item moved from `from` to the insert slot `slot` (slot counted before removal).
pub fn reorder_target(from: usize, slot: usize) -> usize {
    if slot > from { slot - 1 } else { slot }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_rules() {
        assert!(accepts(&[], "track"));
        assert!(accepts(&["track".into()], "track"));
        assert!(!accepts(&["release".into()], "track"));
        assert!(accepts(&["*".into()], "anything"));
    }

    #[test]
    fn drop_slot_and_reorder_math() {
        let before = DropInfo { before: true, index: Some(3) };
        let after = DropInfo { before: false, index: Some(3) };
        assert_eq!(drop_index(before, 10), 3);
        assert_eq!(drop_index(after, 10), 4);
        assert_eq!(drop_index(DropInfo { before: false, index: None }, 10), 10);
        assert_eq!(drop_index(DropInfo { before: false, index: Some(9) }, 10), 10);
        // move row 1 to slot 4 (after row 3): ends at index 3
        assert_eq!(reorder_target(1, 4), 3);
        // move row 5 to slot 2: ends at index 2
        assert_eq!(reorder_target(5, 2), 2);
        assert_eq!(reorder_target(2, 2), 2);
        assert_eq!(reorder_target(2, 3), 2);
    }
}
