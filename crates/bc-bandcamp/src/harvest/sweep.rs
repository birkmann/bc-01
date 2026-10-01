//! Checking many Bandcamp pages for new releases in one pass (port of `services/harvest/sweep.py`).
//!
//! The per-source flow -- open the folder's or artist's menu, "Find new releases", press
//! Download -- answers for one of them. Asked of a shelf of hundreds it means hundreds of
//! rounds of menu, wait, button, so what a quiet label dropped last month goes unnoticed until
//! someone happens to open its folder.
//!
//! A sweeper walks every source that has a Bandcamp page pinned, absorbs each `/music` grid
//! into the inbox exactly as the per-source run does, and queues everything still missing as
//! one download job. Sources without a page are counted and skipped rather than located:
//! locating is a search plus up to four page fetches each and occasionally wrong, so it stays a
//! deliberate per-source action. The free evidence-based URL backfills do run first, so
//! anything the inbox can already prove a page for is swept rather than skipped.
//!
//! Two walks, differing only in what they walk:
//!
//! * **labels** -- the label shelf, whole or a hand-picked set of folders. A shelf of thousands
//!   is hours of polite fetching, so "these eight, now" is the form the feature usually wants;
//!   the whole-shelf run is that with nothing ticked.
//! * **favorites** -- everything pinned with a heart: the artists *and* the labels, since a
//!   favourite is a bookmark and the user does not think of the two as separate lists. Pinned
//!   *tags* are deliberately not swept: a tag is not a page, and "everything new tagged deep
//!   house" is the unbounded discover query the run limit exists to prevent, not a catalogue.
//!
//! As jobs: a sweep is a `sweep` job (`params.sweep = "labels" | "favorites"`, one item) run by
//! a `KindWorker`; the in-memory [`SweepStatus`] is the source of truth for the legacy
//! `GET /harvest/{labels,favorites}/sweep` routes and is published on `labels.sweep` /
//! `favorites.sweep`. The actual downloading is handed to the durable download queue, so a
//! restart loses at most the walk, never the queue. Stop = cancel the walk now, keep (and
//! queue) what it found.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bc_db::rusqlite::Connection;
use bc_jobs::{Complete, HandlerOutcome, ItemCtx, ItemHandler, KindWorker, NewItem, NewJob, WorkerSpec};
use bc_types::bandcamp::{SweepStatus, TOPIC_FAVORITES_SWEEP, TOPIC_LABELS_SWEEP};
use bc_types::jobs::KIND_SWEEP;
use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use super::artists::backfill_artist_urls;
use super::inbox::{self, AbsorbOpts, QueueOpts};
use super::labels::{LabelResolver, backfill_label_urls};
use crate::download::dedup::backfill_release_labels;
use crate::error::{HarvestError, Result};
use crate::net::BandcampClient;
use crate::service::Ctx;
use crate::sources::{self, EventStream};
use crate::urls;

/// Matches the per-source "Find new releases" action: a /music grid is the whole catalogue, and
/// the biggest shelves here run to four figures.
pub const PER_SOURCE_LIMIT: usize = 2000;

/// SQLite takes 999 bound parameters; a selection is chunked well under it because "select all"
/// on the shelf hands over a page of five hundred.
pub const ID_CHUNK: usize = 400;

/// Concurrent sweep-kind jobs (a label sweep and a favourites sweep may overlap).
pub const SWEEP_CONCURRENCY: usize = 2;

/// How a sweeper obtains a page's discography stream: `(client, url, depth, limit)`. Defaults
/// to [`sources::harvest_artist`]; tests substitute fixed grids (the Python `monkeypatch`).
pub type ArtistStreamFn = Arc<dyn Fn(BandcampClient, String, String, Option<usize>) -> EventStream + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepKind {
    Labels,
    Favorites,
}

impl SweepKind {
    pub fn param(self) -> &'static str {
        match self {
            Self::Labels => "labels",
            Self::Favorites => "favorites",
        }
    }
    pub fn topic(self) -> &'static str {
        match self {
            Self::Labels => TOPIC_LABELS_SWEEP,
            Self::Favorites => TOPIC_FAVORITES_SWEEP,
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s {
            "labels" => Some(Self::Labels),
            "favorites" => Some(Self::Favorites),
            _ => None,
        }
    }
}

/// One Bandcamp page to check, and what kind of thing lives on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub url: Option<String>,
    /// `artist` | `label`
    pub kind: &'static str,
}

