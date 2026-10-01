//! Tracklists: upload DJ tracklist CSVs, find every track on Bandcamp (two searches at a time,
//! every guess visible and correctable), then queue the picks as tracks into one folder.
use std::sync::Arc;

use bc_types::bandcamp::{DownloadRequest, MatchOut, MatchRequest, ParsedFileOut, TrackRowOut};
use bc_types::jobs::JobOut;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;

use crate::api;
use crate::ds::{self, Button, EmptyState, Icon, PageHeader, SelectOption, Size, Variant};
use crate::logic::format::format_count;
use logic::{CONCURRENCY, IDLE, RowState, RowStatus, apply_error, apply_manual, apply_match, candidate_label, run_pool, selected_urls, summarise, tier_of};

mod logic;
mod upload;

/// One row of the review table: the parsed row and its live state.
#[derive(Clone)]
struct RowCell {
    row: TrackRowOut,
    state: RwSignal<RowState>,
}

fn tier_view(tier: &str) -> (&'static str, &'static str, &'static str) {
    // (badge class, icon, label): status is never colour alone.
    match tier {
        "strong" => ("badge badge-ok", "check-circle", "strong"),
        "likely" => ("badge badge-warn", "alert", "likely"),
        _ => ("badge", "clock", "weak"),
    }
}

