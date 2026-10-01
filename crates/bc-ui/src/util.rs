//! Small DOM helpers shared by the UI. All storage access is wrapped: private
//! windows and blocked storage must never break rendering.
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

pub fn window() -> web_sys::Window {
    web_sys::window().expect("no window")
}
pub fn document() -> web_sys::Document {
    window().document().expect("no document")
}
pub fn document_element() -> web_sys::HtmlElement {
    document().document_element().expect("no <html>").unchecked_into()
}

/// Milliseconds since page load (monotonic).
pub fn perf_now() -> f64 {
    window().performance().map(|p| p.now()).unwrap_or(0.0)
}
/// Unix ms.
pub fn unix_ms() -> f64 {
    js_sys::Date::now()
}

pub fn ls_get(key: &str) -> Option<String> {
    window().local_storage().ok().flatten().and_then(|s| s.get_item(key).ok().flatten())
}
pub fn ls_set(key: &str, value: &str) {
    if let Ok(Some(s)) = window().local_storage() {
        let _ = s.set_item(key, value);
    }
}
pub fn ls_remove(key: &str) {
    if let Ok(Some(s)) = window().local_storage() {
        let _ = s.remove_item(key);
    }
}

pub fn media_matches(query: &str) -> bool {
    window().match_media(query).ok().flatten().map(|m| m.matches()).unwrap_or(false)
}

pub fn is_mobile() -> bool {
    media_matches("(max-width: 640px)")
}

pub fn copy_text(text: &str) {
    let nav = window().navigator();
    let clip = nav.clipboard();
    let _ = clip.write_text(text);
}

/// Offer `content` as a file download.
pub fn download_text(filename: &str, mime: &str, content: &str) {
    let parts = js_sys::Array::new();
    parts.push(&JsValue::from_str(content));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type(mime);
    let Ok(blob) = web_sys::Blob::new_with_str_sequence_and_options(&parts, &opts) else { return };
    let Ok(url) = web_sys::Url::create_object_url_with_blob(&blob) else { return };
    if let Ok(a) = document().create_element("a") {
        let a: web_sys::HtmlAnchorElement = a.unchecked_into();
        a.set_href(&url);
        a.set_download(filename);
        a.click();
    }
    let _ = web_sys::Url::revoke_object_url(&url);
}

/// Percent-encode a query component.
pub fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_default()
}

/// Query string from `(key, value)` pairs, skipping empty values.
pub fn qs(params: &[(&str, String)]) -> String {
    let parts: Vec<String> =
        params.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect();
    if parts.is_empty() { String::new() } else { format!("?{}", parts.join("&")) }
}

/// Run `f` on the next animation frame.
pub fn raf(f: impl FnOnce() + 'static) {
    let cb = Closure::once_into_js(f);
    let _ = window().request_animation_frame(cb.unchecked_ref());
}

/// Repeated rAF loop; return `false` from the callback to stop.
pub fn raf_loop(mut f: impl FnMut(f64) -> bool + 'static) {
    use std::cell::RefCell;
    use std::rc::Rc;
    let slot: Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>> = Rc::new(RefCell::new(None));
    let slot2 = slot.clone();
    *slot.borrow_mut() = Some(Closure::new(move |t: f64| {
        if f(t) {
            if let Some(c) = slot2.borrow().as_ref() {
                let _ = window().request_animation_frame(c.as_ref().unchecked_ref());
            }
        } else {
            slot2.borrow_mut().take();
        }
    }));
    if let Some(c) = slot.borrow().as_ref() {
        let _ = window().request_animation_frame(c.as_ref().unchecked_ref());
    }
}

/// One-shot timeout (ms).
pub fn after(ms: i32, f: impl FnOnce() + 'static) {
    let cb = Closure::once_into_js(f);
    let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), ms);
}

pub fn pathname() -> String {
    window().location().pathname().unwrap_or_default()
}

/// Entropy for ids without a crypto dependency.
pub fn entropy() -> u64 {
    (js_sys::Math::random() * 4_294_967_296.0) as u64 ^ ((unix_ms() as u64) << 7)
}

/// Observe an element's content-box size; returns a disconnect function.
/// Callback receives `(width, height)` in CSS px.
pub fn observe_resize(el: &web_sys::Element, f: impl Fn(f64, f64) + 'static) -> Box<dyn FnOnce()> {
    let cb = Closure::<dyn FnMut(js_sys::Array)>::new(move |entries: js_sys::Array| {
        if let Some(e) = entries.get(0).dyn_ref::<web_sys::ResizeObserverEntry>() {
            let r = e.content_rect();
            f(r.width(), r.height());
        }
    });
    match web_sys::ResizeObserver::new(cb.as_ref().unchecked_ref()) {
        Ok(obs) => {
            obs.observe(el);
            Box::new(move || {
                obs.disconnect();
                drop(cb);
            })
        }
        Err(_) => Box::new(|| {}),
    }
}

/// Query string from owned pairs (repeated keys allowed), skipping empty values.
pub fn qs_pairs(pairs: &[(String, String)]) -> String {
    let parts: Vec<String> =
        pairs.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect();
    if parts.is_empty() { String::new() } else { format!("?{}", parts.join("&")) }
}

thread_local! {
    static ROOT_OWNER: leptos::prelude::Owner = leptos::prelude::Owner::new();
}

/// A signal that lives for the whole session: created under a root owner, so it is never
/// disposed with the component that happened to touch it first (global stores, toasts, DnD).
pub fn root_signal<T: Send + Sync + 'static>(value: T) -> leptos::prelude::RwSignal<T> {
    ROOT_OWNER.with(|o| o.with(|| leptos::prelude::RwSignal::new(value)))
}

/// Stable hue (0..360) for a tag name (FNV-1a), used when "Tag colours" is on.
pub fn tag_hue(name: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in name.to_lowercase().bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    h % 360
}

#[cfg(test)]
mod tests {
    use super::tag_hue;
    #[test]
    fn tag_hue_is_stable_and_case_insensitive() {
        assert_eq!(tag_hue("Techno"), tag_hue("techno"));
        assert!(tag_hue("dub") < 360);
        assert_ne!(tag_hue("dub"), tag_hue("house"));
    }
}