impl Target {
    /// The imprint every release on this page belongs to, if any.
    ///
    /// Set for a label and never for an artist: everything on a label's own page is on that
    /// label, and the `/music` grid never says so per item, so this is the strongest label
    /// evidence there is. An *artist's* page says nothing of the kind -- their records come out
    /// on other people's imprints, and filing them under a label named after the artist would
    /// invent a shelf that does not exist.
    pub fn label_name(&self) -> Option<String> {
        (self.kind == "label").then(|| self.name.clone())
    }
}

fn idle() -> SweepStatus {
    SweepStatus { phase: "idle".into(), scope: "all".into(), ..Default::default() }
}

fn running(s: &SweepStatus) -> bool {
    matches!(s.phase.as_str(), "harvesting" | "queueing")
}

/// How a run ended, for the job item.
#[derive(Debug)]
pub enum RunEnd {
    Done,
    Failed(String),
    /// Interrupted from outside (job cancel/pause) rather than by the sweeper's own stop.
    Interrupted,
}

/// Runs at most one sweep of its kind at a time and reports where it has got to.
pub struct Sweeper {
    ctx: Arc<Ctx>,
    kind: SweepKind,
    state: Mutex<SweepStatus>,
    /// Cancelled by `stop()`; replaced per run.
    stop: Mutex<CancellationToken>,
    stop_requested: Mutex<bool>,
    streamer: Arc<RwLock<ArtistStreamFn>>,
}

pub struct SweepService {
    pub labels: Arc<Sweeper>,
    pub favorites: Arc<Sweeper>,
    streamer: Arc<RwLock<ArtistStreamFn>>,
    worker: Arc<KindWorker>,
}

pub fn init(ctx: &Arc<Ctx>) {
    let streamer: Arc<RwLock<ArtistStreamFn>> = Arc::new(RwLock::new(Arc::new(|client, url, depth, limit| {
        sources::harvest_artist(client, url, depth, limit)
    })));
    let mk = |kind| {
        Arc::new(Sweeper {
            ctx: ctx.clone(),
            kind,
            state: Mutex::new(idle()),
            stop: Mutex::new(CancellationToken::new()),
            stop_requested: Mutex::new(false),
            streamer: streamer.clone(),
        })
    };
    let worker = KindWorker::new(
        ctx.jobs.store().clone(),
        WorkerSpec::new(KIND_SWEEP, SWEEP_CONCURRENCY),
        Arc::new(SweepHandler { ctx: ctx.clone() }),
    );
    ctx.jobs.add_hooks(worker.clone());
    ctx.put(Arc::new(SweepService { labels: mk(SweepKind::Labels), favorites: mk(SweepKind::Favorites), streamer, worker }));
}

pub async fn start(ctx: &Arc<Ctx>) {
    ctx.expect::<SweepService>().worker.start();
}

impl SweepService {
    pub fn get(&self, kind: SweepKind) -> &Arc<Sweeper> {
        match kind {
            SweepKind::Labels => &self.labels,
            SweepKind::Favorites => &self.favorites,
        }
    }

    /// Substitute how discography pages are streamed (tests).
    pub fn set_artist_stream(&self, f: ArtistStreamFn) {
        *self.streamer.write() = f;
    }
}

struct SweepHandler {
    ctx: Arc<Ctx>,
}

#[async_trait]
impl ItemHandler for SweepHandler {
    async fn run(&self, ic: ItemCtx) -> HandlerOutcome {
        let params = ic.job.params_json();
        let which = params.get("sweep").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if which == "resolve" {
            let resolver = self.ctx.expect::<LabelResolver>();
            return if resolver.run(&ic.cancel).await {
                let s = resolver.status();
                HandlerOutcome::Done(Complete { message: Some(format!("{} labelled", s.labelled)), result: serde_json::to_value(&s).ok(), release_id: None })
            } else {
                HandlerOutcome::Interrupted
            };
        }
        if which == "feed" {
            // One worker for the shared `sweep` kind: feed items belong to `feed.rs`.
            return crate::harvest::feed::FeedSweepHandler(self.ctx.expect::<crate::harvest::feed::FeedSweeper>()).run(ic).await;
        }
        let Some(kind) = SweepKind::parse(&which) else {
            return HandlerOutcome::Failed { error: format!("unknown sweep {which:?}"), class: "internal".into(), retryable: false };
        };
        let ids: Option<Vec<i64>> = params
            .get("ids")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_i64()).collect::<Vec<_>>())
            .filter(|v| !v.is_empty());
        let sweeper = self.ctx.expect::<SweepService>().get(kind).clone();
        match sweeper.run(ids, &ic.cancel).await {
            RunEnd::Done => {
                let s = sweeper.status();
                HandlerOutcome::Done(Complete {
                    message: Some(format!("{} queued", s.queued)),
                    result: serde_json::to_value(&s).ok(),
                    release_id: None,
                })
            }
            RunEnd::Failed(e) => HandlerOutcome::Failed { error: e, class: "internal".into(), retryable: false },
            RunEnd::Interrupted => HandlerOutcome::Interrupted,
        }
    }
}

