//! Shared actions of the library pages: playing releases and shelves, the
//! "add to playlist / DJ set" picker, the delete-albums dialog (with blacklist and
//! progress) and the release context menu. A page mounts `<LibraryHost/>` once and
//! any card / row below it opens the dialogs through [`use_library_host`].
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bc_types::library::*;
use bc_types::player::{PlayerCommand, QueueSource};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::logic;
use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Button, Dialog, Icon, MenuEntry, MenuItem, SegmentedControl, Variant, toast_err, toast_info, toast_ok, toast_warn};
use crate::logic::format::format_count;
use crate::player::PlayerCtx;
use crate::widgets::common::queue_item;

// ---- tracks of releases ----------------------------------------------------------------------

pub const QUEUE_MAX: usize = 500;

/// Every track of a release in album order (`/tracks?release_id=`).
pub async fn fetch_release_tracks(id: i64) -> Result<Vec<TrackOut>, api::ApiErr> {
    let page: TrackPage = api::get(&format!("/tracks?release_id={id}&sort=album&order=asc&limit={QUEUE_MAX}")).await?;
    Ok(page.page.items)
}

/// A shelf of covers as one queue: all their tracks in one request, re-dealt into shelf order.
pub async fn fetch_shelf_tracks(release_ids: &[i64]) -> Result<Vec<TrackOut>, api::ApiErr> {
    if release_ids.is_empty() {
        return Ok(vec![]);
    }
    let ids = release_ids.iter().map(|i| format!("release_ids={i}")).collect::<Vec<_>>().join("&");
    let page: TrackPage = api::get(&format!("/tracks?{ids}&sort=album&order=asc&limit={QUEUE_MAX}")).await?;
    Ok(logic::order_by_release_rank(&page.page.items, |t| t.release.as_ref().map(|r| r.id), release_ids))
}

pub fn shuffle_in_place<T>(v: &mut [T]) {
    for i in (1..v.len()).rev() {
        let j = (js_sys::Math::random() * (i as f64 + 1.0)) as usize;
        v.swap(i, j.min(i));
    }
}

pub fn play_items(player: PlayerCtx, tracks: &[TrackOut], start: usize, source: Option<QueueSource>, shuffled: bool) {
    if tracks.is_empty() {
        return;
    }
    if shuffled {
        // the queue is already dealt; the player's own shuffle would re-roll it
        player.cmd(PlayerCommand::SetShuffle { on: false });
    }
    player.cmd(PlayerCommand::PlayQueue { items: tracks.iter().map(queue_item).collect(), start_index: start, source });
}

/// The album's continuation is the listing it was played from (the server queues the next album).
pub fn play_release(player: PlayerCtx, release_id: i64, listing: serde_json::Value, shuffle: bool) {
    player.cmd(PlayerCommand::StartSource { source: QueueSource::Release { release_id, listing }, shuffle });
}

pub fn library_listing() -> serde_json::Value {
    serde_json::json!({"sort": "added", "order": "desc"})
}

/// Run an async step, toast errors.
pub fn spawn_toast<F: Future<Output = Result<(), api::ApiErr>> + 'static>(f: F) {
    spawn_local(async move {
        if let Err(e) = f.await {
            toast_err(&e.message());
        }
    });
}

// ---- host ---------------------------------------------------------------------------------------

pub type IdsFut = Pin<Box<dyn Future<Output = Vec<i64>>>>;
pub type IdsFn = Arc<dyn Fn() -> IdsFut + Send + Sync>;

#[derive(Clone)]
pub struct PickerReq {
    pub ids: IdsFn,
}

#[derive(Clone)]
pub struct DeleteReq {
    pub release_ids: Vec<i64>,
    pub tracks: Option<i64>,
    pub on_done: Option<Callback<Vec<i64>>>,
}

#[derive(Clone, Copy)]
pub struct LibraryHostCtx {
    pub picker: RwSignal<Option<PickerReq>>,
    pub delete: RwSignal<Option<DeleteReq>>,
}

