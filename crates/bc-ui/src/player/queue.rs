//! Queue button: opens the planner (the "Up next" list lives there). The badge counts the
//! tracks still to come.
use leptos::prelude::*;

use super::store::use_player;
use crate::app::{Panel, use_app};
use crate::ds::Icon;

#[component]
pub fn QueueButton() -> impl IntoView {
    let app = use_app();
    let player = use_player();
    let upcoming = move || player.state.with(|s| (s.queue.len() as i64 - s.queue_index - 1).max(0));
    view! {
        <button type="button" class="btn btn-ghost btn-icon" style="position:relative"
            aria-pressed=move || (app.panel.get() == Some(Panel::Plan)).to_string()
            aria-label=move || format!("Set planner, {} tracks up next", upcoming())
            title="Set planner (q)"
            on:click=move |_| app.toggle_panel(Panel::Plan)>
            <Icon name="queue" />
            {move || (upcoming() > 0).then(|| view! {
                <span class="mono" style="position:absolute;top:0;right:-2px;background:var(--color-surface-4);color:var(--color-ink-muted);border-radius:99px;padding:0 5px;font-size:9px;line-height:14px">
                    {if upcoming() > 99 { "99+".to_string() } else { upcoming().to_string() }}
                </span>
            })}
        </button>
    }
}
