//! Finding each release's Bandcamp page again.
//!
//! A release's `bandcamp_url` is what everything Bandcamp-aware in the library hangs off: the
//! label resolver files releases by the host their page lives on, the artist backfill pins
//! artists to their own pages, and "find new releases" starts from both. Records downloaded
//! through the app get it from the job that fetched them, but a library rebuilt from a rescan
//! has none -- bandcamp-dl's MP3s carry no URL and no publisher tag -- so the label shelf
//! collapses to the handful of records whose files happen to name a label.
//!
//! This walks the releases without a page and asks Bandcamp's search for each, by title and
//! artist. A hit is taken only when the title folds to the release's own title *and* the byline
//! is the release's artist (or one side contains the other, for "A & B" against "A"): search
//! happily returns a stranger's record of the same name, and a wrong URL would file the release
//! under a stranger's label. Anything less certain stays unlinked.
//!
//! One search per release through the shared, rate-limited client: a five-figure library is a
//! long night, so every release searched is recorded in `release_relink` and a stopped or
//! restarted run carries on where it was. Labels follow as the run goes: the label resolver is
//! kicked every [`KICK_EVERY`] links and once more at the end.
//!
//! As a job: a `relink` job with one item, run by its own `KindWorker` so the hours it takes
//! never hold a slot the label and feed sweeps need. The in-memory [`RelinkStatus`] is the
//! source of truth for `GET /harvest/relink` and is published as `harvest.relink`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bc_db::rusqlite::{Connection, OptionalExtension, params};
use bc_db::util::{name_key, now_db};
use bc_jobs::{Complete, HandlerOutcome, ItemCtx, ItemHandler, KindWorker, NewItem, NewJob, WorkerSpec};
use bc_types::bandcamp::{RelinkStatus, TOPIC_HARVEST_RELINK, TOPIC_LIBRARY_CHANGED};
use bc_types::jobs::KIND_RELINK;
use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use super::artists::backfill_artist_urls;
use super::labels::{LabelResolver, PageSource};
use crate::error::{HarvestError, Result};
use crate::extract;
use crate::service::Ctx;
use crate::sources::SearchHit;

/// Search hits read per release: the record is nearly always among the first few.
const SEARCH_LIMIT: usize = 10;

/// Kick the label resolver after this many new links, so labels fill in while the walk goes on.
pub const KICK_EVERY: i64 = 500;

/// Consecutive failed searches that end the run: the network or Bandcamp is down, and grinding
/// through the rest would only mark nothing.
const MAX_FAILURES_IN_A_ROW: usize = 25;

/// A release still without its Bandcamp page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub id: i64,
    pub title: String,
    pub artist: String,
    pub tracks: i64,
}

/// Releases with no `bandcamp_url` that no earlier run has searched for, oldest first.
pub fn pending(c: &Connection) -> bc_db::Result<Vec<Pending>> {
    let mut st = c.prepare(
        "SELECT r.id, r.title, coalesce(a.name, ''), (SELECT count(*) FROM tracks t WHERE t.release_id = r.id)
           FROM releases r LEFT JOIN artists a ON a.id = r.artist_id
          WHERE r.bandcamp_url IS NULL
            AND NOT EXISTS (SELECT 1 FROM release_relink x WHERE x.release_id = r.id)
          ORDER BY r.id",
    )?;
    let rows = st.query_map([], |r| Ok(Pending { id: r.get(0)?, title: r.get(1)?, artist: r.get(2)?, tracks: r.get(3)? }))?;
    let mut out = Vec::new();
    for p in rows {
        let p = p?;
        if !name_key(&p.title).is_empty() {
            out.push(p);
        }
    }
    Ok(out)
}

/// A name folded down to its letters and digits: "Sebo K" and "SEBO-K" are one byline.
fn compact(s: &str) -> String {
    name_key(s).chars().filter(|c| !c.is_whitespace()).collect()
}

fn is_various(artist: &str) -> bool {
    matches!(compact(artist).as_str(), "variousartists" | "va" | "various")
}