pub fn provide_library_host() -> LibraryHostCtx {
    let ctx = LibraryHostCtx { picker: RwSignal::new(None), delete: RwSignal::new(None) };
    provide_context(ctx);
    ctx
}

pub fn use_library_host() -> LibraryHostCtx {
    expect_context::<LibraryHostCtx>()
}

/// Ids resolver for a fixed list.
pub fn ids_of(v: Vec<i64>) -> IdsFn {
    Arc::new(move || {
        let v = v.clone();
        Box::pin(async move { v })
    })
}

/// Ids resolver for the tracks of releases (album order).
pub fn release_track_ids(release_ids: Vec<i64>) -> IdsFn {
    Arc::new(move || {
        let r = release_ids.clone();
        Box::pin(async move {
            match fetch_shelf_tracks(&r).await {
                Ok(t) => t.into_iter().map(|t| t.id).collect(),
                Err(e) => {
                    toast_err(&e.message());
                    vec![]
                }
            }
        })
    })
}

fn after_library_change() {
    crate::data::invalidate_all();
    crate::data::invalidate_prefix("home");
}

#[component]
pub fn LibraryHost() -> impl IntoView {
    view! {
        <PickerDialog />
        <DeleteDialog />
    }
}

#[component]
fn PickerDialog() -> impl IntoView {
    let ctx = use_library_host();
    let open = RwSignal::new(false);
    Effect::new(move |_| open.set(ctx.picker.with(|p| p.is_some())));
    Effect::new(move |_| {
        if !open.get() && ctx.picker.get_untracked().is_some() {
            ctx.picker.set(None);
        }
    });
    let tab = RwSignal::new("playlist".to_string());
    let name = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let playlists = use_query::<Vec<PlaylistOut>>(move || open.get().then(|| QuerySpec::new("/playlists", &["playlist"])));
    let sets = use_query::<Vec<bc_types::sets::DjSetListOut>>(move || open.get().then(|| QuerySpec::new("/sets", &["set"])));

    let add = Arc::new(move |kind: &'static str, id: i64, label: String| {
        let Some(req) = ctx.picker.get_untracked() else { return };
        busy.set(true);
        spawn_local(async move {
            let ids = (req.ids)().await;
            if ids.is_empty() {
                busy.set(false);
                toast_warn("Nothing to add.");
                return;
            }
            let n = ids.len() as i64;
            let r = if kind == "playlist" {
                api::post::<_, PlaylistAdded>(&format!("/playlists/{id}/tracks"), &PlaylistAddTracks { track_ids: ids, at_index: None }).await.map(|_| ())
            } else {
                api::post::<_, serde_json::Value>(&format!("/sets/{id}/items"), &bc_types::sets::AddTracks { track_ids: ids }).await.map(|_| ())
            };
            busy.set(false);
            match r {
                Ok(()) => {
                    toast_ok(&format!("Added {} track{} to {label}", format_count(n), if n == 1 { "" } else { "s" }));
                    crate::data::invalidate_entity(kind, &[id]);
                    ctx.picker.set(None);
                }
                Err(e) => toast_err(&e.message()),
            }
        });
    });
    let create = {
        let add = add.clone();
        move || {
            let n = name.get_untracked().trim().to_string();
            if n.is_empty() {
                return;
            }
            let add = add.clone();
            busy.set(true);
            spawn_local(async move {
                let body = PlaylistCreate { name: n.clone(), ..Default::default() };
                match api::post::<_, PlaylistOut>("/playlists", &body).await {
                    Ok(p) => {
                        name.set(String::new());
                        add("playlist", p.id, p.name);
                    }
                    Err(e) => {
                        busy.set(false);
                        toast_err(&e.message());
                    }
                }
            });
        }
    };
    let create = Arc::new(create);
    let cr1 = StoredValue::new(create.clone());
    let cr2 = StoredValue::new(create);
    let add_p = StoredValue::new(add.clone());
    let add_s = StoredValue::new(add);

    view! {
        <Dialog open=open title="Add to…">
            {move || {
                
                view! {
                    <div class="lib-picker">
                        <SegmentedControl options=vec![("playlist", "Playlist"), ("set", "DJ set")] value=tab />
                        <div class="lib-picker-list" role="listbox">
                            {move || {
                                let (add_p, add_s) = (add_p.get_value(), add_s.get_value());
                                if tab.get() == "playlist" {
                                    let list = playlists.data.get().map(|d| (*d).clone()).unwrap_or_default();
                                    if list.is_empty() {
                                        return view! { <p class="faint lib-picker-empty">"No playlists yet. Name one below."</p> }.into_any();
                                    }
                                    list.into_iter().filter(|p| p.kind != "smart").map(move |p| {
                                        let (add, name, id) = (add_p.clone(), p.name.clone(), p.id);
                                        view! {
                                            <button type="button" class="lib-picker-row" role="option" disabled=move || busy.get() on:click=move |_| add("playlist", id, name.clone())>
                                                <Icon name="list" /><span class="truncate grow">{p.name.clone()}</span><span class="mono faint">{format_count(p.track_count)}</span>
                                            </button>
                                        }
                                    }).collect_view().into_any()
                                } else {
                                    let list = sets.data.get().map(|d| (*d).clone()).unwrap_or_default();
                                    if list.is_empty() {
                                        return view! { <p class="faint lib-picker-empty">"No DJ sets yet. Create one on the DJ Sets page."</p> }.into_any();
                                    }
                                    list.into_iter().map(move |s| {
                                        let (add, name, id) = (add_s.clone(), s.name.clone(), s.id);
                                        view! {
                                            <button type="button" class="lib-picker-row" role="option" disabled=move || busy.get() on:click=move |_| add("set", id, name.clone())>
                                                <Icon name="sliders" /><span class="truncate grow">{s.name.clone()}</span><span class="mono faint">{format_count(s.track_count)}</span>
                                            </button>
                                        }
                                    }).collect_view().into_any()
                                }
                            }}
                        </div>
                        {move || (tab.get() == "playlist").then(|| {
                            let (c1, c2) = (cr1.get_value(), cr2.get_value());
                            view! {
                                <div class="lib-picker-new">
                                    <input class="input" type="text" placeholder="New playlist name…" aria-label="New playlist name" prop:value=move || name.get()
                                        on:input=move |ev| name.set(event_target_value(&ev))
                                        on:keydown=move |ev| if ev.key() == "Enter" { c1(); } />
                                    <Button variant=Variant::Primary icon="plus" disabled=Signal::derive(move || name.get().trim().is_empty() || busy.get()) on_click=move |_| c2()>"Create"</Button>
                                </div>
                            }
                        })}
                    </div>
                }
            }}
        </Dialog>
    }
}

