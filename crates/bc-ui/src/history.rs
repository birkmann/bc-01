//! Browser-style history for the app window, which has no browser chrome: back / forward
//! buttons, a page "Back" link that returns to wherever you came from, and pages put back the
//! way they were left when you return to them (scroll position; Explore also keeps its feed).
//!
//! Built on the Navigation API (Chromium, Firefox 147+): each history entry has a stable key,
//! and every entry change says whether it was a traversal (back / forward) or a new page.
//! Without the API, back and forward still work; only the restoring is skipped.
//!
//! Timing note: leptos_router sets its URL signal *before* it calls `pushState` (a new path
//! even waits for the route to be ready), so a freshly mounted page cannot rely on
//! `navigation.currentEntry` yet. Anything the page itself records is keyed by its URL
//! instead (titles, Explore's feed); scroll is recorded from user scrolling, which only
//! happens once the entry is current.
use std::cell::RefCell;
use std::collections::HashMap;

use js_sys::Reflect;
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::util::{document, window};

/// Scroll containers a page scrolls in (the window itself never scrolls).
const SCROLLERS: &str = ".route-host .page-scroll, .route-host .cg-scroll";
/// How long a restore waits for the page's content to grow tall enough.
const RESTORE_MS: f64 = 2500.0;
/// Per-entry memory kept for this many entries (older ones are forgotten).
const KEEP: usize = 200;

/// A map that forgets its oldest keys past [`KEEP`].
struct Recent<V> {
    map: HashMap<String, V>,
    order: std::collections::VecDeque<String>,
}

impl<V> Default for Recent<V> {
    fn default() -> Self {
        Self { map: HashMap::new(), order: Default::default() }
    }
}

