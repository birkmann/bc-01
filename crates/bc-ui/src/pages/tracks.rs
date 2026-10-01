//! Tracks / Loved: the reference DataTable page, extended. Unlimited rows, server
//! sort, tag-AND filter with suggestions, added window, list / album-grid toggle,
//! selection with bulk actions, exports, save as playlist, analyse all, Bandcamp
//! search on a miss. Loved adds loved streams, suggestions and DJ mix.
use std::sync::Arc;

use bc_types::library::*;
use bc_types::player::PlayerCommand;
use bc_types::ui::Selection;
use leptos::prelude::*;

use crate::api;
use crate::ds::{Button, EmptyState, Icon, MenuEntry, MenuItem, PageHeader, Variant, toast_err, toast_info, toast_ok};
use crate::logic::format::{format_bpm, format_count, format_duration_ms, format_playtime};
use crate::pages::albums::card::{AlbumCard, META_H};
use crate::pages::albums::filters::{AddedFilter, AddedFilterBar, ReleaseSortBar, ReleaseSortState, TagFilterBar, TagScope, UrlState, describe_tags};
use crate::pages::albums::host::{LibraryHost, PickerReq, ids_of, play_items, provide_library_host, use_library_host};
use crate::pages::albums::logic as alogic;
use crate::player::use_player;
use crate::util::{enc, qs_pairs};
use crate::widgets::card_grid::CardGrid;
use crate::widgets::common::{Art, LoveButton, album_link, artist_link, queue_item, title_link};
use crate::widgets::dnd::DragPayload;
use crate::widgets::{Column, DataTable, PageFetcher, PageRes};

mod bulk;
mod loved;
mod miss;

use bulk::{SelectionBar, analyse_ids, confirm_delete_tracks, love_ids, queue_ids, selection_ids};
use miss::BandcampSearchShelf;

fn sort_of(key: &str) -> Option<TrackSort> {
    serde_json::from_value(serde_json::Value::String(key.into())).ok()
}

/// The `TrackQuery` of the page from its URL state.
pub fn build_query(q: &str, tags: &[String], added: &alogic::AddedBounds, loved_only: bool) -> TrackQuery {
    TrackQuery {
        q: Some(q.trim().to_string()).filter(|q| !q.is_empty()),
        tags: tags.to_vec(),
        loved: loved_only.then_some(true),
        added_after: added.after.clone(),
        added_before: added.before.clone(),
        ..Default::default()
    }
}

fn tracks_fetcher(base: Arc<dyn Fn() -> TrackQuery + Send + Sync>, dur: RwSignal<i64>) -> PageFetcher<TrackOut> {
    Arc::new(move |req| {
        let mut tq = base();
        tq.offset = Some(req.offset as i64);
        tq.limit = Some(req.limit as i64);
        // A search with the default sort ranks by relevance (bm25): the server's fast path. Any other sort filters.
        let default_sort = req.sort == "added" && req.desc;
        if tq.q.is_some() && default_sort {
            // leave `sort` unset
        } else if let Some(s) = sort_of(&req.sort) {
            tq.sort = Some(s);
            tq.order = Some(if req.desc { SortDir::Desc } else { SortDir::Asc });
        }
        let url = format!("/tracks{}", qs_pairs(&tq.to_pairs()));
        Box::pin(async move {
            let page: TrackPage = api::get(&url).await?;
            dur.set(page.total_duration_ms);
            Ok(PageRes { rows: page.page.items, total: page.page.total as usize })
        })
    })
}

#[component]
fn TrackCell(t: TrackOut) -> impl IntoView {
    let art = t.release.as_ref().and_then(|r| r.art_url.clone()).or(t.art_url.clone()).map(|u| alogic::art_size(&u, "thumb"));
    view! {
        <Art src=art size=26.0 />
        <span class="tc-two">
            <span class="truncate" style="font-weight:550">{title_link(&t.title, t.release.as_ref())}</span>
            // On phones the artist column is hidden: show it under the title.
            <span class="tc-artist truncate muted">{artist_link(t.artist.as_ref())}</span>
        </span>
        {t.is_snippet.then(|| view! { <span class="badge">"clip"</span> })}
    }
}

