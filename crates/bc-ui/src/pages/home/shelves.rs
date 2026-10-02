//! The Home shelves: new in library, crate dig, recently played, top artists,
//! rediscover / buried treasure, dust off. All are views of one `HomeShelves` snapshot;
//! only "reshuffle" and the genre chips ask for more (under `home:` keys, so a blanket
//! invalidation never redraws them under the reader).
use std::sync::Arc;

use bc_types::library::*;
use leptos::prelude::*;

use super::logic;
use super::parts::{SectionHeader, ShelfControls, TracksFn, TrackRow};
use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Icon, toast_err};
use crate::logic::format::{format_count, format_duration_ms};
use crate::pages::albums::card::AlbumCard;
use crate::pages::albums::host::{fetch_shelf_tracks, play_items, shuffle_in_place};
use crate::pages::albums::logic as alogic;
use crate::player::use_player;
use crate::util::{enc, qs_pairs};
use crate::widgets::card_grid::grid_metrics;
use crate::widgets::common::Art;

pub type Shelves = Signal<Option<Arc<HomeShelves>>>;

fn shelf_tracks_fn(ids: Vec<i64>) -> TracksFn {
    Arc::new(move || {
        let ids = ids.clone();
        Box::pin(async move { fetch_shelf_tracks(&ids).await })
    })
}

fn this_year() -> i64 {
    alogic::civil_from_days((crate::util::unix_ms() / 86_400_000.0).floor() as i64).0
}

#[component]
pub fn NewInLibrary(shelves: Shelves) -> impl IntoView {
    let items = Memo::new(move |_| shelves.get().map(|s| s.new_in_library.iter().skip(1).cloned().collect::<Vec<_>>()).unwrap_or_default());
    view! {
        {move || {
            let list = items.get();
            if list.is_empty() {
                return ().into_any();
            }
            let ids: Vec<i64> = list.iter().map(|r| r.id).collect();
            view! {
                <section class="hm-sec">
                    <SectionHeader title="Latest additions" to="/albums"><ShelfControls get_tracks=shelf_tracks_fn(ids) /></SectionHeader>
                    <div class="lib-row">
                        {list.into_iter().map(|r| view! { <div class="lib-row-item"><AlbumCard release=r /></div> }).collect_view()}
                    </div>
                </section>
            }.into_any()
        }}
    }
}

// ---- crate dig -------------------------------------------------------------------------------------