const DELETE_CHUNK: usize = 50;

#[component]
fn DeleteDialog() -> impl IntoView {
    let ctx = use_library_host();
    let open = RwSignal::new(false);
    let blacklist = RwSignal::new(true);
    let busy = RwSignal::new(false);
    let progress = RwSignal::new(None::<(usize, usize)>);
    let stop = StoredValue::new(false);
    let error = RwSignal::new(None::<String>);
    Effect::new(move |_| open.set(ctx.delete.with(|p| p.is_some())));
    Effect::new(move |_| {
        if !open.get() && !busy.get_untracked() && ctx.delete.get_untracked().is_some() {
            ctx.delete.set(None);
        }
    });
    let count = Memo::new(move |_| ctx.delete.with(|d| d.as_ref().map(|d| d.release_ids.len()).unwrap_or(0)));
    let tracks = Memo::new(move |_| ctx.delete.with(|d| d.as_ref().and_then(|d| d.tracks)));
    let title = Signal::derive(move || format!("Delete {} album{}?", format_count(count.get() as i64), if count.get() == 1 { "" } else { "s" }));

    let run = move || {
        let Some(req) = ctx.delete.get_untracked() else { return };
        let bl = blacklist.get_untracked();
        busy.set(true);
        error.set(None);
        stop.set_value(false);
        spawn_local(async move {
            let ids = req.release_ids.clone();
            let mut tally = DeleteReleasesResult::default();
            let mut sent: Vec<i64> = vec![];
            progress.set(Some((0, ids.len())));
            let mut failed = None;
            for chunk in ids.chunks(DELETE_CHUNK) {
                if stop.get_value() {
                    break;
                }
                let body = DeleteReleasesRequest { ids: chunk.to_vec(), blacklist: bl, reason: Some("albums".into()) };
                match api::post::<_, DeleteReleasesResult>("/releases/delete", &body).await {
                    Ok(r) => {
                        tally.releases += r.releases;
                        tally.tracks += r.tracks;
                        tally.files += r.files;
                        tally.blacklisted += r.blacklisted;
                        tally.inbox_ignored += r.inbox_ignored;
                        tally.errors.extend(r.errors);
                        sent.extend_from_slice(chunk);
                        progress.set(Some((sent.len(), ids.len())));
                    }
                    Err(e) => {
                        failed = Some(e.message());
                        break;
                    }
                }
            }
            busy.set(false);
            progress.set(None);
            after_library_change();
            if let Some(cb) = req.on_done {
                cb.run(sent);
            }
            let mut msg = format!("Deleted {} album{} · {} files", format_count(tally.releases), if tally.releases == 1 { "" } else { "s" }, format_count(tally.files));
            if tally.blacklisted > 0 {
                msg.push_str(&format!(" · blacklisted {}", format_count(tally.blacklisted)));
            }
            if tally.inbox_ignored > 0 {
                msg.push_str(&format!(" · retired {} inbox items", format_count(tally.inbox_ignored)));
            }
            match failed {
                Some(e) => {
                    error.set(Some(e.clone()));
                    toast_err(&format!("{msg}. Stopped: {e}"));
                }
                None => {
                    if let Some(first) = tally.errors.first() {
                        toast_warn(&format!("{msg} · {} failed: {first}", tally.errors.len()));
                    } else {
                        toast_ok(&msg);
                    }
                    ctx.delete.set(None);
                }
            }
        });
    };
    let run = Arc::new(run);
    let run2 = run.clone();

    view! {
        <Dialog open=open title=title footer=crate::ds::children(move || {
            let run = run2.clone();
            view! {
                <Button variant=Variant::Ghost on_click=move |_| if busy.get_untracked() { stop.set_value(true) } else { open.set(false) }>
                    {move || if busy.get() { "Stop" } else { "Cancel" }}
                </Button>
                <Button variant=Variant::Danger icon="trash" busy=busy on_click=move |_| run()>"Delete"</Button>
            }
        })>
            {move || view! {
                <p class="lib-del-text">
                    {move || match tracks.get() {
                        Some(t) => format!("{} track{} and their files are erased from disk. ", format_count(t), if t == 1 { "" } else { "s" }),
                        None => "Their tracks and files are erased from disk. ".to_string(),
                    }}
                    "This cannot be undone."
                </p>
                <label class="lib-del-check">
                    <input type="checkbox" prop:checked=move || blacklist.get() disabled=move || busy.get() on:change=move |ev| blacklist.set(event_target_checked(&ev)) />
                    <span>"Also blacklist these"
                        <span class="faint lib-del-hint">"Never download or queue them again. Without this the wishlist they came from will fetch them straight back."</span>
                    </span>
                </label>
                {move || progress.get().map(|(d, t)| view! {
                    <div class="lib-del-progress" role="status" aria-live="polite">
                        <div class="row"><span class="faint">"Deleting…"</span><span class="spacer"></span><span class="mono">{format!("{} / {}", format_count(d as i64), format_count(t as i64))}</span></div>
                        <crate::ds::Meter value=Signal::derive(move || Some(d as f64 / (t.max(1)) as f64)) tone=crate::ds::Tone::Danger label="Deleting albums" />
                    </div>
                })}
                {move || error.get().map(|e| view! { <p class="lib-del-err">{e}</p> })}
            }}
        </Dialog>
    }
}

