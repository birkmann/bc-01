//! "More like this": the library shelves (`RelatedShelf`, two paged rows per group)
//! and the Bandcamp side of an album (`BandcampShelves`: find the record on Bandcamp,
//! then its tag / band / recommended feeds).
use std::sync::Arc;

use bc_types::bandcamp::{ExploreReleaseOut, RelatedOut, ReleaseCardOut, SearchHitOut};
use bc_types::library::{RelatedGroup, ReleaseOut};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::card::AlbumCard;
use super::host::{fetch_shelf_tracks, play_items, shuffle_in_place};
use crate::data::{QuerySpec, use_query};
use crate::ds::{Icon, Skeleton};
use crate::logic::paging;
use crate::player::use_player;
use crate::util::enc;
use crate::widgets::card_grid::grid_metrics;
use crate::widgets::common::Art;

pub const SHELF_ROWS: usize = 2;
pub const SHELF_DEPTH: usize = 60;

pub fn release_path(url: &str) -> String {
    format!("/explore/release?url={}", enc(url))
}

#[component]
pub fn RelatedShelf(group: RelatedGroup) -> impl IntoView {
    let player = use_player();
    let width = RwSignal::new(900.0f64);
    let holder = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        if let Some(el) = holder.get() {
            use wasm_bindgen::JsCast;
            let off = crate::util::observe_resize(el.unchecked_ref(), move |w, _| width.set(w));
            let guard = send_wrapper::SendWrapper::new(off);
            on_cleanup(move || (guard.take())());
        }
    });
    let cols = Memo::new(move |_| grid_metrics(width.get().max(120.0), 160.0, 16.0).0);
    let per_page = Memo::new(move |_| cols.get() * SHELF_ROWS);
    let page = RwSignal::new(0usize);
    let items = group.items.clone();
    let n_items = items.len();
    let page_count = Memo::new(move |_| n_items.div_ceil(per_page.get()).max(1));
    let current = Memo::new(move |_| page.get().min(page_count.get() - 1));
    let last_page = move || current.get() + 1 >= page_count.get();
    let starting = RwSignal::new(None::<bool>);
    let ids: Vec<i64> = group.items.iter().map(|r| r.id).collect();
    let see_all = match (group.id, group.kind.as_str()) {
        (Some(id), "artist") => Some(format!("/artists/{id}")),
        (Some(id), "label") => Some(format!("/labels/{id}")),
        (Some(_), _) | (None, "tag") => Some(format!("/tracks?tag={}", enc(&group.key))),
        _ => None,
    };
    let play = Arc::new(move |shuffle: bool| {
        let ids = ids.clone();
        starting.set(Some(shuffle));
        spawn_local(async move {
            match fetch_shelf_tracks(&ids).await {
                Ok(mut t) => {
                    if shuffle {
                        shuffle_in_place(&mut t);
                    }
                    play_items(player, &t, 0, None, shuffle);
                }
                Err(e) => crate::ds::toast_err(&e.message()),
            }
            starting.set(None);
        });
    });
    let (pa, pb) = (play.clone(), play);
    let title = group.title.clone();
    let (t1, t2, t3, t4) = (title.clone(), title.clone(), title.clone(), title.clone());
    let tags = group.tags.clone();

    view! {
        <section class="lib-shelf">
            <div class="lib-shelf-head">
                <h2 class="section-title">{title}</h2>
                <button type="button" class="lib-roundbtn primary" aria-label=format!("Play {t1}") disabled=move || starting.get().is_some() on:click=move |_| pa(false)><Icon name="play" size=13 /></button>
                <button type="button" class="lib-roundbtn" aria-label=format!("Shuffle {t2}") disabled=move || starting.get().is_some() on:click=move |_| pb(true)><Icon name="shuffle" size=13 /></button>
                {see_all.map(|h| view! { <a class="lib-link" href=h>"See all"</a> })}
                {tags.into_iter().map(|t| view! { <a class="lib-link faint" href=format!("/tracks?tag={}", enc(&t))>{t.clone()}</a> }).collect_view()}
                {move || (page_count.get() > 1).then(|| view! {
                    <span class="lib-pager">
                        <button type="button" class="btn btn-ghost btn-sm btn-icon" aria-label=format!("Previous page of {t3}") disabled=move || current.get() == 0 on:click=move |_| page.set(current.get().saturating_sub(1))><Icon name="chevron-left" /></button>
                        <span class="mono faint">{move || format!("{}/{}", current.get() + 1, page_count.get())}</span>
                        <button type="button" class="btn btn-ghost btn-sm btn-icon" aria-label=format!("Next page of {t4}") disabled=last_page on:click=move |_| page.set(current.get() + 1)><Icon name="chevron-right" /></button>
                    </span>
                })}
            </div>
            <div node_ref=holder class="lib-shelf-grid" style=move || format!("grid-template-columns:repeat({}, minmax(0, 1fr))", cols.get())>
                {move || {
                    let (a, b) = (current.get() * per_page.get(), (current.get() * per_page.get() + per_page.get()).min(n_items));
                    items[a..b].iter().cloned().map(|r| view! { <AlbumCard release=r /> }).collect_view()
                }}
            </div>
        </section>
    }
}

