//! Filling in what the grid harvests could not see: tags, above all (port of
//! `services/harvest/enrich.py`).
//!
//! A band page's `/music` grid names covers and titles but never tags -- those live only on each
//! release's own page. The feed is therefore full of rows whose `tags` column is `[]`, and a tag
//! filter over it has nothing to hold onto. [`TagEnricher`] closes that gap on request: given
//! inbox item ids, it fetches each release page (through the shared client, so the page cache
//! and the polite rate limit both apply) and writes the page's tags back onto the row.
//!
//! Deliberately *not* `inbox::absorb`: absorb also reassigns `source_kind` and `source_label`,
//! which would rip the rows out of the feed slice they were found in. This writes tags -- plus a
//! track count or release date where the row had none -- and nothing else.
//!
//! As a job: an `enrich` job with one item (`params.item_ids`) run by a `KindWorker`; the
//! in-memory [`EnrichState`] is the source of truth for the legacy status route and is published
//! as `harvest.enrich`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bc_db::rusqlite::{OptionalExtension, params};
use bc_jobs::{Complete, HandlerOutcome, ItemCtx, ItemHandler, KindWorker, NewItem, NewJob, WorkerSpec};
use bc_types::bandcamp::{EnrichState, TOPIC_HARVEST_ENRICH};
use bc_types::jobs::KIND_ENRICH;
use futures::future::BoxFuture;
use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use super::inbox::sync_tags;
use crate::error::{HarvestError, Result};
use crate::extract::HarvestedRelease;
use crate::net::BandcampClient;
use crate::service::Ctx;
use crate::sources;

/// One request per item: a page of the feed is a few minutes of polite fetching, but a
/// six-figure backlog through this endpoint would be days.
pub const MAX_ITEMS: usize = 500;

/// How a release page is fetched (default [`sources::fetch_release`]; tests substitute).
pub type FetchFn = Arc<dyn Fn(BandcampClient, String) -> BoxFuture<'static, Result<HarvestedRelease>> + Send + Sync>;

fn idle() -> EnrichState {
    EnrichState { phase: "idle".into(), ..Default::default() }
}

/// Runs at most one enrich pass at a time and reports where it has got to.
pub struct TagEnricher {
    ctx: Arc<Ctx>,
    state: Mutex<EnrichState>,
    stop: Mutex<CancellationToken>,
    stop_requested: Mutex<bool>,
    fetch: RwLock<FetchFn>,
    worker: Arc<KindWorker>,
    /// Full error list (the state only ever shows ten).
    errors: Mutex<Vec<String>>,
}

pub fn init(ctx: &Arc<Ctx>) {
    let worker = KindWorker::new(ctx.jobs.store().clone(), WorkerSpec::new(KIND_ENRICH, 1), Arc::new(EnrichHandler { ctx: ctx.clone() }));
    ctx.jobs.add_hooks(worker.clone());
    let fetch: FetchFn = Arc::new(|client, url| Box::pin(async move { sources::fetch_release(&client, &url).await }));
    ctx.put(Arc::new(TagEnricher {
        ctx: ctx.clone(),
        state: Mutex::new(idle()),
        stop: Mutex::new(CancellationToken::new()),
        stop_requested: Mutex::new(false),
        fetch: RwLock::new(fetch),
        worker,
        errors: Mutex::new(Vec::new()),
    }));
}

pub async fn start(ctx: &Arc<Ctx>) {
    ctx.expect::<TagEnricher>().worker.start();
}

struct EnrichHandler {
    ctx: Arc<Ctx>,
}