#[component]
pub fn TracklistsPage() -> impl IntoView {
    let navigate = use_navigate();
    crate::pages::explore::qh::use_title(true, || "Tracklists".to_string());
    let file_ref = NodeRef::<leptos::html::Input>::new();

    let rows = RwSignal::new(Vec::<RowCell>::new());
    let files = RwSignal::new(Vec::<ParsedFileOut>::new());
    let dupes = RwSignal::new(0i64);
    let loaded = RwSignal::new(false);
    let uploading = RwSignal::new(false);
    let upload_err = RwSignal::new(None::<String>);
    let title = RwSignal::new(String::new());
    let force = RwSignal::new(true);
    let running = RwSignal::new(false);
    let manual_for = RwSignal::new(None::<i64>);
    let manual_busy = RwSignal::new(false);
    let manual_err = RwSignal::new(None::<String>);
    let dragging = RwSignal::new(false);
    let queue_busy = RwSignal::new(false);
    let queue_err = RwSignal::new(None::<String>);
    let run_gen = StoredValue::new(0u64);

    let summary = Signal::derive(move || {
        rows.with(|rs| {
            let st: Vec<RowState> = rs.iter().map(|c| c.state.get()).collect();
            summarise(&st)
        })
    });
    let urls = Signal::derive(move || {
        rows.with(|rs| {
            let st: Vec<RowState> = rs.iter().map(|c| c.state.get()).collect();
            selected_urls(&st)
        })
    });

    // ---- upload ----
    let accept = move |list: Vec<web_sys::File>| {
        if list.is_empty() {
            return;
        }
        uploading.set(true);
        upload_err.set(None);
        spawn_local(async move {
            match upload::parse_files(list).await {
                Ok(p) => {
                    run_gen.update_value(|g| *g += 1);
                    running.set(false);
                    title.set(p.suggested_title);
                    files.set(p.files);
                    dupes.set(p.duplicates);
                    rows.set(p.rows.into_iter().map(|row| RowCell { row, state: RwSignal::new(IDLE) }).collect());
                    manual_for.set(None);
                    loaded.set(true);
                }
                Err(e) => upload_err.set(Some(crate::pages::explore::qh::err_text(&e))),
            }
            let _ = uploading.try_set(false);
        });
    };
    let accept = Arc::new(accept);

    // ---- the search pass ----
    let find = move |only: Option<Vec<RowCell>>| {
        let targets: Vec<RowCell> = match only {
            Some(v) => v,
            None => rows.with_untracked(|rs| rs.iter().filter(|c| c.state.with_untracked(|s| s.status != RowStatus::Done)).cloned().collect()),
        };
        if targets.is_empty() {
            return;
        }
        run_gen.update_value(|g| *g += 1);
        let g = run_gen.get_value();
        running.set(true);
        for c in &targets {
            c.state.update(|s| s.status = RowStatus::Searching);
        }
        spawn_local(async move {
            // Never throws: a row that fails is a row that says so, and the pool keeps going.
            run_pool(targets, CONCURRENCY, move |c| async move {
                let back_to_idle = move |c: &RowCell| {
                    let _ = c.state.try_update(|s| {
                        if s.status == RowStatus::Searching {
                            s.status = RowStatus::Idle;
                        }
                    });
                };
                if run_gen.try_get_value() != Some(g) {
                    back_to_idle(&c);
                    return;
                }
                let req = MatchRequest { artist: c.row.artist.clone(), title: c.row.title.clone(), label: c.row.label.clone(), query: None };
                let res = api::post::<_, MatchOut>("/tracklists/match", &req).await;
                if run_gen.try_get_value() != Some(g) {
                    back_to_idle(&c);
                    return;
                }
                let _ = c.state.try_update(|s| {
                    *s = match res {
                        Ok(m) => apply_match(s, m),
                        Err(e) => apply_error(s, e.message()),
                    }
                });
            })
            .await;
            if run_gen.try_get_value() == Some(g) {
                let _ = running.try_set(false);
            }
        });
    };
    let find = Arc::new(find);
    let stop = move || {
        run_gen.update_value(|g| *g += 1);
        running.set(false);
        rows.with_untracked(|rs| {
            for c in rs {
                c.state.update(|s| {
                    if s.status == RowStatus::Searching {
                        s.status = RowStatus::Idle;
                    }
                });
            }
        });
    };

    // ---- correcting a row by hand ----
    let manual_search = Callback::new(move |(cell, q): (RowCell, String)| {
        manual_busy.set(true);
        manual_err.set(None);
        let req = MatchRequest { artist: cell.row.artist.clone(), title: cell.row.title.clone(), label: cell.row.label.clone(), query: Some(q) };
        spawn_local(async move {
            match api::post::<_, MatchOut>("/tracklists/match", &req).await {
                Ok(m) => {
                    let _ = cell.state.try_update(|s| *s = apply_manual(s, m));
                    let _ = manual_for.try_set(None);
                }
                Err(e) => {
                    let _ = manual_err.try_set(Some(e.message()));
                }
            }
            let _ = manual_busy.try_set(false);
        });
    });

    // ---- queueing ----
    let queue = move || {
        let list = urls.get_untracked();
        let name = title.get_untracked().trim().to_string();
        if list.is_empty() || name.is_empty() {
            return;
        }
        queue_busy.set(true);
        queue_err.set(None);
        let body = DownloadRequest { urls: list, label: Some(name.clone()), target_subdir: Some(name), priority: 100, force: force.get_untracked(), single_folder: true, tracks_only: true, ..Default::default() };
        let navigate = navigate.clone();
        spawn_local(async move {
            match api::post::<_, JobOut>("/downloads", &body).await {
                Ok(_) => {
                    ds::toast_ok("Queued for download");
                    navigate("/downloads", Default::default());
                }
                Err(e) => {
                    let _ = queue_err.try_set(Some(e.message()));
                }
            }
            let _ = queue_busy.try_set(false);
        });
    };
    let queue = Arc::new(queue);

    let named = Signal::derive(move || !title.with(|t| t.trim().is_empty()));
    let searched = Signal::derive(move || {
        let s = summary.get();
        s.searched + s.failed
    });
    let subtitle = Signal::derive(move || {
        if loaded.get() {
            let s = summary.get();
            format!("{} track{} \u{b7} {} selected", s.total, if s.total == 1 { "" } else { "s" }, s.selected)
        } else {
            "Upload a DJ tracklist CSV and find its tracks on Bandcamp".to_string()
        }
    });

    let (acc1, acc2) = (accept.clone(), accept.clone());
    let (find1, find2) = (find.clone(), find.clone());
    let (queue1, queue2) = (queue.clone(), queue.clone());

    view! {
        <div class="page tl-page">
            <PageHeader title="Tracklists" subtitle=subtitle />
            <div class="page-scroll tl-scroll">
                <section class=move || if dragging.get() { "tl-drop card dragging" } else { "tl-drop card" }
                    on:dragover=move |ev| { ev.prevent_default(); dragging.set(true); }
                    on:dragleave=move |_| dragging.set(false)
                    on:drop=move |ev| {
                        ev.prevent_default();
                        dragging.set(false);
                        if let Some(dt) = ev.data_transfer() { acc1(upload::files_of(dt.files())); }
                    }>
                    <h2 class="section-title">"Tracklist CSVs"</h2>
                    <div class="tl-drop-row">
                        <Button icon="upload" busy=uploading on_click=move |_| if let Some(i) = file_ref.get_untracked() { i.click() }>"Choose files"</Button>
                        <input node_ref=file_ref type="file" accept=".csv,text/csv" multiple class="hidden" aria-label="Tracklist CSV files"
                            on:change=move |ev| {
                                let input = event_target::<web_sys::HtmlInputElement>(&ev);
                                acc2(upload::files_of(input.files()));
                                input.set_value("");
                            } />
                        <span class="faint tl-hint">"\u{2026}or drop them here. Columns: "<code>"Artist"</code>", "<code>"Title"</code>", optionally "<code>"Label"</code>"."</span>
                    </div>
                    {move || upload_err.get().map(|e| view! { <p class="tl-err" role="alert">{e}</p> })}
                    {move || (loaded.get() && !files.with(|f| f.is_empty())).then(|| view! {
                        <div class="tl-files">
                            {files.get().into_iter().map(|f| {
                                let bad = f.error.is_some();
                                view! {
                                    <span class=if bad { "chip bad" } else { "chip" } title=f.error.clone().unwrap_or_default()>
                                        <Icon name="list" />{f.filename.clone()}
                                        <span class="mono">{if bad { "unreadable".to_string() } else { f.rows.to_string() }}</span>
                                    </span>
                                }
                            }).collect_view()}
                            {move || (dupes.get() > 0).then(|| view! { <span class="mono faint tl-dupes">{format!("{} duplicate{} removed", dupes.get(), if dupes.get() == 1 { "" } else { "s" })}</span> })}
                        </div>
                    })}
                </section>

                {move || {
                    if !loaded.get() {
                        return view! {
                            <EmptyState icon="list-music" title="No tracklist yet" hint="Drop a CSV exported from your DJ software or a set list. Each row is searched on Bandcamp and the best match is picked for you to review." />
                        }.into_any();
                    }
                    if rows.with(|r| r.is_empty()) {
                        return view! { <EmptyState icon="alert-circle" title="No tracks found" hint="None of the files had rows with an Artist and a Title column." /> }.into_any();
                    }
                    let (find1, find2, queue1, queue2) = (find1.clone(), find2.clone(), queue1.clone(), queue2.clone());
                    view! {
                        <div class="tl-bar card" role="region" aria-label="Tracklist actions">
                            <div class="tl-bar-row">
                                <input class="input tl-title" aria-label="Folder name" placeholder="Folder name (required)" prop:value=move || title.get()
                                    on:input=move |ev| title.set(event_target_value(&ev)) />
                                <label class="check tl-force"><input type="checkbox" prop:checked=move || force.get() on:change=move |ev| force.set(event_target_checked(&ev)) />"Include tracks I already have"</label>
                                <span class="spacer"></span>
                                {move || if running.get() {
                                    view! {
                                        <span class="mono muted" role="status">{move || format!("Searching {}/{}\u{2026}", searched.get(), summary.get().total)}</span>
                                        <Button icon="x" on_click=move |_| stop()>"Stop"</Button>
                                    }.into_any()
                                } else {
                                    let f = find1.clone();
                                    view! {
                                        <Button icon="search" title="Search Bandcamp for every row that has not been searched yet." on_click=move |_| f(None)>
                                            {move || if searched.get() > 0 { "Search the rest" } else { "Find on Bandcamp" }}
                                        </Button>
                                    }.into_any()
                                }}
                                {
                                    let q = queue1.clone();
                                    view! {
                                        <Button variant=Variant::Primary icon="download" busy=queue_busy
                                            disabled=Signal::derive(move || urls.with(|u| u.is_empty()) || !named.get() || running.get())
                                            on_click=move |_| q()>
                                            {move || format!("Download {}", format_count(urls.with(|u| u.len()) as i64))}
                                        </Button>
                                    }
                                }
                            </div>
                            {move || (searched.get() > 0).then(|| {
                                let s = summary.get();
                                view! {
                                    <p class="mono faint tl-sum" role="status">
                                        {format!("{} selected \u{b7} {} unmatched \u{b7} {} skipped", s.selected, s.unmatched, s.skipped)}
                                        {(s.failed > 0).then(|| view! { <span class="tl-warn">{format!(" \u{b7} {} failed", s.failed)}</span> })}
                                        {(s.already_have > 0).then(|| view! { <span class="tl-ok">{format!(" \u{b7} {} already in library", s.already_have)}</span> })}
                                    </p>
                                }
                            })}
                            {move || (!named.get() && !urls.with(|u| u.is_empty())).then(|| view! { <p class="faint tl-sum">"Name the folder to continue."</p> })}
                            {move || queue_err.get().map(|e| view! { <p class="tl-err" role="alert">{e}</p> })}
                        </div>
                        <ol class="tl-rows card">
                            <For each=move || rows.get() key=|c| c.row.seq let:cell>
                                <TrackRowView cell=cell manual_for=manual_for manual_busy=manual_busy manual_err=manual_err
                                    on_manual=manual_search on_retry=Callback::new({ let f = find2.clone(); move |c: RowCell| f(Some(vec![c])) }) />
                            </For>
                        </ol>
                        <div class="tl-foot">
                            {
                                let q = queue2.clone();
                                view! {
                                    <p class="muted">{move || {
                                        let n = urls.with(|u| u.len());
                                        if n == 0 { "Nothing selected yet.".to_string() }
                                        else if named.get() { format!("{n} track{} into one folder, as tracks rather than whole albums.", if n == 1 { "" } else { "s" }) }
                                        else { "Name the folder to continue.".to_string() }
                                    }}</p>
                                    <Button variant=Variant::Primary size=Size::Lg icon="download" busy=queue_busy
                                        disabled=Signal::derive(move || urls.with(|u| u.is_empty()) || !named.get() || running.get())
                                        on_click=move |_| q()>
                                        {move || format!("Download {}", urls.with(|u| u.len()))}
                                    </Button>
                                }
                            }
                        </div>
                    }.into_any()
                }}
            </div>
        </div>
    }
}

