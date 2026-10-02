//! One artist: hero, releases, tracks, similar artists and the Bandcamp catalogue.
use std::sync::Arc;

use bc_types::library::{ArtistDetailOut, ArtistRelatedOut, Page, ReleaseOut, ReleaseQuery, ReleaseSort, SortDir, TrackOut, TrackPage, TrackSort};
use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};

use crate::api;
use crate::data::QuerySpec;
use crate::ds::{Button, EmptyState, ErrorPanel, Icon, MenuButton, MenuEntry, MenuItem, Variant};
use crate::logic::format::{format_bpm, format_count, format_duration_ms};
use crate::pages::labels::bandcamp::{Actions, BandCatalogue, BandcampPicker, BandcampRelated, CatalogDownloadButton, Entity, use_band};
use crate::pages::labels::edit::{EditDialog, EditTarget};
use crate::pages::labels::logic as lg;
use crate::pages::labels::shared::{Cover, FavButton, Kind, NoticeBar, Q, ReleaseCard, open_tab, use_favs, use_q};
use crate::util::{qs, qs_pairs};
use crate::widgets::card_grid::CardGrid;
use crate::widgets::common::{Art, LoveButton, album_link, enqueue_tracks, play_next_tracks, play_tracks, title_link};
use crate::widgets::dnd::DragPayload;
use crate::widgets::{Column, DataTable, PageFetcher, PageRes};

fn listing(id: i64) -> serde_json::Value {
    serde_json::to_value(ReleaseQuery { artist_id: Some(id), sort: Some(ReleaseSort::Year), order: Some(SortDir::Desc), ..Default::default() }).unwrap_or_default()
}

fn sort_of(key: &str) -> Option<TrackSort> {
    serde_json::from_value(serde_json::Value::String(key.into())).ok()
}

fn go(path: &str) {
    let _ = crate::util::window().location().set_href(path);
}

