//! What is coming, in order, and the means to change it: drag a row to a new place, drop a
//! suggestion or a library track between two rows, remove one, or open its menu to play it now,
//! play it next, or turn it into a wish. The playing track heads the list, fixed.
use bc_types::library::LabelOut;
use bc_types::player::{HORIZON, PlanOp, PlayerCommand, QueueItem, Wish};
use leptos::prelude::*;

use super::track_row::{RowTrack, TrackRowView};
use super::{Planner, fetch_items};
use crate::api;
use crate::ds::popover::Rect;
use crate::ds::{Button, Icon, MenuCtx, MenuEntry, MenuItem, Size, Variant, confirm};
use crate::logic::format::format_duration_ms;
use crate::widgets::dnd::{self, DragPayload, drop_index, reorder_target};

const ROWS: &str = "plan-rows";

fn row_menu(pl: Planner, index: usize, t: QueueItem) -> Vec<MenuEntry> {
    let queue_index = pl.player.state.with_untracked(|s| s.queue_index);
    let p = pl.player;
    let mut v: Vec<MenuEntry> = vec![
        MenuItem::new("Play now").icon("play").on(move || p.cmd(PlayerCommand::JumpTo { index })).into(),
        MenuItem::new("Play next").icon("skip-next").disabled(index as i64 == queue_index + 1)
            .on(move || p.cmd(PlayerCommand::MoveInQueue { from: index, to: (queue_index + 1).max(0) as usize })).into(),
        MenuItem::new("Remove from queue").icon("trash").danger().on(move || p.cmd(PlayerCommand::RemoveAt { index })).into(),
    ];
    if t.is_library() {
        v.push(MenuEntry::Sep);
        v.push(MenuEntry::Label("Wish for".into()));
        if let (Some(id), Some(name)) = (t.artist_id, t.artist.clone()) {
            v.push(MenuItem::new(format!("Artist: {name}")).icon("user").on(move || pl.op(PlanOp::AddWish { wish: Wish::Artist { id, name: name.clone() } })).into());
        }
        if let Some(lid) = t.label_id {
            v.push(MenuItem::new("Same label").icon("folder").on(move || {
                leptos::task::spawn_local(async move {
                    if let Ok(l) = api::get::<LabelOut>(&format!("/labels/{lid}")).await {
                        pl.op(PlanOp::AddWish { wish: Wish::Label { id: l.id, name: l.name } });
                    }
                });
            }).into());
        }
        for tag in t.tags.iter().take(4) {
            let tag = tag.clone();
            v.push(MenuItem::new(format!("Tag: {tag}")).icon("tag").on(move || pl.op(PlanOp::AddWish { wish: Wish::Tag { name: tag.clone() } })).into());
        }
    }
    v
}

