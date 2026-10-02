//! Downloads: URL form with preview, job cards with grouped items, pointer-event DnD reorder,
//! pause / resume / retry / cancel, disk-hold banner, per-item progress. The jobs come from the
//! live store (`data::use_jobs`), patched in place by `job.*` events: progress never refetches.
use std::sync::Arc;

use bc_types::bandcamp::{DownloadRequest, ParseUrlsRequest, ParsedUrls};
use bc_types::jobs::*;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::data::{JobsStore, QuerySpec, ws_connected, use_jobs, use_query, use_topic};
use crate::ds::{Button, EmptyState, Icon, Meter, PageHeader, Size, Skeleton, Variant, use_debounced};
use crate::logic::format::format_bytes;

mod items;
mod logic;

use items::{ItemStatus, JobItems};
use logic::*;

fn job_of(store: JobsStore, id: &str) -> Option<JobOut> {
    store.jobs.with(|v| v.iter().find(|j| j.id == id).cloned())
}

#[component]
pub fn DownloadsPage() -> impl IntoView {
    let store = use_jobs();
    let connected = ws_connected();
    let show_all = RwSignal::new(false);
    // `dnd::over()` / `dnd::active()` create their signals lazily under whichever owner calls first;
    // make that the page, which outlives every row (rows come and go on reload).
    let _ = (crate::widgets::dnd::over(), crate::widgets::dnd::active());

    // Ids only: cards read their own job, so a progress tick re-renders one card, not the list.
    let ids = Memo::new(move |_| {
        let all = show_all.get();
        store.jobs.with(|v| {
            let keep = |j: &&JobOut| all || j.kind != KIND_ANALYZE;
            let mut open: Vec<String> = v.iter().filter(keep).filter(|j| is_open(&j.status)).map(|j| j.id.clone()).collect();
            open.extend(v.iter().filter(keep).filter(|j| !is_open(&j.status)).map(|j| j.id.clone()));
            open
        })
    });
    let active = Signal::derive(move || store.jobs.with(|v| v.iter().filter(|j| is_active(&j.status)).count()));
    let finished = Signal::derive(move || store.jobs.with(|v| v.iter().filter(|j| j.status == JOB_COMPLETED && j.failed == 0).count()));
    let subtitle = Signal::derive(move || {
        if !store.loaded.get() {
            return String::new();
        }
        let n = ids.with(|i| i.len());
        format!("{n} job{}{}", if n == 1 { "" } else { "s" }, if active.get() > 0 { format!(" \u{b7} {} active", active.get()) } else { String::new() })
    });
    let clearing = RwSignal::new(false);
    let clear = move || {
        clearing.set(true);
        spawn_local(async move {
            match api::post::<_, ClearedOut>("/jobs/clear", &()).await {
                Ok(c) => {
                    crate::ds::toast_ok(&format!("Cleared {} finished job(s)", c.deleted));
                    store.reload().await;
                }
                Err(e) => crate::ds::toast_err(&e.message()),
            }
            clearing.set(false);
        });
    };

    view! {
        <div class="page dl-page">
            <PageHeader title="Downloads" subtitle=subtitle
                actions=crate::ds::children(move || view! {
                    {move || (finished.get() > 0).then(|| view! {
                        <Button size=Size::Sm icon="trash" busy=clearing title="Delete every job that finished clean. Anything with a failure to retry stays." on_click=move |_| clear()>
                            <span class="hide-sm">{move || format!("Clear {} finished", finished.get())}</span>
                        </Button>
                    })}
                    <span class=move || if connected.get() { "dl-live ok" } else { "dl-live warn" } title=move || if connected.get() { "Live event stream connected" } else { "Offline: reconnecting" }>
                        <span class="dot"></span><span class="hide-sm">{move || if connected.get() { "live" } else { "offline" }}</span>
                    </span>
                }) />
            <div class="page-scroll dl-scroll">
                <DiskBanner />
                <AddForm />
                <div class="dl-listhead">
                    <span class="section-title"><Icon name="download" />"Jobs"</span>
                    <label class="check small" title="Also list analysis jobs">
                        <input type="checkbox" prop:checked=move || show_all.get() on:change=move |ev| show_all.set(event_target_checked(&ev)) />
                        "include analysis jobs"
                    </label>
                </div>
                {move || if !store.loaded.get() {
                    view! { <div class="dl-list"><Skeleton height="68px" /><Skeleton height="68px" /></div> }.into_any()
                } else if ids.with(|i| i.is_empty()) {
                    view! { <EmptyState title="No downloads yet" hint="Paste some Bandcamp URLs above, or queue releases from Feed, Fans and Explore." icon="download" /> }.into_any()
                } else {
                    view! {
                        <div class="dl-list">
                            <For each=move || ids.get() key=|id| id.clone() let:id>
                                <JobCard id=id />
                            </For>
                        </div>
                    }.into_any()
                }}
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------------------------
// Disk hold banner
// ---------------------------------------------------------------------------------------------

#[component]
fn DiskBanner() -> impl IntoView {
    let store = use_jobs();
    let disk = RwSignal::new(None::<DiskOut>);
    let q = use_query::<DiskOut>(|| Some(QuerySpec::new("/downloads/disk", &["downloads"])));
    Effect::new(move |_| {
        if let Some(d) = q.data.get() {
            disk.set(Some((*d).clone()));
        }
    });
    use_topic::<DiskOut>(TOPIC_DOWNLOADS_DISK, move |d| disk.set(Some(d)));
    let held = Signal::derive(move || disk.with(|d| d.as_ref().map(|d| d.held)).unwrap_or_else(|| store.disk_held.get()));
    view! {
        {move || held.get().then(|| {
            let d = disk.get();
            view! {
                <div class="banner warn dl-hold" role="status">
                    <Icon name="pause-circle" />
                    <div class="grow">
                        <strong>"Downloads on hold: the disk is nearly full."</strong>
                        {d.map(|d| view! {
                            <div class="small muted">
                                <span class="mono">{d.free_bytes.map(|b| format_bytes(b as f64)).unwrap_or_else(|| "Unknown".into())}</span>" free on "
                                <span class="mono">{d.path}</span>", limit "<span class="mono">{format_bytes(d.min_free_bytes as f64)}</span>
                                ". Nothing new is fetched and in-flight albums were handed back to the queue; downloading resumes by itself once space is freed."
                            </div>
                        })}
                    </div>
                </div>
            }
        })}
    }
}

// ---------------------------------------------------------------------------------------------
// Add URLs
// ---------------------------------------------------------------------------------------------

fn new_job_id() -> String {
    let (a, b) = (crate::util::entropy(), crate::util::entropy());
    format!("{a:016x}{b:016x}")
}

#[component]
fn AddForm() -> impl IntoView {
    let store = use_jobs();
    let text = RwSignal::new(String::new());
    let name = RwSignal::new(String::new());
    let force = RwSignal::new(false);
    let single = RwSignal::new(false);
    let tracks_only = RwSignal::new(false);
    let submitting = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // Client-supplied so a resubmit (double click, flaky network) is idempotent.
    let job_id = StoredValue::new(new_job_id());

    let debounced = use_debounced(text, 300);
    let preview = RwSignal::new(None::<Result<ParsedUrls, String>>);
    let checking = RwSignal::new(false);
    let seq = StoredValue::new(0u64);
    Effect::new(move |_| {
        let t = debounced.get();
        seq.update_value(|s| *s += 1);
        let me = seq.get_value();
        if t.trim().is_empty() {
            preview.set(None);
            checking.set(false);
            return;
        }
        checking.set(true);
        spawn_local(async move {
            let r = api::post::<_, ParsedUrls>("/downloads/parse", &ParseUrlsRequest { text: t }).await;
            if seq.try_get_value() != Some(me) {
                return;
            }
            preview.set(Some(r.map_err(|e| e.message())));
            checking.set(false);
        });
    });

    // Counted locally so the submit path never depends on the preview request succeeding.
    let queue_count = Signal::derive(move || match preview.get() {
        Some(Ok(p)) if debounced.get() == text.get() => p.valid.len(),
        _ => local_url_count(&text.get()),
    });

    let submit = move || {
        if submitting.get_untracked() || queue_count.get_untracked() == 0 {
            return;
        }
        submitting.set(true);
        error.set(None);
        let req = DownloadRequest {
            urls: text.get_untracked().lines().map(str::to_string).collect(),
            target_subdir: Some(name.get_untracked()).filter(|s| !s.trim().is_empty()),
            force: force.get_untracked(),
            single_folder: single.get_untracked(),
            tracks_only: tracks_only.get_untracked(),
            job_id: Some(job_id.get_value()),
            ..Default::default()
        };
        spawn_local(async move {
            match api::post::<_, JobOut>("/downloads", &req).await {
                Ok(job) => {
                    crate::ds::toast_ok(&format!("Queued {} item(s)", job.total));
                    store.upsert(job);
                    text.set(String::new());
                    name.set(String::new());
                    force.set(false);
                    job_id.set_value(new_job_id());
                }
                Err(e) => error.set(Some(e.message())),
            }
            submitting.set(false);
        });
    };
    let submit = Arc::new(submit);
    let s1 = submit.clone();

    view! {
        <section class="card dl-add">
            <h2 class="section-title"><Icon name="plus" />"Add Bandcamp URLs"</h2>
            <textarea class="input dl-text mono" rows="4" spellcheck="false" aria-label="Bandcamp URLs, one per line"
                placeholder="https://artist.bandcamp.com/album/\u{2026}\nhttps://label.bandcamp.com/track/\u{2026}\nhttps://artist.bandcamp.com  (a band page queues its whole discography)"
                prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev))
                on:keydown=move |ev| if (ev.ctrl_key() || ev.meta_key()) && ev.key() == "Enter" { s1() }></textarea>
            {move || preview.get().map(|r| match r {
                Ok(p) => view! { <PreviewLine p=p force=force.get() /> }.into_any(),
                Err(_) => view! { <p class="small warn-text dl-prev"><Icon name="alert" />"Couldn't check these URLs (server unreachable): you can still queue them; the server will validate."</p> }.into_any(),
            })}
            <div class="dl-form-row">
                <input class="input dl-name" placeholder="Folder name (optional)" aria-label="Folder name" prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                <label class="check small"><input type="checkbox" prop:checked=move || force.get() on:change=move |ev| force.set(event_target_checked(&ev)) />"Re-download even if already in library"</label>
                <label class="check small" title="Every file directly in the folder, no artist/album subfolders"><input type="checkbox" prop:checked=move || single.get() on:change=move |ev| single.set(event_target_checked(&ev)) />"Single folder"</label>
                <label class="check small" title="Download a /track/ URL as that track instead of widening to its album"><input type="checkbox" prop:checked=move || tracks_only.get() on:change=move |ev| tracks_only.set(event_target_checked(&ev)) />"Tracks only"</label>
                <span class="spacer"></span>
                {let submit = submit.clone(); view! {
                    <Button variant=Variant::Primary icon="download" busy=submitting disabled=Signal::derive(move || queue_count.get() == 0) on_click=move |_| submit()>
                        {move || if submitting.get() { "Queueing\u{2026}".to_string() } else { format!("Queue {}", queue_count.get()) }}
                    </Button>
                }}
            </div>
            {move || error.get().map(|e| view! { <p class="small danger-text dl-prev"><Icon name="alert" />{e}</p> })}
            <p class="small faint">"Releases you bought come in the format chosen under Settings \u{203a} Downloads (when you are signed in to Bandcamp); everything else is Bandcamp's public stream."</p>
        </section>
    }
}

