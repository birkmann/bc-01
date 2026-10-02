//! Home on an empty library, the Bandcamp half: somewhere to start before anything is on disk.
//! A search box, Bandcamp's own genres, and the artists and labels selling right now, the way
//! a streaming app greets a new account. Every tile is a door into Explore.
use bc_types::bandcamp::{SpotlightBandOut, SpotlightOut};
use leptos::prelude::*;
use leptos_router::hooks::use_navigate;
use serde_json::Value;

use crate::data::QuerySpec;
use crate::ds::{Button, Icon, Size, Skeleton, Variant};
use crate::pages::explore::logic::{Facet, band_path, explore_path, parse_facets};
use crate::pages::explore::qh;

/// Genre tiles shown before "All genres" opens the rest.
const GENRES_FOLDED: usize = 12;

#[component]
pub fn BandcampDig() -> impl IntoView {
    view! {
        <section class="hm-start" aria-labelledby="hm-start-title">
            <div class="hm-start-head">
                <h3 id="hm-start-title" class="hm-wel-dig-title">"Or start on Bandcamp"</h3>
                <StartSearch />
            </div>
            <GenreTiles />
            <Spotlight />
        </section>
    }
}

#[component]
fn StartSearch() -> impl IntoView {
    let navigate = use_navigate();
    let text = RwSignal::new(String::new());
    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let q = text.get_untracked().trim().to_string();
        if !q.is_empty() {
            navigate(&explore_path(&q), Default::default());
        }
    };
    view! {
        <form class="hm-start-search" role="search" on:submit=submit>
            <Icon name="search" size=16 />
            <label class="sr-only" for="hm-start-q">"Search Bandcamp"</label>
            <input id="hm-start-q" class="grow" type="search" autocomplete="off" spellcheck="false"
                placeholder="Search Bandcamp for an artist, label or record"
                prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev)) />
            <Button size=Size::Sm variant=Variant::Primary kind="submit" disabled=Signal::derive(move || text.get().trim().is_empty())>"Search"</Button>
        </form>
    }
}

/// Browse a genre at its best-sellers: a newcomer wants what people buy, not this hour's uploads.
fn genre_path(slug: &str) -> String {
    format!("/explore?genre={}&slice=top", crate::util::enc(slug))
}

#[component]
fn GenreTiles() -> impl IntoView {
    // Same key as the Explore screen's dropdowns: one fetch serves both.
    let genres_q = qh::use_q::<Value>(|| Some(QuerySpec::new("/explore/genres", &[])));
    let genres = Signal::derive(move || genres_q.data.with(|d| parse_facets(d.as_deref()).genres));
    let open = RwSignal::new(false);
    view! {
        <div class="hm-start-block">
            <div class="hm-start-sub">
                <h4>"Genres"</h4>
                {move || (genres.with(Vec::len) > GENRES_FOLDED).then(|| view! {
                    <button type="button" class="lib-link faint" aria-expanded=move || open.get().to_string() on:click=move |_| open.update(|o| *o = !*o)>
                        {move || if open.get() { "Fewer".to_string() } else { format!("All {} genres", genres.with(Vec::len)) }}
                    </button>
                })}
            </div>
            <div class="hm-genres">
                {move || if genres_q.first_load() {
                    (0..GENRES_FOLDED).map(|_| view! { <Skeleton height="72px" class="hm-genre-skel" /> }).collect_view().into_any()
                } else {
                    let all = genres.get();
                    let n = if open.get() { all.len() } else { GENRES_FOLDED };
                    all.into_iter().take(n).map(|g| view! { <GenreTile genre=g /> }).collect_view().into_any()
                }}
            </div>
        </div>
    }
}

#[component]
fn GenreTile(genre: Facet) -> impl IntoView {
    view! {
        <a class="hm-genre" href=genre_path(&genre.slug) style=format!("--g-h:{}", crate::util::tag_hue(&genre.slug))>
            <span class="hm-genre-name">{genre.label}</span>
        </a>
    }
}

// ---- who is selling ----------------------------------------------------------------------------

#[component]
fn Spotlight() -> impl IntoView {
    let q = qh::use_q::<SpotlightOut>(|| Some(QuerySpec::new("/explore/spotlight", &[])));
    view! {
        {move || {
            if q.first_load() {
                return view! {
                    <SpotlightRow title="Artists selling now" bands=vec![] round=true loading=true />
                    <SpotlightRow title="Labels to dig into" bands=vec![] round=false loading=true />
                }.into_any();
            }
            if let Some(e) = q.failure() {
                return view! {
                    <p class="faint hm-wel-fine hm-start-err" role="status">
                        <Icon name="wifi-off" size=14 />
                        {format!("Couldn\u{2019}t load who\u{2019}s selling on Bandcamp: {}", qh::err_text(&e))}
                        <button type="button" class="lib-link" on:click=move |_| q.refetch()>"Retry"</button>
                    </p>
                }.into_any();
            }
            let s = q.data.get().unwrap_or_default();
            view! {
                <SpotlightRow title="Artists selling now" bands=s.artists.clone() round=true loading=false />
                <SpotlightRow title="Labels to dig into" bands=s.labels.clone() round=false loading=false />
            }.into_any()
        }}
    }
}

#[component]
fn SpotlightRow(title: &'static str, bands: Vec<SpotlightBandOut>, round: bool, loading: bool) -> impl IntoView {
    if !loading && bands.is_empty() {
        return ().into_any();
    }
    let shape = if round { "hm-bands" } else { "hm-bands square" };
    view! {
        <div class="hm-start-block">
            <div class="hm-start-sub"><h4>{title}</h4></div>
            <div class=shape aria-busy=loading.to_string()>
                {if loading {
                    (0..8).map(|_| view! { <div class="band-tile"><Skeleton width="104px" height="104px" class="band-face" /><Skeleton width="80%" height="12px" /></div> }).collect_view().into_any()
                } else {
                    bands.into_iter().map(|b| view! { <SpotlightTile band=b /> }).collect_view().into_any()
                }}
            </div>
        </div>
    }
    .into_any()
}

#[component]
fn SpotlightTile(band: SpotlightBandOut) -> impl IntoView {
    view! {
        <a class="band-tile" href=band_path(&band.url)>
            <span class="band-face">
                {match band.image_url.clone() {
                    Some(src) => view! { <img src=src alt="" loading="lazy" /> }.into_any(),
                    None => view! { <Icon name="users" /> }.into_any(),
                }}
            </span>
            <span class="band-name truncate" title=band.name.clone()>{band.name.clone()}</span>
            {band.location.clone().map(|l| view! { <span class="band-sub truncate" title=l.clone()>{l.clone()}</span> })}
        </a>
    }
}
