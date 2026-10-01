//! DJ sets: the shelf (`SetsPage`) and a set (`SetDetailPage`: Plan | Arrange tabs).
//! The Arrange tab is owned by another module (`arrange`); it receives the live detail and a
//! `on_changed` callback.
pub mod arrange;
mod automix;
mod list;
mod mutations;
mod plan_view;
mod pool_builder;
mod pool_panel;
pub mod set_logic;
mod status;
mod track_row;

use std::sync::Arc;

use bc_types::library::{Page, TrackOut};
use bc_types::player::QueueSource;
use bc_types::sets::{DjSetDetail, PoolPage};
use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};

use crate::api;
use crate::data::QuerySpec;
use crate::ds::{Button, Dialog, ErrorPanel, Icon, MenuEntry, MenuItem, Size, Skeleton, Variant, confirm, toast_err, toast_ok};
use crate::logic::format::{format_duration_ms, format_long_duration};
use crate::player::plan::qh::use_qh;
use automix::AutomixButton;
use mutations::SetMut;
use plan_view::PlanView;
use status::StatusPill;

pub use list::SetsPage;

#[component]
pub fn SetDetailPage() -> impl IntoView {
    let params = use_params_map();
    let id = Memo::new(move |_| params.get().get("id").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0));
    // remount when navigating from one set to another
    view! { {move || view! { <SetDetail id=id.get() /> }} }
}

fn inline_name(name: Signal<String>, on_rename: Callback<String>) -> impl IntoView {
    let editing = RwSignal::new(false);
    let value = RwSignal::new(String::new());
    let commit = move || {
        editing.set(false);
        let v = value.get_untracked().trim().to_string();
        if !v.is_empty() && v != name.get_untracked() {
            on_rename.run(v);
        }
    };
    view! {
        <Show when=move || editing.get() fallback=move || view! {
            <button type="button" class="sd-title" title="Rename" on:click=move |_| { value.set(name.get_untracked()); editing.set(true); }>
                <span class="truncate">{move || name.get()}</span><Icon name="edit" size=13 />
            </button>
        }>
            <input class="input sd-title-in" aria-label="Set name" autofocus prop:value=move || value.get()
                on:input=move |ev| value.set(event_target_value(&ev))
                on:blur=move |_| commit()
                on:keydown=move |ev| match ev.key().as_str() {
                    "Enter" => commit(),
                    "Escape" => editing.set(false),
                    _ => {}
                } />
        </Show>
    }
}

