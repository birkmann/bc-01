use leptos::prelude::*;

#[derive(Clone, Debug, PartialEq)]
pub struct TabDef {
    pub id: String,
    pub label: String,
    pub count: Option<i64>,
}

impl TabDef {
    pub fn new(id: &str, label: &str) -> Self {
        Self { id: id.into(), label: label.into(), count: None }
    }
    pub fn count(mut self, c: i64) -> Self {
        self.count = Some(c);
        self
    }
}

/// Tabs with arrow-key navigation. `value` holds the selected tab id.
#[component]
pub fn Tabs(#[prop(into)] tabs: Signal<Vec<TabDef>>, value: RwSignal<String>) -> impl IntoView {
    view! {
        <div class="tabs" role="tablist"
            on:keydown=move |ev| {
                let t = tabs.get_untracked();
                let cur = value.get_untracked();
                let Some(i) = t.iter().position(|x| x.id == cur) else { return };
                let n = t.len();
                let next = match ev.key().as_str() {
                    "ArrowRight" => Some((i + 1) % n),
                    "ArrowLeft" => Some((i + n - 1) % n),
                    "Home" => Some(0),
                    "End" => Some(n - 1),
                    _ => None,
                };
                if let Some(j) = next { ev.prevent_default(); value.set(t[j].id.clone()); }
            }>
            {move || tabs.get().into_iter().map(|t| {
                let id = t.id.clone();
                let id2 = t.id.clone();
                view! {
                    <button class="tab" role="tab" type="button"
                        aria-selected=move || (value.get() == id).to_string()
                        tabindex=move || if value.get() == id2 { "0" } else { "-1" }
                        on:click={let id = t.id.clone(); move |_| value.set(id.clone())}>
                        <span>{t.label.clone()}</span>
                        {t.count.map(|c| view! { <span class="count">{crate::logic::format::format_count(c)}</span> })}
                    </button>
                }
            }).collect_view()}
        </div>
    }
}

/// Small segmented toggle (list/grid, ranges...). Options are `(value, label)`.
#[component]
pub fn SegmentedControl(options: Vec<(&'static str, &'static str)>, value: RwSignal<String>) -> impl IntoView {
    view! {
        <div class="segmented" role="group">
            {options.into_iter().map(|(v, l)| view! {
                <button type="button" aria-pressed=move || (value.get() == v).to_string() on:click=move |_| value.set(v.to_string())>{l}</button>
            }).collect_view()}
        </div>
    }
}