#[component]
pub fn ArtistDetailPage() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    let navigate = use_navigate();
    let favs = use_favs();
    let id = Memo::new(move |_| params.get().get("id").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0));
    let artist = use_q::<ArtistDetailOut>(move || {
        let id = id.get();
        (id > 0).then(|| QuerySpec::new(format!("/artists/{id}"), &["artist"]))
    });
    let band_url = Signal::derive(move || artist.data.get().and_then(|a| a.bandcamp_url.clone()));
    let band = use_band(band_url);
    let related = use_q::<ArtistRelatedOut>(move || {
        let id = id.get();
        (id > 0).then(|| QuerySpec::new(format!("/artists/{id}/related?limit=14"), &["artist", "release"]))
    });
    // the newest release that knows its Bandcamp page: the anchor for "more like this"
    let anchor = use_q::<Page<ReleaseOut>>(move || {
        let id = id.get();
        (id > 0).then(|| QuerySpec::new(format!("/releases?artist_id={id}&sort=year&order=desc&limit=30"), &["release"]))
    });
    let anchor_url = Signal::derive(move || anchor.data.get().and_then(|p| p.items.iter().find_map(|r| r.bandcamp_url.clone().filter(|u| !u.is_empty()))));

    let tab = Memo::new(move |_| query.get().get("tab").unwrap_or_else(|| "releases".into()));
    let set_tab = {
        let navigate = navigate.clone();
        Arc::new(move |t: &str| {
            let url = if t == "releases" { format!("/artists/{}", id.get_untracked()) } else { format!("/artists/{}?tab={t}", id.get_untracked()) };
            navigate(&url, NavigateOptions { replace: true, ..Default::default() });
        })
    };
    let edit: RwSignal<Option<EditTarget>> = RwSignal::new(None);
    let actions = Actions::new(Callback::new(move |_| {
        crate::data::invalidate_entity("artist", &[]);
        artist.refetch();
    }));
    let bio_open = RwSignal::new(false);
    let name = Signal::derive(move || artist.data.get().map(|a| a.name.clone()).unwrap_or_default());
    let entity = move || artist.data.get_untracked().map(|a| Entity { kind: Kind::Artist, id: a.id, name: a.name.clone(), url: a.bandcamp_url.clone() });
    let band_data = Signal::derive(move || band.data.get());
    let missing = Signal::derive(move || band_data.get().map(|b| lg::catalogue_counts(&b.releases).0).unwrap_or(0));
    let exact = Signal::derive(move || band_data.get().map(|b| !b.truncated).unwrap_or(false));
    let no_tracks = Signal::derive(move || artist.data.get().map(|a| a.track_count == 0).unwrap_or(true));
    // Shuffle reaches the whole Bandcamp catalogue too, so it needs no track on disk.
    let nothing_to_shuffle = Signal::derive(move || no_tracks.get() && band_data.get().is_none_or(|b| b.releases.is_empty()));
    let player = crate::player::use_player();
    let hero_art = Signal::derive(move || band_data.get().and_then(|b| b.image_url.clone()).or_else(|| artist.data.get().and_then(|a| a.art_url.clone()).map(|u| lg::thumb(&u).replace("size=thumb", "size=full"))));
    let location = Signal::derive(move || band_data.get().and_then(|b| b.location.clone()).filter(|l| !l.is_empty()).or_else(|| artist.data.get().and_then(|a| a.location.clone())));

    let menu = Callback::new(move |_| -> Vec<MenuEntry> {
        let Some(a) = artist.data.get_untracked() else { return vec![] };
        let (b, c) = (a.clone(), a.clone());
        let mut v: Vec<MenuEntry> = vec![
            MenuItem::new("Edit artist\u{2026}").icon("edit").on(move || edit.set(Some(EditTarget { kind: Kind::Artist, id: a.id, name: a.name.clone(), url: a.bandcamp_url.clone() }))).into(),
            MenuItem::new("Download as ZIP").icon("download").disabled(b.track_count == 0).on(move || open_tab(&format!("/api/tracks/export{}", qs(&[("artist_id", b.id.to_string()), ("format", "zip".into())])))).into(),
        ];
        if let Some(u) = c.bandcamp_url.clone().filter(|u| !u.is_empty()) {
            v.push(MenuItem::new("Open on Bandcamp").icon("external").on(move || open_tab(&u)).into());
        }
        v
    });

    // releases
    let rel_fetch: PageFetcher<ReleaseOut> = Arc::new(move |req| {
        let url = format!("/releases{}", qs(&[("artist_id", id.get_untracked().to_string()), ("sort", "year".into()), ("order", "desc".into()), ("offset", req.offset.to_string()), ("limit", req.limit.to_string())]));
        Box::pin(async move {
            let p: Page<ReleaseOut> = api::get(&url).await?;
            Ok(PageRes { rows: p.items, total: p.total as usize })
        })
    });
    let rel_key = Signal::derive(move || id.get().to_string());
    let rel_listing = Signal::derive(move || listing(id.get()));

    // tracks
    let sort = RwSignal::new(("album".to_string(), false));
    let track_total = RwSignal::new(None::<usize>);
    let trk_fetch: PageFetcher<TrackOut> = Arc::new(move |req| {
        let mut tq = bc_types::library::TrackQuery { artist_id: Some(id.get_untracked()), offset: Some(req.offset as i64), limit: Some(req.limit as i64), ..Default::default() };
        if let Some(s) = sort_of(&req.sort) {
            tq.sort = Some(s);
            tq.order = Some(if req.desc { SortDir::Desc } else { SortDir::Asc });
        }
        let url = format!("/tracks{}", qs_pairs(&tq.to_pairs()));
        Box::pin(async move {
            let p: TrackPage = api::get(&url).await?;
            Ok(PageRes { rows: p.page.items, total: p.page.total as usize })
        })
    });
    let columns = track_columns();

    let on_saved = Callback::new(move |_| {
        crate::data::invalidate_entity("artist", &[]);
        artist.refetch();
    });
    let (st1, st2, st3, st4) = (set_tab.clone(), set_tab.clone(), set_tab.clone(), set_tab.clone());
    let n_similar = Signal::derive(move || related.data.get().map(|r| r.similar_artists.len()).unwrap_or(0));
    let years = Signal::derive(move || artist.data.get().and_then(|a| lg::year_span(a.year_min, a.year_max)));

    view! {
        <div class="page pp-page pp-detail">
            <header class="pp-hero">
                <a class="pp-back" href="/artists" aria-label="All artists"><Icon name="arrow-left" /><span class="hide-sm">"All artists"</span></a>
                <div class="pp-hero-art pp-hero-round"><Cover src=hero_art icon="user" /></div>
                <div class="pp-hero-body">
                    <div class="pp-eyebrow hide-sm">"Artist"</div>
                    <h1 class="pp-title">{move || if name.get().is_empty() { "\u{2026}".to_string() } else { name.get() }}</h1>
                    <div class="pp-meta num">
                        {move || artist.data.get().map(|a| view! { <span>{format!("{} \u{b7} {}", lg::count_of(a.release_count, "release"), lg::count_of(a.track_count, "track"))}</span> })}
                        {move || years.get().map(|y| view! { <span>{format!("\u{b7} {y}")}</span> })}
                        {move || location.get().map(|l| view! { <span>{format!("\u{b7} {l}")}</span> })}
                        {move || artist.data.get().map(|a| a.labels.iter().take(3).map(|l| view! {
                            <a class="pp-labellink" href=format!("/labels/{}", l.id) title=format!("{} on {}", lg::count_of(l.release_count, "release"), l.name)><Icon name="folder" />{l.name.clone()}</a>
                        }).collect_view())}
                    </div>
                    {move || band_data.get().and_then(|b| b.bio.clone()).filter(|b| !b.is_empty()).map(|bio| view! {
                        <button type="button" class=move || if bio_open.get() { "pp-bio open" } else { "pp-bio" } aria-expanded=move || bio_open.get().to_string()
                            title="Show the whole bio" on:click=move |_| bio_open.update(|o| *o = !*o)>{bio}</button>
                    })}
                    {move || artist.data.get().filter(|a| !a.tags.is_empty()).map(|a| view! {
                        <div class="pp-tags">{a.tags.iter().take(8).map(|t| view! { <a class="chip" href=format!("/tracks?tag={}", crate::util::enc(&t.name))>{t.name.clone()}</a> }).collect_view()}</div>
                    })}
                    <div class="pp-actions">
                        <Button variant=Variant::Primary icon="play" disabled=no_tracks on_click=move |_| super::play_artist(id.get_untracked(), false)>"Play"</Button>
                        <Button icon="shuffle" disabled=nothing_to_shuffle on_click=move |_| super::shuffle_artist(player, id.get_untracked(), band_data.get_untracked())
                            title="Shuffle every track of every release, downloaded or not: what is missing streams from Bandcamp"><span class="hide-sm">"Shuffle"</span></Button>
                        <Button icon="rss" busy=actions.find_busy disabled=Signal::derive(move || artist.data.get().is_none())
                            on_click=move |_| if let Some(e) = entity() { actions.find_new(e) }
                            title=match band_url.get_untracked() { Some(u) if !u.is_empty() => format!("Check {u} for releases not yet harvested"), _ => "Search Bandcamp for this artist, verify the page against your releases, then check it".to_string() }>
                            <span class="hide-sm">"Find new releases"</span>
                        </Button>
                        {move || band_url.get().filter(|u| !u.is_empty()).map(|u| view! { <CatalogDownloadButton url=u missing=missing exact=exact /> })}
                        {move || artist.data.get().map(|a| view! { <FavButton kind=Kind::Artist id=a.id favs=favs name=a.name.clone() /> })}
                        <MenuButton entries=menu title="Artist menu" />
                    </div>
                </div>
            </header>
            <div class="pp-notices"><NoticeBar notice=actions.notice busy=actions.busy() on_download=Callback::new(move |ids| actions.queue(ids)) /></div>
            <div class="tabs pp-tabs" role="tablist">
                {move || {
                    let t = tab.get();
                    let a = artist.data.get();
                    let (s1, s2, s3, s4) = (st1.clone(), st2.clone(), st3.clone(), st4.clone());
                    view! {
                        <button class="tab" role="tab" type="button" aria-selected=(t == "releases").to_string() on:click=move |_| s1("releases")>
                            <Icon name="disc" />"Releases"{a.as_ref().map(|a| view! { <span class="count">{format_count(a.release_count)}</span> })}
                        </button>
                        <button class="tab" role="tab" type="button" aria-selected=(t == "tracks").to_string() on:click=move |_| s2("tracks")>
                            <Icon name="music" />"Tracks"{a.as_ref().map(|a| view! { <span class="count">{format_count(a.track_count)}</span> })}
                        </button>
                        <button class="tab" role="tab" type="button" aria-selected=(t == "similar").to_string() on:click=move |_| s3("similar")>
                            <Icon name="similar" />"Similar"{(n_similar.get() > 0).then(|| view! { <span class="count">{n_similar.get()}</span> })}
                        </button>
                        <button class="tab" role="tab" type="button" aria-selected=(t == "bandcamp").to_string() on:click=move |_| s4("bandcamp")>
                            <Icon name="compass" />"On Bandcamp"
                            {(missing.get() > 0).then(|| view! { <span class="badge badge-accent">{format!("{} missing", format_count(missing.get() as i64))}</span> })}
                        </button>
                    }
                }}
            </div>
            <div class="pp-tabbody">
                {move || {
                    if let Some(e) = artist.error.get() {
                        if artist.data.get().is_none() {
                            return view! { <ErrorPanel message=e.message() on_retry=Callback::new(move |_| artist.refetch()) /> }.into_any();
                        }
                    }
                    match tab.get().as_str() {
                        "tracks" => view! {
                            <div class="pp-fill">
                                <DataTable columns=columns.clone() fetch=trk_fetch.clone() source_key=Signal::derive(move || id.get().to_string()) sort=sort
                                    row_id=Callback::new(|t: TrackOut| t.id) table_id="artist-tracks" total_out=track_total entities=vec!["track"]
                                    on_row_dblclick=Callback::new(move |t: TrackOut| play_from(id.get_untracked(), t.id))
                                    row_menu=Callback::new(track_menu)
                                    drag=Callback::new(|t: TrackOut| DragPayload { kind: "track".into(), ids: vec![t.id], label: t.title.clone(), index: None })
                                    empty=move || view! { <EmptyState icon="music" title="No tracks" /> } />
                            </div>
                        }.into_any(),
                        "similar" => view! { <Similar related=related /> }.into_any(),
                        "bandcamp" => view! { <ArtistBandcamp artist=artist band=band anchor=anchor_url actions=actions edit=edit /> }.into_any(),
                        _ => view! {
                            <div class="pp-fill">
                                <CardGrid fetch=rel_fetch.clone() source_key=rel_key min_card_w=144.0 meta_h=48.0 gap=12.0 entities=vec!["release"]
                                    render=Callback::new(move |(r, _w): (ReleaseOut, f64)| view! { <ReleaseCard r=r listing=rel_listing /> }.into_any())
                                    empty=move || view! { <EmptyState icon="disc" title="No releases in the library" hint="Tracks of this artist sit on compilations or other artists' releases." /> } />
                            </div>
                        }.into_any(),
                    }
                }}
            </div>
            <EditDialog target=edit on_saved=on_saved />
        </div>
    }
}

