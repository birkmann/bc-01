//! A row of chosen tags with a search box offering more from the library's own tag cloud. Shared
//! by the genre-switch target and the tag rules.
use bc_types::library::TagOut;
use leptos::prelude::*;

use crate::data::QuerySpec;
use crate::ds::Icon;
use crate::player::plan::qh::use_qh;

#[component]
pub fn TagPicker(
    #[prop(into)] value: Signal<Vec<String>>,
    #[prop(into)] on_toggle: Callback<String>,
    #[prop(into)] placeholder: String,
    #[prop(into)] label: String,
    #[prop(optional)] danger: bool,
) -> impl IntoView {
    let q = RwSignal::new(String::new());
    let focused = RwSignal::new(false);
    let cloud = use_qh::<Vec<TagOut>>(|| Some(QuerySpec::new("/tags?limit=1000", &["tag"])));
    let matches = Memo::new(move |_| {
        let all = cloud.data.get();
        let Some(all) = all else { return vec![] };
        let chosen: Vec<String> = value.get().iter().map(|t| t.to_lowercase()).collect();
        let needle = q.get().trim().to_lowercase();
        all.iter()
            .filter(|t| !chosen.contains(&t.name.to_lowercase()))
            .filter(|t| needle.is_empty() || t.name.to_lowercase().contains(&needle))
            .take(if needle.is_empty() { 8 } else { 12 })
            .map(|t| (t.id, t.name.clone(), t.track_count))
            .collect::<Vec<_>>()
    });
    let ph = placeholder.clone();
    view! {
        <div class="pp-tagpick">
            <div class="pp-chips">
                {move || value.get().into_iter().map(|t| {
                    let t2 = t.clone();
                    view! {
                        <button type="button" class=if danger { "pp-chip danger" } else { "pp-chip on" } title="Remove" on:click=move |_| on_toggle.run(t2.clone())>
                            {t}<Icon name="x" size=10 />
                        </button>
                    }
                }).collect_view()}
                <input class="input pp-tag-in" type="search" aria-label=label
                    placeholder=move || if value.with(|v| v.is_empty()) { ph.clone() } else { "more…".to_string() }
                    prop:value=move || q.get()
                    on:input=move |ev| q.set(event_target_value(&ev))
                    on:focus=move |_| focused.set(true)
                    on:blur=move |_| crate::util::after(150, move || { let _ = focused.try_set(false); }) />
            </div>
            {move || (focused.get() || !q.get().is_empty()).then(|| view! {
                <div class="pp-chips">
                    {matches.get().into_iter().map(|(_, name, n)| {
                        let n2 = name.clone();
                        view! {
                            <button type="button" class="pp-chip" on:click=move |_| { on_toggle.run(n2.clone()); q.set(String::new()); }>
                                {name}<span class="mono faint">{n}</span>
                            </button>
                        }
                    }).collect_view()}
                </div>
            })}
        </div>
    }
}
