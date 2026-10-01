//! Which signals count. Each chip reweights the ranking live rather than filtering: one chip
//! alone becomes the whole score, so "only Label" is "same label only".
use leptos::prelude::*;

use super::{PAGE, SIGNAL_KEYS, SimilarCtx, signal_get, signal_toggle};

fn label(k: &str) -> &'static str {
    match k {
        "tags" => "Tags",
        "artist" => "Artist",
        "label" => "Label",
        "tempo" => "Tempo",
        "key" => "Key",
        _ => "Loved",
    }
}
fn hint(k: &str) -> &'static str {
    match k {
        "tags" => "Genre and style tags, weighted so a rare tag counts for far more than a common one",
        "artist" => "Tracks by the same artist",
        "label" => "Tracks on the same label",
        "tempo" => "Similar BPM (half and double time count) and energy",
        "key" => "Related musical key",
        _ => "Nudge tracks you have loved up the list",
    }
}

#[component]
pub(crate) fn SignalChips(ctx: SimilarCtx) -> impl IntoView {
    let all_on = Signal::derive(move || ctx.signals.with(|s| SIGNAL_KEYS.iter().all(|k| signal_get(s, k))));
    view! {
        <section class="pp-sec" aria-label="Match on">
            <div class="pp-un-head">
                <span class="pp-lbl wide">"Match on"</span>
                <span class="spacer"></span>
                <Show when=move || !all_on.get()>
                    <button type="button" class="pp-link" on:click=move |_| { ctx.signals.set(Default::default()); ctx.limit.set(PAGE); }>"Reset"</button>
                </Show>
            </div>
            <div class="pp-chips">
                {SIGNAL_KEYS.into_iter().map(|k| view! {
                    <button type="button" class=move || if ctx.signals.with(|s| signal_get(s, k)) { "pp-chip on" } else { "pp-chip" }
                        aria-pressed=move || ctx.signals.with(|s| signal_get(s, k)).to_string() title=hint(k)
                        on:click=move |_| { ctx.signals.update(|s| signal_toggle(s, k)); ctx.limit.set(PAGE); }>{label(k)}</button>
                }).collect_view()}
            </div>
        </section>
    }
}