/// Play the artist's tracks starting at the double-clicked one.
fn play_from(artist_id: i64, track_id: i64) {
    let tq = bc_types::library::TrackQuery { artist_id: Some(artist_id), sort: Some(TrackSort::Album), order: Some(SortDir::Asc), limit: Some(500), offset: Some(0), ..Default::default() };
    let url = format!("/tracks{}", qs_pairs(&tq.to_pairs()));
    leptos::task::spawn_local(async move {
        match api::get::<TrackPage>(&url).await {
            Ok(p) => {
                let items = p.page.items;
                let at = items.iter().position(|t| t.id == track_id).unwrap_or(0);
                play_tracks(&items, at, None);
            }
            Err(e) => crate::ds::toast_err(&e.message()),
        }
    });
}

fn track_menu(t: TrackOut) -> Vec<MenuEntry> {
    let (a, b, c) = (t.clone(), t.clone(), t.clone());
    let mut v: Vec<MenuEntry> = vec![
        MenuItem::new("Play").icon("play").on(move || play_tracks(std::slice::from_ref(&a), 0, None)).into(),
        MenuItem::new("Play next").icon("skip-next").on(move || play_next_tracks(std::slice::from_ref(&b))).into(),
        MenuItem::new("Add to queue").icon("queue").on(move || enqueue_tracks(std::slice::from_ref(&c))).into(),
        MenuEntry::Sep,
    ];
    if let Some(r) = &t.release {
        let href = format!("/albums/{}", r.id);
        v.push(MenuItem::new("Go to album").icon("disc").on(move || go(&href)).into());
    }
    v
}