#[async_trait]
impl ItemHandler for EnrichHandler {
    async fn run(&self, ic: ItemCtx) -> HandlerOutcome {
        let ids: Vec<i64> = ic
            .job
            .params_json()
            .get("item_ids")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default();
        let enricher = self.ctx.expect::<TagEnricher>();
        match enricher.run(ids, &ic.cancel).await {
            Ok(true) => {
                let s = enricher.state();
                HandlerOutcome::Done(Complete {
                    message: Some(format!("{} of {} tagged", s.tagged, s.total)),
                    result: serde_json::to_value(&s).ok(),
                    release_id: None,
                })
            }
            Ok(false) => HandlerOutcome::Interrupted,
            Err(e) => HandlerOutcome::Failed { error: e.to_string(), class: "internal".into(), retryable: false },
        }
    }
}

impl TagEnricher {
    pub fn state(&self) -> EnrichState {
        let mut s = self.state.lock().clone();
        s.errors = self.errors.lock().iter().take(10).cloned().collect();
        s
    }

    /// Substitute how a release page is fetched (tests).
    pub fn set_fetch(&self, f: FetchFn) {
        *self.fetch.write() = f;
    }

    fn publish(&self, f: impl FnOnce(&mut EnrichState)) {
        {
            let mut s = self.state.lock();
            f(&mut s);
            s.running = s.phase == "running";
        }
        self.ctx.bus.publish(TOPIC_HARVEST_ENRICH, &self.state());
    }

    /// Kick off an enrich pass as a job. Errs when one is already running (a second would double
    /// the request rate).
    pub async fn start(&self, item_ids: &[i64]) -> Result<EnrichState> {
        let mut ids: Vec<i64> = Vec::new();
        for i in item_ids {
            if !ids.contains(i) {
                ids.push(*i);
            }
        }
        ids.truncate(MAX_ITEMS);
        {
            let mut s = self.state.lock();
            if s.phase == "running" {
                return Err(HarvestError::other("a tag fetch is already running"));
            }
            *s = EnrichState {
                phase: "running".into(),
                running: true,
                total: ids.len() as i64,
                started_at: Some(bc_db::util::iso_now()),
                ..Default::default()
            };
            self.errors.lock().clear();
            *self.stop.lock() = CancellationToken::new();
            *self.stop_requested.lock() = false;
        }
        self.ctx.bus.publish(TOPIC_HARVEST_ENRICH, &self.state());
        let nj = NewJob::new(KIND_ENRICH, vec![NewItem { source: Some("enrich".into()), ..Default::default() }])
            .label(format!("Fetch tags for {} items", ids.len()))
            .params(serde_json::json!({"item_ids": ids}));
        let store = self.ctx.jobs.store().clone();
        if let Err(e) = store.run(move |s| s.create_job(nj)).await {
            self.publish(|s| {
                s.phase = "failed".into();
                s.error = Some(e.to_string());
                s.finished_at = Some(bc_db::util::iso_now());
            });
            return Err(e.into());
        }
        Ok(self.state())
    }