#[component]
pub fn CrateDig(shelves: Shelves, session_seed: i64) -> impl IntoView {
    let genre = RwSignal::new(None::<String>);
    let page = RwSignal::new(0usize);
    let roll = RwSignal::new(0i64);
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
    let cols = Memo::new(move |_| grid_metrics(width.get().max(120.0), 150.0, 12.0).0.min(6));
    let seed = Memo::new(move |_| logic::seed_for(session_seed, roll.get()));
    let is_default = move || genre.with(|g| g.is_none()) && page.get() == 0 && roll.get() == 0;

    let dealt = use_query::<Page<ReleaseOut>>(move || {
        if is_default() {
            return None;
        }
        let q = ReleaseQuery {
            sort: Some(ReleaseSort::Random),
            seed: Some(seed.get()),
            limit: Some(logic::CRATE_PER_PAGE as i64),
            offset: Some((page.get() * logic::CRATE_PER_PAGE) as i64),
            tags: genre.get().into_iter().collect(),
            ..Default::default()
        };
        let key = format!("home:crate:{}:{}:{}", genre.get().unwrap_or_default(), seed.get(), page.get());
        Some(QuerySpec::keyed(key, format!("/releases{}", qs_pairs(&q.to_pairs())), &[]))
    });
    let dealt_data = dealt.data;
    let items = Memo::new(move |_| -> Vec<ReleaseOut> {
        if is_default() {
            shelves.get().map(|s| s.crate_dig.iter().take(logic::CRATE_PER_PAGE).cloned().collect()).unwrap_or_default()
        } else {
            dealt_data.get().map(|d| d.items.clone()).unwrap_or_default()
        }
    });
    let total = Memo::new(move |_| {
        if is_default() {
            // the shelf only hands out one page; the true size comes with the first query
            shelves.get().map(|s| s.crate_dig.len()).unwrap_or(0).max(logic::CRATE_PER_PAGE + 1)
        } else {
            dealt_data.get().map(|d| d.total as usize).unwrap_or(0)
        }
    });
    let pages = Memo::new(move |_| logic::page_count(total.get(), logic::CRATE_PER_PAGE));
    let tags = Memo::new(move |_| shelves.get().map(|s| s.top_tags.clone()).unwrap_or_default());
    let reset = move |f: Box<dyn FnOnce()>| {
        page.set(0);
        f();
    };
    let reset = Arc::new(reset);
    let (rs1, rs2) = (reset.clone(), reset);
    let genre_chip = move |label: String, value: Option<String>| {
        let (v1, v2) = (value.clone(), value.clone());
        let rs = rs2.clone();
        view! {
            <button type="button" class="lib-pill" class:on=move || genre.get() == v1 aria-pressed=move || (genre.get() == v2).to_string()
                on:click=move |_| {
                    let v = value.clone();
                    rs(Box::new(move || genre.update(|g| *g = if *g == v { None } else { v })))
                }>{label}</button>
        }
    };
    let at_end = move || page.get() + 1 >= pages.get();
    let show = Memo::new(move |_| shelves.get().is_some());
    let href = move || match genre.get() { Some(g) => format!("/albums?tag={}", enc(&g)), None => "/albums".to_string() };

    view! {
        {move || show.get().then(|| {
            let (rs1, genre_chip) = (rs1.clone(), genre_chip.clone());
            view! {
                <section class="hm-sec">
                    <div class="hm-sec-head">
                        <h2 class="hm-sec-title">"Dig through the crate"</h2>
                        <div class="hm-sec-actions">
                            <ShelfControls get_tracks={
                                let f: TracksFn = Arc::new(move || {
                                    let ids: Vec<i64> = items.get_untracked().iter().map(|r| r.id).collect();
                                    Box::pin(async move { fetch_shelf_tracks(&ids).await })
                                });
                                f
                            } />
                            <button type="button" class="hm-act" on:click=move |_| { let rs = rs1.clone(); rs(Box::new(move || roll.update(|r| *r += 1))) }><Icon name="refresh" size=13 />"Reshuffle"</button>
                            <a class="hm-viewall" href=href>"View all"<Icon name="chevron-right" size=13 /></a>
                        </div>
                    </div>
                    <div node_ref=holder class="hm-crate" style=move || format!("--cols:{}", cols.get())>
                        {move || {
                            let n = cols.get() * 2;
                            items.get().into_iter().take(n).map(|r| view! { <AlbumCard release=r /> }).collect_view()
                        }}
                    </div>
                    {move || (items.get().is_empty() && !dealt.loading.get() && genre.get().is_some()).then(|| view! { <p class="faint lib-note">{format!("Nothing in the library is tagged {}.", genre.get().unwrap_or_default())}</p> })}
                    <div class="hm-crate-foot">
                        <div class="hm-chips">
                            {genre_chip("Everything".into(), None)}
                            {tags.get().into_iter().map(|t| genre_chip(t.name.clone(), Some(t.name))).collect_view()}
                        </div>
                        {move || (total.get() > logic::CRATE_PER_PAGE).then(|| view! {
                            <div class="hm-pager">
                                <span class="mono faint">{move || format!("{} / {}", page.get() + 1, pages.get())}</span>
                                <button type="button" class="lib-roundbtn" aria-label="Previous page" title="Previous page" disabled=move || page.get() == 0 on:click=move |_| page.update(|p| *p = p.saturating_sub(1))><Icon name="chevron-left" size=14 /></button>
                                <button type="button" class="lib-roundbtn" aria-label="Next page" title="Next page" disabled=at_end on:click=move |_| page.update(|p| *p += 1)><Icon name="chevron-right" size=14 /></button>
                            </div>
                        })}
                    </div>
                </section>
            }
        })}
    }
}