#[component]
pub fn UpNext(
    pl: Planner,
    rows: Memo<Vec<(usize, QueueItem)>>,
    offsets: Memo<Vec<i64>>,
    #[prop(into)] hidden: Signal<usize>,
    current: Memo<Option<QueueItem>>,
) -> impl IntoView {
    let player = pl.player;
    let menu = expect_context::<MenuCtx>();
    let over = dnd::over();
    let auto_fill = Signal::derive(move || pl.plan.with(|p| p.auto_fill));
    let mix = Signal::derive(move || player.state.with(|s| s.mix));
    let shuffle = Signal::derive(move || player.state.with(|s| s.shuffle));
    let repeat_one = Signal::derive(move || player.state.with(|s| s.repeat == bc_types::player::RepeatMode::One));
    let max_play_s = Signal::derive(move || player.state.with(|s| s.mix_settings.max_play_s));
    let queue_len = Signal::derive(move || player.state.with(|s| s.queue.len()));
    let queue_index = Signal::derive(move || player.state.with(|s| s.queue_index));
    let upcoming_count = Signal::derive(move || (queue_len.get() as i64 - queue_index.get() - 1).max(0) as usize);

    let clear = move || {
        let from = (queue_index.get_untracked() + 1).max(0) as usize;
        player.cmd(PlayerCommand::RemoveRange { from, to: queue_len.get_untracked() });
    };
    let ask_clear = move |_| {
        let n = upcoming_count.get_untracked();
        if n > 5 {
            leptos::task::spawn_local(async move {
                if confirm("Clear the queue?", &format!("Remove all {n} upcoming tracks. The playing track keeps playing."), "Clear", true).await {
                    clear();
                }
            });
        } else {
            clear();
        }
    };

    // drops: reorder a row, or insert dragged tracks at the slot
    dnd::register_target(ROWS, &["queue-row", "track", "suggestion"], move |p: DragPayload, info| {
        let (first, n) = rows.with_untracked(|r| (r.first().map(|x| x.0).unwrap_or(0), r.len()));
        let local_slot = drop_index(info, n);
        let slot = first + local_slot;
        if p.kind == "queue-row" {
            if let Some(from) = p.index {
                let to = reorder_target(from, slot);
                if to != from {
                    player.cmd(PlayerCommand::MoveInQueue { from, to });
                }
            }
        } else {
            leptos::task::spawn_local(async move {
                let items = fetch_items(&p.ids).await;
                if !items.is_empty() {
                    player.cmd(PlayerCommand::InsertAt { index: slot, items });
                }
            });
        }
    });
    let end_over = move || over.get().map(|(id, _)| id == "plan-tail").unwrap_or(false);
    dnd::register_target("plan-tail", &["queue-row", "track", "suggestion"], move |p: DragPayload, _| {
        let len = queue_len.get_untracked();
        if p.kind == "queue-row" {
            if let Some(from) = p.index {
                if len > 0 {
                    player.cmd(PlayerCommand::MoveInQueue { from, to: len - 1 });
                }
            }
        } else {
            leptos::task::spawn_local(async move {
                let items = fetch_items(&p.ids).await;
                if !items.is_empty() {
                    player.cmd(PlayerCommand::InsertAt { index: len, items });
                }
            });
        }
    });

    view! {
        <section class="pp-sec un" aria-label="Up next">
            <div class="pp-un-head">
                <span class="pp-lbl wide">"Up next "<span class="mono">{move || upcoming_count.get()}</span></span>
                {move || max_play_s.get().map(|m| view! { <span class="badge" title="Tracks are moved on after this long (DJ mix settings)">{format!("≤ {} min each", (m / 60.0 * 10.0).round() / 10.0)}</span> })}
                <span class="spacer"></span>
                <button type="button" class=move || if auto_fill.get() { "pp-link on" } else { "pp-link" } aria-pressed=move || auto_fill.get().to_string()
                    title=move || if auto_fill.get() {
                        format!("Auto-fill is on: while DJ mix is on, the next {HORIZON} tracks are kept filled with suggestions so the set never runs out. Click to turn off.")
                    } else { "Auto-fill is off: the queue ends where it ends. Click to keep it topped up while mixing.".to_string() }
                    on:click=move |_| pl.op(PlanOp::SetAutoFill { on: !auto_fill.get_untracked() })>
                    <Icon name="sparkles" size=11 />"auto-fill"
                </button>
                <Show when=move || { upcoming_count.get() > 0 }>
                    <button type="button" class="pp-link danger-h" on:click=ask_clear>"clear"</button>
                </Show>
            </div>
            <Show when=move || auto_fill.get() && !mix.get()>
                <div class="pp-note"><span class="grow">"Auto-fill runs while DJ mix is on."</span>
                    <button type="button" class="pp-link" on:click=move |_| player.cmd(PlayerCommand::SetMix { on: true })>"turn on"</button></div>
            </Show>
            <Show when=move || shuffle.get() || repeat_one.get()>
                <div class="pp-note warn"><span class="grow">{move || if shuffle.get() { "Shuffle is on: this order will not be followed." } else { "Repeat one is on: nothing advances." }}</span>
                    <button type="button" class="pp-link" on:click=move |_| if shuffle.get_untracked() { player.cmd(PlayerCommand::SetShuffle { on: false }) } else { player.cmd(PlayerCommand::CycleRepeat) }>"turn off"</button></div>
            </Show>
            <div class="pp-un-list">
                {move || current.get().map(|c| {
                    let rt: RowTrack = (&c).into();
                    view! {
                        <TrackRowView track=rt current=true
                            lead=crate::ds::children(|| view! { <span class="pp-pos"><Icon name="play" size=10 /></span> })
                            trail=crate::ds::children(|| view! { <span class="trow-dur faint">"now"</span> }) />
                    }
                })}
                {move || {
                    rows.get().into_iter().enumerate().map(|(i, (index, t))| {
                        let rt: RowTrack = (&t).into();
                        let label = t.title.clone();
                        let t2 = t.clone();
                        let dropc = Signal::derive(move || over.get().and_then(|(id, info)| (id == ROWS && info.index == Some(i)).then_some(info.before)));
                        let title = t.title.clone();
                        view! {
                            <div class=move || match dropc.get() { Some(true) => "pp-row drop-before", Some(false) => "pp-row drop-after", None => "pp-row" }
                                data-dnd-target=ROWS data-dnd-index=i.to_string()
                                on:contextmenu=move |ev| { ev.prevent_default(); menu.open(Rect::point(ev.client_x() as f64, ev.client_y() as f64), row_menu(pl, index, t2.clone())); }
                                on:pointerdown=move |ev| {
                                    if let Some(el) = ev.target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()) {
                                        if el.closest("button:not(.trow),input,a").ok().flatten().is_some() { return; }
                                    }
                                    dnd::begin_drag(&ev, DragPayload { kind: "queue-row".into(), ids: vec![], label: label.clone(), index: Some(index) });
                                }>
                                <TrackRowView track=rt on_click=Callback::new(move |_| player.cmd(PlayerCommand::JumpTo { index }))
                                    lead=crate::ds::children(move || view! { <span class="pp-pos mono">{i + 1}</span> })
                                    trail=crate::ds::children(move || {
                                        let title = title.clone();
                                        view! {
                                            <span class="trow-dur mono faint pp-off-t" title="Starts in">{move || format!("+{}", format_duration_ms(Some(offsets.with(|o| o.get(i).copied().unwrap_or(0)) as f64)))}</span>
                                            <span role="button" tabindex="0" class="pp-x" title="Remove" aria-label=format!("Remove {title} from the queue")
                                                on:click=move |ev| { ev.stop_propagation(); player.cmd(PlayerCommand::RemoveAt { index }); }
                                                on:keydown=move |ev| if ev.key() == "Enter" || ev.key() == " " { ev.prevent_default(); ev.stop_propagation(); player.cmd(PlayerCommand::RemoveAt { index }); }>
                                                <Icon name="x" size=12 />
                                            </span>
                                        }
                                    }) />
                            </div>
                        }
                    }).collect_view()
                }}
                <div class=move || if end_over() { "pp-tail over" } else { "pp-tail" } data-dnd-target="plan-tail">
                    {move || (hidden.get() > 0).then(|| view! { <div class="pp-more faint">{format!("…and {} more in the queue", hidden.get())}</div> })}
                    {move || rows.with(|r| r.is_empty()).then(|| {
                        let msg = if current.get().is_none() { "Play something to start planning." }
                            else if auto_fill.get() && mix.get() { "Filling in what comes next…" }
                            else { "Nothing planned yet. Take a suggestion below." };
                        view! { <div class="pp-empty">{msg}</div> }
                    })}
                </div>
            </div>
        </section>
    }
}

#[allow(dead_code)]
fn _s(_: Size, _: Variant) {}
#[allow(dead_code)]
fn _b() -> impl IntoView {
    view! { <Button icon="x">""</Button> }
}
