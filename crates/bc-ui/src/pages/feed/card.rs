//! One inbox release card, shared by Feed and Fans: art as the select target, scope badge,
//! ignore toggle, play. Fixed height below the art so it fits the virtualised `CardGrid`.
use std::collections::HashMap;

use bc_types::bandcamp::HarvestItemOut;
use leptos::prelude::*;

use crate::ds::Icon;
use crate::util::enc;

/// Pixels of text area under the (square) art that `InboxCard` needs; pass to `CardGrid::meta_h`.
pub const CARD_META_H: f64 = 76.0;

/// In-place state patches (`queued` / `ignored` / `new`) applied on top of fetched rows.
pub type StateOverrides = RwSignal<HashMap<i64, String>>;

pub fn release_path(url: &str) -> String {
    format!("/explore/release?url={}", enc(url))
}
pub fn band_path(url: &str) -> String {
    format!("/explore/band?url={}", enc(url))
}

/// The Bandcamp page hosting a release: its origin (an artist's or a label's root).
pub fn band_url(release_url: &str) -> Option<String> {
    let rest = release_url.strip_prefix("https://").or_else(|| release_url.strip_prefix("http://"))?;
    let host = rest.split('/').next().filter(|h| !h.is_empty())?;
    Some(format!("https://{host}"))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    InLibrary,
    Owned,
    Free,
    NotOwned,
}

/// Owned or freely offered is in scope; everything else needs confirmation.
pub fn scope_of(i: &HarvestItemOut) -> Scope {
    if i.in_library {
        Scope::InLibrary
    } else if i.in_collection {
        Scope::Owned
    } else if i.is_free_download {
        Scope::Free
    } else {
        Scope::NotOwned
    }
}

#[component]
fn ScopeBadge(scope: Scope) -> impl IntoView {
    let (cls, icon, label) = match scope {
        Scope::InLibrary => ("ib-scope faint", "disc", "in library"),
        Scope::Owned => ("ib-scope ok", "check", "owned"),
        Scope::Free => ("ib-scope ok", "sparkles", "free"),
        Scope::NotOwned => ("ib-scope warn", "alert", "unowned"),
    };
    view! { <span class=cls><Icon name=icon />{label}</span> }
}

