//! Artists: the directory (virtualised grid of 30k+ artists, server sort/filter, favourites,
//! selection) and the artist page.
use std::collections::HashSet;
use std::sync::Arc;

use bc_types::library::{ArtistOut, FavoritesOut, Page, TrackPage, TrackQuery, TrackSort};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Button, EmptyState, PageHeader, SearchInput, Select, SelectOption, Size, Variant, children, use_debounced};
use crate::logic::format::format_count;
use crate::util::{enc, qs, qs_pairs};
use crate::widgets::card_grid::CardGrid;
use crate::widgets::common::play_tracks;
use crate::widgets::{PageFetcher, PageRes};

mod detail;

pub use detail::ArtistDetailPage;

use crate::pages::labels::logic as lg;
use crate::pages::labels::shared::{Cover, FavButton, Favs, Kind, use_favs};
use crate::ds::Icon;

fn artists_url(q: &str, sort: &str, offset: usize, limit: usize) -> String {
    let s = lg::artist_sort(sort);
    qs_url("/artists", &[("q", q.trim().to_string()), ("sort", sort.to_string()), ("order", lg::dir_str(s.order).to_string()), ("offset", offset.to_string()), ("limit", limit.to_string())])
}

fn qs_url(base: &str, pairs: &[(&str, String)]) -> String {
    format!("{base}{}", qs(pairs))
}

/// Ids of the artists at positions `lo..=hi` of a listing.
async fn fetch_ids(q: String, sort: String, lo: usize, hi: usize) -> Vec<i64> {
    let mut out = vec![];
    let mut at = lo;
    while at <= hi {
        let n = (hi - at + 1).min(500);
        match api::get::<Page<ArtistOut>>(&artists_url(&q, &sort, at, n)).await {
            Ok(p) if !p.items.is_empty() => {
                at += p.items.len();
                out.extend(p.items.iter().map(|a| a.id));
            }
            Ok(_) => break,
            Err(e) => {
                crate::ds::toast_err(&e.message());
                break;
            }
        }
    }
    out
}

/// Play every track of an artist (album order), or a random draw of them.
pub fn play_artist(id: i64, shuffle: bool) {
    let mut tq = TrackQuery { artist_id: Some(id), limit: Some(500), offset: Some(0), ..Default::default() };
    if shuffle {
        tq.sort = Some(TrackSort::Random);
        tq.seed = Some((crate::util::entropy() % 1_000_000 + 1) as i64);
    } else {
        tq.sort = Some(TrackSort::Album);
        tq.order = Some(bc_types::library::SortDir::Asc);
    }
    let url = format!("/tracks{}", qs_pairs(&tq.to_pairs()));
    spawn_local(async move {
        match api::get::<TrackPage>(&url).await {
            Ok(p) if !p.page.items.is_empty() => play_tracks(&p.page.items, 0, None),
            Ok(_) => crate::ds::toast_info("No tracks to play"),
            Err(e) => crate::ds::toast_err(&e.message()),
        }
    });
}

#[derive(Clone, PartialEq)]
struct ArtistRow {
    idx: usize,
    artist: ArtistOut,
}

#[component]
fn ArtistCard(row: ArtistRow, favs: Favs, picked: RwSignal<HashSet<i64>>, plays: Signal<bool>, on_pick: Callback<(usize, i64, bool)>) -> impl IntoView {
    let a = row.artist;
    let (idx, id) = (row.idx, a.id);
    let is_picked = move || picked.with(|p| p.contains(&id));
    let selecting = move || picked.with(|p| !p.is_empty());
    let art = a.art_url.as_ref().map(|u| lg::thumb(u));
    let name = a.name.clone();
    let (n_sel, n_fav, n_play) = (name.clone(), name.clone(), name.clone());
    let counts = {
        let a = a.clone();
        move || {
            if plays.get() { vec![lg::count_of(a.play_count, "play")] } else { vec![lg::count_of(a.release_count, "release"), lg::count_of(a.track_count, "track")] }
        }
    };
    view! {
        <div class="pp-card pp-artist" class:sel=is_picked>
            <div class="pp-card-art pp-round">
                <Cover src=art icon="user" />
                {(a.track_count > 0).then(|| view! {
                    <button type="button" class="pp-chipbtn pp-chipbtn-primary pp-card-act pp-card-play" aria-label=format!("Play {n_play}")
                        on:click=move |ev| { ev.stop_propagation(); ev.prevent_default(); play_artist(id, false); }><Icon name="play" /></button>
                })}
            </div>
            <div class="pp-card-meta pp-center">
                <div class="pp-card-title truncate" title=name.clone()>{name.clone()}</div>
                {move || counts().into_iter().map(|c| view! { <div class="pp-card-sub num truncate">{c}</div> }).collect_view()}
            </div>
            <a class="pp-card-link" href=format!("/artists/{id}") aria-label=name.clone()></a>
            <button type="button" role="checkbox" aria-checked=move || is_picked().to_string() aria-label=format!("Select {n_sel}")
                class=move || if is_picked() { "pp-check pp-check-btn on" } else if selecting() { "pp-check pp-check-btn always" } else { "pp-check pp-check-btn" }
                on:click=move |ev: web_sys::MouseEvent| { ev.prevent_default(); ev.stop_propagation(); on_pick.run((idx, id, ev.shift_key())); }>
                <Icon name="check" />
            </button>
            <div class="pp-card-tools"><FavButton kind=Kind::Artist id=id favs=favs overlay=true name=n_fav /></div>
        </div>
    }
}

