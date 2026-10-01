//! Analysis: coverage, live queue, workers, scan scopes, analyzer breakdown
//! (accuracy report inputs). Progress comes from WS `analysis.progress|batch|item` and is
//! patched into the cached status/queue in place: nothing refetches on a progress tick.
use bc_types::analysis::*;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::data::{QuerySpec, cache, use_query, use_topic};
use crate::logic::format::format_bytes;
use crate::ds::{Badge, Button, EmptyState, Icon, Meter, MenuEntry, MenuItem, PageHeader, Size, Skeleton, StatusBadge, Tone, Variant, toast_err, toast_ok};
use crate::logic::format::format_count;
use crate::pages::settings::{QH, QueryError, SysCard, qh};

mod logic;
use logic::*;

#[component]
pub fn AnalysisPage() -> impl IntoView {
    let status = qh(use_query::<AnalysisStatus>(|| Some(QuerySpec::new(STATUS_URL, &["analysis"]))));
    let queue = qh(use_query::<AnalysisQueue>(|| Some(QuerySpec::new(QUEUE_URL, &["analysis"]))));
    let samples = StoredValue::new(Vec::<(f64, i64)>::new());
    let eta = RwSignal::new(None::<f64>);
    let last_msg = RwSignal::new(None::<String>);
    let scanning = RwSignal::new(false);

    let refresh_status = move || status.refetch();
    use_topic::<AnalysisProgressEvent>(TOPIC_ANALYSIS_PROGRESS, move |ev| {
        let mut settled = false;
        cache::patch::<AnalysisQueue>(QUEUE_URL, |q| settled = patch_progress(q, &ev));
        // ETA from the aggregate of unfinished jobs.
        if let Some(q) = cache::get::<AnalysisQueue>(QUEUE_URL) {
            let (done, total) = aggregate(&q.jobs);
            let now = crate::util::perf_now();
            samples.update_value(|s| {
                s.push((now, done));
                s.retain(|(t, _)| now - *t < 60_000.0);
            });
            eta.set(samples.with_value(|s| eta_seconds(s, total - done)));
            if total == 0 {
                eta.set(None);
            }
        }
        if settled {
            samples.set_value(vec![]);
            refresh_status();
            queue.refetch();
        }
    });
    use_topic::<AnalysisBatchEvent>(TOPIC_ANALYSIS_BATCH, move |ev| {
        cache::patch::<AnalysisQueue>(QUEUE_URL, |q| patch_batch(q, &ev));
        cache::patch::<AnalysisStatus>(STATUS_URL, |s| s.running_tracks = ev.tracks.len() as i64);
    });
    use_topic::<AnalysisItemEvent>(TOPIC_ANALYSIS_ITEM, move |ev| {
        cache::patch::<AnalysisQueue>(QUEUE_URL, |q| patch_item(q, &ev));
        cache::patch::<AnalysisStatus>(STATUS_URL, |s| patch_status_item(s, &ev));
    });

    let scan = move |scope: ScanScope| {
        scanning.set(true);
        last_msg.set(None);
        spawn_local(async move {
            let req = ScanRequest { scope, ..Default::default() };
            match api::post::<_, ScanResponse>("/analysis/scan", &req).await {
                Ok(r) => {
                    last_msg.set(Some(if r.queued > 0 { format!("Queued {} track(s). Progress is in the queue above.", format_count(r.queued)) } else { r.detail }));
                    if r.queued > 0 {
                        toast_ok(&format!("Queued {} tracks", format_count(r.queued)));
                    }
                    queue.refetch();
                    status.refetch();
                }
                Err(e) => toast_err(&e.message()),
            }
            scanning.set(false);
        });
    };
    let scan = std::sync::Arc::new(scan);

    let missing = Signal::derive(move || status.data.get().map(|s| s.missing).unwrap_or(0));
    let no_backend = Signal::derive(move || status.data.get().map(|s| s.active_backend.is_none()).unwrap_or(false));
    let subtitle = Signal::derive(move || status.data.get().map(|s| format!("{}% of {} analysed", (s.coverage * 100.0).round(), format_count(s.total_tracks))).unwrap_or_else(|| "BPM, musical key, loudness and waveforms".into()));
    let sc_overflow = scan.clone();
    let overflow = Callback::new(move |_| -> Vec<MenuEntry> {
        SCOPES
            .iter()
            .map(|(scope, label, _)| {
                let (s, scope) = (sc_overflow.clone(), *scope);
                MenuItem::new(format!("Analyse {}", label.to_lowercase())).icon("activity").on(move || s(scope)).into()
            })
            .collect()
    });
    let sc_primary = scan.clone();
    let sc_body = scan.clone();

    view! {
        <div class="page">
            <PageHeader title="Analysis" subtitle=subtitle overflow=overflow
                actions=crate::ds::children(move || {
                    let s = sc_primary.clone();
                    view! {
                        <Show when={move || missing.get() > 0}>
                            {
                                let s = s.clone();
                                view! {
                                    <Button variant=Variant::Primary icon="play" busy=scanning disabled=no_backend title="Analyse missing tracks" on_click=move |_| s(ScanScope::Missing)>
                                        <span class="hide-sm">{move || format!("Analyse {}", format_count(missing.get()))}</span>
                                    </Button>
                                }
                            }
                        </Show>
                    }
                }) />
            <div class="page-scroll">
                <div class="sys-page">
                    <QueryError q=status />
                    <Show when=move || no_backend.get()>
                        <div class="banner warn" role="alert"><Icon name="alert" />
                            <span>"No analysis backend is available, so BPM and key detection cannot run. The built-in analyzer (bc-rs-1) should always be present: check the server log."</span></div>
                    </Show>
                    <QueueCard status=status queue=queue eta=eta />
                    <CoverageCard status=status />
                    <ScanCard status=status scanning=scanning last_msg=last_msg scan=sc_body />
                    <AccuracyCard status=status />
                </div>
            </div>
        </div>
    }
}

