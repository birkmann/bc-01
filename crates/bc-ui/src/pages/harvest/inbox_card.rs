//! One inbox release: the art is the select target, with a scope badge and an ignore toggle.
use std::collections::BTreeSet;

use bc_types::bandcamp::HarvestItemOut;
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::logic::{Scope, band_root, scope};
use crate::api;
use crate::ds::{self, Icon};
use crate::pages::explore::cards::CardPlay;
use crate::pages::explore::logic::{band_path, release_path};
use crate::widgets::common::Art;

#[component]
pub fn InboxCard(
    item: HarvestItemOut,
    selected: Signal<bool>,
    /// Everything matching is selected: individual cards cannot be toggled out of it.
    locked: Signal<bool>,
    on_toggle: Callback<i64>,
    /// Ids whose ignored state this session has flipped (the list itself refreshes lazily).
    flipped: RwSignal<BTreeSet<i64>>,
    on_changed: Callback<()>,
) -> impl IntoView {
    let id = item.id;
    let was_ignored = item.state == "ignored";
    let ignored = Signal::derive(move || was_ignored != flipped.with(|f| f.contains(&id)));
    let sc = scope(&item);
    let title = if item.title.is_empty() { "(untitled)".to_string() } else { item.title.clone() };
    let artist = if !item.artist_name.is_empty() { item.artist_name.clone() } else { item.label_name.clone().unwrap_or_else(|| "Unknown artist".into()) };
    let root = band_root(&item.url);
    let busy = RwSignal::new(false);
    let toggle_ignore = move |_| {
        busy.set(true);
        spawn_local(async move {
            match api::post::<_, HarvestItemOut>(&format!("/harvest/items/{id}/ignore"), &serde_json::json!({})).await {
                Ok(_) => {
                    flipped.update(|f| {
                        if !f.remove(&id) {
                            f.insert(id);
                        }
                    });
                    on_changed.run(());
                }
                Err(e) => ds::toast_err(&e.message()),
            }
            let _ = busy.try_set(false);
        });
    };
    let (t1, t2) = (title.clone(), title.clone());
    view! {
        <div class=move || format!("xc hv{}{}{}", if selected.get() { " picked" } else { "" }, if item.in_library { " lib" } else { "" }, if ignored.get() { " ign" } else { "" })>
            <div class="xc-art">
                <Art src=item.art_url.clone() />
                <button type="button" class="xc-link" aria-pressed=move || selected.get().to_string() aria-label=format!("Select {t1}")
                    disabled=move || locked.get() on:click=move |_| on_toggle.run(id)></button>
                <span class="xc-tick" aria-hidden="true"><Icon name="check" /></span>
                <CardPlay url=item.url.clone() title=title.clone() library_id=item.release_id />
            </div>
            <a class="xc-t truncate" href=release_path(&item.url) title=format!("Open {t2} on the Explore page")>{title}</a>
            {match root {
                Some(r) => view! { <a class="xc-a truncate" href=band_path(&r) title="Open the artist or label page this release lives on">{artist}</a> }.into_any(),
                None => view! { <div class="xc-a truncate">{artist}</div> }.into_any(),
            }}
            <div class="hv-foot">
                <span class=match sc { Scope::InLibrary => "hv-scope", Scope::NotOwned => "hv-scope warn", _ => "hv-scope ok" }>
                    <Icon name=match sc { Scope::InLibrary => "folder", Scope::Owned => "check", Scope::Free => "download", Scope::NotOwned => "alert" } />
                    {match sc { Scope::InLibrary => "in library", Scope::Owned => "owned", Scope::Free => "free", Scope::NotOwned => "not owned" }}
                </span>
                <span class="spacer"></span>
                <a class="hv-ico" href=item.url.clone() target="_blank" rel="noreferrer" title="Open on Bandcamp" aria-label=format!("Open {} on Bandcamp", item.title)><Icon name="external" /></a>
                <button type="button" class=move || if ignored.get() { "hv-ico on" } else { "hv-ico" } disabled=move || busy.get()
                    title=move || if ignored.get() { "Un-ignore" } else { "Ignore" }
                    aria-label=move || if ignored.get() { "Un-ignore this item" } else { "Ignore this item" } on:click=toggle_ignore>
                    <Icon name=ds::dyn_icon(move || if ignored.get() { "eye" } else { "eye-off" }) />
                </button>
            </div>
        </div>
    }
}