impl Sweeper {
    pub fn status(&self) -> SweepStatus {
        self.state.lock().clone()
    }

    fn publish(&self, f: impl FnOnce(&mut SweepStatus)) {
        let snapshot = {
            let mut s = self.state.lock();
            f(&mut s);
            s.running = running(&s);
            s.clone()
        };
        self.ctx.bus.publish(self.kind.topic(), &snapshot);
    }

    /// Kick off a sweep (as a job) and return immediately.
    ///
    /// `ids` narrows the walk to a hand-picked subset, when the sweeper supports one; `None`
    /// walks everything it knows about. An empty list is the same as `None` rather than a sweep
    /// of nothing, so a stray press with the selection already cleared does what the button
    /// says. Errs when a sweep is already running (a second would re-fetch every page again).
    pub async fn start(&self, ids: Option<&[i64]>) -> Result<SweepStatus> {
        let mut picked: Vec<i64> = ids.map(<[i64]>::to_vec).unwrap_or_default();
        picked.sort_unstable();
        picked.dedup();
        {
            let mut s = self.state.lock();
            if running(&s) {
                return Err(HarvestError::other("a sweep is already running"));
            }
            *s = SweepStatus {
                phase: "harvesting".into(),
                scope: if picked.is_empty() { "all" } else { "selection" }.into(),
                running: true,
                started_at: Some(bc_db::util::iso_now()),
                ..Default::default()
            };
            *self.stop.lock() = CancellationToken::new();
            *self.stop_requested.lock() = false;
        }
        let snapshot = self.status();
        self.ctx.bus.publish(self.kind.topic(), &snapshot);

        let label = match self.kind {
            SweepKind::Labels => "Check labels for new releases",
            SweepKind::Favorites => "Check favourites for new releases",
        };
        let nj = NewJob::new(KIND_SWEEP, vec![NewItem { source: Some(self.kind.param().into()), ..Default::default() }])
            .label(label)
            .params(serde_json::json!({"sweep": self.kind.param(), "ids": picked}));
        let store = self.ctx.jobs.store().clone();
        if let Err(e) = store.run(move |s| s.create_job(nj)).await {
            self.publish(|s| {
                s.phase = "failed".into();
                s.error = Some(e.to_string());
                s.finished_at = Some(bc_db::util::iso_now());
            });
            return Err(e.into());
        }
        Ok(self.status())
    }

