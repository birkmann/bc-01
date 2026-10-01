//! Rail tabs "Pool" and "Suggestions": the working pool as a browsable list (search, sort,
//! pre-listen, one-click add, drag into the tracklist) and ranked next-track suggestions.
use std::sync::Arc;

use bc_types::sets::{DjSetDetail, PoolPage, PoolSort, SetSuggestRequest, SetSuggestResponse, SortOrder};
use bc_types::suggest::{SuggestTrack, SuggestionOut};
use leptos::prelude::*;

use super::mutations::SetMut;
use crate::api;
use crate::ds::{Button, Icon, SearchInput, Size, Skeleton, Variant, use_debounced};
use crate::logic::format::format_count;
use crate::player::plan::track_row::{PreviewButton, RowTrack, TrackRowView, VerdictText};
use crate::util::enc;
use crate::widgets::dnd::{self, DragPayload};

const PAGE: i64 = 60;

fn drag_down(ev: &web_sys::PointerEvent, id: i64, label: &str) {
    if let Some(t) = ev.target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()) {
        if t.closest("button,input,a").ok().flatten().is_some() {
            return;
        }
    }
    dnd::begin_drag(ev, DragPayload { kind: "track".into(), ids: vec![id], label: label.to_string(), index: None });
}

#[component]
fn AddButton(id: i64, title: String, m: SetMut) -> impl IntoView {
    view! {
        <button type="button" class="btn btn-ghost btn-sm btn-icon" aria-label=format!("Add {title} to the set") title="Add to the set"
            on:click=move |ev| { ev.stop_propagation(); m.add_items(vec![id], None); }><Icon name="plus" /></button>
    }
}

#[component]
pub fn PoolPanel(m: SetMut, #[prop(into)] used: Signal<std::collections::HashSet<i64>>, #[prop(into)] sources_sig: Signal<String>) -> impl IntoView {
    let needle = RwSignal::new(String::new());
    let q = use_debounced(needle, 250);
    let sort = RwSignal::new("added".to_string());
    let seed = (crate::util::entropy() % 1_000_000) as i64;
    let rows = RwSignal::new(Vec::<SuggestTrack>::new());
    let total = RwSignal::new(0i64);
    let loading = RwSignal::new(false);
    let loaded = RwSignal::new(false);
    let stamp = StoredValue::new(0u64);
    let err = RwSignal::new(None::<String>);
    let id = m.id;

    let load = move |reset: bool| {
        let g = if reset { stamp.update_value(|g| *g += 1); stamp.get_value() } else { stamp.get_value() };
        if !reset && (loading.get_untracked() || rows.with_untracked(|r| r.len() as i64) >= total.get_untracked()) {
            return;
        }
        loading.set(true);
        let offset = if reset { 0 } else { rows.with_untracked(|r| r.len()) as i64 };
        let s = sort.get_untracked();
        let mut url = format!("/sets/{id}/pool?limit={PAGE}&offset={offset}&sort={s}");
        if s == "bpm" {
            url.push_str("&order=asc");
        }
        if s == "random" {
            url.push_str(&format!("&seed={seed}"));
        }
        let qv = q.get_untracked();
        if !qv.trim().is_empty() {
            url.push_str(&format!("&q={}", enc(qv.trim())));
        }
        leptos::task::spawn_local(async move {
            match api::get::<PoolPage>(&url).await {
                Ok(p) => {
                    if stamp.try_get_value() == Some(g) {
                        err.set(None);
                        total.set(p.page.total);
                        if reset { rows.set(p.page.items); } else { rows.update(|r| r.extend(p.page.items)); }
                    }
                }
                Err(e) => { let _ = err.try_set(Some(e.message())); }
            }
            let _ = loading.try_set(false);
            let _ = loaded.try_set(true);
        });
    };
    Effect::new(move |_| {
        q.track();
        sort.track();
        sources_sig.track();
        load(true);
    });
    let _ = (PoolSort::Added, SortOrder::Asc);

    view! {
        <div class="rail-pane">
            <div class="rail-tools">
                <SearchInput value=needle placeholder="Search the pool…" class="grow" />
                <select class="input rail-sort" aria-label="Sort the pool" on:change=move |ev| sort.set(event_target_value(&ev)) prop:value=move || sort.get()>
                    <option value="added">"newest"</option>
                    <option value="bpm">"bpm"</option>
                    <option value="random">"shuffle"</option>
                </select>
            </div>
            <div class="rail-list" on:scroll=move |ev| {
                let el: web_sys::Element = event_target(&ev);
                if (el.scroll_height() - el.scroll_top() - el.client_height()) < 400 { load(false); }
            }>
                {move || {
                    if let Some(e) = err.get() {
                        return view! { <div class="rail-empty">{e}</div> }.into_any();
                    }
                    if !loaded.get() && rows.with(|r| r.is_empty()) {
                        return view! { <div class="rail-skel">{(0..6).map(|_| view! { <Skeleton height="44px" /> }).collect_view()}</div> }.into_any();
                    }
                    if rows.with(|r| r.is_empty()) {
                        let t = if q.get().is_empty() { "The pool is empty. Add sources above." } else { "Nothing in the pool matches." };
                        return view! { <div class="rail-empty">{t}</div> }.into_any();
                    }
                    view! {
                        <For each=move || rows.get() key=|t| t.id let:t>
                            {
                                let rt: RowTrack = (&t).into();
                                let tid = t.id;
                                let label = t.title.clone();
                                let rt2 = rt.clone();
                                let is_used = Signal::derive(move || used.with(|u| u.contains(&tid)));
                                view! {
                                    <div class=move || if is_used.get() { "rail-item used" } else { "rail-item" }
                                        on:pointerdown=move |ev| if !is_used.get_untracked() { drag_down(&ev, tid, &label) }>
                                        <TrackRowView track=rt muted=is_used
                                            trail=crate::ds::children(move || {
                                                let rt2 = rt2.clone();
                                                let t = rt2.title.clone();
                                                view! {
                                                    {move || if is_used.get() {
                                                        view! { <span class="badge">"in set"</span> }.into_any()
                                                    } else {
                                                        view! { <span class="trow-trail"><PreviewButton track=rt2.clone() /><AddButton id=tid title=t.clone() m=m /></span> }.into_any()
                                                    }}
                                                }
                                            }) />
                                    </div>
                                }
                            }
                        </For>
                        {move || loading.get().then(|| view! { <div class="rail-more faint">"Loading…"</div> })}
                    }.into_any()
                }}
            </div>
            <div class="rail-foot mono faint">{move || { let t = total.get(); if t > 0 { format!("{} track{} in the pool", format_count(t), if t == 1 { "" } else { "s" }) } else { String::new() } }}</div>
        </div>
    }
}