#[component]
fn PreviewLine(p: ParsedUrls, force: bool) -> impl IntoView {
    let invalid: Vec<String> = p.invalid.iter().take(3).cloned().collect();
    let extra = p.invalid.len().saturating_sub(3);
    view! {
        <div class="dl-prev" role="status" aria-live="polite">
            <span class="badge badge-ok"><Icon name="check" /><span class="mono">{p.valid.len()}</span>" valid"</span>
            {(p.albums > 0).then(|| view! { <span class="badge"><span class="mono">{p.albums}</span>" albums"</span> })}
            {(p.tracks > 0).then(|| view! { <span class="badge"><span class="mono">{p.tracks}</span>" tracks"</span> })}
            {(p.artists > 0).then(|| view! { <span class="badge badge-info"><span class="mono">{p.artists}</span>{format!(" band page{}: whole discography", if p.artists == 1 { "" } else { "s" })}</span> })}
            {(p.duplicates > 0).then(|| view! { <span class="badge"><span class="mono">{p.duplicates}</span>" duplicates removed"</span> })}
            {(p.already_have > 0 && !force).then(|| view! { <span class="badge badge-ok"><Icon name="disc" /><span class="mono">{p.already_have}</span>" already in library"</span> })}
            {(!p.invalid.is_empty()).then(|| view! { <span class="badge badge-warn"><Icon name="alert" /><span class="mono">{p.invalid.len()}</span>" unrecognised"</span> })}
            {(!invalid.is_empty()).then(|| view! {
                <div class="dl-invalid small faint mono">
                    {invalid.into_iter().map(|l| view! { <div class="truncate">{l}</div> }).collect_view()}
                    {(extra > 0).then(|| view! { <div>{format!("and {extra} more")}</div> })}
                </div>
            })}
        </div>
    }
}

