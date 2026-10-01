//! A slot of the plan: position, art, title/artist (+ tempo and cue info), BPM and key chips,
//! start time and actions; and the transition gutter between two slots.
use bc_types::sets::{SetItemOut, TransitionOut};
use leptos::prelude::*;
use serde_json::json;

use super::mutations::SetMut;
use crate::ds::Icon;
use crate::logic::format::format_duration_ms;
use crate::player::plan::track_row::{PreviewButton, RowTrack, VerdictChip};
use crate::widgets::common::Art;
use crate::widgets::dnd::{self, DragPayload};

pub const ROWS: &str = "set-rows";

/// The join between two rows: cut/blend toggle and key/tempo verdicts. Overlap derives from the
/// incoming item's `transition_beats` server side, so "cut" clears the beats and "blend" sets
/// them. Finer control lives in Arrange.
#[component]
pub fn TransitionGutter(t: TransitionOut, incoming: SetItemOut, m: SetMut) -> impl IntoView {
    let blending = t.overlap_ms > 0;
    let label = if blending {
        format!("{} · {}s", incoming.transition_type.clone().unwrap_or_else(|| "blend".into()), (t.overlap_ms as f64 / 1000.0).round())
    } else {
        "cut".to_string()
    };
    let (iid, beats) = (incoming.id, incoming.transition_beats);
    view! {
        <div class="gutter">
            <button type="button" class="gutter-toggle" aria-pressed=blending.to_string()
                title="Toggle cut/blend (fine-tune in Arrange)"
                on:click=move |_| m.patch_item(iid, if blending {
                    json!({ "transition_type": "cut", "transition_beats": null })
                } else {
                    json!({ "transition_type": "blend", "transition_beats": beats.unwrap_or(32) })
                })>{label}</button>
            <span class="gutter-k">"key"<VerdictChip verdict=t.key_verdict reason=t.key_reason.clone() /></span>
            <span class="gutter-k">"tempo"<VerdictChip verdict=t.bpm_verdict reason=t.bpm_reason.clone() /></span>
            {(t.tempo_delta_pct != 0.0).then(|| view! {
                <span class="mono faint">{format!("{}{:.1}%", if t.tempo_delta_pct > 0.0 { "+" } else { "" }, t.tempo_delta_pct)}</span>
            })}
        </div>
    }
}

#[component]
pub fn SetTrackRow(item: SetItemOut, m: SetMut, #[prop(into)] playing: Signal<bool>) -> impl IntoView {
    let over = dnd::over();
    let idx = item.index;
    let iid = item.id;
    let tid = item.track_id;
    let rt: RowTrack = (&item).into();
    let label = item.title.clone();
    let tempo = (item.tempo_adjust_pct != 0.0).then(|| format!("{}{}%", if item.tempo_adjust_pct > 0.0 { "+" } else { "" }, item.tempo_adjust_pct));
    let cues = (item.cue_in_ms.is_some() || item.cue_out_ms.is_some()).then(|| {
        format!(
            "in {}{}",
            format_duration_ms(Some(item.cue_in_ms.unwrap_or(0) as f64)),
            item.cue_out_ms.map(|o| format!(" · out {}", format_duration_ms(Some(o as f64)))).unwrap_or_default()
        )
    });
    let key_lock = item.key_lock;
    let missing = item.missing;
    view! {
        <div class=move || {
                let mut c = String::from("srow");
                if playing.get() { c.push_str(" playing"); }
                if missing { c.push_str(" missing"); }
                if let Some((tgt, info)) = over.get() {
                    if tgt == ROWS && info.index == Some(idx) { c.push_str(if info.before { " drop-before" } else { " drop-after" }); }
                }
                c
            }
            data-dnd-target=ROWS data-dnd-index=idx.to_string() tabindex="0"
            on:pointerdown=move |ev| {
                if let Some(t) = ev.target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()) {
                    if t.closest("button,input,a").ok().flatten().is_some() { return; }
                }
                dnd::begin_drag(&ev, DragPayload { kind: "set-item".into(), ids: vec![iid], label: label.clone(), index: Some(idx) });
            }
            on:keydown=move |ev| {
                if !ev.alt_key() { return; }
                match ev.key().as_str() {
                    "ArrowUp" => { ev.prevent_default(); m.move_item(iid, idx.saturating_sub(1)); }
                    "ArrowDown" => { ev.prevent_default(); m.move_item(iid, idx + 1); }
                    _ => {}
                }
            }>
            <span class="srow-n mono">{idx + 1}</span>
            <Art src=item.art_url.clone() size=36.0 />
            <div class="srow-main">
                <div class="srow-title truncate">{if item.title.is_empty() { "(missing track)".to_string() } else { item.title.clone() }}</div>
                <div class="srow-sub truncate">
                    {item.artist.clone()}
                    {tempo.map(|t| view! { <span class="mono">{format!(" · {t}")}</span> })}
                    {cues.map(|c| view! { <span class="mono faint">{format!(" · {c}")}</span> })}
                </div>
            </div>
            <div class="srow-chips">
                <span class="trow-chip mono" title=item.bpm.map(|b| format!("file {b:.1} BPM")).unwrap_or_else(|| "no tempo analysis".into())>
                    {item.effective_bpm.map(|b| format!("{}", b.round() as i64)).unwrap_or_else(|| "—".into())}
                </span>
                <span class=if key_lock { "trow-chip mono" } else { "trow-chip mono unlocked" }
                    title=if key_lock { "key lock on" } else { "key follows pitch" }>
                    {item.effective_camelot.clone().unwrap_or_else(|| "—".into())}
                </span>
                <span class="srow-start mono faint">{format_duration_ms(Some(item.start_ms as f64))}</span>
            </div>
            <div class="srow-actions">
                {tid.map(|_| view! { <PreviewButton track=rt.clone() /> })}
                <button type="button" class="btn btn-ghost btn-sm btn-icon touch-only" aria-label="Move up" on:click=move |_| m.move_item(iid, idx.saturating_sub(1))><Icon name="arrow-up" /></button>
                <button type="button" class="btn btn-ghost btn-sm btn-icon touch-only" aria-label="Move down" on:click=move |_| m.move_item(iid, idx + 1)><Icon name="arrow-down" /></button>
                <button type="button" class=if key_lock { "btn btn-ghost btn-sm is-on srow-key" } else { "btn btn-ghost btn-sm srow-key" }
                    aria-pressed=key_lock.to_string() title=if key_lock { "Key lock on" } else { "Key lock off" }
                    on:click=move |_| m.patch_item(iid, json!({ "key_lock": !key_lock }))>
                    <Icon name=if key_lock { "lock" } else { "eye-off" } size=12 />"KEY"
                </button>
                <button type="button" class="btn btn-ghost btn-sm btn-icon" aria-label="Remove from set" title="Remove from set" on:click=move |_| m.remove_item(iid)><Icon name="x" /></button>
            </div>
        </div>
    }
}