    /// Stop the walk now, keeping the tags already written.
    pub async fn stop(&self) -> EnrichState {
        if self.state.lock().phase == "running" {
            *self.stop_requested.lock() = true;
            self.stop.lock().cancel();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            while self.state.lock().phase == "running" && tokio::time::Instant::now() < deadline {
                let queued = self
                    .ctx
                    .jobs
                    .store()
                    .run(|s| Ok(s.list_jobs(Some("queued"), Some(KIND_ENRICH), 0, 1)?.total > 0))
                    .await
                    .unwrap_or(false);
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
        self.state()
    }

    /// Stamp one row; returns whether it now carries tags.
    async fn write(&self, item_id: i64, release: HarvestedRelease) -> Result<bool> {
        let tagged = !release.tags.is_empty();
        self.ctx
            .db
            .write_async(move |tx| {
                let exists: Option<i64> = tx.query_row("SELECT id FROM harvest_items WHERE id = ?1", [item_id], |r| r.get(0)).optional()?;
                if exists.is_none() {
                    return Ok(false);
                }
                if tagged {
                    tx.execute("UPDATE harvest_items SET tags = ?2 WHERE id = ?1", params![item_id, serde_json::to_string(&release.tags).unwrap_or_else(|_| "[]".into())])?;
                    sync_tags(tx, item_id, &release.tags).map_err(|e| bc_db::DbError::Other(e.to_string()))?;
                }
                // Fill gaps while the page is in hand, but never overwrite: the page was fetched
                // for its tags, not to re-decide fields another source stated.
                tx.execute(
                    "UPDATE harvest_items SET label_name = coalesce(nullif(label_name, ''), ?2, label_name), \
                        release_date = coalesce(nullif(release_date, ''), ?3, release_date) WHERE id = ?1",
                    params![item_id, release.label_name, release.release_date],
                )?;
                let tc = release.track_count() as i64;
                if tc > 0 {
                    tx.execute("UPDATE harvest_items SET track_count = ?2 WHERE id = ?1 AND coalesce(track_count, 0) = 0", params![item_id, tc])?;
                }
                Ok(tagged)
            })
            .await
            .map_err(Into::into)
    }

    /// The body of the `enrich` job. `Ok(false)` = interrupted from outside (job cancel/pause).
    pub async fn run(&self, ids: Vec<i64>, outer: &CancellationToken) -> Result<bool> {
        {
            let mut s = self.state.lock();
            if s.phase != "running" {
                *s = EnrichState {
                    phase: "running".into(),
                    running: true,
                    total: ids.len() as i64,
                    started_at: Some(bc_db::util::iso_now()),
                    ..Default::default()
                };
                self.errors.lock().clear();
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
        let result = self.run_inner(ids, &merged).await;
        watcher.abort();
        match result {
            Ok(()) => Ok(!(outer.is_cancelled() && !*self.stop_requested.lock())),
            Err(e) => {
                tracing::error!("tag enrich crashed: {e}");
                let msg: String = format!("Internal error: {e}").chars().take(500).collect();
                self.publish(|s| {
                    s.phase = "failed".into();
                    s.error = Some(msg);
                    s.finished_at = Some(bc_db::util::iso_now());
                });
                Err(e)
            }
        }
    }

    async fn run_inner(&self, ids: Vec<i64>, cancel: &CancellationToken) -> Result<()> {
        let rows: Vec<(i64, String, String)> = self
            .ctx
            .db
            .read_async(move |c| {
                let mut out = Vec::new();
                for id in ids {
                    if let Some(r) = c
                        .query_row("SELECT id, url, title FROM harvest_items WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default())))
                        .optional()?
                    {
                        out.push(r);
                    }
                }
                Ok(out)
            })
            .await?;
        let client = self.ctx.client.clone();
        let fetch = self.fetch.read().clone();
        let mut stopped = false;
        for (i, (item_id, url, title)) in rows.into_iter().enumerate() {
            if cancel.is_cancelled() {
                stopped = true;
                break;
            }
            let shown = if title.is_empty() { url.clone() } else { title.clone() };
            self.publish(|s| s.current = Some(shown.clone()));
            let done = i as i64 + 1;
            let fetched = tokio::select! {
                r = fetch(client.clone(), url.clone()) => r,
                _ = cancel.cancelled() => { stopped = true; break; }
            };
            match fetched {
                Err(e) => {
                    // One dead page must not end the pass; the row just stays untagged.
                    tracing::info!("tag enrich: {url} failed: {e}");
                    self.errors.lock().push(format!("{shown}: {e}").chars().take(300).collect());
                    self.publish(|s| s.done = done);
                }
                Ok(release) => {
                    let tagged = self.write(item_id, release).await?;
                    self.publish(|s| {
                        s.done = done;
                        s.tagged += i64::from(tagged);
                    });
                }
            }
        }
        // Stopping reports what it wrote.
        self.publish(|s| {
            s.phase = "done".into();
            s.current = None;
            s.error = stopped.then(|| "Stopped".to_string());
            s.finished_at = Some(bc_db::util::iso_now());
        });
        Ok(())
    }
}
