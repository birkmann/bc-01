//! Track selection: resolving a `Selection` to ids and the bulk actions (play, queue,
//! love, add to playlist / set, analyse, delete).
use bc_types::library::*;
use bc_types::analysis::{ScanRequest, ScanResponse, ScanScope};
use bc_types::player::PlayerCommand;
use bc_types::ui::Selection;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::ds::{Button, Size, Variant, confirm, toast_err, toast_info, toast_ok};
use crate::logic::format::format_count;
use crate::logic::selection::SelectionOps;
use crate::pages::albums::host::{PickerReq, ids_of, play_items, use_library_host};
use crate::player::{PlayerCtx, use_player};
use crate::util::qs_pairs;
use crate::widgets::common::queue_item;

/// Ids of a selection, in listing order for `All`.
pub async fn selection_ids(sel: &Selection) -> Result<Vec<i64>, api::ApiErr> {
    match sel {
        Selection::None => Ok(vec![]),
        Selection::Include { ids } => Ok(ids.clone()),
        Selection::All { filter, excluded } => {
            let mut q: TrackQuery = serde_json::from_value(filter.clone()).unwrap_or_default();
            q.offset = None;
            q.limit = None;
            let all: Vec<i64> = api::get(&format!("/tracks/ids{}", qs_pairs(&q.to_pairs()))).await?;
            Ok(all.into_iter().filter(|i| !excluded.contains(i)).collect())
        }
    }
}

/// Tracks (full rows) of ids, in the given order, 500 at a time through `/tracks`.
pub async fn tracks_of(ids: &[i64]) -> Result<Vec<TrackOut>, api::ApiErr> {
    let mut out = vec![];
    for chunk in ids.chunks(25) {
        let res = futures::future::join_all(chunk.iter().map(|id| async move { api::get::<TrackOut>(&format!("/tracks/{id}")).await })).await;
        for r in res {
            out.push(r?);
        }
    }
    Ok(out)
}

pub async fn analyse_ids(ids: Vec<i64>) -> Result<String, api::ApiErr> {
    let r: ScanResponse = api::post("/analysis/scan", &ScanRequest { scope: ScanScope::Ids, track_ids: ids, limit: None, only_missing: true }).await?;
    crate::data::invalidate_entity("job", &[]);
    Ok(if r.queued > 0 { format!("Queued {} for analysis", format_count(r.queued)) } else { "All tracks are already analysed".to_string() })
}

pub async fn love_ids(ids: Vec<i64>, loved: bool) -> Result<i64, api::ApiErr> {
    let r: ChangedOut = api::post("/tracks/love", &SetLoved { track_ids: ids, loved }).await?;
    crate::data::invalidate_entity("track", &[]);
    Ok(r.changed)
}

pub async fn delete_tracks(ids: Vec<i64>) -> usize {
    let mut n = 0;
    for id in ids {
        match api::call("DELETE", &format!("/tracks/{id}")).await {
            Ok(()) => n += 1,
            Err(e) => {
                toast_err(&e.message());
                break;
            }
        }
    }
    crate::data::invalidate_all();
    crate::data::invalidate_prefix("home");
    n
}

pub fn confirm_delete_tracks(ids: Vec<i64>, on_done: Option<Callback<()>>) {
    spawn_local(async move {
        let n = ids.len();
        let ok = confirm(
            &format!("Delete {} track{}?", format_count(n as i64), if n == 1 { "" } else { "s" }),
            "The files are erased from disk. This cannot be undone.",
            "Delete",
            true,
        )
        .await;
        if !ok {
            return;
        }
        let done = delete_tracks(ids).await;
        toast_ok(&format!("Deleted {} track{}", format_count(done as i64), if done == 1 { "" } else { "s" }));
        if let Some(cb) = on_done {
            cb.run(());
        }
    });
}