#[component]
fn SetDetail(id: i64) -> impl IntoView {
    let navigate = use_navigate();
    let q = use_query_map();
    let query = use_qh::<DjSetDetail>(move || Some(QuerySpec::new(format!("/sets/{id}"), &["set"])));
    let pool_meta = use_qh::<PoolPage>(move || Some(QuerySpec::new(format!("/sets/{id}/pool?limit=1"), &["set"])));
    let detail = RwSignal::new(None::<Arc<DjSetDetail>>);
    Effect::new(move |_| {
        if let Some(d) = query.data.get() {
            detail.set(Some(d));
        }
    });
    let m = SetMut::new(id, detail);
    let view_name = Memo::new(move |_| if q.get().get("view").as_deref() == Some("arrange") { "arrange" } else { "plan" });
    let view_sig = RwSignal::new(view_name.get_untracked().to_string());
    Effect::new(move |_| view_sig.set(view_name.get().to_string()));
    let nav_view = navigate.clone();
    Effect::new(move |prev: Option<()>| {
        let v = view_sig.get();
        if prev.is_some() && v != view_name.get_untracked() {
            let url = if v == "arrange" { format!("/sets/{id}?view=arrange") } else { format!("/sets/{id}") };
            nav_view(&url, NavigateOptions { replace: true, ..Default::default() });
        }
    });
    // `v` flips the view (not while typing)
    let handle = window_event_listener(leptos::ev::keydown, move |e| {
        let typing = e.target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()).map(|t| {
            let tag = t.tag_name();
            tag == "INPUT" || tag == "TEXTAREA" || tag == "SELECT" || t.get_attribute("contenteditable").is_some()
        }).unwrap_or(false);
        if !typing && e.key() == "v" && !e.meta_key() && !e.ctrl_key() && !e.alt_key() {
            view_sig.update(|v| *v = if v == "plan" { "arrange".into() } else { "plan".into() });
        }
    });
    on_cleanup(move || handle.remove());

    let name = Signal::derive(move || detail.with(|d| d.as_ref().map(|d| d.set.name.clone()).unwrap_or_default()));
    let status = Signal::derive(move || detail.with(|d| d.as_ref().map(|d| d.set.status.clone()).unwrap_or_default()));
    let n_items = Signal::derive(move || detail.with(|d| d.as_ref().map(|d| d.items.len()).unwrap_or(0)));
    let playing = RwSignal::new(false);
    let player = crate::player::use_player();
    let play = move |_| {
        playing.set(true);
        let name = name.get_untracked();
        leptos::task::spawn_local(async move {
            match api::get::<Page<TrackOut>>(&format!("/sets/{id}/tracks")).await {
                Ok(p) if !p.items.is_empty() => crate::pages::playlists::play_items(player, &p.items, 0, Some(QueueSource::Set { id, name })),
                Ok(_) => {}
                Err(e) => toast_err(&e.message()),
            }
            let _ = playing.try_set(false);
        });
    };

    let target_open = RwSignal::new(false);
    let target_val = RwSignal::new(String::new());
    let save_target = move |clear: bool| {
        target_open.set(false);
        let v = if clear { None } else { target_val.get_untracked().trim().parse::<i64>().ok().filter(|m| *m > 0) };
        m.update(serde_json::json!({ "target_minutes": v }));
    };

    let nav_del = navigate.clone();
    let overflow = Callback::new(move |_| -> Vec<MenuEntry> {
        let mk = |label: &'static str, icon: &'static str, fmt: &'static str| -> MenuEntry {
            MenuItem::new(label).icon(icon).on(move || crate::pages::playlists::open_url(&format!("/api/sets/{id}/export?format={fmt}"))).into()
        };
        let render = |label: &'static str, fmt: &'static str| -> MenuEntry {
            MenuItem::new(label).icon("disc").on(move || crate::pages::playlists::open_url(&format!("/api/sets/{id}/render?format={fmt}"))).into()
        };
        let nav = nav_del.clone();
        vec![
            mk("Export as M3U8", "download", "m3u8"),
            mk("Export as CSV", "download", "csv"),
            mk("Audio files (zip)", "download", "zip"),
            MenuEntry::Sep,
            render("Rendered mix (mp3)", "mp3"),
            render("Rendered mix (wav)", "wav"),
            MenuEntry::Sep,
            MenuItem::new("Analyse tracks").icon("activity").on(move || {
                let ids: Vec<i64> = detail.with_untracked(|d| d.as_ref().map(|d| d.items.iter().filter_map(|i| i.track_id).collect()).unwrap_or_default());
                crate::pages::playlists::analyse_ids(ids);
            }).into(),
            MenuItem::new("Target length…").icon("clock").on(move || {
                target_val.set(detail.with_untracked(|d| d.as_ref().and_then(|d| d.set.target_minutes).map(|t| t.to_string()).unwrap_or_default()));
                target_open.set(true);
            }).into(),
            MenuEntry::Sep,
            MenuItem::new("Delete set…").icon("trash").danger().on(move || {
                let nav = nav.clone();
                leptos::task::spawn_local(async move {
                    if !confirm("Delete this set?", &format!("\"{}\" and its plan will be deleted. The tracks stay in your library.", name.get_untracked()), "Delete set", true).await {
                        return;
                    }
                    match api::call("DELETE", &format!("/sets/{id}")).await {
                        Ok(()) => { toast_ok("Set deleted"); nav("/sets", Default::default()); }
                        Err(e) => toast_err(&e.message()),
                    }
                });
            }).into(),
        ]
    });

    let summary = move || {
        detail.with(|d| {
            d.as_ref().map(|d| {
                let s = &d.set.summary;
                let mut t = format!("{} track{} · {}", s.track_count, if s.track_count == 1 { "" } else { "s" }, format_long_duration(s.total_ms as f64));
                if let Some(b) = s.avg_bpm { t.push_str(&format!(" · avg {} BPM", b.round() as i64)); }
                if s.overlap_ms > 0 { t.push_str(&format!(" · {}s overlap", (s.overlap_ms as f64 / 1000.0).round() as i64)); }
                t
            })
        })
    };
    let clock = move || {
        detail.with(|d| {
            d.as_ref().map(|d| {
                let s = &d.set.summary;
                let total = format_duration_ms(Some(s.total_ms as f64));
                match s.target_ms {
                    Some(t) => {
                        let over = s.total_ms - t;
                        let mut txt = format!("{total} / {}", format_duration_ms(Some(t as f64)));
                        if over > 0 { txt.push_str(&format!(" (+{} over)", format_duration_ms(Some(over as f64)))); }
                        (txt, over > 0)
                    }
                    None => (total, false),
                }
            })
        })
    };
    let pool_total = Signal::derive(move || pool_meta.data.get().map(|p| p.page.total).unwrap_or(0));
    let automix_max = Signal::derive(move || pool_meta.data.get().map(|p| p.automix_max).unwrap_or(200));
    let problems = Signal::derive(move || detail.with(|d| d.as_ref().map(|d| d.set.summary.problem_transitions).unwrap_or(0)));

    // the body must not re-create (and close its dialogs) when the detail's value changes
    let has_detail = Memo::new(move |_| detail.with(|d| d.is_some()));
    let err_msg = Memo::new(move |_| query.error.with(|e| e.as_ref().map(|e| e.message())));
    view! {
        <div class="page dj-page">
            <header class="sd-head">
                <div class="sd-titles">
                    <a class="pls-back" href="/sets"><Icon name="arrow-left" size=13 />"All sets"</a>
                    {inline_name(name, Callback::new(move |n: String| m.update(serde_json::json!({ "name": n }))))}
                    <div class="sd-meta">
                        <span class="muted mono">{move || summary().unwrap_or_default()}</span>
                        {move || clock().map(|(t, over)| view! {
                            <button type="button" class=if over { "sd-clock over mono" } else { "sd-clock mono" } title="Set target length"
                                on:click=move |_| {
                                    target_val.set(detail.with_untracked(|d| d.as_ref().and_then(|d| d.set.target_minutes).map(|t| t.to_string()).unwrap_or_default()));
                                    target_open.set(true);
                                }>
                                <Icon name="clock" size=12 />{t}{over.then(|| view! { <Icon name="alert" size=12 /> })}
                            </button>
                        })}
                        {move || detail.with(|d| d.is_some()).then(|| view! { <StatusPill status=status on_change=Callback::new(move |s: String| m.update(serde_json::json!({ "status": s }))) /> })}
                        {move || (problems.get() > 0).then(|| view! { <span class="vchip warn" title="Transitions with a risky or clashing key or tempo"><Icon name="alert" size=11 />{format!("{} rough transition{}", problems.get(), if problems.get() == 1 { "" } else { "s" })}</span> })}
                    </div>
                </div>
                <div class="sd-actions">
                    <Button variant=Variant::Primary icon="play" busy=playing disabled=Signal::derive(move || n_items.get() == 0)
                        title="Play the set in order; turn on DJ mix in the player for blended transitions" on_click=play>"Play"</Button>
                    <div class="segmented" role="tablist" aria-label="View">
                        <button type="button" role="tab" aria-pressed=move || (view_sig.get() == "plan").to_string() aria-selected=move || (view_sig.get() == "plan").to_string() on:click=move |_| view_sig.set("plan".into())>"Plan"</button>
                        <button type="button" role="tab" aria-pressed=move || (view_sig.get() == "arrange").to_string() aria-selected=move || (view_sig.get() == "arrange").to_string() on:click=move |_| view_sig.set("arrange".into())>"Arrange"</button>
                    </div>
                    <AutomixButton m=m detail=detail pool_size=pool_total automix_max=automix_max />
                    <crate::ds::MenuButton entries=overflow title="More" />
                </div>
            </header>
            {move || match (has_detail.get(), err_msg.get()) {
                (true, _) => {
                    if view_sig.get() == "arrange" {
                        view! {
                            <arrange::ArrangeView set_id=id detail=detail on_changed=Callback::new(move |_| query.refetch()) />
                        }.into_any()
                    } else {
                        view! { <PlanView m=m detail=detail pool_meta=Signal::derive(move || pool_meta.data.get()) /> }.into_any()
                    }
                }
                (false, Some(e)) => view! { <ErrorPanel message=e on_retry=Callback::new(move |_| query.refetch()) /> }.into_any(),
                (false, None) => view! { <div class="plan"><Skeleton height="320px" class="grow" /></div> }.into_any(),
            }}
            <Dialog open=target_open title="Target length"
                footer=crate::ds::children(move || view! {
                    <Button variant=Variant::Ghost on_click=move |_| save_target(true)>"No target"</Button>
                    <Button variant=Variant::Primary on_click=move |_| save_target(false)>"Set"</Button>
                })>
                <label class="field-row"><span class="muted">"Minutes"</span>
                    <input class="input num-in" type="number" min="1" placeholder="none" prop:value=move || target_val.get()
                        on:input=move |ev| target_val.set(event_target_value(&ev))
                        on:keydown=move |ev| if ev.key() == "Enter" { save_target(false) } /></label>
            </Dialog>
        </div>
    }
}

#[allow(dead_code)]
fn _s(_: Size) {}
