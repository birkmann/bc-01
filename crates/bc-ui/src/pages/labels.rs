//! Labels: the shelf of label folders (virtualised), sweeps, selection, play; and the label page.
use std::collections::HashSet;
use std::sync::Arc;

use bc_types::library::{LabelOut, LabelQuery, Page};
use bc_types::player::{LabelMode, PlayerCommand, QueueSource};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Button, EmptyState, MenuEntry, MenuItem, PageHeader, SearchInput, Select, SelectOption, Size, Variant, children, use_debounced};
use crate::logic::format::format_count;
use crate::player::use_player;
use crate::util::{enc, qs};
use crate::widgets::card_grid::CardGrid;
use crate::widgets::{PageFetcher, PageRes};

pub mod bandcamp;
mod detail;
pub mod edit;
mod folder;
pub mod logic;
pub mod shared;
mod strays;
mod sweep;

pub use detail::LabelDetailPage;

use bandcamp::Actions;
use edit::{EditDialog, EditTarget};
use folder::{LabelCard, LabelOps, LabelRow, play_label};
use logic as lg;
use shared::{NoticeBar, StatusBar, use_favs};

/// `/labels?...` for a shelf page (offset/limit optional).
fn shelf_url(q: &str, sort: &str, offset: Option<usize>, limit: usize) -> String {
    let s = lg::label_sort(sort);
    let mut pairs = vec![("q", q.trim().to_string()), ("sort", sort.to_string()), ("order", lg::dir_str(s.order).to_string())];
    if let Some(o) = offset {
        pairs.push(("offset", o.to_string()));
    }
    pairs.push(("limit", limit.to_string()));
    format!("/labels{}", qs(&pairs))
}

/// The shelf this folder sits in, as the engine's `listing` value.
fn listing_json(q: &str, sort: &str) -> serde_json::Value {
    let s = lg::label_sort(sort);
    let q = q.trim();
    serde_json::to_value(LabelQuery { q: (!q.is_empty()).then(|| q.to_string()), sort: Some(s.sort), order: Some(s.order), ..Default::default() }).unwrap_or_default()
}

/// Ids of the labels at positions `lo..=hi` of a listing.
async fn fetch_ids(q: String, sort: String, lo: usize, hi: usize) -> Vec<i64> {
    let mut out = vec![];
    let mut at = lo;
    while at <= hi {
        let n = (hi - at + 1).min(500);
        match api::get::<Page<LabelOut>>(&shelf_url(&q, &sort, Some(at), n)).await {
            Ok(p) => {
                if p.items.is_empty() {
                    break;
                }
                at += p.items.len();
                out.extend(p.items.iter().map(|l| l.id));
            }
            Err(e) => {
                crate::ds::toast_err(&e.message());
                break;
            }
        }
    }
    out
}

