//! One playlist, opened: tracks in playlist order, playable from any row, reorderable by
//! pointer drag (or Alt+Up/Down), removable per row. Smart playlists are read-only (live filter).
use bc_types::library::{PlaylistMove, PlaylistOut, PlaylistPatch, TrackOut};
use bc_types::player::QueueSource;
use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_params_map};

use super::collage::Collage;
use super::logic::{collage_urls, shuffle};
use super::{analyse_ids, dj_mix, export_menu, fetch_playlist_tracks, live_of, make_similar, play_items};
use crate::api;
use crate::data::QuerySpec;
use crate::player::plan::qh::use_qh;
use crate::ds::{Button, Dialog, EmptyState, ErrorPanel, Icon, MenuEntry, MenuItem, Size, Skeleton, Variant, confirm, toast_err, toast_ok};
use crate::logic::format::{format_bpm, format_duration_ms, format_long_duration};
use crate::player::use_player;
use crate::util::enc;
use crate::widgets::common::{Art, LoveButton, album_link, artist_link, queue_item, title_link};
use crate::widgets::dnd::{self, DragPayload, drop_index, reorder_target};

const ROWS: &str = "pl-rows";

#[component]
pub fn PlaylistDetailPage() -> impl IntoView {
    let params = use_params_map();
    let id = Memo::new(move |_| params.get().get("id").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0));
    let navigate = use_navigate();
    let player = use_player();
    let app = crate::app::use_app();

    let meta = use_qh::<PlaylistOut>(move || Some(QuerySpec::new(format!("/playlists/{}", id.get()), &["playlist"])));
    let tracks = use_qh::<bc_types::library::Page<TrackOut>>(move || {
        Some(QuerySpec::new(format!("/playlists/{}/tracks", id.get()), &["playlist", "track"]))
    });
    // local copy: reorders and removals show instantly, the server answer re-syncs it
    let items = RwSignal::new(Vec::<TrackOut>::new());
    Effect::new(move |_| {
        if let Some(p) = tracks.data.get() {
            items.set(p.items.clone());
        }
    });

    let has_tracks = Memo::new(move |_| tracks.data.with(|d| d.is_some()));
    let err_msg = Memo::new(move |_| tracks.error.with(|e| e.as_ref().map(|e| e.message())));
    let name = Signal::derive(move || meta.data.get().map(|p| p.name.clone()).unwrap_or_default());
    let smart = Signal::derive(move || meta.data.get().map(|p| p.kind == "smart").unwrap_or(false));
    let live = Signal::derive(move || live_of(id.get()));
    let covers = Memo::new(move |_| items.with(|t| collage_urls(t, 4)));
    let total_ms = Signal::derive(move || items.with(|t| t.iter().map(|t| t.duration_ms.unwrap_or(0)).sum::<i64>()));
    let source = move || QueueSource::Playlist { id: id.get_untracked(), name: name.get_untracked() };

    let play_from = move |i: usize| {
        let it = items.get_untracked();
        if !it.is_empty() {
            play_items(player, &it, i, Some(source()));
        }
    };
    let play_or_pause = move |_| {
        if live.get_untracked().current.get_untracked() {
            player.toggle();
        } else {
            play_from(0);
        }
    };
    let shuffle_play = move |_| {
        let mut it = items.get_untracked();
        shuffle(&mut it, crate::util::entropy());
        // already in the order we want: play it straight, the player's own shuffle would re-draw
        player.cmd(bc_types::player::PlayerCommand::SetShuffle { on: false });
        play_items(player, &it, 0, Some(source()));
    };

    // --- edits ---------------------------------------------------------------------
    let remove_item = move |item_id: i64| {
        let pid = id.get_untracked();
        let before = items.get_untracked();
        items.update(|v| v.retain(|t| t.item_id != Some(item_id)));
        leptos::task::spawn_local(async move {
            if let Err(e) = api::call("DELETE", &format!("/playlists/{pid}/tracks/{item_id}")).await {
                toast_err(&e.message());
                let _ = items.try_set(before);
            }
        });
    };
    let move_item = move |from: usize, to: usize| {
        let cur = items.get_untracked();
        if from >= cur.len() || to >= cur.len() || from == to || smart.get_untracked() {
            return;
        }
        let Some(item_id) = cur[from].item_id else { return };
        let pid = id.get_untracked();
        let mut next = cur.clone();
        let t = next.remove(from);
        next.insert(to, t);
        items.set(next);
        leptos::task::spawn_local(async move {
            let r: Result<serde_json::Value, _> =
                api::post(&format!("/playlists/{pid}/tracks/{item_id}/move"), &PlaylistMove { to_index: to as i64 }).await;
            if let Err(e) = r {
                toast_err(&e.message());
                let _ = items.try_set(cur);
            }
        });
    };
    dnd::register_target(ROWS, &["pl-row"], move |payload: DragPayload, info| {
        let len = items.with_untracked(|v| v.len());
        if let Some(from) = payload.index {
            move_item(from, reorder_target(from, drop_index(info, len)));
        }
    });
    let over = dnd::over();

    // --- dialogs ---------------------------------------------------------------------
    let rename_open = RwSignal::new(false);
    let rename_to = RwSignal::new(String::new());
    let do_rename = move || {
        let n = rename_to.get_untracked().trim().to_string();
        if n.is_empty() {
            return;
        }
        let pid = id.get_untracked();
        rename_open.set(false);
        leptos::task::spawn_local(async move {
            match api::patch::<_, PlaylistOut>(&format!("/playlists/{pid}"), &PlaylistPatch { name: Some(n), ..Default::default() }).await {
                Ok(_) => meta.refetch(),
                Err(e) => toast_err(&e.message()),
            }
        });
    };
    let add_open = RwSignal::new(false);
    let add_ids = RwSignal::new(Vec::<i64>::new());

    let similar_busy = RwSignal::new(false);
    let nav_similar = navigate.clone();
    let similar = move |_| {
        if let Some(p) = meta.data.get_untracked() {
            let nav = nav_similar.clone();
            make_similar(&p, move |c| nav(&format!("/playlists/{}", c.id), Default::default()), similar_busy);
        }
    };
    let nav_del = navigate.clone();
    let overflow = Callback::new(move |_| -> Vec<MenuEntry> {
        let pid = id.get_untracked();
        let mut v: Vec<MenuEntry> = vec![
            MenuItem::new("Play next").icon("skip-next").on(move || super::queue_playlist(player, pid, true)).into(),
            MenuItem::new("Add to queue").icon("queue").on(move || super::queue_playlist(player, pid, false)).into(),
            MenuEntry::Sep,
        ];
        v.extend(export_menu(pid));
        v.push(MenuEntry::Sep);
        v.push(MenuItem::new("Analyse tracks").icon("activity").on(move || analyse_ids(items.get_untracked().iter().map(|t| t.id).collect())).into());
        v.push(MenuItem::new("Add all to a DJ set…").icon("sliders").on(move || {
            add_ids.set(items.get_untracked().iter().map(|t| t.id).collect());
            add_open.set(true);
        }).into());
        v.push(MenuItem::new("Rename…").icon("edit").on(move || {
            rename_to.set(name.get_untracked());
            rename_open.set(true);
        }).into());
        let nav = nav_del.clone();
        v.push(MenuEntry::Sep);
        v.push(MenuItem::new("Delete playlist").icon("trash").danger().on(move || {
            let nav = nav.clone();
            leptos::task::spawn_local(async move {
                if !confirm("Delete playlist", &format!("Delete \"{}\"? The tracks stay in your library.", name.get_untracked()), "Delete", true).await {
                    return;
                }
                match api::call("DELETE", &format!("/playlists/{pid}")).await {
                    Ok(()) => { toast_ok("Playlist deleted"); nav("/playlists", Default::default()); }
                    Err(e) => toast_err(&e.message()),
                }
            });
        }).into());
        v
    });

    // --- rows --------------------------------------------------------------------------
    let current_id = Signal::derive(move || player.current_track_id());
    let row = move |i: usize, t: TrackOut| {
        let t2 = t.clone();
        let tid = t.id;
        let item_id = t.item_id;
        let label = t.title.clone();
        let title = t.title.clone();
        let artist = artist_link(t.artist.as_ref());
        let album = album_link(t.release.as_ref());
        let title_view = title_link(&t.title, t.release.as_ref());
        let art = t.release.as_ref().and_then(|r| r.art_url.clone()).or(t.art_url.clone());
        let can_edit = move || !smart.get() && item_id.is_some();
        let menu_t = t.clone();
        let menu = Callback::new(move |_| -> Vec<MenuEntry> {
            let (a, b, c, d) = (menu_t.clone(), menu_t.clone(), menu_t.clone(), menu_t.clone());
            let mut v: Vec<MenuEntry> = vec![
                MenuItem::new("Play from here").icon("play").on(move || play_from(i)).into(),
                MenuItem::new("Play next").icon("skip-next").on(move || player.cmd(bc_types::player::PlayerCommand::PlayNext { items: vec![queue_item(&a)] })).into(),
                MenuItem::new("Add to queue").icon("queue").on(move || player.cmd(bc_types::player::PlayerCommand::AddToQueue { items: vec![queue_item(&b)] })).into(),
                MenuItem::new("Add to a DJ set…").icon("sliders").on(move || { add_ids.set(vec![c.id]); add_open.set(true); }).into(),
            ];
            if let Some(r) = &d.release {
                let href = format!("/albums/{}", r.id);
                v.push(MenuEntry::Sep);
                v.push(MenuItem::new("Go to album").icon("disc").on(move || { let _ = crate::util::window().location().set_href(&href); }).into());
            }
            if let Some(ar) = &d.artist {
                let href = format!("/artists/{}", ar.id);
                v.push(MenuItem::new("Go to artist").icon("user").on(move || { let _ = crate::util::window().location().set_href(&href); }).into());
            }
            if let Some(iid) = d.item_id {
                if !smart.get_untracked() {
                    v.push(MenuEntry::Sep);
                    v.push(MenuItem::new("Remove from playlist").icon("x").danger().on(move || remove_item(iid)).into());
                }
            }
            v
        });
        let menu_ctx = expect_context::<crate::ds::MenuCtx>();
        let menu2 = menu;
        view! {
            <div class=move || {
                    let mut c = String::from("pls-track");
                    if current_id.get() == Some(tid) { c.push_str(" playing"); }
                    if let Some((tgt, info)) = over.get() {
                        if tgt == ROWS && info.index == Some(i) { c.push_str(if info.before { " drop-before" } else { " drop-after" }); }
                    }
                    c
                }
                data-dnd-target=ROWS data-dnd-index=i.to_string() tabindex="0" role="row"
                on:dblclick=move |_| play_from(i)
                on:contextmenu=move |ev| { ev.prevent_default(); menu_ctx.open(crate::ds::popover::Rect::point(ev.client_x() as f64, ev.client_y() as f64), menu2.run(())); }
                on:keydown=move |ev| {
                    match (ev.key().as_str(), ev.alt_key()) {
                        ("ArrowUp", true) => { ev.prevent_default(); move_item(i, i.saturating_sub(1)); }
                        ("ArrowDown", true) => { ev.prevent_default(); move_item(i, i + 1); }
                        ("Enter", false) => { ev.prevent_default(); play_from(i); }
                        ("Delete", false) if can_edit() => { if let Some(iid) = item_id { remove_item(iid); } }
                        _ => {}
                    }
                }>
                <span class="pls-grip" title=move || if can_edit() { "Drag to reorder (Alt+Up/Down)" } else { "" }
                    on:pointerdown=move |ev| {
                        if can_edit() {
                            dnd::begin_drag(&ev, DragPayload { kind: "pl-row".into(), ids: vec![tid], label: label.clone(), index: Some(i) });
                        }
                    }>
                    {move || can_edit().then(|| view! { <Icon name="rows" size=14 /> })}
                </span>
                <button type="button" class="pls-num mono" aria-label=format!("Play {title}") on:click=move |_| play_from(i)>
                    <span class="n">{i + 1}</span><span class="p"><Icon name="play" size=14 /></span>
                </button>
                <Art src=art size=36.0 />
                <div class="pls-tt">
                    <span class="truncate pls-title">{title_view}</span>
                    <span class="truncate muted pls-artist">{artist}</span>
                </div>
                <span class="truncate muted pls-album">{album}</span>
                <span class="mono pls-bpm">{format_bpm(t.bpm)}</span>
                <span class="camelot pls-key">{t.camelot.clone().unwrap_or_default()}</span>
                <span class="mono muted pls-time">{format_duration_ms(t.duration_ms.map(|d| d as f64))}</span>
                <LoveButton track_id=tid loved=t2.loved />
                <crate::ds::MenuButton entries=menu icon="more" title="Track actions" class="btn-sm" />
            </div>
        }
    };

    view! {
        <div class="page dj-page">
            <div class="pls-hero">
                <Collage urls=Signal::derive(move || covers.get()) />
                <div class="pls-hero-main">
                    <a class="pls-back" href="/playlists"><Icon name="arrow-left" size=13 />"All playlists"</a>
                    <h1 class="pls-h1">
                        {move || if meta.error.get().is_some() && meta.data.get().is_none() { "Playlist not found".to_string() } else if name.get().is_empty() { "…".to_string() } else { name.get() }}
                    </h1>
                    <div class="pls-meta">
                        <span class="mono">{move || format!("{} track{} · {}", items.with(|v| v.len()), if items.with(|v| v.len()) == 1 { "" } else { "s" }, format_long_duration(total_ms.get() as f64))}</span>
                        {move || smart.get().then(|| view! { <span class="badge badge-info">"smart · live filter"</span> })}
                    </div>
                    <div class="pls-hero-actions">
                        <Button variant=Variant::Primary size=Size::Lg icon=crate::ds::dyn_icon(move || if live.get().playing.get() { "pause" } else { "play" })
                            disabled=Signal::derive(move || items.with(|v| v.is_empty())) on_click=play_or_pause>
                            {move || if live.get().playing.get() { "Pause" } else { "Play" }}
                        </Button>
                        <Button size=Size::Lg icon="shuffle" disabled=Signal::derive(move || items.with(|v| v.is_empty()))
                            title="Play this playlist in a random order" on_click=shuffle_play><span class="hide-xs">"Shuffle"</span></Button>
                        <Button size=Size::Lg icon="mix" pressed=Signal::derive(move || live.get().mixing.get())
                            disabled=Signal::derive(move || items.with(|v| v.is_empty()))
                            title="DJ mix this playlist: beatmatched blends, topped up from it"
                            on_click=move |_| dj_mix(player, app, id.get_untracked(), &name.get_untracked())><span class="hide-xs">"DJ mix"</span></Button>
                        <Button size=Size::Lg icon="sparkles" busy=similar_busy
                            disabled=Signal::derive(move || items.with(|v| v.is_empty()))
                            title="A new playlist that sounds like this one" on_click=similar><span class="hide-xs">"Similar"</span></Button>
                        <crate::ds::MenuButton entries=overflow title="More" class="btn-lg" />
                    </div>
                </div>
            </div>
            <div class="pls-tracks" role="table" aria-label="Tracks">
                {move || {
                    match (has_tracks.get(), err_msg.get()) {
                        (true, _) if items.with(|v| v.is_empty()) => view! {
                            <EmptyState title="Nothing in this playlist yet" hint="Add tracks from the track menu on any track or album." icon="list-music" />
                        }.into_any(),
                        (true, _) => view! {
                            <div class="pls-th" aria-hidden="true">
                                <span></span><span></span><span></span><span>"Title"</span><span class="pls-album">"Album"</span>
                                <span class="pls-bpm">"BPM"</span><span class="pls-key">"Key"</span><span class="pls-time">"Time"</span><span></span><span></span>
                            </div>
                            <div class="pls-body">
                                {move || items.get().into_iter().enumerate().map(|(i, t)| row(i, t)).collect_view()}
                            </div>
                        }.into_any(),
                        (false, Some(e)) => view! { <ErrorPanel message=e on_retry=Callback::new(move |_| tracks.refetch()) /> }.into_any(),
                        (false, None) => view! { <div class="pls-body">{(0..8).map(|_| view! { <Skeleton height="44px" class="pls-skel" /> }).collect_view()}</div> }.into_any(),
                    }
                }}
            </div>
            <Dialog open=rename_open title="Rename playlist"
                footer=crate::ds::children(move || view! {
                    <Button variant=Variant::Ghost on_click=move |_| rename_open.set(false)>"Cancel"</Button>
                    <Button variant=Variant::Primary on_click=move |_| do_rename()>"Rename"</Button>
                })>
                <input class="input" aria-label="Playlist name" prop:value=move || rename_to.get()
                    on:input=move |ev| rename_to.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" { do_rename() } />
            </Dialog>
            <Dialog open=add_open title="Add to a DJ set">
                <super::SetPicker ids=Callback::new(move |_| add_ids.get_untracked()) on_done=Callback::new(move |_| add_open.set(false)) />
            </Dialog>
        </div>
    }
}

#[allow(dead_code)]
async fn _unused() {
    let _ = fetch_playlist_tracks(0).await;
    let _ = enc("");
}