fn track_menu(t: TrackOut, player: crate::player::PlayerCtx, nav: Callback<String>) -> Vec<MenuEntry> {
    let host = use_library_host();
    let id = t.id;
    let (a, b, c) = (t.clone(), t.clone(), t.clone());
    let loved = t.loved;
    let mut v: Vec<MenuEntry> = vec![
        MenuItem::new("Play").icon("play").on(move || play_items(player, std::slice::from_ref(&a), 0, None, false)).into(),
        MenuItem::new("Play next").icon("skip-next").on(move || player.cmd(PlayerCommand::PlayNext { items: vec![queue_item(&b)] })).into(),
        MenuItem::new("Add to queue").icon("queue").on(move || player.cmd(PlayerCommand::AddToQueue { items: vec![queue_item(&c)] })).into(),
        MenuEntry::Sep,
        MenuItem::new(if loved { "Unlove" } else { "Love" }).icon(if loved { "heart-fill" } else { "heart" }).on(move || {
            leptos::task::spawn_local(async move {
                if let Err(e) = love_ids(vec![id], !loved).await {
                    toast_err(&e.message());
                }
            });
        }).into(),
        MenuItem::new("Add to playlist or set…").icon("list").on(move || host.picker.set(Some(PickerReq { ids: ids_of(vec![id]) }))).into(),
        MenuItem::new("Analyse BPM and key").icon("activity").on(move || {
            leptos::task::spawn_local(async move {
                match analyse_ids(vec![id]).await {
                    Ok(m) => toast_info(&m),
                    Err(e) => toast_err(&e.message()),
                }
            });
        }).into(),
        MenuEntry::Sep,
    ];
    if let Some(r) = &t.release {
        let href = format!("/albums/{}", r.id);
        v.push(MenuItem::new("Go to album").icon("disc").on(move || nav.run(href.clone())).into());
    }
    if let Some(a) = &t.artist {
        let href = format!("/artists/{}", a.id);
        v.push(MenuItem::new("Go to artist").icon("user").on(move || nav.run(href.clone())).into());
    }
    for tag in t.tags.iter().take(3) {
        let href = format!("/tracks?tag={}", enc(tag));
        v.push(MenuItem::new(format!("Tracks tagged {tag}")).icon("tag").on(move || nav.run(href.clone())).into());
    }
    v.push(MenuEntry::Sep);
    v.push(MenuItem::new("Delete track…").icon("trash").danger().on(move || confirm_delete_tracks(vec![id], None)).into());
    v
}

#[component]
pub fn TracksPage() -> impl IntoView {
    view! { <TracksView loved_only=false /> }
}

#[component]
pub fn LovedPage() -> impl IntoView {
    view! { <TracksView loved_only=true /> }
}