// ---- recently played ------------------------------------------------------------------------------

#[component]
pub fn RecentlyPlayed(shelves: Shelves) -> impl IntoView {
    let player = use_player();
    let tracks = Memo::new(move |_| shelves.get().map(|s| s.recently_played.iter().map(|e| (e.track.clone(), e.started_at.clone())).collect::<Vec<_>>()).unwrap_or_default());
    view! {
        {move || {
            let list = tracks.get();
            if list.is_empty() {
                return ().into_any();
            }
            let all: Vec<TrackOut> = list.iter().map(|(t, _)| t.clone()).collect();
            let all2 = all.clone();
            let get: TracksFn = Arc::new(move || { let a = all2.clone(); Box::pin(async move { Ok(a) }) });
            let now = crate::util::unix_ms();
            view! {
                <section class="hm-sec">
                    <SectionHeader title="Recently played"><ShelfControls get_tracks=get /></SectionHeader>
                    <div class="hm-list">
                        {list.into_iter().enumerate().map(|(i, (t, at))| {
                            let all = all.clone();
                            let ago = alogic::parse_iso_ms(&at).map(|ms| crate::logic::format::format_ago(((now - ms) / 1000.0) as i64)).unwrap_or_default();
                            view! {
                                <TrackRow track=t index=i trailing=ago on_play=Callback::new(move |ix| play_items(player, &all, ix, None, false)) />
                            }
                        }).collect_view()}
                    </div>
                </section>
            }.into_any()
        }}
    }
}

// ---- top artists ------------------------------------------------------------------------------------

#[component]
pub fn TopArtists(shelves: Shelves) -> impl IntoView {
    let items = Memo::new(move |_| shelves.get().map(|s| s.top_artists.clone()).unwrap_or_default());
    view! {
        {move || {
            let list = items.get();
            if list.is_empty() {
                return ().into_any();
            }
            let ids: Vec<i64> = list.iter().map(|a| a.id).collect();
            // each artist's most played tracks, dealt in shelf order (one request per artist; the shelf is 8 wide)
            let get: TracksFn = Arc::new(move || {
                let ids = ids.clone();
                Box::pin(async move {
                    let mut out = vec![];
                    for id in ids {
                        let p: TrackPage = api::get(&format!("/tracks?artist_id={id}&sort=play_count&order=desc&limit=60")).await?;
                        out.extend(p.page.items);
                    }
                    out.truncate(500);
                    Ok(out)
                })
            });
            view! {
                <section class="hm-sec">
                    <SectionHeader title="Most played artists" to="/artists"><ShelfControls get_tracks=get /></SectionHeader>
                    <div class="lib-row">
                        {list.into_iter().map(|a| view! {
                            <a class="hm-artist" href=format!("/artists/{}", a.id)>
                                <span class="hm-artist-art"><Art src=a.art_url.clone() class="alb-art" /></span>
                                <span class="hm-artist-name truncate">{a.name.clone()}</span>
                                <span class="mono faint hm-artist-plays">{format!("{} plays", format_count(a.play_count))}</span>
                            </a>
                        }).collect_view()}
                    </div>
                </section>
            }.into_any()
        }}
    }
}

// ---- rediscover / buried treasure ------------------------------------------------------------------------

