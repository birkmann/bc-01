//! The live set planner (key `q`), beside the page: set clock, direction, wishes, tag rules and
//! presets, the pool chain, the next twenty tracks in order and what could follow them.
//! State lives server-side in the player session (`PlayerState.plan`); every change is a
//! `PlayerCommand::Plan { op }`.
mod clock;
mod direction;
mod pools;
pub mod qh;
pub mod suggest_body;
mod suggestions;
mod tag_picker;
mod tag_rules;
mod timing;
pub mod track_row;
mod up_next;
mod wishes;

use bc_types::library::TrackOut;
use bc_types::player::{HORIZON, PlanOp, PlanState, PlayerCommand, QueueItem};
use leptos::prelude::*;

use crate::player::{PlayerCtx, use_player};
use crate::widgets::common::queue_item;
use crate::widgets::dnd::{self, DragPayload};

/// Shared handle of the planner sections.
#[derive(Clone, Copy)]
pub(crate) struct Planner {
    pub player: PlayerCtx,
    pub plan: Memo<PlanState>,
}

impl Planner {
    pub fn op(&self, op: PlanOp) {
        self.player.cmd(PlayerCommand::Plan { op });
    }
}

/// Library tracks by id -> queue items (drops from lists carry ids only).
pub(crate) async fn fetch_items(ids: &[i64]) -> Vec<QueueItem> {
    let mut out = vec![];
    for id in ids {
        if let Ok(t) = crate::api::get::<TrackOut>(&format!("/tracks/{id}")).await {
            out.push(queue_item(&t));
        }
    }
    out
}

#[component]
pub fn PlanPanel() -> impl IntoView {
    let player = use_player();
    let plan = Memo::new(move |_| player.state.with(|s| s.plan.clone()));
    let pl = Planner { player, plan };

    let queue_index = Signal::derive(move || player.state.with(|s| s.queue_index));
    let current = Memo::new(move |_| player.state.with(|s| s.current.clone()));
    let rows = Memo::new(move |_| {
        player.state.with(|s| timing::upcoming(&s.queue, s.queue_index, HORIZON).into_iter().map(|(i, t)| (i, t.clone())).collect::<Vec<_>>())
    });
    let queue_len = Signal::derive(move || player.state.with(|s| s.queue.len()));
    let max_play_s = Signal::derive(move || player.state.with(|s| s.mix_settings.max_play_s));
    // whole seconds left of the playing track: the clock ticks at 30 Hz, the panel need not
    let entries = Memo::new(move |_| player.state.with(|s| s.entry_points.clone()));
    let remaining_ms = Memo::new(move |_| {
        // the playing track is moved on at its own exit point when the engine mixes it out
        let out = player.clock.with(|c| c.track_uid).and_then(|u| entries.with(|e| e.iter().find(|x| x.uid == u).and_then(|x| x.out_s)));
        player.clock.with(|c| {
            let end = out.map(|o| o.min(c.duration_s)).unwrap_or(c.duration_s);
            ((end - c.position_s).max(0.0).floor() as i64) * 1000
        })
    });
    let lengths = Memo::new(move |_| {
        let mp = max_play_s.get();
        let ep = entries.get();
        rows.with(|r| {
            r.iter()
                .map(|(_, t)| match ep.iter().find(|e| e.uid == t.uid && t.uid != 0) {
                    Some(e) => timing::entry_length_ms(t.duration_ms, e.drop_s, e.out_s, mp),
                    None => timing::effective_length_ms(t.duration_ms, 0.0, mp),
                })
                .collect::<Vec<_>>()
        })
    });
    let offsets = Memo::new(move |_| timing::plays_at_offsets(remaining_ms.get(), &lengths.get()));
    let planned = Signal::derive(move || timing::planned_ms(remaining_ms.get(), &lengths.get()));
    let hidden = Signal::derive(move || (queue_len.get() as i64 - queue_index.get() - 1 - rows.with(|r| r.len()) as i64).max(0) as usize);

    // Suggestions follow the plan: after the last planned row, or the playing track.
    let seed = Memo::new(move |_| rows.with(|r| r.last().map(|(_, t)| t.clone())).or_else(|| current.get()));
    let exclude = Memo::new(move |_| {
        player.state.with(|s| {
            let up: Vec<&QueueItem> = timing::upcoming(&s.queue, s.queue_index, HORIZON).into_iter().map(|(_, t)| t).collect();
            suggest_body::exclude_ids(s.current.as_ref(), &up, &s.history, &s.queue)
        })
    });

    // Tracks dropped anywhere on the panel that is not a queue row go on the end.
    dnd::register_target("plan-panel", &["track", "suggestion"], move |p: DragPayload, _| {
        leptos::task::spawn_local(async move {
            let items = fetch_items(&p.ids).await;
            if !items.is_empty() {
                player.cmd(PlayerCommand::AddToQueue { items });
            }
        });
    });
    // Escape closes the panel (a menu or dialog above owns it first; a filled input clears itself)
    let app = crate::app::use_app();
    let handle = window_event_listener(leptos::ev::keydown, move |e| {
        if e.key() != "Escape" || crate::util::document().query_selector("[role='menu'],[role='dialog']").ok().flatten().is_some() {
            return;
        }
        let in_filled_input = e.target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlInputElement>(t).ok()).map(|i| !i.value().is_empty()).unwrap_or(false);
        if in_filled_input {
            return;
        }
        app.panel.set(None);
    });
    on_cleanup(move || handle.remove());
    let over = dnd::over();
    let is_over = move || over.get().map(|(id, _)| id == "plan-panel").unwrap_or(false);

    view! {
        <div class=move || if is_over() { "rp-body plan-panel over" } else { "rp-body plan-panel" } data-dnd-target="plan-panel">
            <clock::SetClock pl=pl planned=planned />
            <pools::PoolBar pl=pl />
            <direction::DirectionBar pl=pl />
            <wishes::WishBar pl=pl />
            <up_next::UpNext pl=pl rows=rows offsets=offsets hidden=hidden current=current />
            <suggestions::Suggestions pl=pl seed=seed exclude=exclude />
        </div>
    }
}
