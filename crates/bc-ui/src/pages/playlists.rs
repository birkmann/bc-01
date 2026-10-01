//! Playlists shelf and playlist detail (legacy `pages/Playlists.tsx`): create, play, DJ mix,
//! "another like this", scratch pool, exports, analyse, delete; detail with pointer-DnD reorder.
mod collage;
mod detail;
pub(crate) use collage::Collage;
mod logic;
mod save_as;
mod scratch;
mod set_picker;

use bc_types::analysis::{ScanRequest, ScanResponse, ScanScope};
use bc_types::library::{Page, PlaylistCreate, PlaylistOut, TrackOut};
use bc_types::player::{PlayerCommand, Pool, QueueSource};
use bc_types::suggest::{SimilarPlaylistOut, SimilarPlaylistRequest};
use leptos::prelude::*;

use crate::api;
use crate::data::QuerySpec;
use crate::player::plan::qh::use_qh;
use crate::ds::{Button, EmptyState, ErrorPanel, Icon, MenuEntry, MenuItem, PageHeader, Skeleton, Size, Variant, confirm, toast_err, toast_info, toast_ok};
use crate::logic::format::format_long_duration;
use crate::player::use_player;
use crate::util::enc;
use crate::widgets::dnd::{self, DragPayload};

pub use detail::PlaylistDetailPage;
pub use save_as::SaveAsPlaylist;
pub use set_picker::SetPicker;
use scratch::{ScratchCtx, ScratchPool};

/// All tracks of a playlist in order (the server returns one page with everything).
pub async fn fetch_playlist_tracks(id: i64) -> Result<Vec<TrackOut>, api::ApiErr> {
    let p: Page<TrackOut> = api::get(&format!("/playlists/{id}/tracks")).await?;
    Ok(p.items)
}

pub(crate) fn open_url(url: &str) {
    let _ = crate::util::window().open_with_url_and_target(url, "_blank");
}

pub(crate) fn export_menu(id: i64) -> Vec<MenuEntry> {
    let mk = |label: &'static str, icon: &'static str, fmt: &'static str| -> MenuEntry {
        MenuItem::new(label).icon(icon).on(move || open_url(&format!("/api/playlists/{id}/export?format={fmt}"))).into()
    };
    vec![mk("Export as M3U8", "download", "m3u8"), mk("Export as CSV", "download", "csv"), mk("Download audio as ZIP", "download", "zip")]
}

/// Queue analysis for the tracks without one yet (runs on click, never per row on mount).
pub(crate) fn analyse_ids(ids: Vec<i64>) {
    leptos::task::spawn_local(async move {
        let req = ScanRequest { scope: ScanScope::Ids, track_ids: ids, limit: None, only_missing: true };
        match api::post::<_, ScanResponse>("/analysis/scan", &req).await {
            Ok(r) if r.queued > 0 => toast_ok(&format!("Queued {} tracks for analysis", r.queued)),
            Ok(_) => toast_info("Everything here is analysed already"),
            Err(e) => toast_err(&e.message()),
        }
    });
}

pub(crate) fn make_similar(p: &PlaylistOut, on_done: impl Fn(SimilarPlaylistOut) + 'static, busy: RwSignal<bool>) {
    let id = p.id;
    busy.set(true);
    leptos::task::spawn_local(async move {
        let req = SimilarPlaylistRequest { shuffle_seed: (crate::util::entropy() % 1_000_000) as i64, ..Default::default() };
        match api::post::<_, SimilarPlaylistOut>(&format!("/playlists/{id}/similar"), &req).await {
            Ok(created) => {
                toast_ok(&format!("Created {}", created.name));
                on_done(created);
            }
            Err(e) => toast_err(&e.message()),
        }
        let _ = busy.try_set(false);
    });
}

/// Playing / current / mixing flags of one playlist, for lighting its row up.
#[derive(Clone, Copy)]
pub(crate) struct Live {
    pub current: Signal<bool>,
    pub playing: Signal<bool>,
    pub mixing: Signal<bool>,
}

pub(crate) fn live_of(pid: i64) -> Live {
    use bc_types::player::PlayerStatus;
    let p = use_player();
    let current = Signal::derive(move || p.state.with(|s| matches!(&s.source, Some(QueueSource::Playlist { id, .. }) if *id == pid)));
    let playing = Signal::derive(move || current.get() && p.state.with(|s| s.status == PlayerStatus::Playing));
    let mixing = Signal::derive(move || {
        p.state.with(|s| s.mix && matches!(s.plan.pools.first(), Some(Pool::Playlist { id, .. }) if *id == pid))
    });
    Live { current, playing, mixing }
}

/// Start playback of `tracks` at `start` (the player handle is passed in: contexts are not
/// reachable after an await).
pub(crate) fn play_items(player: crate::player::PlayerCtx, tracks: &[TrackOut], start: usize, source: Option<QueueSource>) {
    let items = tracks.iter().map(crate::widgets::common::queue_item).collect();
    player.cmd(PlayerCommand::PlayQueue { items, start_index: start, source });
}

