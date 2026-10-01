//! Album detail: dominant-colour header, track table, supporters, related shelves
//! (library) and the Bandcamp tab. Play / shuffle / queue / zip / fill missing.
use std::sync::Arc;

use bc_types::library::*;
use bc_types::player::{PlayerCommand, QueueSource};
use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_params_map};

use super::card::{FanName, SnippetBadge};
use super::filters::UrlState;
use super::host::{LibraryHost, fill_release, library_listing, play_items, provide_library_host, release_menu, shuffle_in_place};
use super::logic;
use super::related::{BandcampShelves, RelatedShelf, SHELF_DEPTH};
use super::supporters::Supporters;
use crate::data::{QuerySpec, use_query};
use crate::ds::popover::Rect;
use crate::ds::{Button, EmptyState, ErrorPanel, Icon, MenuCtx, Skeleton, Tabs, Variant, toast_ok};
use crate::ds::tabs::TabDef;
use crate::logic::format::{format_bpm, format_duration_ms, format_long_duration};
use crate::player::use_player;
use crate::util::enc;
use crate::widgets::common::{Art, LoveButton, artist_link, queue_item};

fn color_ok(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit())
}

#[component]
pub fn AlbumDetailPage() -> impl IntoView {
    provide_library_host();
    let player = use_player();
    let menu = expect_context::<MenuCtx>();
    let params = use_params_map();
    let navigate = use_navigate();
    let url = UrlState::new();
    let id = Memo::new(move |_| params.with(|p| p.get("id").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0)));

    let release = use_query::<ReleaseOut>(move || Some(QuerySpec::new(format!("/releases/{}", id.get()), &["release", "track"])));
    let rel_data = release.data;
    let rel_err = release.error;
    let release_q = StoredValue::new(release);
    let tracks = use_query::<TrackPage>(move || Some(QuerySpec::new(format!("/tracks?release_id={}&sort=album&order=asc&limit=500", id.get()), &["track", "release"])));
    let trk_data = tracks.data;
    let trk_err = tracks.error;
    let tracks_q = StoredValue::new(tracks);
    let related = use_query::<Vec<RelatedGroup>>(move || Some(QuerySpec::new(format!("/releases/{}/related?limit={SHELF_DEPTH}", id.get()), &["release"])));
    let rel2_data = related.data;
    let rel2_loading = related.loading;

    // Lazy: once per album, after the page is up, ask whether the missing tracks are out yet. Cards
    // only ever see what is already cached; this is the one place that may cost a Bandcamp read.
    let checked = StoredValue::new(0i64);
    Effect::new(move |_| {
        let Some(r) = rel_data.get() else { return };
        if checked.get_value() == r.id || r.bandcamp_url.is_none() || !(logic::missing_tracks(&r) > 0 || r.is_preorder) {
            return;
        }
        checked.set_value(r.id);
        let rid = r.id;
        leptos::task::spawn_local(async move {
            if let Ok(Some(a)) = crate::api::get::<Option<ReleaseAvailability>>(&format!("/releases/{rid}/availability")).await
                && a.fetched
            {
                // Targeted invalidation: this page's release query and any grid card of it refetch.
                crate::data::invalidate_entity("release", &[rid]);
            }
        });
    });

    let scroller = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        id.track();
        if let Some(el) = scroller.get_untracked() {
            el.set_scroll_top(0);
        }
    });

    let tab = RwSignal::new(if url.get_untracked("related").as_deref() == Some("explore") { "explore".to_string() } else { "local".to_string() });
    {
        let url = url.clone();
        Effect::new(move |prev: Option<()>| {
            let t = tab.get();
            if prev.is_some() {
                url.set("related", (t == "explore").then(|| "explore".to_string()));
            }
        });
    }
    let tabs = Signal::derive(move || {
        vec![TabDef::new("local", "In your library"), TabDef::new("explore", "On Bandcamp")]
    });

    let items = Memo::new(move |_| trk_data.get().map(|p| p.page.items.clone()).unwrap_or_default());
    let source = move || QueueSource::Release { release_id: id.get_untracked(), listing: library_listing() };
    let play_from = Arc::new(move |from: usize| {
        let t = items.get_untracked();
        play_items(player, &t, from, Some(source()), false);
    });
    let (pf1, pf2) = (play_from.clone(), play_from.clone());
    let shuffle_album = move || {
        let mut t = items.get_untracked();
        shuffle_in_place(&mut t);
        play_items(player, &t, 0, Some(source()), true);
    };
    let fill_state = RwSignal::new(false);

    let nav_cb = {
        let navigate = navigate.clone();
        Callback::new(move |p: String| navigate(&p, Default::default()))
    };
    let open_menu = move |ev: leptos::ev::MouseEvent| {
        use wasm_bindgen::JsCast;
        let Some(r) = rel_data.get_untracked() else { return };
        let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
        let nav = nav_cb;
        let done = Callback::new(move |_gone: Vec<i64>| nav.run("/albums".to_string()));
        menu.open_titled(Rect::of(&el), &r.title, release_menu(&r, library_listing(), player, nav_cb, Some(done)));
    };

    let is_current_release = Memo::new(move |_| player.state.with(|s| s.current.as_ref().and_then(|c| c.release_id) == Some(id.get())));
    let nav2 = navigate.clone();

    view! {
        <div class="page">
            <div class="page-scroll lib-detail" node_ref=scroller>
                {move || {
                    if let Some(e) = rel_err.get().filter(|_| rel_data.get().is_none()) {
                        let rl = release_q;
                        return view! {
                            <ErrorPanel message=Signal::derive(move || e.message()) on_retry=Callback::new(move |_| rl.get_value().refetch()) />
                        }.into_any();
                    }
                    let Some(r) = rel_data.get() else {
                        return view! {
                            <div class="lib-hero"><div class="lib-hero-art"><Skeleton height="100%" /></div>
                                <div class="lib-hero-text"><Skeleton width="60%" height="34px" /><Skeleton width="40%" height="16px" /></div></div>
                        }.into_any();
                    };
                    let missing = logic::missing_tracks(&r);
                    let fillable = logic::fillable_tracks(&r);
                    let warn_short = logic::shortfall_is_warning(&r);
                    let preorder = logic::preorder_label(&r);
                    let tint = r.art_color.clone().filter(|c| color_ok(c));
                    let hero_style = tint.map(|c| format!("--tint:{c}")).unwrap_or_default();
                    let title = r.title.clone();
                    let (pf1, shuffle_album) = (pf1.clone(), shuffle_album.clone());
                    let zip = format!("/api/tracks/export?release_id={}&format=zip", r.id);
                    let artist = r.artist.clone();
                    let label = r.label.clone();
                    let label_id = r.label_id;
                    let bandcamp = r.bandcamp_url.clone();
                    let search_q = [r.artist.as_ref().map(|a| a.name.clone()), Some(r.title.clone())].into_iter().flatten().collect::<Vec<_>>().join(" ");
                    let tags: Vec<String> = r.tags.iter().take(8).cloned().collect();
                    let (tc, et, dur) = (r.track_count, r.expected_track_count, r.duration_ms);
                    let year = r.year;
                    let kind = r.kind.clone();
                    let (snip, fan) = (r.snippet_only, r.source_fan_id);
                    let rid = r.id;
                    let artwork = r.art_url.clone();
                    view! {
                        <header class="lib-hero" style=hero_style>
                            <div class="lib-hero-art"><Art src=artwork class="alb-art" /></div>
                            <div class="lib-hero-text">
                                <div class="lib-kind">{kind}</div>
                                <h1 class="lib-hero-title">{title.clone()}</h1>
                                <div class="lib-hero-meta">
                                    {artist.map(|a| view! { <a class="lib-artist" href=format!("/artists/{}", a.id)>{a.name}</a> })}
                                    {year.map(|y| view! { <span class="mono">{format!("· {y}")}</span> })}
                                    <span class="mono">
                                        "· "
                                        {if warn_short {
                                            view! { <span class="warn-text" title=format!("{missing} of the record's tracks are missing")>{format!("{} of {} tracks", tc, et.unwrap_or(tc))}</span> }.into_any()
                                        } else if missing > 0 {
                                            view! { <span>{format!("{} of {} tracks", tc, et.unwrap_or(tc))}</span> }.into_any()
                                        } else { view! { {format!("{tc} tracks")} }.into_any() }}
                                        {format!(" · {}", format_long_duration(dur as f64))}
                                    </span>
                                    {preorder.map(|label| view! {
                                        <span class="lib-pill info" title="Bandcamp releases the rest of the record on the release date; they cannot be downloaded before then."><Icon name="clock" size=12 />{label}</span>
                                    })}
                                    {snip.then(|| view! { <SnippetBadge /> })}
                                    {fan.map(|f| view! { <FanName fan_id=f /> })}
                                    {label.clone().map(|l| match label_id {
                                        Some(lid) => view! { <a class="lib-labellink" href=format!("/labels/{lid}") title=format!("All releases on {l}")><Icon name="folder" size=13 />{l.clone()}</a> }.into_any(),
                                        None => view! { <span class="lib-labellink"><Icon name="folder" size=13 />{l.clone()}</span> }.into_any(),
                                    })}
                                    {match bandcamp {
                                        Some(u) => view! { <a class="lib-pill" href=u.clone() target="_blank" rel="noreferrer" title=u><Icon name="external" size=11 />"Open on Bandcamp"</a> }.into_any(),
                                        None => view! { <a class="lib-pill" href=format!("/explore?q={}", enc(&search_q)) title="No Bandcamp page is recorded for this release; search for it."><Icon name="compass" size=11 />"Find on Bandcamp"</a> }.into_any(),
                                    }}
                                </div>
                                {(!tags.is_empty()).then(|| view! {
                                    <div class="lib-tagrow">
                                        {tags.into_iter().map(|t| view! { <a class="lib-tag" href=format!("/tracks?tag={}", enc(&t))>{t.clone()}</a> }).collect_view()}
                                    </div>
                                })}
                                <div class="lib-hero-actions">
                                    {move || (!items.get().is_empty()).then(|| {
                                        let (pf1, sh, zip) = (pf1.clone(), shuffle_album.clone(), zip.clone());
                                        view! {
                                            <Button variant=Variant::Primary icon=crate::ds::dyn_icon(move || if is_current_release.get() && player.is_playing() { "pause" } else { "play" }) on_click=move |_| {
                                                if is_current_release.get_untracked() { player.cmd(PlayerCommand::Toggle) } else { pf1(0) }
                                            }>{move || if is_current_release.get() && player.is_playing() { "Pause" } else { "Play" }}</Button>
                                            <Button icon="shuffle" title="Play this album in a random order" on_click=move |_| sh()>"Shuffle"</Button>
                                            <a class="btn btn-outline" href=zip title="Download every track in one flat folder"><Icon name="download" />"Zip"</a>
                                        }
                                    })}
                                    {(fillable > 0).then(|| view! {
                                        <Button icon=crate::ds::dyn_icon(move || if fill_state.get() { "check" } else { "download" }) class="warn-btn" disabled=fill_state
                                            title="Download the whole record again to fill the gaps"
                                            on_click=move |_| { fill_state.set(true); fill_release(rid); toast_ok("Queued the fill"); }>
                                            {move || if fill_state.get() { "Queued".to_string() } else { format!("Fill {fillable} missing track{}", if fillable == 1 { "" } else { "s" }) }}
                                        </Button>
                                    })}
                                    <button type="button" class="btn btn-outline btn-icon lib-more" aria-label="More options" aria-haspopup="menu" on:click=open_menu><Icon name="more" /></button>
                                </div>
                            </div>
                        </header>
                    }.into_any()
                }}

                <section class="lib-tracks">
                    {move || {
                        if trk_err.get().is_some() && trk_data.get().is_none() {
                            let t = tracks_q;
                            return view! { <ErrorPanel message=Signal::derive(move || trk_err.get().map(|e| e.message()).unwrap_or_default()) on_retry=Callback::new(move |_| t.get_value().refetch()) /> }.into_any();
                        }
                        if trk_data.get().is_none() {
                            return (0..6).map(|_| view! { <div class="lib-trk"><Skeleton height="16px" /></div> }).collect_view().into_any();
                        }
                        let list = items.get();
                        if list.is_empty() {
                            return view! { <EmptyState title="No tracks in the library" hint="The files of this release are missing." icon="music" /> }.into_any();
                        }
                        let album_artist = rel_data.get().and_then(|r| r.artist.as_ref().map(|a| a.id));
                        let pf2 = pf2.clone();
                        list.into_iter().enumerate().map(move |(i, t)| {
                            let pf = pf2.clone();
                            let pf_click = pf2.clone();
                            let (tid, tt) = (t.id, t.clone());
                            let playing = Memo::new(move |_| player.current_track_id() == Some(tid));
                            let show_artist = t.artist.as_ref().map(|a| Some(a.id) != album_artist).unwrap_or(false);
                            let t_menu = t.clone();
                            view! {
                                <div class="lib-trk" class:playing=move || playing.get() on:dblclick=move |_| pf(i)
                                    on:contextmenu=move |ev: leptos::ev::MouseEvent| {
                                        ev.prevent_default();
                                        let t = t_menu.clone();
                                        menu.open(Rect::point(ev.client_x() as f64, ev.client_y() as f64), track_entries(t, player));
                                    }>
                                    <button type="button" class="lib-trk-no" aria-label=format!("Play {}", t.title) on:click={let pf = pf_click.clone(); move |_| pf(i)}>
                                        <span class="mono n">{t.track_no.map(|n| n.to_string()).unwrap_or_else(|| (i + 1).to_string())}</span>
                                        <Icon name="play" size=14 />
                                    </button>
                                    <div class="lib-trk-main grow">
                                        <span class="truncate lib-trk-title">{t.title.clone()}</span>
                                        {t.is_snippet.then(|| view! { <SnippetBadge compact=true /> })}
                                        {show_artist.then(|| view! { <span class="truncate faint">{artist_link(t.artist.as_ref())}</span> })}
                                    </div>
                                    <span class="mono faint lib-trk-bpm">{format_bpm(t.bpm)}</span>
                                    <span class="camelot lib-trk-key">{t.camelot.clone().unwrap_or_default()}</span>
                                    <span class="mono lib-trk-time">{format_duration_ms(t.duration_ms.map(|d| d as f64))}</span>
                                    <LoveButton track_id=tid loved=tt.loved />
                                </div>
                            }
                        }).collect_view().into_any()
                    }}
                    {move || rel_data.get().filter(|r| !r.unreleased_tracks.is_empty()).map(|r| {
                        let date = r.release_date.clone();
                        let badge = logic::out_badge(date.as_deref());
                        let full = date.as_deref().and_then(|d| logic::format_release_date(d, true));
                        let hint = format!("Not released yet{}", full.map(|d| format!(" (out {d})")).unwrap_or_default());
                        r.unreleased_tracks.iter().map(|t| {
                            let no = t.track_num.map(|n| n.to_string()).unwrap_or_default();
                            let dur = t.duration_sec.map(|s| format_duration_ms(Some(s * 1000.0))).unwrap_or_default();
                            view! {
                                <div class="lib-trk unreleased" aria-disabled="true" title=hint.clone()>
                                    <span class="lib-trk-no"><span class="mono n">{no}</span></span>
                                    <div class="lib-trk-main grow">
                                        <span class="truncate lib-trk-title">{t.title.clone()}</span>
                                        <span class="lib-badge info compact"><Icon name="clock" size=10 />{badge.clone()}</span>
                                    </div>
                                    <span class="lib-trk-bpm"></span>
                                    <span class="lib-trk-key"></span>
                                    <span class="mono lib-trk-time">{dur}</span>
                                    <span></span>
                                </div>
                            }
                        }).collect_view()
                    })}
                </section>

                {move || rel_data.get().and_then(|r| r.bandcamp_url.clone()).map(|u| view! { <div class="lib-block"><Supporters url=u /></div> })}

                <div class="lib-block">
                    <Tabs tabs=tabs value=tab />
                    {move || if tab.get() == "local" {
                        match (rel2_data.get(), rel2_loading.get()) {
                            (Some(d), _) if !d.is_empty() => d.iter().cloned().map(|g| view! { <RelatedShelf group=g /> }).collect_view().into_any(),
                            (_, true) => view! { <p class="faint lib-note">"Looking for neighbours…"</p> }.into_any(),
                            _ => view! { <p class="faint lib-note">"Nothing else in your library sits near this one."</p> }.into_any(),
                        }
                    } else {
                        match rel_data.get() {
                            Some(r) => view! { <BandcampShelves release=(*r).clone() /> }.into_any(),
                            None => ().into_any(),
                        }
                    }}
                </div>
            </div>
            <LibraryHost />
            {let _ = nav2; ()}
        </div>
    }
}

fn track_entries(t: TrackOut, player: crate::player::PlayerCtx) -> Vec<crate::ds::MenuEntry> {
    use crate::ds::{MenuEntry, MenuItem};
    let (a, b, c) = (t.clone(), t.clone(), t.clone());
    let host = super::host::use_library_host();
    let id = t.id;
    vec![
        MenuItem::new("Play").icon("play").on(move || play_items(player, std::slice::from_ref(&a), 0, None, false)).into(),
        MenuItem::new("Play next").icon("skip-next").on(move || player.cmd(PlayerCommand::PlayNext { items: vec![queue_item(&b)] })).into(),
        MenuItem::new("Add to queue").icon("queue").on(move || player.cmd(PlayerCommand::AddToQueue { items: vec![queue_item(&c)] })).into(),
        MenuEntry::Sep,
        MenuItem::new("Add to playlist or set…").icon("list").on(move || host.picker.set(Some(super::host::PickerReq { ids: super::host::ids_of(vec![id]) }))).into(),
    ]
}