#[component]
pub fn ArtistsPage() -> impl IntoView {
    let params = use_query_map();
    let navigate = use_navigate();
    let favs = use_favs();
    let init = params.get_untracked();
    let filter = RwSignal::new(init.get("q").unwrap_or_default());
    let sort = RwSignal::new(init.get("sort").filter(|s| lg::ARTIST_SORTS.iter().any(|d| d.value == s)).unwrap_or_else(|| "name".into()));
    let only_favs = RwSignal::new(init.get("fav").is_some());
    let q = use_debounced(filter, 120);
    let total = RwSignal::new(None::<usize>);
    let picked: RwSignal<HashSet<i64>> = RwSignal::new(HashSet::new());
    let anchor = StoredValue::new(None::<usize>);

    {
        let navigate = navigate.clone();
        Effect::new(move |prev: Option<()>| {
            let (qv, sv, fv) = (q.get(), sort.get(), only_favs.get());
            if prev.is_none() {
                return;
            }
            let mut parts = vec![];
            if !qv.trim().is_empty() {
                parts.push(format!("q={}", enc(qv.trim())));
            }
            if sv != "name" {
                parts.push(format!("sort={sv}"));
            }
            if fv {
                parts.push("fav=1".into());
            }
            let url = if parts.is_empty() { "/artists".to_string() } else { format!("/artists?{}", parts.join("&")) };
            navigate(&url, NavigateOptions { replace: true, ..Default::default() });
        });
    }

    let directory = use_query::<Page<ArtistOut>>(|| Some(QuerySpec::keyed("artists:count", "/artists?limit=1", &["artist"])));
    let fav_list = use_query::<FavoritesOut>(|| Some(QuerySpec::keyed("favorites", "/favorites", &["artist", "label", "favorite"])));

    let fetcher: PageFetcher<ArtistRow> = Arc::new(move |req| {
        let offset = req.offset;
        if only_favs.get_untracked() {
            // The favourites shelf is small: one request, filtered here.
            let needle = q.get_untracked().trim().to_lowercase();
            return Box::pin(async move {
                let f: FavoritesOut = api::get("/favorites").await?;
                let mut rows: Vec<ArtistOut> = f.artists.into_iter().filter(|a| needle.is_empty() || a.name.to_lowercase().contains(&needle)).collect();
                rows.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                let total = rows.len();
                Ok(PageRes { rows: rows.into_iter().skip(offset).take(req.limit).enumerate().map(|(i, artist)| ArtistRow { idx: offset + i, artist }).collect(), total })
            });
        }
        let url = artists_url(&q.get_untracked(), &sort.get_untracked(), offset, req.limit);
        Box::pin(async move {
            let p: Page<ArtistOut> = api::get(&url).await?;
            Ok(PageRes { rows: p.items.into_iter().enumerate().map(|(i, artist)| ArtistRow { idx: offset + i, artist }).collect(), total: p.total as usize })
        })
    });
    let source_key = Signal::derive(move || format!("{}|{}|{}", q.get().trim(), sort.get(), only_favs.get()));
    let plays = Signal::derive(move || sort.get() == "plays");

    let on_pick = Callback::new(move |(idx, id, shift): (usize, i64, bool)| {
        let from = anchor.get_value();
        anchor.set_value(Some(idx));
        match (shift, from, only_favs.get_untracked()) {
            (true, Some(a), false) => {
                let (lo, hi) = lg::shift_range(a, idx);
                let (qv, sv) = (q.get_untracked(), sort.get_untracked());
                spawn_local(async move {
                    let ids = fetch_ids(qv, sv, lo, hi).await;
                    let _ = picked.try_update(|p| p.extend(ids));
                });
            }
            _ => picked.update(|p| {
                if !p.remove(&id) {
                    p.insert(id);
                }
            }),
        }
    });
    let select_all = move |_| {
        let n = total.get_untracked().unwrap_or(0);
        if n == 0 || only_favs.get_untracked() {
            return;
        }
        let (qv, sv) = (q.get_untracked(), sort.get_untracked());
        spawn_local(async move {
            let ids = fetch_ids(qv, sv, 0, n - 1).await;
            let _ = picked.try_set(ids.into_iter().collect());
        });
    };
    let fav_picked = move |on: bool| {
        let ids: Vec<i64> = picked.get_untracked().into_iter().collect();
        for id in ids {
            favs.put(Kind::Artist, id, on);
        }
        crate::ds::toast_ok(if on { "Added to favourites" } else { "Removed from favourites" });
    };

    let n_picked = Signal::derive(move || picked.with(|p| p.len()));
    let subtitle = Signal::derive(move || {
        let t = total.get()?;
        let all = directory.data.get().map(|d| d.total as usize);
        Some(match all {
            Some(a) if a != t => format!("{} of {} artists", format_count(t as i64), format_count(a as i64)),
            _ => format!("{} artists", format_count(t as i64)),
        })
    });
    let sort_options = Signal::derive(|| lg::ARTIST_SORTS.iter().map(|s| SelectOption::new(s.value, s.label)).collect::<Vec<_>>());
    let n_favs = Signal::derive(move || fav_list.data.get().map(|f| f.artists.len()).unwrap_or(0));

    view! {
        <div class="page pp-page">
            <PageHeader title="Artists" subtitle=subtitle
                actions=children(move || view! {
                    <Button icon="shuffle" title="Shuffle favourite artists" disabled=Signal::derive(move || n_favs.get() == 0) on_click=move |_| shuffle_favourites(favs)>
                        <span class="hide-sm">"Shuffle favourites"</span>
                    </Button>
                }) />
            <div class="pp-toolbar">
                <SearchInput value=filter placeholder="Filter artists\u{2026}" class="pp-search" />
                <button type="button" class="chip" aria-pressed=move || only_favs.get().to_string() on:click=move |_| only_favs.update(|f| *f = !*f) title="Only favourite artists">
                    <Icon name=move || if only_favs.get() { "heart-fill".to_string() } else { "heart".to_string() } />"Favourites"
                    <span class="num faint">{move || n_favs.get()}</span>
                </button>
                <span class="spacer"></span>
                <Select options=sort_options value=sort aria_label="Sort artists" class="pp-sort" />
            </div>
            <div class="pp-notices">
                {move || (n_picked.get() > 0).then(|| view! {
                    <div class="pp-selbar" role="status">
                        <span class="num">{move || format_count(n_picked.get() as i64)}</span>
                        <span>{move || if n_picked.get() == 1 { "artist selected" } else { "artists selected" }}</span>
                        <span class="spacer"></span>
                        <Button size=Size::Sm variant=Variant::Ghost icon="heart-fill" on_click=move |_| fav_picked(true)>"Favourite"</Button>
                        <Button size=Size::Sm variant=Variant::Ghost icon="heart" on_click=move |_| fav_picked(false)>"Unfavourite"</Button>
                        <Button size=Size::Sm variant=Variant::Ghost on_click=select_all disabled=Signal::derive(move || only_favs.get() || Some(n_picked.get()) == total.get())>
                            {move || format!("Select all {}", format_count(total.get().unwrap_or(0) as i64))}
                        </Button>
                        <Button size=Size::Sm variant=Variant::Ghost on_click=move |_| { picked.set(HashSet::new()); anchor.set_value(None); }>"Clear"</Button>
                    </div>
                })}
            </div>
            <div class="pp-fill">
                {move || total.get().is_none().then(|| view! { <div class="pp-skel-grid round" aria-hidden="true">{(0..14).map(|_| view! { <div class="pp-skel-card"><div class="skeleton"></div><div class="skeleton"></div></div> }).collect_view()}</div> })}
                <CardGrid fetch=fetcher source_key=source_key min_card_w=144.0 meta_h=64.0 gap=12.0 entities=vec!["artist"] total_out=total
                    render=Callback::new(move |(row, _w): (ArtistRow, f64)| view! { <ArtistCard row=row favs=favs picked=picked plays=plays on_pick=on_pick /> }.into_any())
                    empty=move || {
                        let f = filter.get().trim().to_string();
                        let (t, h) = if only_favs.get() {
                            ("No favourite artists".to_string(), "Tap the heart on an artist to keep them here.".to_string())
                        } else if !f.is_empty() {
                            (format!("No artist matches \u{201c}{f}\u{201d}"), "Try fewer letters, or search tracks and albums from the top bar.".to_string())
                        } else if sort.get() == "plays" {
                            ("Nothing has been played yet".to_string(), "\u{201c}Most played\u{201d} lists artists once they have plays.".to_string())
                        } else {
                            ("No artists yet".to_string(), "Artists appear once the library has been scanned.".to_string())
                        };
                        view! { <EmptyState icon="user" title=t hint=h /> }
                    } />
            </div>
        </div>
    }
}

/// A random draw across the favourite artists' tracks.
fn shuffle_favourites(favs: Favs) {
    if favs.artists.with_untracked(|a| a.is_empty()) {
        return;
    }
    spawn_local(async move {
        let mut tq = TrackQuery { limit: Some(500), offset: Some(0), sort: Some(TrackSort::Random), seed: Some((crate::util::entropy() % 1_000_000 + 1) as i64), ..Default::default() };
        tq.favorites = Some(true);
        let url = format!("/tracks{}", qs_pairs(&tq.to_pairs()));
        match api::get::<TrackPage>(&url).await {
            Ok(p) if !p.page.items.is_empty() => play_tracks(&p.page.items, 0, None),
            Ok(_) => crate::ds::toast_info("No tracks from favourites"),
            Err(e) => crate::ds::toast_err(&e.message()),
        }
    });
}