/// Queue a whole playlist: `next` puts it right after the playing track, else at the end.
pub(crate) fn queue_playlist(player: crate::player::PlayerCtx, id: i64, next: bool) {
    leptos::task::spawn_local(async move {
        match fetch_playlist_tracks(id).await {
            Ok(items) if !items.is_empty() => {
                let items = items.iter().map(crate::widgets::common::queue_item).collect();
                player.cmd(if next { PlayerCommand::PlayNext { items } } else { PlayerCommand::AddToQueue { items } });
                toast_ok(if next { "Playing next" } else { "Added to the queue" });
            }
            Ok(_) => toast_info("This playlist is empty"),
            Err(e) => toast_err(&e.message()),
        }
    });
}

pub(crate) fn dj_mix(player: crate::player::PlayerCtx, app: crate::app::AppCtx, id: i64, name: &str) {
    player.cmd(PlayerCommand::StartMixFrom { pool: Pool::Playlist { id, name: name.to_string() } });
    app.panel.set(Some(crate::app::Panel::Plan));
}

pub(crate) fn play_playlist(player: crate::player::PlayerCtx, id: i64, name: String) {
    leptos::task::spawn_local(async move {
        match fetch_playlist_tracks(id).await {
            Ok(items) if !items.is_empty() => play_items(player, &items, 0, Some(QueueSource::Playlist { id, name })),
            Ok(_) => toast_info("This playlist is empty"),
            Err(e) => toast_err(&e.message()),
        }
    });
}

#[component]
fn PlaylistRow(p: PlaylistOut, on_deleted: Callback<()>) -> impl IntoView {
    let player = use_player();
    let app = crate::app::use_app();
    let scratch = expect_context::<ScratchCtx>();
    let live = live_of(p.id);
    let busy_similar = RwSignal::new(false);
    let busy_pool = RwSignal::new(false);
    let empty = p.track_count == 0;
    let smart = p.kind == "smart";
    let navigate = leptos_router::hooks::use_navigate();
    let (id, name) = (p.id, p.name.clone());
    let lit = move || live.current.get() || live.mixing.get();
    let held = RwSignal::new(false);

    let pool = {
        let name = name.clone();
        move |_| {
            busy_pool.set(true);
            let name = name.clone();
            leptos::task::spawn_local(async move {
                match fetch_playlist_tracks(id).await {
                    Ok(items) => scratch.pool.update(|pl| pl.add_source(&format!("playlist:{id}"), &name, &items)),
                    Err(e) => toast_err(&e.message()),
                }
                let _ = busy_pool.try_set(false);
            });
        }
    };
    let delete = {
        let name = name.clone();
        move || {
            let name = name.clone();
            leptos::task::spawn_local(async move {
                if !confirm("Delete playlist", &format!("Delete \"{name}\"? The tracks stay in your library."), "Delete", true).await {
                    return;
                }
                match api::call("DELETE", &format!("/playlists/{id}")).await {
                    Ok(()) => {
                        toast_ok("Playlist deleted");
                        on_deleted.run(());
                    }
                    Err(e) => toast_err(&e.message()),
                }
            });
        }
    };
    let menu = {
        let delete = delete.clone();
        let name = name.clone();
        Callback::new(move |_| -> Vec<MenuEntry> {
            let mut v: Vec<MenuEntry> = vec![
                MenuItem::new("Play next").icon("skip-next").disabled(empty).on(move || queue_playlist(player, id, true)).into(),
                MenuItem::new("Add to queue").icon("queue").disabled(empty).on(move || queue_playlist(player, id, false)).into(),
                MenuEntry::Sep,
            ];
            v.extend(export_menu(id));
            v.push(MenuEntry::Sep);
            v.push(
                MenuItem::new("Analyse tracks")
                    .icon("activity")
                    .on(move || {
                        leptos::task::spawn_local(async move {
                            match fetch_playlist_tracks(id).await {
                                Ok(items) => analyse_ids(items.iter().map(|t| t.id).collect()),
                                Err(e) => toast_err(&e.message()),
                            }
                        });
                    })
                    .into(),
            );
            let d = delete.clone();
            let n = name.clone();
            v.push(MenuItem::new("Open").icon("list").on({ let nav = navigate.clone(); move || nav(&format!("/playlists/{id}"), Default::default()) }).into());
            let _ = n;
            v.push(MenuEntry::Sep);
            v.push(MenuItem::new("Delete").icon("trash").danger().on(move || d()).into());
            v
        })
    };
    let play_label = {
        let name = name.clone();
        move |_| {
            if live.current.get_untracked() {
                player.toggle();
            } else {
                play_playlist(player, id, name.clone());
            }
        }
    };
    let name_for_mix = name.clone();
    let sim = p.clone();
    let nav2 = leptos_router::hooks::use_navigate();
    let drag_label = name.clone();

    view! {
        <div class=move || format!("pls-row{}{}", if lit() { " live" } else { "" }, if held.get() { " held" } else { "" })
            on:pointerdown=move |ev| {
                // drag the whole row (not its buttons) into the scratch pool
                if let Some(t) = ev.target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()) {
                    if t.closest("button,input").ok().flatten().is_some() { return; }
                }
                dnd::begin_drag(&ev, DragPayload { kind: "playlist".into(), ids: vec![id], label: drag_label.clone(), index: None });
                held.set(false);
            }>
            <a class="pls-main" href=format!("/playlists/{id}") draggable="false">
                <span class="pls-ico"><Icon name=move || if live.playing.get() { "volume" } else { "list-music" } /></span>
                <span class="pls-name truncate">{name.clone()}</span>
                {smart.then(|| view! { <span class="badge badge-info">"smart"</span> })}
                <span class="pls-count mono">{format!("{} tracks · {}", p.track_count, format_long_duration(p.duration_ms as f64))}</span>
            </a>
            <div class="pls-actions">
                <Button size=Size::Sm variant=Variant::Ghost icon=crate::ds::dyn_icon(move || if live.playing.get() { "pause" } else { "play" })
                    pressed=Signal::derive(move || live.current.get()) disabled=empty
                    title="Play this playlist from the top" on_click=move |_| play_label(())>
                    <span class="pls-lbl">{move || if live.playing.get() { "Pause" } else { "Play" }}</span>
                </Button>
                <Button size=Size::Sm variant=Variant::Ghost icon="mix" disabled=empty pressed=Signal::derive(move || live.mixing.get())
                    title="DJ mix this playlist: beatmatched blends, topped up from it"
                    on_click=move |_| dj_mix(player, app, id, &name_for_mix)>
                    <span class="pls-lbl">"DJ mix"</span>
                </Button>
                <Button size=Size::Sm variant=Variant::Ghost icon="sparkles" disabled=empty busy=busy_similar
                    title="A new playlist of tracks that sound like this one; press again for another"
                    on_click=move |_| { let nav = nav2.clone(); make_similar(&sim, move |c| nav(&format!("/playlists/{}", c.id), Default::default()), busy_similar) }>
                    <span class="pls-lbl">"Similar"</span>
                </Button>
                <Button size=Size::Sm variant=Variant::Ghost icon="layers" disabled=empty busy=busy_pool
                    title="Add this playlist to the scratch pool at the foot of the page" on_click=pool>
                    <span class="pls-lbl">"Pool"</span>
                </Button>
                <crate::ds::MenuButton entries=menu class="btn-sm" />
            </div>
        </div>
    }
}