#[component]
fn QueueCard(status: QH<AnalysisStatus>, queue: QH<AnalysisQueue>, eta: RwSignal<Option<f64>>) -> impl IntoView {
    let jobs = Signal::derive(move || queue.data.get().map(|q| q.jobs.clone()).unwrap_or_default());
    let agg = Signal::derive(move || aggregate(&jobs.get()));
    // Active jobs always, finished ones only as a short history.
    let shown_jobs = Signal::derive(move || {
        let mut finished = 0;
        jobs.get().into_iter().filter(|j| { if is_active_job(&j.status) { true } else { finished += 1; finished <= 3 } }).collect::<Vec<_>>()
    });
    let active = Signal::derive(move || jobs.with(|j| j.iter().any(|j| is_active_job(&j.status))));
    let frac = Signal::derive(move || {
        let (d, t) = agg.get();
        fraction(d, t)
    });
    let running = Signal::derive(move || status.data.get().map(|s| s.running_tracks).unwrap_or(0));
    let cancel = move |id: String| {
        spawn_local(async move {
            match api::post::<_, serde_json::Value>(&format!("/jobs/{id}/cancel"), &serde_json::json!({})).await {
                Ok(_) => {
                    queue.refetch();
                    status.refetch();
                }
                Err(e) => toast_err(&e.message()),
            }
        });
    };
    let items = Signal::derive(move || queue.data.get().map(|q| q.items.clone()).unwrap_or_default());
    view! {
        <SysCard title="Queue" icon="activity"
            actions=crate::ds::children(move || view! {
                <span class="mono faint" aria-live="polite">{move || if active.get() {
                    let (d, t) = agg.get();
                    format!("{} of {}", format_count(d), format_count(t))
                } else { "idle".to_string() }}</span>
            })>
            <Show when=move || queue.data.get().is_none() && queue.error.get().is_none()><Skeleton height="64px" /></Show>
            <Show when=move || active.get()>
                <Meter value=frac label="Analysis progress" />
                <div class="queue-stats mono faint">
                    <span>{move || format!("{} analysing", format_count(running.get()))}</span>
                    <span>{move || { let (d, t) = agg.get(); format!("{} waiting", format_count((t - d - running.get()).max(0))) }}</span>
                    <span>{move || frac.get().map(|f| format!("{}%", (f * 100.0).round())).unwrap_or_default()}</span>
                    {move || eta.get().filter(|e| *e > 0.0).map(|e| view! { <span>{format!("about {} left", format_eta(e))}</span> })}
                </div>
            </Show>
            <Show when=move || !active.get() && jobs.get().is_empty() && queue.data.get().is_some()>
                <EmptyState icon="check-circle" title="Nothing queued" hint="Analyse missing tracks, or new downloads are queued automatically." />
            </Show>
            <div class="job-list">
                <For each=move || shown_jobs.get() key=|j| j.id.clone() let:job>
                    <JobRow job_id=job.id.clone() jobs=jobs on_cancel=Callback::new(cancel.clone()) />
                </For>
            </div>
            <Show when=move || !items.get().is_empty()>
                <div class="queue-items" role="list" aria-label="Queue items" aria-live="off">
                    <For each=move || items.get() key=|i| i.id let:it>
                        <QueueItem item_id=it.id items=items />
                    </For>
                </div>
            </Show>
        </SysCard>
    }
}

