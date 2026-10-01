//! The set clock: how long the set is, how much is left, and how much of what is left the queue
//! already covers. "To fill" is the number a DJ plans against.
use bc_types::player::PlanOp;
use leptos::prelude::*;

use super::Planner;
use super::timing::remaining_ms;
use crate::ds::{Button, Icon, Size, Variant};
use crate::logic::format::format_duration_ms;

const PRESETS: [f64; 4] = [60.0, 90.0, 120.0, 180.0];

fn fmt(ms: f64) -> String {
    format_duration_ms(Some(ms.max(0.0)))
}

#[component]
pub fn SetClock(pl: Planner, #[prop(into)] planned: Signal<i64>) -> impl IntoView {
    let length = Signal::derive(move || pl.plan.with(|p| p.set_length_min));
    let started = Signal::derive(move || pl.plan.with(|p| p.set_started_at_ms));
    let running = Signal::derive(move || started.get().is_some());
    let now = RwSignal::new(crate::util::unix_ms());
    // tick once a second while the set runs
    let timer = StoredValue::new(None::<i32>);
    Effect::new(move |_| {
        use wasm_bindgen::JsCast;
        let w = crate::util::window();
        if let Some(t) = timer.get_value() {
            w.clear_timeout_with_handle(t);
            w.clear_interval_with_handle(t);
            timer.set_value(None);
        }
        if running.get() {
            now.set(crate::util::unix_ms());
            let cb = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
                let _ = now.try_set(crate::util::unix_ms());
            });
            let id = w.set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 1000).ok();
            cb.forget();
            timer.set_value(id);
        }
    });
    on_cleanup(move || {
        if let Some(t) = timer.get_value() {
            crate::util::window().clear_interval_with_handle(t);
        }
    });
    let remaining = Signal::derive(move || remaining_ms(started.get(), length.get(), now.get()));
    let to_fill = Signal::derive(move || remaining.get().map(|r| r - planned.get() as f64));
    let ends_at = Signal::derive(move || match (started.get(), length.get()) {
        (Some(s), Some(l)) => {
            let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(s as f64 + l * 60_000.0));
            Some(format!("{:02}:{:02}", d.get_hours(), d.get_minutes()))
        }
        _ => None,
    });
    let set_len = move |m: Option<f64>| pl.op(PlanOp::SetSetLength { minutes: m });

    view! {
        <section class="pp-sec" aria-label="Set clock">
            <div class="pp-row">
                <Icon name="clock" size=14 />
                <label class="pp-inline">
                    <span class="muted">"Set"</span>
                    <input class="input pp-num mono" type="number" min="1" max="720" inputmode="numeric" placeholder="min" aria-label="Set length in minutes"
                        disabled=move || running.get()
                        prop:value=move || length.get().map(|l| format!("{l}")).unwrap_or_default()
                        on:change=move |ev| {
                            let v = event_target_value(&ev).trim().parse::<f64>().ok().filter(|v| *v > 0.0);
                            set_len(v.map(|v| v.round()));
                        } />
                    <span class="muted">"min"</span>
                </label>
                <Show when=move || !running.get()>
                    <span class="pp-presets">
                        {PRESETS.into_iter().map(|m| view! {
                            <button type="button" class="pp-preset" aria-pressed=move || (length.get() == Some(m)).to_string() on:click=move |_| set_len(Some(m))>{format!("{m}")}</button>
                        }).collect_view()}
                    </span>
                </Show>
                <span class="spacer"></span>
                {move || if running.get() {
                    view! { <Button size=Size::Sm variant=Variant::Outline icon="pause" on_click=move |_| pl.op(PlanOp::StopSet)>"Stop"</Button> }.into_any()
                } else {
                    view! { <Button size=Size::Sm variant=Variant::Primary icon="play" disabled=Signal::derive(move || length.get().is_none()) on_click=move |_| pl.op(PlanOp::StartSet)>"Start"</Button> }.into_any()
                }}
            </div>
            <div class="pp-stats mono">
                <div><span class="k">"Planned"</span><span class="v">{move || fmt(planned.get() as f64)}</span></div>
                <div><span class="k">"Remaining"</span>
                    <span class=move || if remaining.get().map(|r| r < 0.0).unwrap_or(false) { "v warn" } else { "v" }>
                        {move || match remaining.get() { None => "—".to_string(), Some(r) if r < 0.0 => format!("-{}", fmt(-r)), Some(r) => fmt(r) }}
                    </span></div>
                <div><span class="k">"To fill"</span>
                    <span class=move || if to_fill.get().map(|r| r < 0.0).unwrap_or(false) { "v warn" } else { "v" }>
                        {move || match to_fill.get() { None => "—".to_string(), Some(r) if r < 0.0 => format!("+{} over", fmt(-r)), Some(r) => fmt(r) }}
                    </span></div>
            </div>
            {move || ends_at.get().map(|t| view! { <div class="pp-ends faint">"ends at "<span class="mono">{t}</span></div> })}
        </section>
    }
}