#[component]
pub fn PlaylistsPage() -> impl IntoView {
    let list = use_qh::<Vec<PlaylistOut>>(|| Some(QuerySpec::new("/playlists", &["playlist"])));
    let name = RwSignal::new(String::new());
    let creating = RwSignal::new(false);
    let scratch = ScratchCtx::provide();

    let create = move || {
        if creating.get_untracked() {
            return;
        }
        creating.set(true);
        let n = name.get_untracked();
        leptos::task::spawn_local(async move {
            let body = PlaylistCreate { name: if n.trim().is_empty() { "Untitled".into() } else { n.trim().to_string() }, ..Default::default() };
            match api::post::<_, PlaylistOut>("/playlists", &body).await {
                Ok(p) => {
                    name.set(String::new());
                    toast_ok(&format!("Created {}", p.name));
                    list.refetch();
                }
                Err(e) => toast_err(&e.message()),
            }
            let _ = creating.try_set(false);
        });
    };
    let create2 = create;
    let subtitle = Signal::derive(move || list.data.get().map(|l| format!("{} playlist{} · ordered collections you can turn into a set", l.len(), if l.len() == 1 { "" } else { "s" })).unwrap_or_default());
    let _ = scratch;

    view! {
        <div class="page dj-page">
            <PageHeader title="Playlists" subtitle=subtitle />
            <div class="page-scroll pls-page">
                <form class="pls-create" on:submit=move |ev| { ev.prevent_default(); create(); }>
                    <input class="input" placeholder="New playlist name…" aria-label="New playlist name"
                        prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                    <Button variant=Variant::Primary icon="plus" kind="submit" busy=creating on_click=move |_| create2()>"Create"</Button>
                </form>
                {move || {
                    let err = list.error.get();
                    match (list.data.get(), err) {
                        (Some(l), _) if l.is_empty() => view! {
                            <EmptyState title="No playlists yet" hint="Create one above, or keep a view from Tracks or Loved with Save as playlist." icon="list-music" />
                        }.into_any(),
                        (Some(l), _) => view! {
                            <div class="pls-list">
                                {l.iter().cloned().map(|p| view! { <PlaylistRow p=p on_deleted=Callback::new(move |_| list.refetch()) /> }).collect_view()}
                            </div>
                        }.into_any(),
                        (None, Some(e)) => view! { <ErrorPanel message=e.message() on_retry=Callback::new(move |_| list.refetch()) /> }.into_any(),
                        (None, None) => view! {
                            <div class="pls-list">{(0..4).map(|_| view! { <Skeleton height="58px" class="pls-skel" /> }).collect_view()}</div>
                        }.into_any(),
                    }
                }}
                <ScratchPool />
            </div>
        </div>
    }
}

#[allow(dead_code)]
fn _e() -> String {
    enc("")
}