#[component]
fn JobRow(job_id: String, jobs: Signal<Vec<AnalysisJobOut>>, on_cancel: Callback<String>) -> impl IntoView {
    let id = job_id.clone();
    let job = Signal::derive(move || jobs.with(|j| j.iter().find(|x| x.id == id).cloned()));
    let id2 = job_id.clone();
    view! {
        {move || job.get().map(|j| {
            let active = is_active_job(&j.status);
            let settled = j.completed + j.failed + j.skipped;
            let (tone, label) = match j.status.as_str() {
                "running" => (Tone::Info, "Running"),
                "queued" => (Tone::Neutral, "Queued"),
                "completed" => (Tone::Ok, "Done"),
                "failed" => (Tone::Danger, "Failed"),
                "cancelled" => (Tone::Neutral, "Cancelled"),
                _ => (Tone::Neutral, "Idle"),
            };
            let id3 = id2.clone();
            let name = j.label.clone().unwrap_or_else(|| j.id.clone());
            view! {
                <div class="job-row">
                    <div class="job-head">
                        <span class="grow truncate">{name.clone()}</span>
                        <span class="mono faint">{format!("{} / {}", format_count(settled), format_count(j.total))}</span>
                        {(j.failed > 0).then(|| view! { <span class="mono danger-text">{format!("{} failed", format_count(j.failed))}</span> })}
                        <StatusBadge tone=tone label=label />
                        {active.then(|| view! {
                            <button type="button" class="btn btn-ghost btn-icon btn-sm" title="Cancel job" aria-label=format!("Cancel {name}")
                                on:click=move |_| on_cancel.run(id3.clone())><Icon name="x" /></button>
                        })}
                    </div>
                    <Meter value=Some(j.progress) tone={if j.failed > 0 { Tone::Warn } else { Tone::Neutral }} label="Job progress" />
                </div>
            }
        })}
    }
}

#[component]
fn QueueItem(item_id: i64, items: Signal<Vec<AnalysisItemOut>>) -> impl IntoView {
    let item = Signal::derive(move || items.with(|v| v.iter().find(|i| i.id == item_id).cloned()));
    view! {
        {move || item.get().map(|it| {
            let (icon, cls, label) = match it.status.as_str() {
                "running" => ("refresh", "run", "Analysing"),
                "done" => ("check-circle", "ok", "Done"),
                "failed" => ("x-circle", "bad", "Failed"),
                "skipped" => ("skip-fwd", "idle", "Skipped"),
                "cancelled" => ("x", "idle", "Cancelled"),
                _ => ("clock", "idle", "Waiting"),
            };
            let name = it.title.clone().unwrap_or_else(|| format!("track {}", it.track_id.map(|t| t.to_string()).unwrap_or_else(|| "?".into())));
            let detail = if it.status == "failed" { it.last_error.clone().unwrap_or_else(|| "failed".into()) } else { it.message.clone().unwrap_or_default() };
            let detail_t = detail.clone();
            view! {
                <div class=format!("qi {cls}") role="listitem">
                    <span class="qi-icon" title=label><Icon name=icon /><span class="sr-only">{label}</span></span>
                    <span class="grow truncate">{name}{it.artist.clone().map(|a| view! { <span class="faint">{format!(" - {a}")}</span> })}</span>
                    <span class="mono qi-detail truncate" title=detail_t>{detail}</span>
                </div>
            }
        })}
    }
}