// ---- Bandcamp side ------------------------------------------------------------------------------

#[component]
fn BcCard(card: ReleaseCardOut) -> impl IntoView {
    let href = match card.library_release_id {
        Some(id) => format!("/albums/{id}"),
        None => release_path(&card.url),
    };
    view! {
        <a class="alb bc" href=href>
            <div class="alb-cover"><Art src=card.art_url.clone() class="alb-art" />
                {card.in_library.then(|| view! { <span class="alb-now" title="Already in your library"><Icon name="check" size=12 />"Owned"</span> })}
            </div>
            <div class="alb-meta" style="height:60px">
                <div class="alb-title truncate">{card.title.clone()}</div>
                <div class="alb-artist truncate">{card.artist_name.clone()}</div>
            </div>
        </a>
    }
}

#[component]
fn BandcampRelated(url: String, seed: Vec<String>) -> impl IntoView {
    let tags = RwSignal::new(seed.iter().take(3).cloned().collect::<Vec<_>>());
    let u = url.clone();
    let related = use_query::<RelatedOut>(move || {
        let mut q = format!("/explore/related?url={}&size=48&tag_limit=3&include_band=true", enc(&u));
        for t in tags.get() {
            q.push_str(&format!("&tags={}", enc(&t)));
        }
        Some(QuerySpec::new(q, &[]))
    });
    let seed2 = seed.clone();
    view! {
        {(seed.len() > 1).then(|| view! {
            <div class="lib-feeds">
                <span class="faint lib-feeds-label">"Feeds"</span>
                {seed2.iter().map(|t| {
                    let (t1, t2) = (t.clone(), t.clone());
                    view! {
                        <button type="button" class="lib-pill" class:on=move || tags.get().contains(&t1) aria-pressed=move || tags.get().contains(&t2).to_string()
                            on:click={let t = t.clone(); move |_| tags.update(|v| {
                                if let Some(i) = v.iter().position(|x| *x == t) { v.remove(i); } else { v.push(t.clone()); let n = v.len(); if n > 3 { v.remove(0); } }
                            })}>{t.clone()}</button>
                    }
                }).collect_view()}
            </div>
        })}
        {move || match (related.data.get(), related.error.get()) {
            (Some(d), _) => {
                if d.sections.iter().all(|s| s.items.is_empty()) {
                    view! { <p class="faint lib-note">"Bandcamp returned nothing related."</p> }.into_any()
                } else {
                    d.sections.iter().filter(|s| !s.items.is_empty()).cloned().map(|s| view! {
                        <section class="lib-shelf">
                            <div class="lib-shelf-head"><h2 class="section-title">{s.title.clone()}</h2></div>
                            <div class="lib-row">
                                {s.items.into_iter().map(|c| view! { <div class="lib-row-item"><BcCard card=c /></div> }).collect_view()}
                            </div>
                        </section>
                    }).collect_view().into_any()
                }
            }
            (None, Some(e)) => view! { <p class="lib-note danger-text">{format!("Bandcamp would not serve this release ({})", e.message())}</p> }.into_any(),
            (None, None) => view! { <div class="lib-note"><Skeleton height="120px" /></div> }.into_any(),
        }}
    }
}