fn track_columns() -> Vec<Column<TrackOut>> {
    vec![
        Column::new("title", "Title", 220.0, |t: &TrackOut| {
            let art = t.release.as_ref().and_then(|r| r.art_url.clone()).or(t.art_url.clone());
            let title = title_link(&t.title, t.release.as_ref());
            view! { <Art src=art size=26.0 /><span class="truncate" style="font-weight:550">{title}</span> }.into_any()
        }).grow().sortable("title"),
        Column::new("album", "Album", 160.0, |t: &TrackOut| view! { <span class="truncate muted">{album_link(t.release.as_ref())}</span> }.into_any()).grow().sortable("album").from_width(560.0),
        Column::new("bpm", "BPM", 56.0, |t: &TrackOut| view! { <span class="mono">{format_bpm(t.bpm)}</span> }.into_any()).right().sortable("bpm").from_width(480.0),
        Column::new("key", "Key", 48.0, |t: &TrackOut| view! { <span class="camelot">{t.camelot.clone().unwrap_or_default()}</span> }.into_any()).sortable("key").from_width(480.0),
        Column::new("duration", "Time", 62.0, |t: &TrackOut| view! { <span class="mono">{format_duration_ms(t.duration_ms.map(|d| d as f64))}</span> }.into_any()).right().sortable("duration"),
        Column::new("plays", "Plays", 56.0, |t: &TrackOut| view! { <span class="mono faint">{if t.play_count > 0 { t.play_count.to_string() } else { String::new() }}</span> }.into_any()).right().sortable("play_count").from_width(900.0),
        Column::new("love", "", 40.0, |t: &TrackOut| view! { <LoveButton track_id=t.id loved=t.loved /> }.into_any()),
    ]
}