#[component]
fn CoverageCard(status: QH<AnalysisStatus>) -> impl IntoView {
    let cov = Signal::derive(move || status.data.get().map(|s| s.coverage));
    let tile = move |label: &'static str, f: fn(&AnalysisStatus) -> i64, tone: &'static str| {
        let v = Signal::derive(move || status.data.get().map(|s| format_count(f(&s))).unwrap_or_else(|| "-".into()));
        view! { <div class="sys-stat"><div class="k">{label}</div><div class=format!("v mono {tone}")>{move || v.get()}</div></div> }
    };
    view! {
        <SysCard title="Coverage" icon="layers"
            actions=crate::ds::children(move || view! {
                <span class="mono">{move || cov.get().map(|c| format!("{}%", (c * 100.0).round())).unwrap_or_default()}</span>
            })>
            <Meter value=cov label="Library analysed" />
            <div class="sys-stats">
                {tile("Analysed", |s| s.analysed, "ok-text")}
                {tile("Missing", |s| s.missing, "")}
                {tile("Failed", |s| s.failed, "danger-text")}
                {tile("Stale", |s| s.stale, "")}
                {tile("Tracks", |s| s.total_tracks, "")}
            </div>
        </SysCard>
    }
}

#[component]
fn ScanCard(
    status: QH<AnalysisStatus>,
    scanning: RwSignal<bool>,
    last_msg: RwSignal<Option<String>>,
    scan: std::sync::Arc<dyn Fn(ScanScope) + Send + Sync>,
) -> impl IntoView {
    let workers = Signal::derive(move || status.data.get().map(|s| (s.running_tracks, s.queued_tracks, s.running_jobs)));
    view! {
        <SysCard title="Engine and scans" icon="sliders"
            hint="Analysis runs on a background pool at lowered priority, so a backfill never delays playback or a download you are waiting for.">
            <ul class="backend-list">
                {move || status.data.get().map(|s| s.backends.iter().map(|(name, on)| {
                    let active = s.active_backend.as_deref() == Some(name.as_str());
                    view! {
                        <li>
                            {if *on { view! { <Icon name="check-circle" /> }.into_any() } else { view! { <Icon name="alert" /> }.into_any() }}
                            <span class="mono">{name.clone()}</span>
                            <span class="faint">{if *on { "available" } else { "not available" }}</span>
                            {active.then(|| view! { <Badge tone=Tone::Accent>"active"</Badge> })}
                        </li>
                    }
                }).collect_view())}
            </ul>
            {move || workers.get().map(|(run, queued, jobs)| view! {
                <div class="queue-stats mono faint">
                    <span>{format!("{} workers busy", format_count(run))}</span>
                    <span>{format!("{} waiting", format_count(queued))}</span>
                    <span>{format!("{} active job{}", jobs, if jobs == 1 { "" } else { "s" })}</span>
                </div>
            })}
            <div class="scope-grid">
                {SCOPES.iter().map(|(scope, label, desc)| {
                    let (s, scope) = (scan.clone(), *scope);
                    view! {
                        <button type="button" class="scope-btn" disabled=move || scanning.get() on:click=move |_| s(scope)>
                            <span class="strong">{format!("Analyse {}", label.to_lowercase())}</span>
                            <span class="faint">{*desc}</span>
                        </button>
                    }
                }).collect_view()}
            </div>
            {move || last_msg.get().map(|m| view! { <p class="sys-notice info" role="status"><Icon name="info" />{m}</p> })}
        </SysCard>
    }
}

