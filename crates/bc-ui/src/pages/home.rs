//! Home: the library as a storefront. One request (`GET /library/home?seed=`) returns
//! every shelf as a snapshot: fetched once, never swept up by a blanket invalidation
//! (`home:` cache keys), and changed only by the Refresh button, which lights a dot
//! when the library has something newer than the hero on show.
use std::cell::Cell;
use std::sync::Arc;

use bc_types::library::*;
use bc_types::player::PlayerCommand;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::data::{QuerySpec, use_query, use_topic};
use crate::ds::{Button, ErrorPanel, Icon, PageHeader, Skeleton, Variant, toast_err};
use crate::logic::format::format_count;
use crate::pages::albums::host::{LibraryHost, library_listing, play_items, play_release, provide_library_host};
use crate::player::use_player;
use crate::widgets::common::{Art, label_link};
use crate::widgets::{ColSize, Side, Splitter};

mod logic;
mod parts;
mod rail;
mod shelves;

use rail::{Favorites, Layout, StatTiles, TopTen};
use shelves::{CrateDig, DiscoveryShelves, DustOff, NewInLibrary, RecentlyPlayed, TopArtists};

thread_local! {
    /// One draw per page load: the shelves keep their hand until Refresh deals a new one.
    static SESSION_SEED: Cell<i64> = const { Cell::new(0) };
}

fn session_seed() -> i64 {
    SESSION_SEED.with(|s| {
        if s.get() == 0 {
            s.set((crate::util::entropy() % logic::MAX_SEED as u64) as i64 + 1);
        }
        s.get()
    })
}

const RAIL: ColSize = ColSize { key: "bc:ui:home-rail-w", default: 340.0, min: 260.0, max: 560.0 };

fn is_wide() -> bool {
    crate::util::media_matches("(min-width: 1280px)")
}

#[component]
pub fn HomePage() -> impl IntoView {
    provide_library_host();
    let player = use_player();
    let session = session_seed();
    let roll = RwSignal::new(0i64);
    let seed = Memo::new(move |_| logic::seed_for(session, roll.get()));
    let wide = RwSignal::new(is_wide());
    let _ = window_event_listener(leptos::ev::resize, move |_| wide.set(is_wide()));
    let rail_w = RAIL.signal();

    let home = use_query::<HomeShelves>(move || {
        let s = seed.get();
        Some(QuerySpec::keyed(format!("home:shelves:{s}"), format!("/library/home?seed={s}"), &[]))
    });
    let shelves: Signal<Option<Arc<HomeShelves>>> = home.data.into();
    let home_err = home.error;
    let home_q = StoredValue::new(home);

    // ---- freshness: is the newest release still the one on the page? ---------------------------
    let probe = use_query::<Page<ReleaseOut>>(|| Some(QuerySpec::keyed("probe:home-newest", "/releases?sort=added&order=desc&limit=1", &["release"])));
    let probe_data = probe.data;
    let probe_q = StoredValue::new(probe);
    use_topic::<serde_json::Value>("library.changed", move |_| probe_q.get_value().refetch());
    {
        use gloo_timers::callback::Interval;
        let iv = send_wrapper::SendWrapper::new(Interval::new(60_000, move || probe_q.get_value().refetch()));
        on_cleanup(move || drop(iv));
    }
    let shown = Memo::new(move |_| shelves.get().and_then(|s| s.new_in_library.first().map(|r| r.id)));
    let latest = Memo::new(move |_| probe_data.get().and_then(|p| p.items.first().map(|r| r.id)));
    let fresh = Memo::new(move |_| logic::is_fresh(shown.get(), latest.get()));
    let refreshing = RwSignal::new(false);
    let refresh = move || {
        refreshing.set(true);
        roll.update(|r| *r += 1);
        crate::data::invalidate_prefix("home");
        probe_q.get_value().refetch();
        crate::util::after(700, move || {
            let _ = refreshing.try_set(false);
        });
    };
    let refresh = Arc::new(refresh);

    // ---- play library / shuffle --------------------------------------------------------------------
    let starting = RwSignal::new(None::<bool>);
    let start = Arc::new(move |shuffle: bool| {
        starting.set(Some(shuffle));
        spawn_local(async move {
            let url = if shuffle { "/tracks?sort=random&limit=500" } else { "/tracks?sort=added&order=desc&limit=500" };
            match api::get::<TrackPage>(url).await {
                Ok(p) => play_items(player, &p.page.items, 0, None, shuffle),
                Err(e) => toast_err(&e.message()),
            }
            starting.set(None);
        });
    });
    let (st1, st2) = (start.clone(), start);

    let subtitle = Signal::derive(move || {
        shelves.get().map(|s| format!("{} albums · {} tracks · {} artists", format_count(s.stats.releases), format_count(s.stats.tracks), format_count(s.stats.artists)))
    });
    let rf = refresh.clone();

    view! {
        <div class="page">
            <PageHeader title="Home" subtitle=subtitle
                actions=crate::ds::children(move || {
                    let (st1, st2, rf) = (st1.clone(), st2.clone(), rf.clone());
                    view! {
                        <button type="button" class="hm-refresh" class:fresh=move || fresh.get() disabled=move || refreshing.get()
                            title=move || if fresh.get() { "New in the library: refresh the shelves" } else { "Refresh the shelves" }
                            aria-label=move || if fresh.get() { "Refresh the shelves, new content available" } else { "Refresh the shelves" }
                            on:click=move |_| rf()>
                            <Icon name="refresh" size=14 />
                            <span class="hide-sm">"Refresh"</span>
                            {move || fresh.get().then(|| view! { <span class="hm-dot" aria-hidden="true"></span> })}
                        </button>
                        <Button variant=Variant::Primary icon="play" busy=Signal::derive(move || starting.get() == Some(false)) title="Play the library" on_click=move |_| st1(false)><span class="hide-sm">"Play library"</span></Button>
                        <Button icon="shuffle" title="Shuffle the whole library" busy=Signal::derive(move || starting.get() == Some(true)) on_click=move |_| st2(true)><span class="hide-sm">"Shuffle"</span></Button>
                    }
                }) />
            <div class="page-scroll hm">
                {move || {
                    if home_err.get().is_some() && shelves.get().is_none() {
                        let hq = home_q;
                        return view! { <ErrorPanel message=Signal::derive(move || home_err.get().map(|e| e.message()).unwrap_or_default()) on_retry=Callback::new(move |_| hq.get_value().refetch()) /> }.into_any();
                    }
                    if shelves.get().is_none() {
                        return view! { <HomeSkeleton /> }.into_any();
                    }
                    view! {
                        <div class="hm-cols" style=move || format!("--hm-rail-w:{}px", rail_w.get())>
                            <div class="hm-main">
                                <Hero shelves=shelves />
                                <NewInLibrary shelves=shelves />
                                <CrateDig shelves=shelves session_seed=session />
                                <RecentlyPlayed shelves=shelves />
                                <TopArtists shelves=shelves />
                                <DiscoveryShelves shelves=shelves />
                                <DustOff shelves=shelves session_seed=session />
                                {move || (!wide.get()).then(|| view! {
                                    <TopTen shelves=shelves />
                                    <StatTiles shelves=shelves layout=Layout::Band />
                                    <Favorites shelves=shelves layout=Layout::Band />
                                })}
                            </div>
                            {move || wide.get().then(|| view! {
                                <Splitter width=rail_w size=RAIL side=Side::Right label="Resize the side column" class="hm-splitter" />
                                <aside class="hm-rail">
                                    <TopTen shelves=shelves />
                                    <StatTiles shelves=shelves layout=Layout::Rail />
                                    <Favorites shelves=shelves layout=Layout::Rail />
                                </aside>
                            })}
                        </div>
                    }.into_any()
                }}
            </div>
            <LibraryHost />
        </div>
    }
}