/// Neighbours in the library (shared tags) and the "more like this" shelves.
#[component]
fn Similar(related: Q<ArtistRelatedOut>) -> impl IntoView {
    view! {
        <div class="pp-scroll">
            {move || {
                if let Some(e) = related.error.get() {
                    return view! { <ErrorPanel message=e.message() on_retry=Callback::new(move |_| related.refetch()) /> }.into_any();
                }
                let Some(d) = related.data.get() else {
                    return view! { <p class="pp-hint"><span class="pp-spin"></span>" Looking for neighbours\u{2026}"</p> }.into_any();
                };
                if d.similar_artists.is_empty() && d.shelves.is_empty() {
                    return view! { <EmptyState icon="similar" title="Nothing else in your library sits near this artist yet" /> }.into_any();
                }
                view! {
                    {(!d.similar_artists.is_empty()).then(|| view! {
                        <section class="pp-section">
                            <h2 class="section-title">"Similar artists"</h2>
                            <div class="pp-strip">
                                {d.similar_artists.iter().map(|a| {
                                    let why = if a.shared_tags.is_empty() { lg::count_of(a.release_count, "release") } else { a.shared_tags.iter().take(2).cloned().collect::<Vec<_>>().join(" \u{b7} ") };
                                    let title = if a.shared_tags.is_empty() { String::new() } else { format!("Shares {}", a.shared_tags.join(", ")) };
                                    let art = a.art_url.as_ref().map(|u| lg::thumb(u));
                                    view! {
                                        <a class="pp-sim" href=format!("/artists/{}", a.id) title=title>
                                            <Cover src=art class="pp-round" icon="user" />
                                            <span class="pp-card-title truncate">{a.name.clone()}</span>
                                            <span class="pp-card-sub truncate">{why}</span>
                                        </a>
                                    }
                                }).collect_view()}
                            </div>
                        </section>
                    })}
                    {d.shelves.iter().filter(|g| !g.items.is_empty()).map(|g| {
                        let href = match (g.kind.as_str(), g.id) {
                            ("artist", Some(id)) => Some(format!("/artists/{id}")),
                            ("label", Some(id)) => Some(format!("/labels/{id}")),
                            ("tag", _) => Some(format!("/tracks?tag={}", crate::util::enc(&g.key))),
                            _ => None,
                        };
                        let items = g.items.clone();
                        let lj = Signal::derive(|| serde_json::Value::Null);
                        view! {
                            <section class="pp-section">
                                <h2 class="section-title">{match href { Some(h) => view! { <a class="pp-link" href=h>{g.title.clone()}</a> }.into_any(), None => view! { <span>{g.title.clone()}</span> }.into_any() }}</h2>
                                <div class="pp-strip">
                                    {items.into_iter().map(|r| view! { <div class="pp-strip-item"><ReleaseCard r=r listing=lj show_artist=true /></div> }).collect_view()}
                                </div>
                            </section>
                        }
                    }).collect_view()}
                }.into_any()
            }}
        </div>
    }
}