/// Analyzer breakdown: the inputs of the accuracy report. The accuracy gate itself
/// (`bc analyze --gate`) is a CLI tool; the server exposes no accuracy endpoint yet.
#[component]
fn AccuracyCard(status: QH<AnalysisStatus>) -> impl IntoView {
    let acc = use_query::<bc_types::analysis::AccuracyOut>(|| Some(QuerySpec::new("/analysis/accuracy", &["analysis"])));
    let cache = use_query::<bc_types::analysis::WaveformCacheOut>(|| Some(QuerySpec::new("/analysis/waveform-cache", &["analysis"])));
    let rows = Signal::derive(move || status.data.get().map(|s| analyzer_rows(&s.by_analyzer)).unwrap_or_default());
    let version = Signal::derive(move || status.data.get().map(|s| s.analyzer_version).unwrap_or(0));
    view! {
        <SysCard title="Analyzer accuracy" icon="flask"
            hint="Where each track's BPM, key and loudness came from. Results imported from the old app (essentia) stay valid; the native analyzer (bc-rs-1) adds waveforms and beat grids. Re-analysing replaces an import only after the accuracy gate has compared them.">
            <Show when=move || rows.get().is_empty()>
                <p class="sys-empty">"No analysis results yet."</p>
            </Show>
            <ul class="analyzer-rows">
                <For each=move || rows.get() key=|r| (r.id.clone(), r.count) let:r>
                    <li>
                        <div class="ar-head">
                            <span class="strong">{r.label.clone()}</span>
                            <span class="mono faint">{r.id.clone()}</span>
                            <span class="spacer"></span>
                            <span class="mono">{format_count(r.count)}</span>
                            <span class="mono faint">{format!("{:.1}%", r.share * 100.0)}</span>
                        </div>
                        <Meter value=Some(r.share) label=r.label.clone() />
                        {(!r.detail.is_empty()).then(|| view! { <div class="faint ar-detail">{r.detail}</div> })}
                    </li>
                </For>
            </ul>
            <div class="row gap wrap faint">
                <Badge icon="info">{move || format!("analyzer version {}", version.get())}</Badge>
                {move || acc.data.get().map(|a| view! {
                    <Badge tone=native_tone(a.native_bpm_key) icon="check-circle">
                        {if a.native_bpm_key { "native BPM/key active" } else { "essentia BPM/key kept" }}</Badge>
                })}
            </div>
            {move || match acc.data.get().and_then(|a| a.report.clone()) {
                Some(r) => {
                    let row = |label: &'static str, v: f64, need: f64| {
                        let ok = v >= need;
                        view! { <li><div class="ar-head"><span class="strong">{label}</span><span class="spacer"></span>
                            <span class="mono">{format!("{:.1}%", v * 100.0)}</span>
                            <span class=if ok { "status ok" } else { "status danger" }><Icon name=if ok { "check-circle" } else { "x-circle" } />{if ok { format!("pass (>= {:.0}%)", need * 100.0) } else { format!("below {:.0}%", need * 100.0) }}</span></div>
                            <Meter value=Some(v) tone=ok_tone(ok) label=label /></li> }
                    };
                    view! {
                        <div class="section-title">"Accuracy gate vs the essentia import"
                            <span class="spacer"></span>
                            <span class=if r.passed { "status ok" } else { "status danger" }><Icon name=if r.passed { "check-circle" } else { "x-circle" } />{if r.passed { "gate passed" } else { "gate failed" }}</span></div>
                        <ul class="analyzer-rows">
                            {row("BPM within 0.5% (octave-equivalent)", r.bpm_within_0_5_pct_octave, 0.95)}
                            {row("Key exact", r.key_exact, 0.80)}
                            {row("Key exact or compatible neighbour", r.key_exact_or_compatible, 0.92)}
                        </ul>
                        <p class="sys-hint faint mono">{format!("{} tracks, {} failed, {:.1} tracks/s ({:.0}x realtime, {} threads), {}", r.n, r.failed, r.tracks_per_s, r.realtime_x, r.threads, r.generated_at)}</p>
                    }.into_any()
                }
                None => view! { <p class="sys-hint faint">"No accuracy report stored yet. Run the gate from a terminal: "<code class="mono">"bc analyze --gate"</code></p> }.into_any(),
            }}
            {move || cache.data.get().map(|c| {
                let used = if c.cap_bytes > 0 { c.bytes as f64 / c.cap_bytes as f64 } else { 0.0 };
                view! {
                    <div class="section-title">"Waveform cache"</div>
                    <div class="ar-head"><span class="mono">{format_bytes(c.bytes as f64)}</span><span class="faint">{format!(" of {} cap", format_bytes(c.cap_bytes as f64))}</span><span class="spacer"></span>
                        <span class="mono faint">{format!("{} files ({} with detail, {} overview only)", format_count(c.files as i64), format_count(c.detail_files as i64), format_count(c.overview_only_files as i64))}</span></div>
                    <Meter value=Some(used.clamp(0.0, 1.0)) tone=cache_tone(used) label="Waveform cache used" />
                }
            })}
        </SysCard>
    }
}

#[allow(dead_code)]
fn _size(_: Size) {}

fn native_tone(on: bool) -> Tone {
    if on { Tone::Accent } else { Tone::Neutral }
}
fn ok_tone(ok: bool) -> Tone {
    if ok { Tone::Ok } else { Tone::Danger }
}
fn cache_tone(used: f64) -> Tone {
    if used > 0.9 { Tone::Warn } else { Tone::Neutral }
}