#[component]
fn DiscoveryPanel(#[prop(into)] title: String, icon: &'static str, items: Vec<TrackOut>, dates: Vec<String>) -> impl IntoView {
    let player = use_player();
    let all = Arc::new(items.clone());
    let (a1, a2) = (all.clone(), all.clone());
    let year = this_year();
    view! {
        <div class="hm-panel">
            <div class="hm-panel-head">
                <h3 class="hm-panel-title"><span class="hm-panel-icon"><Icon name=icon /></span>{title}</h3>
                <div class="hm-panel-btns">
                    <button type="button" class="hm-pill" on:click=move |_| play_items(player, &a1, 0, None, true)><Icon name="play" size=11 />"Play"</button>
                    <button type="button" class="hm-pill" on:click=move |_| { let mut d = (*a2).clone(); shuffle_in_place(&mut d); play_items(player, &d, 0, None, true) }><Icon name="shuffle" size=11 />"Shuffle"</button>
                </div>
            </div>
            <div class="hm-list">
                {items.into_iter().enumerate().zip(dates).map(|((i, t), d)| {
                    let all = all.clone();
                    view! { <TrackRow track=t index=i trailing=alogic::short_date(&d, year) on_play=Callback::new(move |ix| play_items(player, &all, ix, None, true)) /> }
                }).collect_view()}
            </div>
        </div>
    }
}

#[component]
pub fn DiscoveryShelves(shelves: Shelves) -> impl IntoView {
    view! {
        {move || shelves.get().map(|s| {
            let (re, bu) = (s.rediscover.clone(), s.buried_treasure.clone());
            if re.is_empty() && bu.is_empty() {
                return ().into_any();
            }
            let both = !re.is_empty() && !bu.is_empty();
            let re_dates = re.iter().map(|t| t.last_played_at.clone().unwrap_or_default()).collect::<Vec<_>>();
            let bu_dates = bu.iter().map(|t| t.added_at.clone().unwrap_or_default()).collect::<Vec<_>>();
            view! {
                <section class="hm-sec hm-panels" class:both=both>
                    {(!re.is_empty()).then(|| view! { <DiscoveryPanel title="Rediscover" icon="clock" items=re.clone() dates=re_dates.clone() /> })}
                    {(!bu.is_empty()).then(|| view! { <DiscoveryPanel title="Buried treasure" icon="sparkles" items=bu.clone() dates=bu_dates.clone() /> })}
                </section>
            }.into_any()
        })}
    }
}

// ---- dust off -----------------------------------------------------------------------------------------------

#[component]
pub fn DustOff(shelves: Shelves, session_seed: i64) -> impl IntoView {
    let roll = RwSignal::new(0i64);
    let dealt = use_query::<Page<ReleaseOut>>(move || {
        if roll.get() == 0 {
            return None;
        }
        let q = ReleaseQuery { sort: Some(ReleaseSort::Random), seed: Some(logic::seed_for(session_seed, roll.get() + 100)), limit: Some(12), ..Default::default() };
        Some(QuerySpec::keyed(format!("home:dust-off:{}", roll.get()), format!("/releases{}", qs_pairs(&q.to_pairs())), &[]))
    });
    let dealt_data = dealt.data;
    let items = Memo::new(move |_| -> Vec<ReleaseOut> {
        if roll.get() == 0 {
            shelves.get().map(|s| s.dust_off.clone()).unwrap_or_default()
        } else {
            dealt_data.get().map(|d| d.items.clone()).unwrap_or_default()
        }
    });
    view! {
        {move || {
            let list = items.get();
            if list.is_empty() {
                return ().into_any();
            }
            let get: TracksFn = Arc::new(move || {
                let ids: Vec<i64> = items.get_untracked().iter().map(|r| r.id).collect();
                Box::pin(async move { fetch_shelf_tracks(&ids).await })
            });
            view! {
                <section class="hm-sec">
                    <SectionHeader title="Dust off a record">
                        <ShelfControls get_tracks=get />
                        <button type="button" class="hm-act" on:click=move |_| roll.update(|r| *r += 1)><Icon name="refresh" size=13 />"Reshuffle"</button>
                    </SectionHeader>
                    <div class="lib-row">
                        {list.into_iter().map(|r| view! { <div class="lib-row-item"><AlbumCard release=r /></div> }).collect_view()}
                    </div>
                </section>
            }.into_any()
        }}
    }
}

#[allow(dead_code)]
fn _keep() {
    let _ = toast_err;
    let _ = format_duration_ms;
}