#[component]
fn HomeSkeleton() -> impl IntoView {
    view! {
        <div class="hm-main" aria-busy="true" aria-label="Loading the shelves">
            <div class="hm-hero">
                <div class="hm-hero-art"><Skeleton height="100%" /></div>
                <div class="hm-hero-text"><Skeleton width="30%" height="12px" /><Skeleton width="70%" height="40px" /><Skeleton width="40%" height="16px" /></div>
            </div>
            <div class="lib-row">{(0..8).map(|_| view! { <div class="lib-row-item"><Skeleton height="170px" /><div style="height:8px"></div><Skeleton width="80%" height="12px" /></div> }).collect_view()}</div>
        </div>
    }
}

/// The newest release, at poster size.
#[component]
fn Hero(shelves: Signal<Option<Arc<HomeShelves>>>) -> impl IntoView {
    let player = use_player();
    let release = Memo::new(move |_| shelves.get().and_then(|s| s.new_in_library.first().cloned()));
    view! {
        {move || release.get().map(|r| {
            let id = r.id;
            let is_current = Memo::new(move |_| player.state.with(|s| s.current.as_ref().and_then(|c| c.release_id) == Some(id)));
            let playing = Memo::new(move |_| is_current.get() && player.is_playing());
            let tint = r.art_color.clone().filter(|c| c.len() == 7 && c.starts_with('#')).map(|c| format!("--tint:{c}")).unwrap_or_default();
            let tags: Vec<String> = r.tags.iter().take(5).cloned().collect();
            view! {
                <section class="hm-hero" style=tint>
                    <a class="hm-hero-art" href=format!("/albums/{id}") aria-label=r.title.clone()><Art src=r.art_url.clone() class="alb-art" /></a>
                    <div class="hm-hero-text">
                        <div class="hm-eyebrow">"New in library"</div>
                        <a class="hm-hero-title" href=format!("/albums/{id}")>{r.title.clone()}</a>
                        <div class="hm-hero-sub">
                            {r.artist.clone().map(|a| view! { <a href=format!("/artists/{}", a.id)>{a.name}</a> })}
                            {r.label.clone().map(|l| view! { <span class="faint">" · "{label_link(Some(&l), r.label_id)}</span> })}
                            {r.year.map(|y| view! { <span class="faint mono">{format!(" · {y}")}</span> })}
                        </div>
                        {(!tags.is_empty()).then(|| view! {
                            <div class="lib-tagrow">{tags.into_iter().map(|t| view! { <a class="lib-tag" href=format!("/tracks?tag={}", crate::util::enc(&t))>{t.clone()}</a> }).collect_view()}</div>
                        })}
                        <div class="hm-hero-actions">
                            <Button variant=Variant::Primary icon=crate::ds::dyn_icon(move || if playing.get() { "pause" } else { "play" }) on_click=move |_| {
                                if is_current.get_untracked() { player.cmd(PlayerCommand::Toggle) } else { play_release(player, id, library_listing(), false) }
                            }>{move || if playing.get() { "Pause" } else { "Play album" }}</Button>
                            <a class="btn btn-outline" href=format!("/albums/{id}")>"Open album"</a>
                        </div>
                    </div>
                </section>
            }
        })}
    }
}