pub fn queue_ids(player: PlayerCtx, ids: Vec<i64>, next: bool) {
    spawn_local(async move {
        match tracks_of(&ids).await {
            Ok(t) => {
                let items = t.iter().map(queue_item).collect();
                player.cmd(if next { PlayerCommand::PlayNext { items } } else { PlayerCommand::AddToQueue { items } });
                toast_info(&format!("Queued {} track{}", t.len(), if t.len() == 1 { "" } else { "s" }));
            }
            Err(e) => toast_err(&e.message()),
        }
    });
}

/// Bar above the list while rows are selected.
#[component]
pub fn SelectionBar(selection: RwSignal<Selection>, total: RwSignal<Option<usize>>) -> impl IntoView {
    let player = use_player();
    let host = use_library_host();
    let busy = RwSignal::new(false);
    let count = Memo::new(move |_| selection.with(|s| s.count(total.get().unwrap_or(0) as i64)));
    let with_ids = move |f: Box<dyn FnOnce(Vec<i64>) + 'static>| {
        let sel = selection.get_untracked();
        busy.set(true);
        spawn_local(async move {
            match selection_ids(&sel).await {
                Ok(ids) if !ids.is_empty() => f(ids),
                Ok(_) => toast_info("Nothing selected"),
                Err(e) => toast_err(&e.message()),
            }
            busy.set(false);
        });
    };
    let with_ids = std::sync::Arc::new(with_ids);
    let (w1, w2, w3, w4, w5, w6) = (with_ids.clone(), with_ids.clone(), with_ids.clone(), with_ids.clone(), with_ids.clone(), with_ids);
    view! {
        <Show when=move || !selection.with(|s| s.is_empty())>
            <div class="lib-selbar" role="status">
                <span class="mono">{move || format_count(count.get())}</span>
                <span>"selected"</span>
                <span class="spacer"></span>
                {
                    let (w1, w2, w3, w4, w5, w6) = (w1.clone(), w2.clone(), w3.clone(), w4.clone(), w5.clone(), w6.clone());
                    view! {
                        <Button size=Size::Sm icon="play" disabled=busy on_click=move |_| w1(Box::new(move |ids| { spawn_local(async move { match tracks_of(&ids.into_iter().take(500).collect::<Vec<_>>()).await { Ok(t) => play_items(player, &t, 0, None, false), Err(e) => toast_err(&e.message()) } }); }))>"Play"</Button>
                        <Button size=Size::Sm icon="queue" disabled=busy on_click=move |_| w2(Box::new(move |ids| queue_ids(player, ids.into_iter().take(500).collect(), false)))>"Queue"</Button>
                        <Button size=Size::Sm icon="heart" disabled=busy on_click=move |_| w3(Box::new(move |ids| { spawn_local(async move { match love_ids(ids, true).await { Ok(n) => toast_ok(&format!("Loved {}", format_count(n))), Err(e) => toast_err(&e.message()) } }); }))>"Love"</Button>
                        <Button size=Size::Sm icon="list" disabled=busy on_click=move |_| w4(Box::new(move |ids| host.picker.set(Some(PickerReq { ids: ids_of(ids) }))))><span class="hide-sm">"Add to…"</span></Button>
                        <Button size=Size::Sm icon="activity" disabled=busy on_click=move |_| w5(Box::new(move |ids| { spawn_local(async move { match analyse_ids(ids).await { Ok(m) => toast_ok(&m), Err(e) => toast_err(&e.message()) } }); }))><span class="hide-sm">"Analyse"</span></Button>
                        <Button size=Size::Sm variant=Variant::Danger icon="trash" disabled=busy on_click=move |_| w6(Box::new(move |ids| confirm_delete_tracks(ids, Some(Callback::new(move |_| selection.set(Selection::None))))))><span class="hide-sm">"Delete"</span></Button>
                    }
                }
                <Button size=Size::Sm variant=Variant::Ghost on_click=move |_| selection.set(Selection::None)>"Clear"</Button>
            </div>
        </Show>
    }
}