impl<V> Recent<V> {
    fn put(&mut self, key: String, v: V) {
        if self.map.insert(key.clone(), v).is_none() {
            self.order.push_back(key);
            if self.order.len() > KEEP {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
    fn get(&self, key: &str) -> Option<&V> {
        self.map.get(key)
    }
}

#[derive(Default)]
struct State {
    /// The URL the last back / forward arrived at (`None` after a new page).
    traversal: Option<String>,
    /// Scroll offset per history entry key.
    scroll: Recent<f64>,
    /// Page title per URL (`path?search`).
    titles: Recent<String>,
    /// Set by a traversal: the URL arrived at and the offset to put back.
    pending: Option<(String, f64)>,
    user_moved: bool,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    static REV: RwSignal<u64> = crate::util::root_signal(0);
}

fn rev() -> RwSignal<u64> {
    REV.with(|r| *r)
}

fn navigation() -> Option<JsValue> {
    Reflect::get(&window(), &"navigation".into()).ok().filter(|n| n.is_object())
}

fn get(o: &JsValue, k: &str) -> JsValue {
    Reflect::get(o, &k.into()).unwrap_or(JsValue::UNDEFINED)
}

fn current_entry() -> Option<JsValue> {
    navigation().map(|n| get(&n, "currentEntry")).filter(|e| e.is_object())
}

fn current_key() -> Option<String> {
    current_entry().and_then(|e| get(&e, "key").as_string())
}

/// `path?search` of an absolute or relative URL.
fn path_of(url: &str) -> String {
    web_sys::Url::new_with_base(url, "http://x").map(|u| format!("{}{}", u.pathname(), u.search())).unwrap_or_default()
}

fn here() -> String {
    let l = window().location();
    format!("{}{}", l.pathname().unwrap_or_default(), l.search().unwrap_or_default())
}

/// Wire the listeners; once, from the shell.
pub fn install() {
    if let Some(nav) = navigation() {
        let on_change = Closure::<dyn FnMut(JsValue)>::new(move |ev: JsValue| {
            let kind = get(&ev, "navigationType").as_string().unwrap_or_default();
            let traversal = kind == "traverse";
            let key = current_key();
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                s.traversal = traversal.then(here);
                // Read the target now: by the time the page re-renders it may have scrolled.
                s.pending = if traversal { key.and_then(|k| s.scroll.get(&k).copied()).map(|y| (here(), y)) } else { None };
            });
            rev().update(|n| *n += 1);
        });
        let _ = nav.unchecked_ref::<web_sys::EventTarget>().add_event_listener_with_callback("currententrychange", on_change.as_ref().unchecked_ref());
        on_change.forget();

        // Scroll events do not bubble; listen in the capture phase for every page scroller.
        let on_scroll = Closure::<dyn FnMut(web_sys::Event)>::new(move |ev: web_sys::Event| {
            let Some(el) = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
            if !el.matches(SCROLLERS).unwrap_or(false) {
                return;
            }
            if let Some(k) = current_key() {
                let y = el.scroll_top() as f64;
                STATE.with(|s| s.borrow_mut().scroll.put(k, y));
            }
        });
        let opts = web_sys::AddEventListenerOptions::new();
        opts.set_capture(true);
        opts.set_passive(true);
        let _ = document().add_event_listener_with_callback_and_add_event_listener_options("scroll", on_scroll.as_ref().unchecked_ref(), &opts);
        on_scroll.forget();
        // Any user input wins over a restore still waiting for content.
        let moved = Closure::<dyn FnMut()>::new(move || STATE.with(|s| s.borrow_mut().user_moved = true));
        for ev in ["wheel", "pointerdown", "keydown", "touchstart"] {
            let _ = window().add_event_listener_with_callback_and_add_event_listener_options(ev, moved.as_ref().unchecked_ref(), &opts);
        }
        moved.forget();
    } else {
        let on_pop = Closure::<dyn FnMut()>::new(move || rev().update(|n| *n += 1));
        let _ = window().add_event_listener_with_callback("popstate", on_pop.as_ref().unchecked_ref());
        on_pop.forget();
    }
}

/// Call on every route change (shell effect): bumps the back/forward state and, after a
/// traversal, scrolls the page back to where it was left.
pub fn on_location_change(prev_path: Option<&str>, path: &str) {
    rev().update(|n| *n += 1);
    let Some((url, y)) = STATE.with(|s| s.borrow_mut().pending.take()) else { return };
    if url != here() || y < 1.0 {
        return;
    }
    // A new path mounts a new page; until it has replaced the old one, the scroller found
    // would be the page being left.
    let host = document().query_selector(".route-host").ok().flatten();
    let old_page = (prev_path != Some(path)).then(|| host.as_ref().and_then(|h| h.first_element_child())).flatten();
    STATE.with(|s| s.borrow_mut().user_moved = false);
    let start = crate::util::perf_now();
    crate::util::raf_loop(move |_| {
        if STATE.with(|s| s.borrow().user_moved) || here() != url || crate::util::perf_now() - start > RESTORE_MS {
            return false;
        }
        if let (Some(old), Some(h)) = (&old_page, &host) {
            if h.first_element_child().as_ref() == Some(old) {
                return true;
            }
        }
        let Some(el) = document().query_selector(SCROLLERS).ok().flatten() else { return true };
        let target = y.round() as i32;
        if el.scroll_height() - el.client_height() < target {
            return true; // content still arriving
        }
        el.set_scroll_top(target);
        false
    });
}

/// Whether the page at `url` (`path?search`, from the router) was reached with back / forward.
/// Takes the router's URL because a page mounts before a push reaches the window's history:
/// at that moment the last navigation on record is the one before.
pub fn arrived_by_traversal(url: &str) -> bool {
    STATE.with(|s| s.borrow().traversal.as_deref() == Some(url))
}

/// Record the title a page shows, for "Back to …" labels. Keyed by the page's URL.
pub fn record_title(path: String, title: String) {
    if title.is_empty() {
        return;
    }
    STATE.with(|s| s.borrow_mut().titles.put(path, title));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Back,
    Forward,
}

/// Is there an in-app entry that way? Reactive (tracks entry changes).
pub fn can_go(dir: Dir) -> bool {
    rev().track();
    match navigation() {
        Some(n) => get(&n, if dir == Dir::Back { "canGoBack" } else { "canGoForward" }).as_bool().unwrap_or(false),
        None => window().history().map(|h| h.length().unwrap_or(0) > 1).unwrap_or(false),
    }
}

pub fn go(dir: Dir) {
    if let Ok(h) = window().history() {
        let _ = if dir == Dir::Back { h.back() } else { h.forward() };
    }
}

/// `path?search` of the neighbouring entry, when the Navigation API can tell. Reactive.
fn neighbour(dir: Dir) -> Option<String> {
    rev().track();
    let nav = navigation()?;
    let idx = get(&current_entry()?, "index").as_f64()? as i64 + if dir == Dir::Back { -1 } else { 1 };
    let entries = js_sys::Reflect::get(&nav, &"entries".into()).ok()?.dyn_into::<js_sys::Function>().ok()?.call0(&nav).ok()?;
    let e = js_sys::Array::from(&entries).get(u32::try_from(idx).ok()?);
    get(&e, "url").as_string().map(|u| path_of(&u))
}

/// What the neighbouring entry is called: its recorded title, else a reading of its URL.
/// Empty when it has no name; `None` when there is no in-app entry that way. Reactive.
pub fn label(dir: Dir) -> Option<String> {
    if !can_go(dir) {
        return None;
    }
    let Some(path) = neighbour(dir) else { return Some(String::new()) };
    let title = STATE.with(|s| s.borrow().titles.get(&path).cloned());
    Some(title.unwrap_or_else(|| label_for_path(&path)))
}

/// A readable name for an app URL, for when the page never recorded a title.
pub fn label_for_path(path: &str) -> String {
    let (p, q) = path.split_once('?').unwrap_or((path, ""));
    let param = |k: &str| q.split('&').filter_map(|kv| kv.split_once('=')).find(|(a, _)| *a == k).map(|(_, v)| js_decode(v));
    if p == "/explore" {
        return match param("q").filter(|s| !s.is_empty()) {
            Some(s) => format!("Explore \u{2014} {s}"),
            None => "Explore".into(),
        };
    }
    crate::nav::all().into_iter().find(|n| n.to == p).map(|n| n.label.to_string()).unwrap_or_default()
}

fn js_decode(v: &str) -> String {
    let plus = v.replace('+', " ");
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::decode_uri_component(&plus).map(String::from).unwrap_or(plus)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        plus
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn labels_from_urls() {
        assert_eq!(label_for_path("/explore"), "Explore");
        assert_eq!(label_for_path("/explore?genre=electronic"), "Explore");
        assert_eq!(label_for_path("/explore?q=refraction+records"), "Explore \u{2014} refraction records");
        assert_eq!(label_for_path("/explore/band?url=x"), "");
        assert_eq!(label_for_path("/albums/12"), "");
        assert_eq!(label_for_path("/albums?sort=new"), "Albums");
        assert_eq!(label_for_path("/"), "Home");
        assert_eq!(label_for_path("/nowhere"), "");
    }
}