// ---------------------------------------------------------------------------------------------
// Job card
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobAct {
    Pause,
    Resume,
    Cancel,
    Retry,
    Remove,
}

#[component]
fn JobCard(id: String) -> impl IntoView {
    let store = use_jobs();
    let job = {
        let id = id.clone();
        Signal::derive(move || job_of(store, &id))
    };
    let status = Memo::new(move |_| job.with(|j| j.as_ref().map(|j| j.status.clone()).unwrap_or_default()));
    let failed = Memo::new(move |_| job.with(|j| j.as_ref().map(|j| j.failed).unwrap_or(0)));
    let kind = Memo::new(move |_| job.with(|j| j.as_ref().map(|j| j.kind.clone()).unwrap_or_default()));
    let progress = Signal::derive(move || job.with(|j| j.as_ref().map(|j| j.progress)));
    let open = RwSignal::new(status.get_untracked() == JOB_RUNNING);
    let busy = RwSignal::new(None::<JobAct>);
    let error = RwSignal::new(None::<String>);
    let id_s = StoredValue::new(id.clone());

    let act = Callback::new(move |a: JobAct| {
        if busy.get_untracked().is_some() {
            return;
        }
        busy.set(Some(a));
        error.set(None);
        spawn_local(async move {
            let id = id_s.get_value();
            let r: Result<(), api::ApiErr> = match a {
                JobAct::Remove => api::call("DELETE", &format!("/jobs/{id}")).await.map(|_| store.jobs.update(|v| v.retain(|j| j.id != id))),
                _ => {
                    let verb = match a {
                        JobAct::Pause => "pause",
                        JobAct::Resume => "resume",
                        JobAct::Cancel => "cancel",
                        _ => "retry",
                    };
                    api::send::<(), JobOut>("POST", &format!("/jobs/{id}/{verb}"), &()).await.map(|j| store.upsert(j))
                }
            };
            if let Err(e) = r {
                error.set(Some(e.message()));
            }
            let _ = busy.try_set(None);
        });
    });
    let pending = move |a: JobAct| Signal::derive(move || busy.get() == Some(a));
    let any_busy = Signal::derive(move || busy.get().is_some());
    let is_open_now = Signal::derive(move || is_open(&status.get()));
    let moving = Signal::derive(move || is_active(&status.get()));
    let paused = Signal::derive(move || status.get() == JOB_PAUSED);
    let has_failed = Signal::derive(move || failed.get() > 0);
    let tone = move || match status.get().as_str() {
        JOB_FAILED => " danger",
        JOB_COMPLETED => " ok",
        JOB_PAUSED => " warn",
        _ => "",
    };
    let id_items = id.clone();

    view! {
        <article class=move || format!("card dl-job{}", tone()) data-status=move || status.get()>
            <div class="dl-job-head">
                <button type="button" class="dl-job-toggle" aria-expanded=move || open.get().to_string() aria-label=move || if open.get() { "Collapse items" } else { "Expand items" }
                    on:click=move |_| open.update(|o| *o = !*o)>
                    <Icon name=crate::ds::dyn_icon(move || if open.get() { "chevron-down" } else { "chevron-right" }) />
                </button>
                <div class="dl-job-main">
                    <div class="dl-job-title">
                        <span class="truncate dl-job-name">{move || job.with(|j| j.as_ref().map(|j| j.label.clone().unwrap_or_else(|| j.id.chars().take(8).collect())).unwrap_or_default())}</span>
                        {move || (kind.get() != KIND_DOWNLOAD).then(|| view! { <span class="badge">{kind.get()}</span> })}
                        <ItemStatus status=Signal::derive(move || status.get()) />
                    </div>
                    <div class="mono faint small">{move || job.with(|j| j.as_ref().map(counts_line).unwrap_or_default())}</div>
                </div>
                <div class="dl-job-meter">
                    <Meter value=progress label="Job progress" />
                </div>
                <div class="dl-job-acts">
                    {move || has_failed.get().then(|| view! { <Button size=Size::Sm variant=Variant::Ghost icon="refresh" title="Retry failed items" busy=pending(JobAct::Retry) disabled=any_busy on_click=move |_| act.run(JobAct::Retry)><span class="hide-md">"Retry"</span></Button> })}
                    {move || moving.get().then(|| view! { <Button size=Size::Sm variant=Variant::Ghost icon="pause" title="Pause: stops the downloads in flight too; resume picks them back up" busy=pending(JobAct::Pause) disabled=any_busy on_click=move |_| act.run(JobAct::Pause)><span class="hide-md">"Pause"</span></Button> })}
                    {move || paused.get().then(|| view! { <Button size=Size::Sm variant=Variant::Ghost icon="play" title="Resume" busy=pending(JobAct::Resume) disabled=any_busy on_click=move |_| act.run(JobAct::Resume)><span class="hide-md">"Resume"</span></Button> })}
                    {move || is_open_now.get().then(|| view! { <Button size=Size::Sm variant=Variant::Ghost icon="x" title="Cancel: stops what is downloading now as well" busy=pending(JobAct::Cancel) disabled=any_busy on_click=move |_| act.run(JobAct::Cancel)><span class="hide-md">"Cancel"</span></Button> })}
                    <Button size=Size::Sm variant=Variant::Ghost icon="trash" title="Remove from list (a job still going is cancelled on the way out)" busy=pending(JobAct::Remove) disabled=any_busy on_click=move |_| act.run(JobAct::Remove)><span class="hide-md">"Remove"</span></Button>
                </div>
            </div>
            {move || error.get().map(|e| view! { <p class="small danger-text dl-job-err"><Icon name="alert" />{e}</p> })}
            {move || open.get().then(|| view! { <JobItems job_id=id_items.clone() /> })}
        </article>
    }
}
