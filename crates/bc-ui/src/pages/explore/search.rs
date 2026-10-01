//! Bandcamp search results: the people, the records, the loose tracks. Same sections and
//! affordances wherever a hit shows up.
use bc_types::bandcamp::{ReleaseCardOut, SearchHitOut};
use leptos::prelude::*;

use super::cards::{CardPlay, ReleaseGrid};
use super::logic::{GroupedHits, band_path, hit_to_card, release_path};
use crate::ds::Icon;
use crate::widgets::common::Art;

#[component]
pub fn SectionHeading(#[prop(into)] icon: String, #[prop(into)] title: String, count: usize) -> impl IntoView {
    view! {
        <h2 class="xg-h">
            <span class="display xg-h-l"><Icon name=icon />{title}</span>
            <span class="mono faint">{count}</span>
        </h2>
    }
}

/// One artist or label: a face and a name, the way a profile is scanned.
#[component]
fn BandTile(hit: SearchHitOut) -> impl IntoView {
    let sub = if hit.subtitle.is_empty() { hit.kind.clone() } else { hit.subtitle.clone() };
    view! {
        <a class="band-tile" href=band_path(&hit.url)>
            <span class="band-face">
                {match hit.art_url.clone().filter(|a| !a.is_empty()) {
                    Some(a) => view! { <img src=a alt="" loading="lazy" /> }.into_any(),
                    None => view! { <Icon name="users" /> }.into_any(),
                }}
            </span>
            <span class="band-name truncate">{hit.name.clone()}</span>
            <span class="band-sub truncate">{sub}</span>
        </a>
    }
}

/// One track hit, in the list shape a tracklist is read in.
#[component]
fn TrackHitRow(hit: SearchHitOut) -> impl IntoView {
    let aria = if hit.subtitle.is_empty() { hit.name.clone() } else { format!("{} \u{2014} {}", hit.name, hit.subtitle) };
    view! {
        <div class="hit-row">
            <div class="hit-art">
                <Art src=hit.art_url.clone() />
                <CardPlay url=hit.url.clone() title=hit.name.clone() library_id=hit.library_release_id class="fill" />
            </div>
            <div class="grow">
                <div class="truncate hit-t">{hit.name.clone()}</div>
                {(!hit.subtitle.is_empty()).then(|| view! { <div class="truncate faint hit-s">{hit.subtitle.clone()}</div> })}
            </div>
            {hit.in_library.then(|| view! { <span class="faint hit-lib">"In library"</span> })}
            <a class="xc-link" href=release_path(&hit.url) aria-label=aria></a>
        </div>
    }
}

/// The three sections a search breaks into, each left out when empty.
#[component]
pub fn HitSections(grouped: GroupedHits) -> impl IntoView {
    let GroupedHits { bands, albums, tracks } = grouped;
    let cards: Vec<ReleaseCardOut> = albums.iter().map(hit_to_card).collect();
    view! {
        {(!bands.is_empty()).then(|| view! {
            <section class="xg-section">
                <SectionHeading icon="users" title="Artists & labels" count=bands.len() />
                <div class="band-tiles">
                    {bands.iter().cloned().map(|h| view! { <BandTile hit=h /> }).collect_view()}
                </div>
            </section>
        })}
        {(!cards.is_empty()).then(|| {
            let n = cards.len();
            view! {
                <section class="xg-section">
                    <SectionHeading icon="disc" title="Albums" count=n />
                    <ReleaseGrid items=cards min=140 />
                </section>
            }
        })}
        {(!tracks.is_empty()).then(|| view! {
            <section class="xg-section">
                <SectionHeading icon="list-music" title="Tracks" count=tracks.len() />
                <div class="hit-rows">
                    {tracks.iter().cloned().map(|h| view! { <TrackHitRow hit=h /> }).collect_view()}
                </div>
            </section>
        })}
    }
}