// ---- release menu --------------------------------------------------------------------------------

/// Context-menu entries of a release. `nav` navigates inside the app.
pub fn release_menu(r: &ReleaseOut, listing: serde_json::Value, player: PlayerCtx, nav: Callback<String>, on_deleted: Option<Callback<Vec<i64>>>) -> Vec<MenuEntry> {
    let host = use_library_host();
    let id = r.id;
    let mut v: Vec<MenuEntry> = vec![];
    let l1 = listing.clone();
    v.push(MenuItem::new("Play").icon("play").on(move || play_release(player, id, l1.clone(), false)).into());
    let l2 = listing;
    v.push(MenuItem::new("Shuffle").icon("shuffle").on(move || {
        let l = l2.clone();
        spawn_toast(async move {
            let mut t = fetch_release_tracks(id).await?;
            shuffle_in_place(&mut t);
            play_items(player, &t, 0, Some(QueueSource::Release { release_id: id, listing: l }), true);
            Ok(())
        });
    }).into());
    v.push(MenuItem::new("Play next").icon("skip-next").on(move || {
        spawn_toast(async move {
            let t = fetch_release_tracks(id).await?;
            player.cmd(PlayerCommand::PlayNext { items: t.iter().map(queue_item).collect() });
            Ok(())
        });
    }).into());
    v.push(MenuItem::new("Add to queue").icon("queue").on(move || {
        spawn_toast(async move {
            let t = fetch_release_tracks(id).await?;
            player.cmd(PlayerCommand::AddToQueue { items: t.iter().map(queue_item).collect() });
            toast_info(&format!("Queued {} track{}", t.len(), if t.len() == 1 { "" } else { "s" }));
            Ok(())
        });
    }).into());
    v.push(MenuEntry::Sep);
    v.push(MenuItem::new("Love all tracks").icon("heart").on(move || {
        spawn_toast(async move {
            let t = fetch_release_tracks(id).await?;
            let ids: Vec<i64> = t.iter().map(|t| t.id).collect();
            let res: ChangedOut = api::post("/tracks/love", &SetLoved { track_ids: ids, loved: true }).await?;
            toast_ok(&format!("Loved {} track{}", res.changed, if res.changed == 1 { "" } else { "s" }));
            crate::data::invalidate_entity("track", &[]);
            Ok(())
        });
    }).into());
    v.push(MenuItem::new("Add to playlist or set…").icon("list").on(move || host.picker.set(Some(PickerReq { ids: release_track_ids(vec![id]) }))).into());
    v.push(MenuEntry::Sep);
    if let Some(a) = &r.artist {
        let href = format!("/artists/{}", a.id);
        v.push(MenuItem::new(format!("Go to {}", a.name)).icon("user").on(move || nav.run(href.clone())).into());
    }
    if let Some(l) = r.label_id {
        let href = format!("/labels/{l}");
        v.push(MenuItem::new("Go to label").icon("folder").on(move || nav.run(href.clone())).into());
    }
    if let Some(u) = r.bandcamp_url.clone() {
        v.push(MenuItem::new("Open on Bandcamp").icon("external").on(move || {
            let _ = crate::util::window().open_with_url_and_target(&u, "_blank");
        }).into());
    }
    let missing = logic::fillable_tracks(r);
    if missing > 0 {
        v.push(MenuItem::new(format!("Fill {missing} missing track{}", if missing == 1 { "" } else { "s" })).icon("download").on(move || fill_release(id)).into());
    }
    v.push(MenuEntry::Sep);
    let (tc, rid) = (r.track_count, r.id);
    v.push(MenuItem::new("Delete album…").icon("trash").danger().on(move || {
        host.delete.set(Some(DeleteReq { release_ids: vec![rid], tracks: Some(tc), on_done: on_deleted }));
    }).into());
    v
}

/// Queue a re-download of the whole record to fill the gaps.
pub fn fill_release(id: i64) {
    spawn_toast(async move {
        let _: FillResult = api::post(&format!("/releases/{id}/fill"), &serde_json::json!({})).await?;
        toast_ok("Queued. Progress is on the Downloads page.");
        crate::data::invalidate_entity("job", &[]);
        Ok(())
    });
}