    /// Stop the walk, keeping whatever it has already found. Returns once the run has wound up,
    /// so the state the caller reports is the finished one rather than a sweep still counting.
    pub async fn stop(&self) -> SweepStatus {
        if running(&self.status()) {
            *self.stop_requested.lock() = true;
            self.stop.lock().cancel();
            // Wait for the run to queue what it found and settle (a job that was never claimed
            // gets the Cancelled state below instead).
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            while running(&self.status()) && tokio::time::Instant::now() < deadline {
                if self.ctx.jobs.store().run({
                    let kind = self.kind.param();
                    move |s| {
                        let page = s.list_jobs(Some("queued"), Some(KIND_SWEEP), 0, 50)?;
                        Ok(page.items.iter().any(|j| j.params_json().get("sweep").and_then(|v| v.as_str()) == Some(kind)))
                    }
                })
                .await
                .unwrap_or(false)
                {
                    // Still unclaimed: nothing is in flight to wind up.
                    self.publish(|s| {
                        s.phase = "failed".into();
                        s.error = Some("Cancelled".into());
                        s.finished_at = Some(bc_db::util::iso_now());
                    });
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        self.status()
    }

    /// The body of the sweep job. Stopping means "stop fetching pages", not "throw away what you
    /// found": the walk is the slow half, and the releases already absorbed are queued below. A
    /// press of Stop then still leaves you with the downloads the sweep had turned up by then.
    pub async fn run(&self, ids: Option<Vec<i64>>, outer: &CancellationToken) -> RunEnd {
        {
            // A job re-claimed after a restart starts from a fresh in-memory state.
            let mut s = self.state.lock();
            if !running(&s) {
                *s = SweepStatus {
                    phase: "harvesting".into(),
                    scope: if ids.is_some() { "selection" } else { "all" }.into(),
                    running: true,
                    started_at: Some(bc_db::util::iso_now()),
                    ..Default::default()
                };
                *self.stop.lock() = CancellationToken::new();
                *self.stop_requested.lock() = false;
            }
        }
        // One token for "stop pressed" or "job cancelled/paused".
        let merged = CancellationToken::new();
        let watcher = {
            let (m, stop, outer) = (merged.clone(), self.stop.lock().clone(), outer.clone());
            tokio::spawn(async move {
                tokio::select! { _ = stop.cancelled() => {}, _ = outer.cancelled() => {} }
                m.cancel();
            })
        };
        let out = self.run_inner(ids, &merged).await;
        watcher.abort();
        match out {
            Ok(()) => {
                if outer.is_cancelled() && !*self.stop_requested.lock() {
                    RunEnd::Interrupted
                } else {
                    RunEnd::Done
                }
            }
            Err(e) => {
                tracing::warn!("{} failed: {e}", self.kind.topic());
                let msg: String = e.to_string().chars().take(500).collect();
                let shown = if matches!(e, HarvestError::Db(_) | HarvestError::Io(_)) { format!("Internal error: {msg}") } else { msg.clone() };
                self.publish(|s| {
                    s.phase = "failed".into();
                    s.error = Some(shown);
                    s.finished_at = Some(bc_db::util::iso_now());
                });
                RunEnd::Failed(msg)
            }
        }
    }

    async fn run_inner(&self, ids: Option<Vec<i64>>, cancel: &CancellationToken) -> Result<()> {
        let db = &self.ctx.db;
        let this_kind = self.kind;
        let ids2 = ids.clone();
        // Free, evidence-based URL repair first (every page filled in is a source swept instead
        // of skipped, at no network cost), then the targets, in one write so it is atomic.
        let rows: Vec<Target> = db
            .write_async(move |tx| {
                backfill_label_urls(tx)?;
                if this_kind == SweepKind::Favorites {
                    // Both backfills: the strip mixes the two, and an artist pinned from a page
                    // the library can already prove is one more artist swept, not skipped.
                    backfill_artist_urls(tx)?;
                }
                targets_for(this_kind, tx, ids2.as_deref())
            })
            .await?;
        let targets: Vec<&Target> = rows.iter().filter(|t| t.url.is_some()).collect();
        let (total, no_url) = (targets.len() as i64, (rows.len() - targets.len()) as i64);
        self.publish(|s| {
            s.total = Some(total);
            s.no_url = no_url;
        });

        let client = self.ctx.client.clone();
        let mut pending_ids: Vec<i64> = Vec::new();
        let mut stopped = false;
        for (i, target) in targets.iter().enumerate() {
            if cancel.is_cancelled() {
                stopped = true;
                break;
            }
            let Some(url) = target.url.clone() else { continue };
            let name = target.name.clone();
            self.publish(|s| s.current = Some(name.clone()));
            // An artist page and a label page are the same grid; only what it proves about the
            // releases on it differs -- see Target::label_name.
            let stream = (self.streamer.read().clone())(client.clone(), url.clone(), "shallow".into(), Some(PER_SOURCE_LIMIT));
            let mut opts = AbsorbOpts::new(target.kind, urls::display_name(&url));
            opts.label_name = target.label_name();
            let absorbed = inbox::absorb(db, stream, &opts, None, cancel).await;
            let c = absorbed.counts;
            pending_ids.extend(c.pending_ids.iter().copied());
            if absorbed.cancelled {
                // Stopped while this page was being fetched: keep what it had yielded, but the
                // source itself did not finish, so it is not counted as done.
                self.publish(|s| {
                    s.seen += c.seen;
                    s.new += c.new;
                    s.in_library += c.in_library;
                });
                stopped = true;
                break;
            }
            let done = i as i64 + 1;
            if let Some(err) = absorbed.error {
                // One unreachable page must not kill a shelf-long sweep; that source just stays
                // unchecked this run.
                tracing::info!("{}: {} failed: {err}", self.kind.topic(), target.name);
                let line: String = format!("{}: {err}", target.name).chars().take(300).collect();
                self.publish(|s| {
                    s.done = done;
                    s.seen += c.seen;
                    s.new += c.new;
                    s.in_library += c.in_library;
                    s.errors.push(line);
                });
                continue;
            }
            self.publish(|s| {
                s.done = done;
                s.seen += c.seen;
                s.new += c.new;
                s.in_library += c.in_library;
                s.errors.extend(c.errors.iter().take(3).cloned());
            });
        }
        if cancel.is_cancelled() {
            stopped = true;
        }

        self.publish(|s| {
            s.phase = "queueing".into();
            s.current = None;
        });

        pending_ids.sort_unstable();
        pending_ids.dedup();
        let pending = db
            .read_async(move |c| {
                let mut out = Vec::new();
                // Chunked under SQLite's parameter limit; the first sweep of a big shelf can
                // turn up five figures of items.
                for chunk in pending_ids.chunks(ID_CHUNK) {
                    out.extend(inbox::load_rows(c, chunk).map_err(|e| bc_db::DbError::Other(e.to_string()))?.into_iter().filter(|r| r.state == "new"));
                }
                Ok(out)
            })
            .await?;
        let (dbh, store) = (db.clone(), self.ctx.jobs.store().clone());
        let outcome = tokio::task::spawn_blocking(move || {
            inbox::queue(
                &dbh,
                &store,
                &pending,
                // Same flags as the per-source Download button: finding unowned releases was
                // the point, and a subfolder would duplicate artists already at the downloads
                // root.
                &QueueOpts { allow_unowned: true, target_subdir: Some(String::new()), ..Default::default() },
            )
        })
        .await
        .map_err(|e| HarvestError::other(e.to_string()))??;

        // File the fresh arrivals under their labels now rather than at the next restart.
        db.write_async(|tx| backfill_release_labels(tx)).await?;

        self.publish(|s| {
            s.phase = "done".into();
            // Done, but not finished: the report has to say so, or a walk stopped at source nine
            // of two hundred reads as a shelf with almost nothing new on it.
            s.error = stopped.then(|| "Stopped".to_string());
            s.queued = outcome.queued;
            s.job_id = outcome.job_id.clone();
            s.finished_at = Some(bc_db::util::iso_now());
        });
        if outcome.queued > 0 {
            self.ctx.notify_downloads();
        }
        Ok(())
    }
}

fn targets_for(kind: SweepKind, c: &Connection, ids: Option<&[i64]>) -> bc_db::Result<Vec<Target>> {
    let label_rows = |sql: &str, bind: Vec<i64>| -> bc_db::Result<Vec<Target>> {
        let mut st = c.prepare(sql)?;
        let rows = st.query_map(bc_db::rusqlite::params_from_iter(bind.iter()), |r| {
            Ok(Target { name: r.get(0)?, url: r.get(1)?, kind: "label" })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    };
    match kind {
        SweepKind::Labels => match ids {
            None => label_rows("SELECT name, bandcamp_url FROM labels ORDER BY name", vec![]),
            Some(ids) => {
                let mut rows = Vec::new();
                for chunk in ids.chunks(ID_CHUNK) {
                    let ph = vec!["?"; chunk.len()].join(",");
                    rows.extend(label_rows(&format!("SELECT name, bandcamp_url FROM labels WHERE id IN ({ph}) ORDER BY name"), chunk.to_vec())?);
                }
                // Chunking split the ORDER BY; the progress line reads as a walk down the shelf,
                // so put the chunks back in one order.
                rows.sort_by_key(|t| t.name.to_lowercase());
                Ok(rows)
            }
        },
        // Every pinned artist and label, artists first, each alphabetical. `ids` is ignored: a
        // favourite is a pin, not a selection, and an id would be ambiguous between the two
        // tables anyway. Pinned *tags* are skipped and not counted: they are not sources.
        SweepKind::Favorites => {
            let mut out: Vec<Target> = {
                let mut st =
                    c.prepare("SELECT a.name, a.bandcamp_url FROM artists a JOIN favorites f ON f.artist_id = a.id ORDER BY a.name")?;
                st.query_map([], |r| Ok(Target { name: r.get(0)?, url: r.get(1)?, kind: "artist" }))?.collect::<std::result::Result<_, _>>()?
            };
            out.extend(label_rows("SELECT l.name, l.bandcamp_url FROM labels l JOIN favorites f ON f.label_id = l.id ORDER BY l.name", vec![])?);
            Ok(out)
        }
    }
}