#[component]
fn ArtistBandcamp(artist: Q<ArtistDetailOut>, band: Q<bc_types::bandcamp::BandOut>, anchor: Signal<Option<String>>, actions: Actions, edit: RwSignal<Option<EditTarget>>) -> impl IntoView {
    view! {
        {move || {
            let Some(a) = artist.data.get() else { return view! { <div class="pp-scroll"><p class="pp-hint"><span class="pp-spin"></span></p></div> }.into_any() };
            let Some(url) = a.bandcamp_url.clone().filter(|u| !u.is_empty()) else {
                let (a2, a3) = (a.clone(), a.clone());
                return view! {
                    <div class="pp-scroll">
                        <p class="pp-hint pp-hint-wide">
                            {format!("No Bandcamp page is pinned for {} yet \u{2014} pick theirs from the search below and their bio, catalogue and \u{201c}Find new releases\u{201d} all read from it. Nothing here? ", a.name)}
                            <button type="button" class="pp-linkbtn" on:click=move |_| edit.set(Some(EditTarget { kind: Kind::Artist, id: a2.id, name: a2.name.clone(), url: None }))>"Paste the URL yourself"</button>"."
                        </p>
                        <BandcampPicker name=a.name.clone() kind=Kind::Artist current_url=Signal::derive(|| None::<String>)
                            on_pick=Callback::new(move |u| actions.pin(Entity { kind: Kind::Artist, id: a3.id, name: a3.name.clone(), url: None }, u))
                            picking=actions.pinning error=actions.pin_error />
                    </div>
                }.into_any();
            };
            let tags: Vec<String> = a.tags.iter().take(5).map(|t| t.name.clone()).collect();
            let anchor_u = anchor.get();
            if let Some(b) = band.data.get() {
                return view! {
                    <div class="pp-bcsplit">
                        <BandCatalogue band=(*b).clone() url=url file_under=None />
                        {anchor_u.map(|u| view! { <div class="pp-scroll pp-related"><BandcampRelated url=u tags=tags /></div> })}
                    </div>
                }.into_any();
            }
            if let Some(e) = band.error.get() {
                return view! { <div class="pp-scroll"><p class="pp-hint danger">{format!("Could not read the artist's Bandcamp page: {} ", e.message())}
                    <button type="button" class="pp-linkbtn" on:click=move |_| band.refetch()>"Try again"</button></p></div> }.into_any();
            }
            view! { <div class="pp-scroll"><p class="pp-hint"><span class="pp-spin"></span>{format!(" Reading {url}\u{2026}")}</p></div> }.into_any()
        }}
    }
}