/// The text to search for: title and artist, or the title alone for a compilation (Bandcamp
/// bylines a compilation with the label, never "Various Artists").
pub fn query(p: &Pending) -> String {
    if p.artist.trim().is_empty() || is_various(&p.artist) {
        p.title.trim().to_string()
    } else {
        format!("{} {}", p.title.trim(), p.artist.trim())
    }
}

/// How well a hit's byline names the release's artist: 2 the same, 1 close, 0 someone else.
fn byline_score(artist: &str, byline: &str) -> u8 {
    let (a, b) = (compact(artist), compact(byline));
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    if a == b {
        return 2;
    }
    // "Niereich" against "NIEREICH & Friends", or a credit list against its first name.
    let contains = (a.chars().count() >= 4 && b.contains(&a)) || (b.chars().count() >= 4 && a.contains(&b));
    let parts = extract::artist_parts(artist);
    let shared = extract::artist_parts(byline).iter().any(|p| parts.contains(p));
    u8::from(contains || shared)
}

/// The hit that is this release, if one is certain enough to link.
///
/// The title must fold to the release's own title; the byline must be its artist (a
/// compilation, which Bandcamp credits to the label, may match on title alone). An exact byline
/// beats a close one; between equals the search's own order decides.
pub fn pick<'a>(hits: &'a [SearchHit], title: &str, artist: &str) -> Option<&'a SearchHit> {
    let tk = name_key(title);
    if tk.is_empty() {
        return None;
    }
    let various = is_various(artist);
    let mut best: Option<(u8, &SearchHit)> = None;
    for h in hits {
        if !matches!(h.kind.as_str(), "album" | "track") || name_key(&h.name) != tk || !h.url.contains("://") {
            continue;
        }
        let score = match byline_score(artist, &h.subtitle) {
            0 if various => 1,
            s => s,
        };
        if score > 0 && best.is_none_or(|(s, _)| score > s) {
            best = Some((score, h));
        }
    }
    best.map(|(_, h)| h)
}

/// Record a search for release `id`: link it to `hit` when given (and the page is not already
/// another release's), and remember it was tried either way. Returns whether it was linked.
pub fn record(c: &Connection, id: i64, hit: Option<&SearchHit>) -> bc_db::Result<bool> {
    let mut linked = None;
    if let Some(h) = hit {
        // `bandcamp_url` is unique: a page already held by another release is that release's
        // twin (two folders of one record), which the folder-twin merge settles, not this.
        let taken: Option<i64> =
            c.query_row("SELECT id FROM releases WHERE bandcamp_url = ?1 AND id != ?2", params![h.url, id], |r| r.get(0)).optional()?;
        if taken.is_none() {
            let item_id = if h.kind == "album" { h.item_id } else { None };
            let n = c.execute(
                "UPDATE releases SET bandcamp_url = ?2, bandcamp_item_id = coalesce(bandcamp_item_id, ?3)
                  WHERE id = ?1 AND bandcamp_url IS NULL",
                params![id, h.url, item_id],
            )?;
            if n > 0 {
                linked = Some(h.url.clone());
            }
        }
    }
    c.execute(
        "INSERT OR REPLACE INTO release_relink(release_id, tried_at, url) VALUES (?1, ?2, ?3)",
        params![id, now_db(), linked],
    )?;
    Ok(linked.is_some())
}

/// Find one release's page: albums first, then tracks for a single-track release (a track
/// bought on its own lives at `/track/...`).
async fn find(src: &dyn PageSource, p: &Pending) -> Result<Option<SearchHit>> {
    let q = query(p);
    let hits = src.search(&q, "a", SEARCH_LIMIT).await?;
    if let Some(h) = pick(&hits, &p.title, &p.artist) {
        return Ok(Some(h.clone()));
    }
    if p.tracks == 1 {
        let hits = src.search(&q, "t", SEARCH_LIMIT).await?;
        return Ok(pick(&hits, &p.title, &p.artist).cloned());
    }
    Ok(None)
}

// ---------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------

fn idle() -> RelinkStatus {
    RelinkStatus { phase: "idle".into(), ..Default::default() }
}

