//! What could come next, after the last planned track (or the playing one when nothing is
//! planned). Every row says why it is here (key, tempo, shared tags, wish) and can go straight
//! into the queue: next, at the end, or dragged to a slot.
use bc_types::player::{PlayerCommand, QueueItem};
use bc_types::suggest::{SuggestResponse, SuggestionOut};
use leptos::prelude::*;

use super::Planner;
use super::suggest_body::{pool_label, suggest_request};
use super::track_row::{RowTrack, TrackRowView, VerdictText};
use crate::api;
use crate::ds::{Button, Icon, Size, Skeleton, Variant};
use crate::widgets::dnd::{self, DragPayload};

#[component]
pub fn Suggestions(pl: Planner, seed: Memo<Option<QueueItem>>, exclude: Memo<Vec<i64>>) -> impl IntoView {
    let player = pl.player;
    let items = RwSignal::new(Vec::<SuggestionOut>::new());
    let target_bpm = RwSignal::new(None::<f64>);
    let loading = RwSignal::new(false);
    let loaded = RwSignal::new(false);
    let err = RwSignal::new(false);
    let stamp = StoredValue::new(0u64);
    let last_key = StoredValue::new(String::new());
    let timer = StoredValue::new(None::<i32>);

    let body = Memo::new(move |_| {
        let s = seed.get()?;
        Some(suggest_request(&s, exclude.get(), &pl.plan.get(), 20))
    });
    let fetch = move || {
        // From a debounce timer: the panel may be closed by now.
        let Some(b) = body.try_get_untracked() else { return };
        let Some(b) = b else {
            items.set(vec![]);
            loaded.set(true);
            return;
        };
        stamp.update_value(|s| *s += 1);
        let Some(g) = stamp.try_get_value() else { return };
        loading.set(true);
        leptos::task::spawn_local(async move {
            match api::post::<_, SuggestResponse>("/suggest/next", &b).await {
                Ok(r) => {
                    if stamp.try_get_value() == Some(g) {
                        err.set(false);
                        target_bpm.set(r.target_bpm);
                        items.set(r.items);
                    }
                }
                Err(_) => {
                    let _ = err.try_set(true);
                }
            }
            let _ = loading.try_set(false);
            let _ = loaded.try_set(true);
        });
    };
    // follow the plan; a short debounce so a burst of ops asks once
    Effect::new(move |_| {
        let b = body.get();
        let key = serde_json::to_string(&b).unwrap_or_default();
        if key == last_key.get_value() {
            return;
        }
        last_key.set_value(key);
        use wasm_bindgen::JsCast;
        let w = crate::util::window();
        if let Some(t) = timer.get_value() {
            w.clear_timeout_with_handle(t);
        }
        let cb = wasm_bindgen::closure::Closure::once_into_js(move || fetch());
        timer.set_value(w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), 150).ok());
    });

    let pool_name = Signal::derive(move || pl.plan.with(|p| p.pools.first().filter(|p| !matches!(p, bc_types::player::Pool::Library)).map(pool_label)));
    let harmonic_on = Signal::derive(move || pl.plan.with(|p| p.harmonic != bc_types::suggest::Harmonic::Off));
    let seed_has_analysis = Signal::derive(move || seed.with(|s| s.as_ref().map(|s| s.bpm.is_some() || s.camelot.is_some()).unwrap_or(false)));

    view! {
        <section class="pp-sec" aria-label="Suggestions">
            <div class="pp-un-head">
                <Icon name="sparkles" size=13 />
                <span class="pp-lbl wide truncate grow">
                    "Could come next"
                    {move || seed.get().map(|s| view! { <span class="faint">{format!(" · after {}", s.title)}</span> })}
                    {move || pool_name.get().map(|n| view! { <span class="faint">{format!(" · from {n}")}</span> })}
                </span>
                {move || target_bpm.get().map(|b| view! { <span class="mono faint" title="Target tempo">{format!("→ {}", b.round() as i64)}</span> })}
                <Button size=Size::Sm variant=Variant::Ghost icon="refresh" title="Refresh suggestions" busy=loading
                    disabled=Signal::derive(move || body.with(|b| b.is_none())) on_click=move |_| fetch() />
            </div>
            <div class=move || if loading.get() && !items.with(|i| i.is_empty()) { "pp-sug busy" } else { "pp-sug" }>
                {move || {
                    if seed.with(|s| s.is_none()) {
                        return view! { <div class="pp-empty">"Play something to get suggestions."</div> }.into_any();
                    }
                    let list = items.get();
                    if list.is_empty() {
                        if !loaded.get() || loading.get() {
                            return view! { <div class="pp-skel">{(0..4).map(|_| view! { <Skeleton height="48px" /> }).collect_view()}</div> }.into_any();
                        }
                        if err.get() {
                            return view! { <div class="pp-empty bad">"Suggestions are unavailable right now."</div> }.into_any();
                        }
                        return view! {
                            <div class="pp-empty">
                                {if harmonic_on.get() && seed_has_analysis.get() { view! { "Nothing mixes out of this under these rules. Loosen keys or tempo." }.into_any() }
                                 else if !seed_has_analysis.get() { view! { "This track has no analysis yet, so only tags and wishes can steer. "<a href="/analysis">"Run analysis"</a>"." }.into_any() }
                                 else { view! { "Nothing matches. Widen the tags or wishes." }.into_any() }}
                            </div>
                        }.into_any();
                    }
                    list.into_iter().map(|s| {
                        let rt: RowTrack = (&s.track).into();
                        let tid = s.track.id;
                        let label = s.track.title.clone();
                        let q = rt.queue_item();
                        let (q1, q2) = (q.clone(), q);
                        let showkey = s.key_reason != "key unknown";
                        let showbpm = s.bpm_reason != "tempo unknown";
                        let rest: Vec<String> = s.why.iter().filter(|w| **w != s.key_reason && **w != s.bpm_reason).take(2).cloned().collect();
                        let (kv, kr, bv, br) = (s.key_verdict.clone(), s.key_reason.clone(), s.bpm_verdict.clone(), s.bpm_reason.clone());
                        let (t1, t2) = (rt.title.clone(), rt.title.clone());
                        view! {
                            <div class="pp-row" on:pointerdown=move |ev| {
                                if let Some(el) = ev.target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()) {
                                    if el.closest("button:not(.trow),input,a").ok().flatten().is_some() { return; }
                                }
                                dnd::begin_drag(&ev, DragPayload { kind: "suggestion".into(), ids: vec![tid], label: label.clone(), index: None });
                            }>
                                <TrackRowView track=rt
                                    below=crate::ds::children(move || {
                                        let (kv, kr, bv, br, rest) = (kv.clone(), kr.clone(), bv.clone(), br.clone(), rest.clone());
                                        view! {
                                            <span class="trow-why">
                                                {showkey.then(|| view! { <VerdictText verdict=kv reason=kr /> })}
                                                {showbpm.then(|| view! { <VerdictText verdict=bv reason=br /> })}
                                                {rest.into_iter().map(|w| view! { <span class="faint truncate">{w}</span> }).collect_view()}
                                            </span>
                                        }
                                    })
                                    trail=crate::ds::children(move || {
                                        let (q1, q2, t1, t2) = (q1.clone(), q2.clone(), t1.clone(), t2.clone());
                                        view! {
                                            <span class="trow-trail">
                                                <button type="button" class="btn btn-ghost btn-sm btn-icon" title="Play next" aria-label=format!("Play {t1} next")
                                                    on:click=move |_| player.cmd(PlayerCommand::PlayNext { items: vec![q1.clone()] })><Icon name="skip-next" size=13 /></button>
                                                <button type="button" class="btn btn-ghost btn-sm btn-icon" title="Add to end" aria-label=format!("Add {t2} to the end")
                                                    on:click=move |_| player.cmd(PlayerCommand::AddToQueue { items: vec![q2.clone()] })><Icon name="queue" size=13 /></button>
                                            </span>
                                        }
                                    }) />
                            </div>
                        }
                    }).collect_view().into_any()
                }}
            </div>
        </section>
    }
}