#[component]
fn TrackRowView(
    cell: RowCell,
    manual_for: RwSignal<Option<i64>>,
    manual_busy: RwSignal<bool>,
    manual_err: RwSignal<Option<String>>,
    on_manual: Callback<(RowCell, String)>,
    on_retry: Callback<RowCell>,
) -> impl IntoView {
    let RowCell { row, state } = cell.clone();
    let seq = row.seq;
    let sel_val = RwSignal::new(String::new());
    Effect::new(move |_| {
        let v = state.with(|s| s.chosen.map(|i| i.to_string()).unwrap_or_else(|| "-1".into()));
        if sel_val.get_untracked() != v {
            sel_val.set(v);
        }
    });
    let options = Signal::derive(move || {
        state.with(|s| {
            let mut v = vec![SelectOption::new("-1", "\u{2014} skip this track \u{2014}")];
            if let Some(m) = &s.matched {
                v.extend(m.candidates.iter().enumerate().map(|(i, c)| SelectOption::new(i.to_string(), candidate_label(&c.hit.name, &c.hit.subtitle, &c.hit.kind, c.score))));
            }
            v
        })
    });
    let on_pick = Callback::new(move |v: String| {
        state.update(|s| s.chosen = v.parse::<i64>().ok().and_then(|i| usize::try_from(i).ok()));
    });
    let chosen = Signal::derive(move || {
        state.with(|s| {
            let c = s.matched.as_ref()?.candidates.get(s.chosen?)?;
            Some((c.hit.url.clone(), if c.tier.is_empty() { tier_of(c.score).to_string() } else { c.tier.clone() }, c.hit.in_library, c.score))
        })
    });
    let status = Signal::derive(move || state.with(|s| s.status));
    let skipped = Signal::derive(move || state.with(|s| s.skipped));
    let no_cands = Signal::derive(move || state.with(|s| s.matched.as_ref().is_some_and(|m| m.candidates.is_empty())));
    let initial = format!("{} {}", row.artist, row.title);
    let cell_for_manual = cell.clone();
    let cell_for_retry = cell.clone();
    let sub = [row.label.clone(), row.source_file.clone()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" \u{b7} ");
    view! {
        <li class=move || if skipped.get() { "tl-row skipped" } else { "tl-row" }>
            <div class="tl-main">
                <span class="mono faint tl-seq">{row.seq}</span>
                <div class="grow">
                    <div class="truncate tl-t">{format!("{} \u{2014} {}", row.artist, row.title)}</div>
                    <div class="truncate faint tl-s">{sub}</div>
                </div>
                <div class="tl-acts">
                    <Button variant=Variant::Ghost size=Size::Sm icon="search" title="Search Bandcamp yourself" pressed=Signal::derive(move || manual_for.get() == Some(seq))
                        on_click=move |_| { manual_err.set(None); manual_for.update(|m| *m = if *m == Some(seq) { None } else { Some(seq) }) } />
                    <Button variant=Variant::Ghost size=Size::Sm icon="x" title=Signal::derive(move || if skipped.get() { "Put this track back" } else { "Leave this track out" }).get_untracked()
                        pressed=skipped on_click=move |_| state.update(|s| s.skipped = !s.skipped) />
                </div>
            </div>
            <div class="tl-res">
                {move || match status.get() {
                    RowStatus::Searching => view! { <span class="faint tl-st"><span class="xg-spin"><Icon name="refresh" /></span>"searching\u{2026}"</span> }.into_any(),
                    RowStatus::Error => {
                        let c = cell_for_retry.clone();
                        view! {
                            <span class="status danger tl-st"><Icon name="alert-circle" />{state.with(|s| s.error.clone().unwrap_or_default())}</span>
                            <Button size=Size::Sm variant=Variant::Ghost icon="refresh" on_click=move |_| on_retry.run(c.clone())>"Retry"</Button>
                        }.into_any()
                    }
                    RowStatus::Idle => view! { <span class="faint tl-st">"not searched"</span> }.into_any(),
                    RowStatus::Done if no_cands.get() => view! { <span class="status warn tl-st"><Icon name="alert" />"nothing found"</span> }.into_any(),
                    RowStatus::Done => view! {
                        <ds::Select options=options value=sel_val aria_label="Bandcamp match" class="tl-pick" on_change=on_pick />
                        {move || chosen.get().map(|(_, tier, lib, score)| {
                            let (cls, icon, label) = tier_view(&tier);
                            view! {
                                <span class=cls title=format!("Match confidence: {label} ({:.0}%)", score * 100.0)><Icon name=icon />{label}</span>
                                {lib.then(|| view! { <span class="badge badge-info" title="Already in your library"><Icon name="check" />"in library"</span> })}
                            }
                        })}
                    }.into_any(),
                }}
            </div>
            {move || manual_for.get().filter(|m| *m == seq).map(|_| {
                let (c, init) = (cell_for_manual.clone(), initial.clone());
                view! { <ManualSearch initial=init busy=manual_busy err=manual_err on_search=Callback::new(move |q: String| on_manual.run((c.clone(), q))) /> }
            })}
            {move || chosen.get().map(|(url, ..)| view! { <a class="truncate faint tl-url" href=url.clone() target="_blank" rel="noreferrer">{url.clone()}</a> })}
        </li>
    }
}

#[component]
fn ManualSearch(initial: String, busy: RwSignal<bool>, err: RwSignal<Option<String>>, on_search: Callback<String>) -> impl IntoView {
    let q = RwSignal::new(initial);
    view! {
        <form class="tl-manual" on:submit=move |ev| { ev.prevent_default(); let v = q.get_untracked(); if !v.trim().is_empty() { on_search.run(v.trim().to_string()); } }>
            <input class="input" prop:value=move || q.get() on:input=move |ev| q.set(event_target_value(&ev)) placeholder="Search Bandcamp" aria-label="Search Bandcamp for this track" autofocus />
            <Button kind="submit" icon="search" busy=busy disabled=Signal::derive(move || q.with(|q| q.trim().is_empty()))>"Search"</Button>
            {move || err.get().map(|e| view! { <span class="tl-err" role="alert">{e}</span> })}
        </form>
    }
}
