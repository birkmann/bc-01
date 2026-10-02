//! `data::Query<T>` is only `Copy` when `T` is, which makes it awkward to capture in many
//! view closures. `Q` is the always-Copy handle over the same signals.
use std::sync::Arc;

use leptos::prelude::*;
use serde::de::DeserializeOwned;

use crate::api::ApiErr;
use crate::data::{self, Query, QuerySpec};

pub struct Q<T: Send + Sync + 'static> {
    pub data: RwSignal<Option<Arc<T>>>,
    pub loading: RwSignal<bool>,
    pub error: RwSignal<Option<ApiErr>>,
    handle: StoredValue<Query<T>>,
}

impl<T: Send + Sync + 'static> Clone for Q<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Send + Sync + 'static> Copy for Q<T> {}

impl<T: Send + Sync + 'static> Q<T> {
    pub fn refetch(&self) {
        self.handle.with_value(|q| q.refetch());
    }
    /// `true` only while there is nothing to show yet and a request is running.
    pub fn first_load(&self) -> bool {
        self.data.with(|d| d.is_none()) && self.loading.get()
    }
    /// The error to show: only when there is no (stale) data to keep showing instead.
    pub fn failure(&self) -> Option<ApiErr> {
        self.error.get().filter(|_| self.data.with(|d| d.is_none()))
    }
}

pub fn use_q<T>(spec: impl Fn() -> Option<QuerySpec> + Send + Sync + 'static) -> Q<T>
where
    T: DeserializeOwned + Send + Sync + 'static,
{
    let q = data::use_query::<T>(spec);
    Q { data: q.data, loading: q.loading, error: q.error, handle: StoredValue::new(q) }
}

/// A human reading of an API failure. The Bandcamp routes answer 404 "no such API route" when the
/// server was built without the Bandcamp service, which is not a missing page.
pub fn err_text(e: &ApiErr) -> String {
    if e.status == 404 && e.detail.as_deref().is_some_and(|d| d.contains("no such API route")) {
        "The Bandcamp service is not available on this server.".into()
    } else {
        e.message()
    }
}

/// The window title when no page sets one (`index.html`).
const APP_TITLE: &str = "bc";

/// Set the tab title while this page is mounted, back to the app's on leave. The app's own
/// screens keep its suffix; Bandcamp's records say "Bandcamp" instead (the same record can be
/// open on its library page in another tab, and only the suffix tells them apart). The title is
/// also what "Back to …" calls this page.
///
/// The base is fixed, not the title found at mount: the next page mounts before the last one
/// is cleaned up, so that title is the previous page's, and restoring it on leave would
/// overwrite the next page's.
pub fn use_title(app_suffix: bool, title: impl Fn() -> String + Send + Sync + 'static) {
    let loc = leptos_router::hooks::use_location();
    let mine = Arc::new(std::sync::Mutex::new(None::<String>));
    let mine2 = mine.clone();
    Effect::new(move |_| {
        let t = title();
        let full = match (t.is_empty(), app_suffix) {
            (true, _) => APP_TITLE.to_string(),
            (false, true) => format!("{t} \u{2014} {APP_TITLE}"),
            (false, false) => format!("{t} \u{2014} Bandcamp"),
        };
        crate::util::document().set_title(&full);
        *mine2.lock().unwrap() = Some(full);
        let search = loc.search.get_untracked();
        let url = if search.is_empty() { loc.pathname.get_untracked() } else { format!("{}?{search}", loc.pathname.get_untracked()) };
        crate::history::record_title(url, t);
    });
    on_cleanup(move || {
        let doc = crate::util::document();
        if mine.lock().unwrap().as_ref().is_some_and(|m| doc.title() == *m) {
            doc.set_title(APP_TITLE);
        }
    });
}

/// `value`, but only once it has stood still for `ms`; `None` until then (the first value is
/// the least likely thing anyone meant, so it does not go out at once).
pub fn use_settled(value: Signal<String>, ms: i32) -> RwSignal<Option<String>> {
    let out = RwSignal::new(None::<String>);
    let gen_ = StoredValue::new(0u64);
    Effect::new(move |_| {
        let v = value.get();
        gen_.update_value(|g| *g += 1);
        let g = gen_.get_value();
        crate::util::after(ms, move || {
            if gen_.try_get_value() == Some(g) {
                let _ = out.try_set(Some(v));
            }
        });
    });
    out
}