#[component]
fn TracksView(loved_only: bool) -> impl IntoView {
    provide_library_host();
    let player = use_player();
    let navigate = leptos_router::hooks::use_navigate();
    let nav = Callback::new(move |p: String| navigate(&p, Default::default()));
    let url = UrlState::new();
    let added = AddedFilter::new(&url);
    let sort = RwSignal::new(("added".to_string(), true));
    let dur = RwSignal::new(0i64);
    let total = RwSignal::new(None::<usize>);
    let selection = RwSignal::new(Selection::None);

    let q = {
        let u = url.clone();
        Memo::new(move |_| u.get("q").unwrap_or_default())
    };
    let tags = {
        let u = url.clone();
        Memo::new(move |_| u.all("tag"))
    };
    let grid = {
        let u = url.clone();
        Memo::new(move |_| u.get("view").as_deref() == Some("grid"))
    };
    let sorting = ReleaseSortState::new(&url);
    let bounds = Memo::new(move |_| added.resolved.with(|r| r.as_ref().map(|r| r.bounds.clone()).unwrap_or_default()));
    let source_key = Memo::new(move |_| format!("{}|{:?}|{}|{:?}|{loved_only}", q.get(), tags.get(), added.key(), bounds.get()));
    let base: Arc<dyn Fn() -> TrackQuery + Send + Sync> = Arc::new(move || build_query(&q.get_untracked(), &tags.get_untracked(), &bounds.get_untracked(), loved_only));
    let fetcher = tracks_fetcher(base.clone(), dur);

    // ---- album grid (same filter, the albums the tracks sit on) ----------------------------------
    let sorting_g = sorting.clone();
    let rel_fetcher: PageFetcher<ReleaseOut> = Arc::new(move |req| {
        let (spec, order, _) = sorting_g.current();
        let b = bounds.get_untracked();
        let rq = ReleaseQuery {
            q: Some(q.get_untracked()).filter(|q| !q.trim().is_empty()),
            tags: tags.get_untracked(),
            loved: loved_only.then_some(true),
            added_after: b.after,
            added_before: b.before,
            sort: Some(alogic::release_sort_of(spec.value)),
            order: Some(order),
            offset: Some(req.offset as i64),
            limit: Some(req.limit as i64),
            ..Default::default()
        };
        let url = format!("/releases{}", qs_pairs(&rq.to_pairs()));
        Box::pin(async move {
            let page: Page<ReleaseOut> = api::get(&url).await?;
            Ok(PageRes { total: page.total as usize, rows: page.items })
        })
    });
    let grid_total = RwSignal::new(None::<usize>);
    let grid_key = {
        let sorting = sorting.clone();
        Memo::new(move |_| format!("{}|{:?}", source_key.get(), sorting.current().0.value) + &format!("{:?}", sorting.current().1))
    };

    // ---- actions --------------------------------------------------------------------------------
    let play_all = {
        let base = base.clone();
        move |shuffle: bool| {
            let mut tq = base();
            tq.limit = Some(500);
            tq.offset = Some(0);
            if shuffle {
                tq.sort = Some(TrackSort::Random);
                tq.seed = Some((crate::util::entropy() % 1_000_000 + 1) as i64);
            } else {
                tq.sort = sort_of(&sort.get_untracked().0);
                tq.order = Some(if sort.get_untracked().1 { SortDir::Desc } else { SortDir::Asc });
            }
            let url = format!("/tracks{}", qs_pairs(&tq.to_pairs()));
            leptos::task::spawn_local(async move {
                match api::get::<TrackPage>(&url).await {
                    Ok(p) => {
                        if (p.page.total as usize) > p.page.items.len() && shuffle {
                            toast_info(&format!("Shuffling {} random of {}", format_count(p.page.items.len() as i64), format_count(p.page.total)));
                        }
                        play_items(player, &p.page.items, 0, None, shuffle)
                    }
                    Err(e) => toast_err(&e.message()),
                }
            });
        }
    };
    let play_all = Arc::new(play_all);

    let export = {
        let base = base.clone();
        move |fmt: &'static str| {
            let mut pairs = base().to_pairs();
            pairs.push(("format".into(), fmt.into()));
            let url = format!("/api/tracks/export{}", qs_pairs(&pairs));
            let _ = crate::util::window().open_with_url_and_target(&url, "_blank");
        }
    };
    let export = Arc::new(export);
    let analysing = RwSignal::new(false);
    let analyse_all = {
        let base = base.clone();
        move || {
            analysing.set(true);
            let tq = base();
            leptos::task::spawn_local(async move {
                let r: Result<Vec<i64>, _> = api::get(&format!("/tracks/ids{}", qs_pairs(&tq.to_pairs()))).await;
                match r {
                    Ok(ids) => match analyse_ids(ids).await {
                        Ok(m) => toast_ok(&m),
                        Err(e) => toast_err(&e.message()),
                    },
                    Err(e) => toast_err(&e.message()),
                }
                analysing.set(false);
            });
        }
    };
    let analyse_all = Arc::new(analyse_all);
    let host = use_library_host();
    let save_playlist = {
        let base = base.clone();
        move || {
            let filter = base();
            leptos::task::spawn_local(async move {
                let body = PlaylistFromTracks { name: None, filter };
                match api::post::<_, PlaylistOut>("/playlists/from-tracks", &body).await {
                    Ok(p) => {
                        toast_ok(&format!("Saved \"{}\" ({} tracks)", p.name, format_count(p.track_count)));
                        crate::data::invalidate_entity("playlist", &[]);
                    }
                    Err(e) => toast_err(&e.message()),
                }
            });
        }
    };
    let save_playlist = Arc::new(save_playlist);
    let _ = host;

    let subtitle = Signal::derive(move || {
        let (t, a) = (total.get(), grid_total.get());
        if grid.get() {
            return a.map(|a| format!("{} album{}", format_count(a as i64), if a == 1 { "" } else { "s" }));
        }
        t.map(|t| {
            let mut s = format!("{} track{}", format_count(t as i64), if t == 1 { "" } else { "s" });
            if added.active() {
                s.push_str(&format!(" {}", added.description()));
            }
            if t > 0 {
                s.push_str(&format!(" · {}", format_playtime(dur.get() as f64)));
            }
            s
        })
    });
    let title = if loved_only { "Loved" } else { "Tracks" };

    let (ex1, ex2, ex3, an, sp) = (export.clone(), export.clone(), export, analyse_all, save_playlist);
    let overflow = Callback::new(move |_| -> Vec<MenuEntry> {
        let (e1, e2, e3, an, sp) = (ex1.clone(), ex2.clone(), ex3.clone(), an.clone(), sp.clone());
        let mut v: Vec<MenuEntry> = vec![];
        if loved_only {
            v.push(MenuItem::new("DJ mix the loved tracks").icon("mix").on(move || loved::dj_mix_loved(player)).into());
            v.push(MenuEntry::Sep);
        }
        v.push(MenuItem::new("Save as playlist").icon("list").on(move || sp()).into());
        v.push(MenuItem::new("Analyse BPM and key (all)").icon("activity").on(move || an()).into());
        v.push(MenuEntry::Sep);
        v.push(MenuItem::new("Export as M3U8").icon("download").on(move || e1("m3u8")).into());
        v.push(MenuItem::new("Export as CSV").icon("download").on(move || e2("csv")).into());
        v.push(MenuItem::new("Download as ZIP").icon("download").on(move || e3("zip")).into());
        v
    });

    let current_id = Signal::derive(move || player.current_track_id());
    let columns: Vec<Column<TrackOut>> = vec![
        Column::new("title", "Title", 220.0, |t: &TrackOut| view! { <TrackCell t=t.clone() /> }.into_any()).grow().sortable("title"),
        Column::new("artist", "Artist", 160.0, |t: &TrackOut| {
            view! { <span class="truncate muted">{artist_link(t.artist.as_ref())}</span> }.into_any()
        })
        .grow()
        .sortable("artist")
        .from_width(560.0),
        Column::new("album", "Album", 160.0, |t: &TrackOut| view! { <span class="truncate muted">{album_link(t.release.as_ref())}</span> }.into_any())
            .grow()
            .sortable("album")
            .from_width(900.0),
        Column::new("bpm", "BPM", 56.0, |t: &TrackOut| view! { <span class="mono">{format_bpm(t.bpm)}</span> }.into_any()).right().sortable("bpm").from_width(480.0),
        Column::new("key", "Key", 48.0, |t: &TrackOut| view! { <span class="camelot">{t.camelot.clone().unwrap_or_default()}</span> }.into_any()).sortable("key").from_width(480.0),
        Column::new("energy", "En", 40.0, |t: &TrackOut| view! { <span class="mono">{t.energy.map(|e| format!("{e:.0}")).unwrap_or_default()}</span> }.into_any()).right().sortable("energy").from_width(1000.0),
        Column::new("duration", "Time", 62.0, |t: &TrackOut| view! { <span class="mono">{format_duration_ms(t.duration_ms.map(|d| d as f64))}</span> }.into_any()).right().sortable("duration"),
        Column::new("added", "Added", 84.0, |t: &TrackOut| view! { <span class="mono faint">{t.added_at.clone().map(|d| d.chars().take(10).collect::<String>()).unwrap_or_default()}</span> }.into_any()).right().sortable("added").from_width(1100.0),
        Column::new("plays", "Plays", 56.0, |t: &TrackOut| view! { <span class="mono faint">{if t.play_count > 0 { t.play_count.to_string() } else { String::new() }}</span> }.into_any()).right().sortable("play_count").from_width(1100.0),
        Column::new("wave", "Wave", 110.0, |t: &TrackOut| view! { <crate::widgets::mini_wave::MiniWave track_id=t.id width=96 height=22 /> }.into_any()).hidden_unless(crate::prefs::use_prefs().prefs.get_untracked().row_waveforms).from_width(700.0),
        Column::new("love", "", 40.0, |t: &TrackOut| view! { <LoveButton track_id=t.id loved=t.loved /> }.into_any()),
    ];

    let tag_scope = Signal::derive(move || TagScope { q: Some(q.get()).filter(|q| !q.trim().is_empty()), loved: loved_only.then_some(true), added: bounds.get() });
    let (url_a, url_b, url_c, url_d) = (url.clone(), url.clone(), url.clone(), url.clone());
    let set_view = move |g: bool| url_d.set("view", g.then(|| "grid".to_string()));
    let set_view = Arc::new(set_view);
    let (sv1, sv2) = (set_view.clone(), set_view);
    let (pa, pb) = (play_all.clone(), play_all);
    let sorting_bar = sorting.clone();

    let empty_msg = move |noun: &str| {
        let mut s = format!("No {noun}");
        if !q.get_untracked().is_empty() {
            s.push_str(&format!(" match \u{201c}{}\u{201d}", q.get_untracked()));
        }
        let tg = tags.get_untracked();
        if !tg.is_empty() {
            s.push_str(&format!(" tagged {}", describe_tags(&tg)));
        }
        if added.active() {
            s.push_str(&format!(" {}", added.description()));
        }
        s.push('.');
        s
    };
    let filtered = move || !q.get_untracked().is_empty() || !tags.get_untracked().is_empty() || added.active();
    let list_empty = {
        let empty_msg = empty_msg.clone();
        move || {
            if loved_only && !filtered() {
                return view! { <EmptyState title="Nothing loved yet" hint="Hit the heart on a track." icon="heart" /> }.into_any();
            }
            if filtered() {
                let msg = empty_msg(if loved_only { "loved tracks" } else { "tracks" });
                let term = q.get_untracked();
                if !term.is_empty() && !loved_only {
                    return view! { <BandcampSearchShelf q=term message=msg /> }.into_any();
                }
                return view! { <EmptyState title=msg hint="Try removing a tag or widening the added filter." icon="search" /> }.into_any();
            }
            view! { <EmptyState title="Library is empty" hint="Scan a folder in Settings." icon="music" /> }.into_any()
        }
    };
    let grid_empty = {
        let empty_msg = empty_msg.clone();
        move || {
            let msg = if filtered() { empty_msg(if loved_only { "loved albums" } else { "albums" }) } else if loved_only { "Nothing loved yet.".to_string() } else { "Library is empty. Scan a folder in Settings.".to_string() };
            view! { <EmptyState title=msg icon="disc" /> }
        }
    };

    view! {
        <div class="page">
            <PageHeader title=title subtitle=subtitle overflow=overflow
                actions=crate::ds::children(move || {
                    let (pa, pb) = (pa.clone(), pb.clone());
                    view! {
                        <Button variant=Variant::Primary icon="play" on_click=move |_| pa(false)>"Play"</Button>
                        <Button icon="shuffle" title="Shuffle" on_click=move |_| pb(true)><span class="hide-sm">"Shuffle"</span></Button>
                    }
                }) />
            <div class="lib-filters">
                <TagFilterBar url=url_a scope=tag_scope />
                <AddedFilterBar url=url_b filter=added />
                {move || {
                    let term = q.get();
                    (!term.is_empty() && !loved_only && total.get().unwrap_or(0) > 0).then(|| view! {
                        <a class="lib-pill" href=format!("/explore?q={}", enc(&term))><Icon name="compass" size=12 />"Search Bandcamp"</a>
                    })
                }}
                {move || (!q.get().is_empty()).then(|| {
                    let u = url_c.clone();
                    view! { <button type="button" class="lib-pill on" title="Clear the search" on:click=move |_| u.set("q", None)><Icon name="search" size=12 />{q.get()}<Icon name="x" size=12 /></button> }
                })}
                <span class="spacer"></span>
                {move || grid.get().then(|| view! { <ReleaseSortBar state=sorting_bar.clone() /> })}
                <div class="segmented" role="group" aria-label="View">
                    <button type="button" aria-pressed=move || (!grid.get()).to_string() title="Track list" aria-label="Track list" on:click={let s = sv1.clone(); move |_| s(false)}><Icon name="rows" size=14 /></button>
                    <button type="button" aria-pressed=move || grid.get().to_string() title="Album grid" aria-label="Album grid" on:click={let s = sv2.clone(); move |_| s(true)}><Icon name="grid" size=14 /></button>
                </div>
            </div>
            {move || (loved_only && !grid.get()).then(|| view! {
                <div class="tm-extras"><loved::LovedStreams /><loved::LovedSuggestions /></div>
            })}
            {move || if grid.get() {
                let ge = grid_empty.clone();
                let rf = rel_fetcher.clone();
                let render = Callback::new(move |(r, _w): (ReleaseOut, f64)| view! { <AlbumCard release=r /> }.into_any());
                view! {
                    <div class="lib-grid-wrap">
                        <CardGrid fetch=rf source_key=Signal::derive(move || grid_key.get()) min_card_w=if crate::util::is_mobile() { 140.0 } else { 168.0 } gap=if crate::util::is_mobile() { 12.0 } else { 16.0 } meta_h=META_H render=render total_out=grid_total empty=ge entities=vec!["release"] />
                    </div>
                }.into_any()
            } else {
                let (fetcher, columns, le) = (fetcher.clone(), columns.clone(), list_empty.clone());
                view! {
                    <SelectionBar selection total=total />
                    <DataTable
                        columns=columns
                        fetch=fetcher
                        source_key=Signal::derive(move || source_key.get())
                        sort=sort
                        row_id=Callback::new(|t: TrackOut| t.id)
                        table_id="tracks"
                        selection=selection
                        select_filter=Signal::derive({ let base = base.clone(); move || serde_json::to_value(base()).unwrap_or_default() })
                        on_row_dblclick=Callback::new(move |t: TrackOut| play_items(player, &[t], 0, None, false))
                        row_menu=Callback::new(move |t: TrackOut| track_menu(t, player, nav))
                        row_class=Callback::new(move |t: TrackOut| if current_id.get() == Some(t.id) { "playing".to_string() } else { String::new() })
                        drag=Callback::new(|t: TrackOut| DragPayload { kind: "track".into(), ids: vec![t.id], label: t.title.clone(), index: None })
                        entities=vec!["track"]
                        total_out=total
                        empty=le
                    />
                }.into_any()
            }}
            <LibraryHost />
        </div>
    }
}

#[allow(dead_code)]
fn _keep() {
    let _ = (queue_ids, selection_ids);
}