// ---------------------------------------------------------------------------
// Shared with the library screens (Tracks search miss, artist/label page pinning)
// ---------------------------------------------------------------------------

use bc_types::bandcamp::SearchHitOut as Hit;

use super::cards::GridPlaybackBar;
use super::logic::{explore_path, group_hits};
use super::qh;
use crate::data::QuerySpec;
use crate::ds::{Button, Size};
use crate::util::enc;

/// How long the words have to stand still before Bandcamp is asked: the library searches on every
/// keystroke, and each prefix that matches nothing would otherwise be its own Bandcamp request.
const SETTLE_MS: i32 = 600;

/// What Bandcamp has under the words the library had nothing for ("no, but here it is"). Shares
/// the Explore screen's cache key, so following "open in Explore" costs no second request.
#[component]
pub fn BandcampSearchShelf(#[prop(into)] q: Signal<String>, #[prop(optional, into)] class: String) -> impl IntoView {
    let settled = qh::use_settled(q, SETTLE_MS);
    let ready = Signal::derive(move || settled.get().as_deref() == Some(q.get().as_str()) && !q.with(|q| q.trim().is_empty()));
    let search = qh::use_q::<Vec<Hit>>(move || ready.get().then(|| QuerySpec::new(format!("/explore/search?q={}&limit=30", enc(q.get().trim())), &[])));
    let grouped = Signal::derive(move || search.data.with(|d| d.as_ref().map(|h| group_hits(h)).unwrap_or_default()));
    let playable = Signal::derive(move || grouped.with(|g| g.playable().iter().map(|h| (h.url.clone(), h.library_release_id)).collect::<Vec<_>>()));
    view! {
        <section class=format!("xg-shelf-block {class}") aria-label="On Bandcamp">
            <h2 class="xg-h">
                <span class="display xg-h-l"><Icon name="external" />"On Bandcamp"</span>
                {move || search.data.with(|d| d.is_some()).then(|| view! { <span class="mono faint">{grouped.with(|g| g.bands.len() + g.albums.len() + g.tracks.len())}</span> })}
                <a class="xg-more" href=move || explore_path(&q.get())>"open in Explore \u{2192}"</a>
            </h2>
            {move || {
                if !ready.get() || search.first_load() || (search.loading.get() && search.data.with(|d| d.is_none())) {
                    view! { <super::related::Loading text="Searching Bandcamp\u{2026}" /> }.into_any()
                } else if let Some(e) = search.failure() {
                    view! { <div class="xg-notice err" role="alert">{qh::err_text(&e)}</div> }.into_any()
                } else if grouped.with(|g| g.bands.is_empty() && g.albums.is_empty() && g.tracks.is_empty()) {
                    view! { <div class="faint xg-quiet">{format!("Nothing on Bandcamp matches \u{201c}{}\u{201d} either.", q.get())}</div> }.into_any()
                } else {
                    view! {
                        {move || playable.with(|p| !p.is_empty()).then(|| view! { <GridPlaybackBar items=playable noun="results" /> })}
                        {move || view! { <HitSections grouped=grouped.get() /> }}
                    }.into_any()
                }
            }}
        </section>
    }
}