#[component]
pub fn InboxCard(
    item: HarvestItemOut,
    #[prop(into)] selected: Signal<bool>,
    overrides: StateOverrides,
    on_toggle: Callback<i64>,
    on_ignore: Callback<i64>,
    on_play: Callback<i64>,
    /// Chips for the item's tags (hidden when `None`, e.g. in the virtualised grid).
    #[prop(optional)]
    on_tag: Option<Callback<String>>,
    #[prop(optional, into)] active_tags: Option<Signal<Vec<String>>>,
    #[prop(optional)] show_tabs: bool,
) -> impl IntoView {
    let id = item.id;
    let scope = scope_of(&item);
    let base_state = item.state.clone();
    let state = Signal::derive(move || overrides.with(|m| m.get(&id).cloned()).unwrap_or_else(|| base_state.clone()));
    let title = if item.title.is_empty() { "(untitled)".to_string() } else { item.title.clone() };
    let artist = if !item.artist_name.is_empty() { item.artist_name.clone() } else { item.label_name.clone().unwrap_or_else(|| "Unknown artist".into()) };
    let band = band_url(&item.url);
    let release_href = release_path(&item.url);
    let bc_url = item.url.clone();
    let art = item.art_url.clone().filter(|s| !s.is_empty());
    let loaded = RwSignal::new(false);
    let sel_label = format!("Select {title}");
    let open_label = format!("Open {title} on Bandcamp");
    let tags = item.tags.clone();
    let tab_chips = if show_tabs { item.tabs.clone() } else { vec![] };
    let more_tags = tags.len().saturating_sub(2);
    let more_title = tags.join(", ");
    let shown_tags: Vec<String> = if on_tag.is_some() { tags.into_iter().take(2).collect() } else { vec![] };

    let cls = move || {
        let mut c = String::from("ib-card");
        if selected.get() {
            c.push_str(" selected");
        }
        match state.get().as_str() {
            "ignored" => c.push_str(" is-ignored"),
            "queued" => c.push_str(" is-queued"),
            _ => {}
        }
        if scope == Scope::InLibrary {
            c.push_str(" dim");
        }
        c
    };

    view! {
        <div class=cls>
            <div class="art ib-art">
                <div class="ph"><Icon name="disc" /></div>
                {art.map(|src| view! {
                    <img src=src alt="" loading="lazy" decoding="async"
                        class=move || if loaded.get() { "loaded" } else { "" }
                        on:load=move |_| loaded.set(true) />
                })}
                <button type="button" class="ib-select" aria-pressed=move || selected.get().to_string()
                    aria-label=sel_label on:click=move |_| on_toggle.run(id)></button>
                {move || selected.get().then(|| view! { <span class="ib-tick" aria-hidden="true"><Icon name="check" /></span> })}
                {move || {
                    let s = state.get();
                    (s == "queued" || s == "ignored").then(|| view! {
                        <span class="ib-state" data-state=s.clone()>
                            <Icon name=if s == "queued" { "download" } else { "eye-off" } />{s.clone()}
                        </span>
                    })
                }}
                <button type="button" class="ib-play" title="Play" aria-label="Play" on:click=move |_| on_play.run(id)><Icon name="play" /></button>
            </div>
            <div class="ib-meta">
                <a class="ib-title truncate" href=release_href.clone() title="Open on the Explore page">{title}</a>
                {match band {
                    Some(b) => view! { <a class="ib-artist truncate" href=band_path(&b) title="Open the artist or label page">{artist}</a> }.into_any(),
                    None => view! { <span class="ib-artist truncate">{artist}</span> }.into_any(),
                }}
                {(!shown_tags.is_empty()).then(|| {
                    let at = active_tags;
                    view! {
                        <div class="ib-tags">
                            {shown_tags.into_iter().map(|t| {
                                let t2 = t.clone();
                                let t3 = t.clone();
                                view! {
                                    <button type="button" class="ib-tag" title="Filter by this tag"
                                        aria-pressed=move || at.map(|a| a.with(|v| v.contains(&t3))).unwrap_or(false).to_string()
                                        on:click=move |_| if let Some(cb) = on_tag { cb.run(t2.clone()) }>{t}</button>
                                }
                            }).collect_view()}
                            {(more_tags > 0).then(|| view! { <span class="ib-more" title=more_title.clone()>{format!("+{more_tags}")}</span> })}
                        </div>
                    }
                })}
                <div class="ib-foot">
                    <span class="ib-badges">
                        <ScopeBadge scope=scope />
                        {tab_chips.into_iter().map(|t| {
                            let col = t == "collection";
                            let title = if col { "In their collection" } else { "On their wishlist" };
                            view! { <span class="ib-scope faint" title=title role="img" aria-label=title><Icon name=if col { "disc" } else { "heart" } /></span> }
                        }).collect_view()}
                    </span>
                    <span class="ib-acts">
                        <a href=bc_url target="_blank" rel="noreferrer" class="ib-act" title="Open on Bandcamp" aria-label=open_label><Icon name="external" /></a>
                        <button type="button" class="ib-act" aria-label=move || if state.get() == "ignored" { "Restore this item" } else { "Ignore this item" }
                            title=move || if state.get() == "ignored" { "Restore" } else { "Ignore" }
                            on:click=move |_| on_ignore.run(id)>
                            <Icon name=crate::ds::dyn_icon(move || if state.get() == "ignored" { "eye" } else { "eye-off" }) />
                        </button>
                    </span>
                </div>
            </div>
        </div>
    }
}
