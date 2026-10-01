//! The Plan view of a DJ set: the reorderable tracklist with transition gutters on the left, the
//! Pool | Suggestions rail (with the pool builder above) on the right. An empty set shows a hero:
//! the first decision of planning is where to plan from.
use std::sync::Arc;

use bc_types::sets::{DjSetDetail, PoolPage};
use leptos::prelude::*;

use super::automix::AutomixButton;
use super::mutations::SetMut;
use super::pool_builder::PoolBuilder;
use super::pool_panel::{PoolPanel, SuggestionsPanel};
use super::track_row::{ROWS, SetTrackRow, TransitionGutter};
use crate::ds::tabs::TabDef;
use crate::ds::{Button, Icon, Size, Tabs, Variant};
use crate::logic::format::format_count;
use crate::player::use_player;
use crate::widgets::dnd::{self, DragPayload, drop_index, reorder_target};

const LIST: &str = "set-list";

#[component]
pub fn PlanView(m: SetMut, #[prop(into)] detail: Signal<Option<Arc<DjSetDetail>>>, #[prop(into)] pool_meta: Signal<Option<Arc<PoolPage>>>) -> impl IntoView {
    let player = use_player();
    let n_items = Signal::derive(move || detail.with(|d| d.as_ref().map(|d| d.items.len()).unwrap_or(0)));
    let pool_total = Signal::derive(move || pool_meta.with(|p| p.as_ref().map(|p| p.page.total).unwrap_or(0)));
    let automix_max = Signal::derive(move || pool_meta.with(|p| p.as_ref().map(|p| p.automix_max).unwrap_or(200)));
    let tab = RwSignal::new(if n_items.get_untracked() > 0 { "suggest".to_string() } else { "pool".to_string() });
    let is_empty = Signal::derive(move || detail.with(|d| d.as_ref().map(|d| d.items.is_empty()).unwrap_or(true)));
    let pool_sources = Signal::derive(move || detail.with(|d| d.as_ref().map(|d| d.pool_sources.clone()).unwrap_or_default()));
    let used = Signal::derive(move || {
        detail.with(|d| d.as_ref().map(|d| d.items.iter().filter_map(|i| i.track_id).collect::<std::collections::HashSet<_>>()).unwrap_or_default())
    });
    let sources_sig = Signal::derive(move || {
        detail.with(|d| d.as_ref().map(|d| d.pool_sources.iter().map(super::pool_builder::source_key).collect::<Vec<_>>().join("|")).unwrap_or_default())
    });
    let tabs = Signal::derive(move || {
        vec![
            TabDef::new("pool", "Pool").count(pool_total.get()),
            TabDef::new("suggest", "Suggestions"),
        ]
    });

    // --- drops: reorder a slot / insert dragged tracks ---------------------------------
    dnd::register_target(ROWS, &["set-item", "track"], move |payload: DragPayload, info| {
        let Some(d) = detail.get_untracked() else { return };
        let len = d.items.len();
        let slot = drop_index(info, len);
        if payload.kind == "set-item" {
            if let Some(from) = payload.index {
                let to = reorder_target(from, slot);
                if to != from {
                    if let Some(it) = d.items.get(from) {
                        m.move_item(it.id, to);
                    }
                }
            }
        } else {
            m.add_items(payload.ids, Some(slot as i64));
        }
    });
    dnd::register_target(LIST, &["track"], move |payload: DragPayload, _| m.add_items(payload.ids, None));
    let over = dnd::over();
    let list_over = move || over.get().map(|(id, _)| id == LIST || id == ROWS).unwrap_or(false);

    let current_id = Signal::derive(move || player.current_track_id());
    // the top-ranked suggestion: Enter takes it
    let top = RwSignal::new(None::<i64>);
    let focus_pool_search = move || {
        tab.set("pool".into());
        crate::util::raf(|| {
            if let Some(el) = crate::util::document().query_selector("[data-pool-search] input, .rail-tools input").ok().flatten() {
                let _ = wasm_bindgen::JsCast::unchecked_into::<web_sys::HtmlElement>(el).focus();
            }
        });
    };

    // Page-scoped keys (the shell owns space/arrows/q/s): "/" pool search, "p" audition the tail.
    let handle = window_event_listener(leptos::ev::keydown, move |e| {
        let typing = e.target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()).map(|t| {
            let tag = t.tag_name();
            tag == "INPUT" || tag == "TEXTAREA" || tag == "SELECT" || t.get_attribute("contenteditable").is_some()
        }).unwrap_or(false);
        if typing || e.meta_key() || e.ctrl_key() || e.alt_key() {
            return;
        }
        match e.key().as_str() {
            "/" => { e.prevent_default(); focus_pool_search(); }
            "Enter" => {
                if tab.get_untracked() == "suggest" {
                    if let Some(id) = top.get_untracked() {
                        m.add_items(vec![id], None);
                    }
                }
            }
            "p" => {
                if let Some(d) = detail.get_untracked() {
                    if let Some(tail) = d.items.last().filter(|i| i.track_id.is_some()) {
                        let rt: crate::player::plan::track_row::RowTrack = tail.into();
                        let on = player.state.with_untracked(|s| s.preview.playing && s.preview.track_id == Some(rt.id));
                        player.cmd(if on { bc_types::player::PlayerCommand::PreviewStop } else { bc_types::player::PlayerCommand::PreviewStart { item: rt.queue_item(), at_s: None } });
                    }
                }
            }
            _ => {}
        }
    });
    on_cleanup(move || handle.remove());

    let undo_dismissed = RwSignal::new(false);
    Effect::new(move |_| {
        if m.automix_busy.get() {
            undo_dismissed.set(false);
        }
    });
    let show_undo = move || m.undo.with(|u| u.is_some()) && !m.automix_busy.get() && !undo_dismissed.get();

    view! {
        <div class="plan">
            <section class=move || if list_over() { "plan-list over" } else { "plan-list" } data-dnd-target=LIST aria-label="Tracklist">
                {move || show_undo().then(|| view! {
                    <div class="undo-bar" role="status">
                        <span class="grow truncate muted">"Automix arranged the set."</span>
                        <Button size=Size::Sm variant=Variant::Outline icon="refresh" busy=m.undo_busy on_click=move |_| m.undo_automix()>"Undo"</Button>
                        <Button size=Size::Sm variant=Variant::Ghost icon="x" title="Dismiss" on_click=move |_| undo_dismissed.set(true) />
                    </div>
                })}
                <Show when=move || is_empty.get() fallback=move || view! {
                    <div class="plan-rows">
                        {move || {
                            let Some(d) = detail.get() else { return ().into_any() };
                            let items = d.items.clone();
                            let trans = d.transitions.clone();
                            items.into_iter().enumerate().map(|(i, it)| {
                                let gutter = (i > 0).then(|| trans.get(i - 1).cloned()).flatten().map(|t| view! { <TransitionGutter t=t incoming=it.clone() m=m /> });
                                let tid = it.track_id;
                                let playing = Signal::derive(move || tid.is_some() && current_id.get() == tid);
                                view! { {gutter}<SetTrackRow item=it m=m playing=playing /> }
                            }).collect_view().into_any()
                        }}
                    </div>
                }>
                    <div class="plan-hero">
                        <Icon name="sliders" />
                        <div>
                            <div class="plan-hero-t">"Build a pool to plan from"</div>
                            <div class="muted plan-hero-s">"Tags, playlists, labels, artists or your loved shelf. The pool feeds suggestions and Automix."</div>
                        </div>
                        <PoolBuilder hero=true sources=pool_sources
                            counts=Signal::derive(move || pool_meta.with(|p| p.as_ref().map(|p| p.sources.clone()).unwrap_or_default()))
                            pool_size=Signal::derive(move || pool_meta.with(|p| p.as_ref().map(|p| p.page.total)))
                            on_change=Callback::new(move |s| m.set_pool_sources(s)) />
                        {move || (!pool_sources.with(|s| s.is_empty())).then(|| view! {
                            <div class="row plan-hero-act">
                                <AutomixButton m=m detail=detail pool_size=pool_total automix_max=automix_max hero=true />
                                <Button size=Size::Lg on_click=move |_| focus_pool_search()>"Pick manually"</Button>
                            </div>
                        })}
                    </div>
                </Show>
                {move || m.automix_busy.get().then(|| view! {
                    <div class="plan-busy" role="status"><div class="plan-busy-card"><Icon name="refresh" class="spin" />
                        {format!("Sequencing {} tracks…", format_count(pool_total.get().min(automix_max.get() as i64)))}</div></div>
                })}
            </section>
            <aside class="plan-rail" aria-label="Pool and suggestions">
                <div class="rail-pools">
                    <PoolBuilder sources=pool_sources
                        counts=Signal::derive(move || pool_meta.with(|p| p.as_ref().map(|p| p.sources.clone()).unwrap_or_default()))
                        pool_size=Signal::derive(move || pool_meta.with(|p| p.as_ref().map(|p| p.page.total)))
                        on_change=Callback::new(move |s| m.set_pool_sources(s)) />
                </div>
                <Tabs tabs=tabs value=tab />
                <Show when=move || tab.get() == "pool" fallback=move || view! { <SuggestionsPanel m=m detail=detail top=top /> }>
                    <PoolPanel m=m used=used sources_sig=sources_sig />
                </Show>
            </aside>
        </div>
    }
}