#[component]
pub fn LabelsPage() -> impl IntoView {
    let params = use_query_map();
    let navigate = use_navigate();
    let player = use_player();
    let favs = use_favs();
    let filter = RwSignal::new(params.get_untracked().get("q").unwrap_or_default());
    let sort = RwSignal::new(params.get_untracked().get("sort").filter(|s| lg::LABEL_SORTS.iter().any(|d| d.value == s)).unwrap_or_else(|| "releases".into()));
    let q = use_debounced(filter, 120);
    let total = RwSignal::new(None::<usize>);
    let picked: RwSignal<HashSet<i64>> = RwSignal::new(HashSet::new());
    let anchor = StoredValue::new(None::<usize>);
    let edit: RwSignal<Option<EditTarget>> = RwSignal::new(None);
    let starting = RwSignal::new(None::<&'static str>);

    // URL state: the shelf is shareable and Back keeps the filter.
    {
        let navigate = navigate.clone();
        Effect::new(move |prev: Option<()>| {
            let (qv, sv) = (q.get(), sort.get());
            if prev.is_none() {
                return;
            }
            let mut parts = vec![];
            if !qv.trim().is_empty() {
                parts.push(format!("q={}", enc(qv.trim())));
            }
            if sv != "releases" {
                parts.push(format!("sort={sv}"));
            }
            let url = if parts.is_empty() { "/labels".to_string() } else { format!("/labels?{}", parts.join("&")) };
            navigate(&url, NavigateOptions { replace: true, ..Default::default() });
        });
    }

    let directory = use_query::<Page<LabelOut>>(|| Some(QuerySpec::keyed("labels:count", "/labels?limit=1", &["label"])));
    let sweep = sweep::use_label_sweep(Callback::new(|_| crate::data::invalidate_entity("label", &[])));
    let resolve = sweep::use_label_resolve(Callback::new(|_| {
        crate::data::invalidate_entity("label", &[]);
        crate::data::invalidate_entity("release", &[]);
    }));
    let relink = sweep::use_relink(Callback::new(|_| {
        crate::data::invalidate_entity("label", &[]);
        crate::data::invalidate_entity("release", &[]);
    }));
    let actions = Actions::new(Callback::new(|_| crate::data::invalidate_entity("label", &[])));

    let fetcher: PageFetcher<LabelRow> = Arc::new(move |req| {
        let url = shelf_url(&q.get_untracked(), &sort.get_untracked(), Some(req.offset), req.limit);
        let offset = req.offset;
        Box::pin(async move {
            let p: Page<LabelOut> = api::get(&url).await?;
            Ok(PageRes { rows: p.items.into_iter().enumerate().map(|(i, label)| LabelRow { idx: offset + i, label }).collect(), total: p.total as usize })
        })
    });
    let source_key = Signal::derive(move || format!("{}|{}", q.get().trim(), sort.get()));
    let listing = Signal::derive(move || listing_json(&q.get(), &sort.get()));
    let ops = LabelOps { favs, actions, edit, on_removed: Callback::new(move |id| picked.update(|p| { p.remove(&id); })), listing };

    let on_pick = Callback::new(move |(idx, id, shift): (usize, i64, bool)| {
        let from = anchor.get_value();
        anchor.set_value(Some(idx));
        match (shift, from) {
            (true, Some(a)) => {
                let (lo, hi) = lg::shift_range(a, idx);
                let (qv, sv) = (q.get_untracked(), sort.get_untracked());
                spawn_local(async move {
                    let ids = fetch_ids(qv, sv, lo, hi).await;
                    let _ = picked.try_update(|p| p.extend(ids));
                });
            }
            _ => picked.update(|p| {
                if !p.remove(&id) {
                    p.insert(id);
                }
            }),
        }
    });
    let select_all = move |_| {
        let n = total.get_untracked().unwrap_or(0);
        if n == 0 {
            return;
        }
        let (qv, sv) = (q.get_untracked(), sort.get_untracked());
        spawn_local(async move {
            let ids = fetch_ids(qv, sv, 0, n - 1).await;
            let _ = picked.try_set(ids.into_iter().collect());
        });
    };

    let play_shelf = move |shuffle: bool| {
        if starting.get_untracked().is_some() {
            return;
        }
        if shuffle {
            player.cmd(PlayerCommand::StartSource { source: QueueSource::Labels { listing: listing.get_untracked() }, shuffle: true });
            return;
        }
        starting.set(Some("all"));
        let url = shelf_url(&q.get_untracked(), &sort.get_untracked(), Some(0), 60);
        let lj = listing.get_untracked();
        spawn_local(async move {
            match api::get::<Page<LabelOut>>(&url).await {
                Ok(p) => match p.items.iter().find(|l| l.track_count > 0) {
                    Some(first) => play_label(first.id, lj, LabelMode::All),
                    None => crate::ds::toast_info("Nothing to play on this shelf"),
                },
                Err(e) => crate::ds::toast_err(&e.message()),
            }
            let _ = starting.try_set(None);
        });
    };

    let subtitle = Signal::derive(move || {
        let t = total.get()?;
        let all = directory.data.get().map(|d| d.total as usize);
        Some(match all {
            Some(a) if a != t => format!("{} of {} labels", format_count(t as i64), format_count(a as i64)),
            _ => format!("{} labels", format_count(t as i64)),
        })
    });
    let n_picked = Signal::derive(move || picked.with(|p| p.len()));
    let sweep_running = sweep.running();
    let resolve_running = resolve.running();
    let relink_running = relink.running();
    let sweep_busy = Signal::derive(move || sweep_running.get() || sweep.starting.get());
    let sweep_label = Signal::derive(move || {
        let n = n_picked.get();
        if n > 0 { format!("Find & download {} selected", format_count(n as i64)) } else { "Find & download new".to_string() }
    });
    let sort_options = Signal::derive(|| lg::LABEL_SORTS.iter().map(|s| SelectOption::new(s.value, s.label)).collect::<Vec<_>>());

    let overflow = Callback::new(move |_| -> Vec<MenuEntry> {
        vec![
            MenuItem::new("Find missing labels").icon("tag").disabled(resolve_running.get_untracked()).on(move || resolve.start()).into(),
            MenuItem::new("Link library to Bandcamp").icon("link").disabled(relink_running.get_untracked()).on(move || relink.start()).into(),
        ]
    });
    let start_sweep = move |_| {
        let ids: Vec<i64> = picked.get_untracked().into_iter().collect();
        sweep.start(ids);
    };
    let sweep_title = Signal::derive(move || {
        if n_picked.get() > 0 {
            "Check the selected labels' Bandcamp pages for new releases and download them".to_string()
        } else {
            "Check every label's Bandcamp page for new releases and queue them all for download".to_string()
        }
    });
    let sweep_line = sweep.line();
    let resolve_line = resolve.line();
    let relink_line = relink.line();

    view! {
        <div class="page pp-page">
            <PageHeader title="Labels" subtitle=subtitle overflow=overflow
                actions=children(move || view! {
                    <Button variant=Variant::Primary icon="play" busy=Signal::derive(move || starting.get() == Some("all")) on_click=move |_| play_shelf(false)
                        title="Play the shelf from the top, folder after folder"><span class="hide-sm">"Play all"</span></Button>
                    <Button icon="shuffle" on_click=move |_| play_shelf(true)
                        title="Shuffle every folder together: a share from each label"><span class="hide-sm">"Shuffle all"</span></Button>
                    {move || {
                        let variant = if n_picked.get() > 0 { Variant::Primary } else { Variant::Outline };
                        view! {
                            <Button variant=variant icon="rss" busy=sweep_busy on_click=start_sweep title=sweep_title.get() class="pp-sweep-btn">
                                <span class="hide-sm">{move || sweep_label.get()}</span>
                            </Button>
                        }
                    }}
                }) />
            <div class="pp-toolbar">
                <SearchInput value=filter placeholder="Filter labels\u{2026}" class="pp-search" />
                <span class="spacer"></span>
                <Select options=sort_options value=sort aria_label="Sort labels" class="pp-sort" />
            </div>
            <div class="pp-notices">
                {move || (n_picked.get() > 0).then(|| view! {
                    <div class="pp-selbar" role="status">
                        <span class="num">{move || format_count(n_picked.get() as i64)}</span>
                        <span>{move || if n_picked.get() == 1 { "label selected" } else { "labels selected" }}</span>
                        <span class="spacer"></span>
                        <Button size=Size::Sm variant=Variant::Ghost on_click=select_all disabled=Signal::derive(move || Some(n_picked.get()) == total.get())>
                            {move || format!("Select all {}", format_count(total.get().unwrap_or(0) as i64))}
                        </Button>
                        <Button size=Size::Sm variant=Variant::Ghost on_click=move |_| { picked.set(HashSet::new()); anchor.set_value(None); }>"Clear"</Button>
                    </div>
                })}
                <StatusBar line=sweep_line running=sweep_running on_stop=Callback::new(move |_| sweep.stop()) on_dismiss=Callback::new(move |_| sweep.dismiss())
                    links=children(move || view! {
                        {move || sweep.report.get().then(|| sweep.status.get().filter(|s| s.phase == "done").map(|s| view! {
                            {(s.queued > 0).then(|| view! { <a class="pp-link" href="/downloads">"Open Downloads"</a> })}
                            {(s.new > 0).then(|| view! { <a class="pp-link" href="/harvest">"Review in Harvest"</a> })}
                        }))}
                    }) />
                <StatusBar line=resolve_line running=resolve_running on_dismiss=Callback::new(move |_| resolve.dismiss()) />
                <StatusBar line=relink_line running=relink_running on_stop=Callback::new(move |_| relink.stop()) on_dismiss=Callback::new(move |_| relink.dismiss()) />
                <NoticeBar notice=actions.notice busy=actions.busy() on_download=Callback::new(move |ids| actions.queue(ids)) />
            </div>
            <div class="pp-fill">
                {move || total.get().is_none().then(|| view! { <div class="pp-skel-grid" aria-hidden="true">{(0..12).map(|_| view! { <div class="pp-skel-card"><div class="skeleton"></div><div class="skeleton"></div></div> }).collect_view()}</div> })}
                <CardGrid fetch=fetcher source_key=source_key min_card_w=144.0 meta_h=88.0 gap=12.0 entities=vec!["label"] total_out=total
                    render=Callback::new(move |(row, w): (LabelRow, f64)| view! { <LabelCard row=row width=w ops=ops picked=picked on_pick=on_pick /> }.into_any())
                    empty=move || {
                        let f = filter.get();
                        let f = f.trim().to_string();
                        view! {
                            <EmptyState icon="folder"
                                title=if f.is_empty() { "No labels yet".to_string() } else { format!("No label matches \u{201c}{f}\u{201d}") }
                                hint=if f.is_empty() { "A release is filed under a label when its files carry a LABEL tag, or when Bandcamp named one for it. Rescanning fills in what is known." } else { "Bought it off a label's Bandcamp page? \u{201c}Find missing labels\u{201d} files those releases under the label they came from." }>
                                {(!f.is_empty()).then(|| view! { <Button icon="tag" busy=resolve_running on_click=move |_| resolve.start()>"Find missing labels"</Button> })}
                            </EmptyState>
                        }
                    } />
            </div>
            <EditDialog target=edit on_saved=Callback::new(|_| crate::data::invalidate_entity("label", &[])) />
        </div>
    }
}
