//! Small shared pieces of the system pages: card section, stat tile, inline notice.
use leptos::prelude::*;

use crate::ds::Icon;

/// A titled card. `id` doubles as the anchor and the `aria-labelledby` target.
#[component]
pub fn SysCard(
    #[prop(into)] title: String,
    #[prop(optional, into)] icon: Option<String>,
    #[prop(optional, into)] hint: Option<String>,
    #[prop(optional)] actions: Option<ChildrenFn>,
    children: Children,
) -> impl IntoView {
    let id = format!("sys-{}", title.to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "-"));
    let id_ref = id.clone();
    view! {
        <section class="sys-card" aria-labelledby=id_ref>
            <header class="sys-card-head">
                <h2 id=id class="section-title">
                    {icon.map(|i| view! { <Icon name=i /> })}
                    {title}
                </h2>
                {actions.map(|a| view! { <div class="sys-card-actions">{a()}</div> })}
            </header>
            {hint.map(|h| view! { <p class="sys-hint">{h}</p> })}
            <div class="sys-card-body">{children()}</div>
        </section>
    }
}

/// Label + big mono value.
#[component]
pub fn Stat(#[prop(into)] label: String, #[prop(into)] value: Signal<String>, #[prop(optional, into)] tone: String) -> impl IntoView {
    view! {
        <div class="sys-stat">
            <div class="k">{label}</div>
            <div class=format!("v mono {tone}")>{move || value.get()}</div>
        </div>
    }
}

/// Inline error / info line under a control.
#[component]
pub fn Notice(#[prop(into)] text: Signal<Option<String>>, #[prop(optional, into)] tone: String) -> impl IntoView {
    let cls = if tone.is_empty() { "sys-notice danger".to_string() } else { format!("sys-notice {tone}") };
    view! {
        {move || text.get().map(|t| view! {
            <p class=cls.clone() role="alert"><Icon name="alert" />{t}</p>
        })}
    }
}

/// `Copy` handle on a `data::Query` (the original is `Copy` only when its payload is, which
/// DTOs are not). Lets page closures capture it freely.
pub struct QH<T: Send + Sync + 'static> {
    pub data: RwSignal<Option<std::sync::Arc<T>>>,
    pub error: RwSignal<Option<crate::api::ApiErr>>,
    pub loading: RwSignal<bool>,
    inner: StoredValue<crate::data::Query<T>>,
}
impl<T: Send + Sync + 'static> Clone for QH<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Send + Sync + 'static> Copy for QH<T> {}
impl<T: Send + Sync + 'static> QH<T> {
    pub fn refetch(&self) {
        self.inner.with_value(|q| q.refetch());
    }
}
pub fn qh<T: Send + Sync + 'static>(q: crate::data::Query<T>) -> QH<T> {
    QH { data: q.data, error: q.error, loading: q.loading, inner: StoredValue::new(q) }
}

/// Error panel with Retry, shown only while there is no data yet.
#[component]
pub fn QueryError<T: Send + Sync + 'static>(q: QH<T>) -> impl IntoView {
    view! {
        {move || q.error.get().filter(|_| q.data.get().is_none()).map(|e| view! {
            <crate::ds::ErrorPanel message=e.message() on_retry=Callback::new(move |_| q.refetch()) />
        })}
    }
}