fn fresh_running() -> RelinkStatus {
    RelinkStatus { phase: "running".into(), running: true, started_at: Some(bc_db::util::iso_now()), ..Default::default() }
}

/// Runs at most one relink at a time and reports where it has got to.
pub struct Relinker {
    ctx: Arc<Ctx>,
    state: Mutex<RelinkStatus>,
    stop: Mutex<CancellationToken>,
    stop_requested: Mutex<bool>,
    source: RwLock<Option<Arc<dyn PageSource>>>,
    worker: Arc<KindWorker>,
}

pub fn init(ctx: &Arc<Ctx>) {
    let worker = KindWorker::new(ctx.jobs.store().clone(), WorkerSpec::new(KIND_RELINK, 1), Arc::new(RelinkHandler { ctx: ctx.clone() }));
    ctx.jobs.add_hooks(worker.clone());
    ctx.put(Arc::new(Relinker {
        ctx: ctx.clone(),
        state: Mutex::new(idle()),
        stop: Mutex::new(CancellationToken::new()),
        stop_requested: Mutex::new(false),
        source: RwLock::new(None),
        worker,
    }));
}

pub async fn start(ctx: &Arc<Ctx>) {
    ctx.expect::<Relinker>().worker.start();
}

struct RelinkHandler {
    ctx: Arc<Ctx>,
}

#[async_trait]
impl ItemHandler for RelinkHandler {
    async fn run(&self, ic: ItemCtx) -> HandlerOutcome {
        let relinker = self.ctx.expect::<Relinker>();
        match relinker.run(&ic.cancel).await {
            Ok(true) => {
                let s = relinker.status();
                HandlerOutcome::Done(Complete {
                    message: Some(format!("{} of {} linked", s.linked, s.seen)),
                    result: serde_json::to_value(&s).ok(),
                    release_id: None,
                })
            }
            Ok(false) => HandlerOutcome::Interrupted,
            Err(e) => HandlerOutcome::Failed { error: e.to_string(), class: "internal".into(), retryable: false },
        }
    }
}

impl Relinker {
    pub fn status(&self) -> RelinkStatus {
        self.state.lock().clone()
    }

    /// Replace the search source (tests; the default is the shared client).
    pub fn set_source(&self, src: Arc<dyn PageSource>) {
        *self.source.write() = Some(src);
    }

    fn source(&self) -> Arc<dyn PageSource> {
        self.source.read().clone().unwrap_or_else(|| Arc::new(self.ctx.client.clone()))
    }

    fn publish(&self, f: impl FnOnce(&mut RelinkStatus)) {
        let snapshot = {
            let mut s = self.state.lock();
            f(&mut s);
            s.running = s.phase == "running";
            s.clone()
        };
        self.ctx.bus.publish(TOPIC_HARVEST_RELINK, &snapshot);
    }