/// Choose the Bandcamp page a library artist or label lives at. The automatic locate only accepts
/// a page whose own catalogue holds records this library already has by that name; an artist who
/// only releases through labels can never satisfy it, so the search is put on screen instead: the
/// proof a person reading a name, a location and the page itself supplies in one press.
#[component]
pub fn BandcampPagePicker(
    #[prop(into)] name: String,
    /// The word for what is being pinned ("artist" | "label").
    #[prop(into)]
    noun: String,
    #[prop(optional, into)] current_url: Option<String>,
    on_pick: Callback<String>,
    /// The URL being saved right now, so the row that was pressed shows it.
    #[prop(optional, into)]
    picking: MaybeProp<String>,
    #[prop(optional, into)] error: MaybeProp<String>,
) -> impl IntoView {
    let q = RwSignal::new(name.clone());
    let settled = qh::use_settled(q.into(), SETTLE_MS);
    // The seed is what we already know them as, so it goes straight out.
    let seed = name.clone();
    let query = Signal::derive(move || settled.get().unwrap_or_else(|| seed.clone()).trim().to_string());
    let search = qh::use_q::<Vec<Hit>>(move || {
        let s = query.get();
        (!s.is_empty()).then(|| QuerySpec::new(format!("/explore/search?q={}&kind=artist&limit=20", enc(&s)), &[]))
    });
    let bands = Signal::derive(move || search.data.with(|d| d.as_ref().map(|h| group_hits(h).bands).unwrap_or_default()));
    let same = |a: &str, b: &str| a.trim_end_matches('/').eq_ignore_ascii_case(b.trim_end_matches('/'));
    let noun2 = noun.clone();
    view! {
        <div class="pick">
            <label class="sr-only" for="bc-page-search">{format!("Search Bandcamp for the {noun}'s page")}</label>
            <div class="input-wrap pick-search">
                <Icon name="search" />
                <input id="bc-page-search" class="input" prop:value=move || q.get() on:input=move |ev| q.set(event_target_value(&ev)) spellcheck="false"
                    placeholder=format!("Search Bandcamp for {name}") />
            </div>
            {move || error.get().map(|e| view! { <div class="xg-notice err" role="alert">{e}</div> })}
            {move || {
                let qv = query.get();
                if qv.is_empty() {
                    view! { <div class="faint xg-quiet">"Type the name Bandcamp knows them by."</div> }.into_any()
                } else if search.first_load() || (search.loading.get() && search.data.with(|d| d.is_none())) {
                    view! { <super::related::Loading text="Searching Bandcamp\u{2026}" /> }.into_any()
                } else if let Some(e) = search.failure() {
                    view! { <div class="xg-notice err" role="alert">{qh::err_text(&e)}</div> }.into_any()
                } else if bands.with(|b| b.is_empty()) {
                    view! { <div class="faint xg-quiet">{format!("Bandcamp has no {noun2} page named \u{201c}{qv}\u{201d}. Try another spelling.")}</div> }.into_any()
                } else {
                    let (cur, pk) = (current_url.clone(), picking.get());
                    view! {
                        <ul class="pick-list">
                            {bands.get().into_iter().map(|h| {
                                let pinned = cur.as_deref().is_some_and(|c| same(c, &h.url));
                                let busy = pk.as_deref().is_some_and(|p| same(p, &h.url));
                                let url = h.url.clone();
                                let sub = [h.subtitle.clone(), h.url.replace("https://", "").replace("http://", "")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" \u{b7} ");
                                view! {
                                    <li class="pick-row">
                                        <span class="pick-face">{match h.art_url.clone().filter(|a| !a.is_empty()) {
                                            Some(a) => view! { <img src=a alt="" loading="lazy" /> }.into_any(),
                                            None => view! { <Icon name="users" /> }.into_any(),
                                        }}</span>
                                        <div class="grow">
                                            <div class="truncate pick-n">{h.name.clone()}{(h.kind == "label").then(|| view! { <span class="badge">"label"</span> })}</div>
                                            <div class="truncate faint pick-s">{sub}</div>
                                        </div>
                                        <a class="xg-more" href=band_path(&h.url)>"Preview"</a>
                                        {if pinned {
                                            view! { <span class="status idle"><Icon name="check" />"Pinned"</span> }.into_any()
                                        } else {
                                            view! {
                                                <Button size=Size::Sm busy=busy disabled=pk.is_some() on_click=move |_| on_pick.run(url.clone())>
                                                    {if busy { "Pinning\u{2026}" } else { "This is them" }}
                                                </Button>
                                            }.into_any()
                                        }}
                                    </li>
                                }
                            }).collect_view()}
                        </ul>
                    }.into_any()
                }
            }}
        </div>
    }
}