#[component]
pub fn SuggestionsPanel(m: SetMut, #[prop(into)] detail: Signal<Option<Arc<DjSetDetail>>>, top: RwSignal<Option<i64>>) -> impl IntoView {
    let use_pool = RwSignal::new(true);
    let items = RwSignal::new(Vec::<SuggestionOut>::new());
    let seed_camelot = RwSignal::new(None::<String>);
    let loading = RwSignal::new(true);
    let err = RwSignal::new(None::<String>);
    let stamp = StoredValue::new(0u64);
    let id = m.id;
    let sig = Memo::new(move |_| detail.with(|d| d.as_ref().map(|d| d.items.iter().map(|i| i.track_id.unwrap_or(0).to_string()).collect::<Vec<_>>().join(",")).unwrap_or_default()));
    let has_pool = Signal::derive(move || detail.with(|d| d.as_ref().map(|d| !d.pool_sources.is_empty()).unwrap_or(false)));
    let tail = Signal::derive(move || detail.with(|d| d.as_ref().and_then(|d| d.items.last().map(|i| i.title.clone()))));
    let n_items = Signal::derive(move || detail.with(|d| d.as_ref().map(|d| d.items.len()).unwrap_or(0)));
    let refresh = Callback::new(move |_: ()| {
        stamp.update_value(|g| *g += 1);
        let g = stamp.get_value();
        loading.set(true);
        let req = SetSuggestRequest { limit: 30, use_pool: use_pool.get_untracked(), ..Default::default() };
        leptos::task::spawn_local(async move {
            match api::post::<_, SetSuggestResponse>(&format!("/sets/{id}/suggest"), &req).await {
                Ok(r) => {
                    if stamp.try_get_value() == Some(g) {
                        err.set(None);
                        seed_camelot.set(r.suggest.seed_camelot.clone());
                        top.set(r.suggest.items.first().map(|i| i.track.id));
                        items.set(r.suggest.items);
                    }
                }
                Err(e) => { let _ = err.try_set(Some(e.message())); }
            }
            let _ = loading.try_set(false);
        });
    });
    Effect::new(move |_| {
        sig.track();
        use_pool.track();
        refresh.run(());
    });
    let degraded = move || n_items.get() > 0 && !loading.get() && seed_camelot.get().is_none();

    view! {
        <div class="rail-pane">
            <div class="rail-head">
                <Icon name="sparkles" size=14 />
                <span class="rail-head-t truncate">{move || tail.get().map(|t| format!("After {t}")).unwrap_or_else(|| "From the pool".into())}</span>
                {move || if has_pool.get() {
                    view! {
                        <button type="button" class=move || if use_pool.get() { "pchip small" } else { "pchip small on" } aria-pressed=move || (!use_pool.get()).to_string()
                            title=move || if use_pool.get() { "Ranking only the pool; click to search the whole library" } else { "Ranking the whole library; click to stay inside the pool" }
                            on:click=move |_| use_pool.update(|u| *u = !*u)>{move || if use_pool.get() { "pool" } else { "library" }}</button>
                    }.into_any()
                } else {
                    view! { <span class="badge" title="No pool sources: ranking the whole library">"library"</span> }.into_any()
                }}
                <Button size=Size::Sm variant=Variant::Ghost icon="refresh" title="Refresh suggestions" busy=loading on_click=move |_| refresh.run(()) />
            </div>
            {move || degraded().then(|| view! {
                <div class="rail-note"><Icon name="alert" size=12 />"Seed has no key: ranked by tempo and tags. "<a href="/analysis">"Analyse"</a></div>
            })}
            <div class=move || if loading.get() && !items.with(|i| i.is_empty()) { "rail-list busy" } else { "rail-list" }>
                {move || {
                    if let Some(e) = err.get() {
                        return view! { <div class="rail-empty">{e}</div> }.into_any();
                    }
                    let list = items.get();
                    if list.is_empty() {
                        if loading.get() {
                            return view! { <div class="rail-skel">{(0..6).map(|_| view! { <Skeleton height="52px" />}).collect_view()}</div> }.into_any();
                        }
                        let msg = if n_items.get() == 0 { "Add a first track from the pool; suggestions start from it.".to_string() }
                            else if use_pool.get() && has_pool.get() { "Nothing compatible left in the pool.".to_string() } else { "Nothing compatible found.".to_string() };
                        return view! {
                            <div class="rail-empty">{msg}
                                {move || (n_items.get() > 0 && use_pool.get() && has_pool.get()).then(|| view! {
                                    <button type="button" class="linkish" on:click=move |_| use_pool.set(false)>" Search the whole library"</button> })}
                            </div>
                        }.into_any();
                    }
                    list.into_iter().map(|s| {
                        let rt: RowTrack = (&s.track).into();
                        let tid = s.track.id;
                        let label = s.track.title.clone();
                        let (rt2, rt3) = (rt.clone(), rt.clone());
                        let why: Vec<String> = s.why.iter().take(2).cloned().collect();
                        let (kv, kr, bv, br) = (s.key_verdict.clone(), s.key_reason.clone(), s.bpm_verdict.clone(), s.bpm_reason.clone());
                        view! {
                            <div class="rail-item" on:pointerdown=move |ev| drag_down(&ev, tid, &label)>
                                <TrackRowView track=rt
                                    below=crate::ds::children(move || {
                                        let (kv, kr, bv, br, why) = (kv.clone(), kr.clone(), bv.clone(), br.clone(), why.clone());
                                        view! {
                                            <span class="trow-why">
                                                <VerdictText verdict=kv reason=kr />
                                                <VerdictText verdict=bv reason=br />
                                                {why.into_iter().map(|w| view! { <span class="faint truncate">{w}</span> }).collect_view()}
                                            </span>
                                        }
                                    })
                                    trail=crate::ds::children(move || {
                                        let (rt2, t) = (rt2.clone(), rt3.title.clone());
                                        view! { <span class="trow-trail"><PreviewButton track=rt2 /><AddButton id=tid title=t m=m /></span> }
                                    }) />
                            </div>
                        }
                    }).collect_view().into_any()
                }}
            </div>
        </div>
    }
}