/// The same album, seen from Bandcamp. Mounted only while the tab is on show: every
/// section costs Bandcamp a scrape behind a rate limiter.
#[component]
pub fn BandcampShelves(release: ReleaseOut) -> impl IntoView {
    let known = release.bandcamp_url.clone();
    let terms = [release.artist.as_ref().map(|a| a.name.clone()), Some(release.title.clone())].into_iter().flatten().collect::<Vec<_>>().join(" ");
    let (k1, t1) = (known.clone(), terms.clone());
    let search = use_query::<Vec<SearchHitOut>>(move || {
        if k1.is_some() || t1.is_empty() {
            return None;
        }
        Some(QuerySpec::new(format!("/explore/search?q={}&kind=album&limit=5", enc(&t1)), &[]))
    });
    let known2 = known.clone();
    let url = Memo::new(move |_| known2.clone().or_else(|| search.data.get().and_then(|d| d.first().map(|h| h.url.clone()))));
    let found = use_query::<ExploreReleaseOut>(move || url.get().map(|u| QuerySpec::new(format!("/explore/release?url={}", enc(&u)), &[])));
    let explore_q = format!("/explore?q={}", enc(&terms));
    let terms2 = terms.clone();
    let known3 = known.clone();
    view! {
        {move || {
            let explore_q = explore_q.clone();
            let terms = terms2.clone();
            if known3.is_none() && search.data.get().is_none() && search.error.get().is_none() {
                return view! { <div class="lib-note faint"><Icon name="search" size=13 />" Looking for this album on Bandcamp…"</div> }.into_any();
            }
            let Some(u) = url.get() else {
                return view! {
                    <p class="lib-note faint">{format!("Nothing on Bandcamp matched \u{201c}{terms}\u{201d}. ")}<a class="lib-link" href=explore_q>"Search for it yourself"</a></p>
                }.into_any();
            };
            let is_known = known3.is_some();
            let u2 = u.clone();
            view! {
                <div class="lib-anchor">
                    {move || found.data.get().and_then(|f| f.art_url.clone()).map(|a| view! { <img class="lib-anchor-art" src=a alt="" /> })}
                    <div class="grow truncate">
                        <span class="faint">{if is_known { "Exploring from " } else { "Best guess: " }}</span>
                        <a class="lib-link strong" href=release_path(&u)>{move || found.data.get().map(|f| format!("{} - {}", f.artist_name, f.title)).unwrap_or_else(|| u2.clone())}</a>
                    </div>
                    {(!is_known).then(|| view! { <a class="lib-link faint" href=explore_q.clone()>"wrong album?"</a> })}
                </div>
                {move || match (found.data.get(), found.error.get()) {
                    (Some(f), _) => view! { <BandcampRelated url=u.clone() seed=f.tags.clone() /> }.into_any(),
                    (None, Some(e)) => view! { <p class="lib-note danger-text">{format!("Bandcamp would not serve this release ({})", e.message())}</p> }.into_any(),
                    (None, None) => view! { <div class="lib-note faint">"Reading the release…"</div> }.into_any(),
                }}
            }.into_any()
        }}
    }
}

#[allow(dead_code)]
fn _unused() {
    let _ = paging::PAGE_SIZE;
}