    /// Kick off a relink as a job and return immediately. Errs when one is already running (a
    /// second would double the request rate for nothing).
    pub async fn start_run(&self) -> Result<RelinkStatus> {
        {
            let mut s = self.state.lock();
            if s.phase == "running" {
                return Err(HarvestError::other("the library is already being linked to Bandcamp"));
            }
            *s = fresh_running();
            *self.stop.lock() = CancellationToken::new();
            *self.stop_requested.lock() = false;
        }
        self.ctx.bus.publish(TOPIC_HARVEST_RELINK, &self.status());
        let nj = NewJob::new(KIND_RELINK, vec![NewItem { source: Some("relink".into()), ..Default::default() }])
            .label("Link library to Bandcamp")
            .params(serde_json::json!({}));
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

    /// Stop now, keeping every link already made; the next run resumes after them.
    pub async fn stop(&self) -> RelinkStatus {
        if self.state.lock().phase == "running" {
            *self.stop_requested.lock() = true;
            self.stop.lock().cancel();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            while self.state.lock().phase == "running" && tokio::time::Instant::now() < deadline {
                let queued =
                    self.ctx.jobs.store().run(|s| Ok(s.list_jobs(Some("queued"), Some(KIND_RELINK), 0, 1)?.total > 0)).await.unwrap_or(false);
                if queued {
                    // Never claimed: nothing is in flight to wind up.
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

    /// The body of the `relink` job. `Ok(false)` = interrupted from outside (job cancel/pause).
    pub async fn run(&self, outer: &CancellationToken) -> Result<bool> {
        {
            // A job re-claimed after a restart starts from a fresh in-memory state.
            let mut s = self.state.lock();
            if s.phase != "running" {
                *s = fresh_running();
                *self.stop.lock() = CancellationToken::new();
                *self.stop_requested.lock() = false;
            }
        }
        let merged = CancellationToken::new();
        let watcher = {
            let (m, stop, outer) = (merged.clone(), self.stop.lock().clone(), outer.clone());
            tokio::spawn(async move {
                tokio::select! { _ = stop.cancelled() => {}, _ = outer.cancelled() => {} }
                m.cancel();
            })
        };
        let result = self.run_inner(&merged).await;
        watcher.abort();
        match result {
            Ok(()) => Ok(!(outer.is_cancelled() && !*self.stop_requested.lock())),
            Err(e) => {
                tracing::error!("relink failed: {e}");
                let msg: String = e.to_string().chars().take(500).collect();
                self.publish(|s| {
                    s.phase = "failed".into();
                    s.current = None;
                    s.error = Some(msg);
                    s.finished_at = Some(bc_db::util::iso_now());
                });
                Err(e)
            }
        }
    }

    async fn run_inner(&self, cancel: &CancellationToken) -> Result<()> {
        let db = &self.ctx.db;
        let todo = db.read_async(pending).await?;
        let total = todo.len() as i64;
        self.publish(|s| s.total = total);
        let src = self.source();
        let mut stopped = false;
        let mut failures = 0usize;
        let mut last_error = None;
        let mut since_kick = 0i64;
        for (i, p) in todo.into_iter().enumerate() {
            if cancel.is_cancelled() {
                stopped = true;
                break;
            }
            let shown = if p.artist.is_empty() { p.title.clone() } else { format!("{} \u{2013} {}", p.artist, p.title) };
            self.publish(|s| s.current = Some(shown));
            let found = tokio::select! {
                r = find(&*src, &p) => r,
                _ = cancel.cancelled() => { stopped = true; break; }
            };
            let hit = match found {
                Ok(h) => {
                    failures = 0;
                    h
                }
                Err(e) => {
                    // Not recorded as tried: a failed search says nothing about the release, and
                    // the next run should ask again.
                    tracing::info!("relink: search for release {} failed: {e}", p.id);
                    failures += 1;
                    last_error = Some(e.to_string());
                    if failures >= MAX_FAILURES_IN_A_ROW {
                        break;
                    }
                    self.publish(|s| s.seen = i as i64 + 1);
                    continue;
                }
            };
            let id = p.id;
            let linked = db.write_async(move |tx| record(tx, id, hit.as_ref())).await?;
            self.publish(|s| {
                s.seen = i as i64 + 1;
                if linked {
                    s.linked += 1;
                } else {
                    s.unmatched += 1;
                }
            });
            if linked {
                since_kick += 1;
                if since_kick >= KICK_EVERY {
                    since_kick = 0;
                    self.settle().await;
                }
            }
        }
        if failures >= MAX_FAILURES_IN_A_ROW {
            return Err(HarvestError::other(format!(
                "Bandcamp search kept failing ({}); stopped, and the next run picks up from here",
                last_error.unwrap_or_default()
            )));
        }
        self.settle().await;
        self.publish(|s| {
            s.phase = "done".into();
            s.current = None;
            s.error = stopped.then(|| "Stopped".to_string());
            s.finished_at = Some(bc_db::util::iso_now());
        });
        Ok(())
    }

    /// Put the new links to work: pin artists to their own pages (no network) and have the label
    /// resolver file releases under the label pages they now point at.
    async fn settle(&self) {
        if let Err(e) = self.ctx.db.write_async(|tx| backfill_artist_urls(tx)).await {
            tracing::warn!("relink: artist URL backfill failed: {e}");
        }
        self.ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &serde_json::json!({"tracks_added": 0}));
        self.ctx.expect::<LabelResolver>().kick().await;
    }
}
