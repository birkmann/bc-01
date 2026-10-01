//! Settings use the shared `crate::prefs` store (ui_state `ui.prefs`); the handle is
//! the shared context, falling back to providing it when the app did not.
use leptos::prelude::*;

pub use crate::prefs::PrefsCtx as PrefsHandle;

pub fn use_prefs() -> PrefsHandle {
    use_context::<PrefsHandle>().unwrap_or_else(crate::prefs::provide_prefs)
}
