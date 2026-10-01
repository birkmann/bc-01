//! The harvest run as a job (`POST /harvest/run`, port of the legacy `run` route).
//!
//! Legacy ran the whole source stream inside the request. Here it is a `harvest` job with one
//! item (`params` = the [`RunRequest`]) claimed by a `KindWorker`, which drains the matching
//! `sources::harvest_*` stream through `inbox::absorb`. Progress goes out coalesced as
//! `harvest.progress`, completion as `harvest.completed`, and the [`RunResult`] is stored in the
//! item's `result` JSON (`GET /harvest/runs/{job_id}`).

use std::sync::Arc;

use async_trait::async_trait;
use bc_db::rusqlite::OptionalExtension;
use bc_jobs::{Complete, HandlerOutcome, ItemCtx, ItemHandler, KindWorker, NewItem, NewJob, Throttle, WorkerSpec};
use bc_types::bandcamp::{HarvestCompleted, HarvestProgress, RunRequest, RunResult, TOPIC_HARVEST_COMPLETED, TOPIC_HARVEST_PROGRESS};
use bc_types::jobs::KIND_HARVEST;
use parking_lot::RwLock;
use tokio::sync::mpsc;

use super::inbox::{AbsorbOpts, absorb};
use crate::download::dedup::backfill_release_labels;
use crate::error::{HarvestError, Result};
use crate::net::BandcampClient;
use crate::service::Ctx;
use crate::sources::{self, DiscoverQuery, EventStream};
use crate::urls;

/// Run-concurrency of harvest jobs (`harvest_concurrency` is for fetches inside one client).
pub const RUN_CONCURRENCY: usize = 2;

/// Upper bound of `RunRequest::limit`: a wishlist runs to five figures -- one collection's is
/// 12,746 -- and `harvest_collection` always walks from the newest item, so a capped run cannot
/// be resumed into the tail. The ceiling has to clear a whole fan list in one pass.
pub const MAX_LIMIT: i64 = 25_000;

/// Builds the source stream of a run (default: the matching `sources::harvest_*`; tests
/// substitute their own).
pub type StreamBuilder = Arc<dyn Fn(&BandcampClient, &RunRequest) -> Result<EventStream> + Send + Sync>;

pub struct RunService {
    ctx: Arc<Ctx>,
    worker: Arc<KindWorker>,
    builder: RwLock<StreamBuilder>,
}

pub fn init(ctx: &Arc<Ctx>) {
    let worker = KindWorker::new(ctx.jobs.store().clone(), WorkerSpec::new(KIND_HARVEST, RUN_CONCURRENCY), Arc::new(RunHandler { ctx: ctx.clone() }));
    ctx.jobs.add_hooks(worker.clone());
    ctx.put(Arc::new(RunService { ctx: ctx.clone(), worker, builder: RwLock::new(Arc::new(default_stream)) }));
}

pub async fn start(ctx: &Arc<Ctx>) {
    ctx.expect::<RunService>().worker.start();
}

/// The label shown for a run: the source's display name, or the kind when there is no URL.
pub fn run_label(req: &RunRequest) -> String {
    match req.url.as_deref().filter(|u| !u.is_empty()) {
        Some(u) => urls::display_name(u),
        None => req.kind.clone(),
    }
}

/// Validate the request the way the legacy route did; the stream is built from it by
/// [`default_stream`].
pub fn validate(req: &RunRequest) -> Result<()> {
    if !(1..=MAX_LIMIT).contains(&req.limit) {
        return Err(HarvestError::other(format!("limit must be between 1 and {MAX_LIMIT}")));
    }
    if !matches!(req.depth.as_str(), "shallow" | "full") {
        return Err(HarvestError::other("depth must be shallow or full"));
    }
    let has_url = req.url.as_deref().is_some_and(|u| !u.is_empty());
    match req.kind.as_str() {
        "url_list" | "discover" => Ok(()),
        "artist" | "label" if has_url => Ok(()),
        "artist" | "label" => Err(HarvestError::other("url is required")),
        "collection" | "wishlist" | "hidden" if has_url => Ok(()),
        "collection" | "wishlist" | "hidden" => Err(HarvestError::other("fan url is required")),
        other => Err(HarvestError::other(format!("unsupported source: {other}"))),
    }
}

fn default_stream(client: &BandcampClient, req: &RunRequest) -> Result<EventStream> {
    validate(req)?;
    let limit = req.limit as usize;
    let url = req.url.clone().unwrap_or_default();
    Ok(match req.kind.as_str() {
        "url_list" => sources::harvest_url_list(client.clone(), req.text.clone().unwrap_or_default(), req.depth.clone(), Some(limit)),
        "artist" | "label" => sources::harvest_artist(client.clone(), url, req.depth.clone(), Some(limit)),
        "discover" => {
            // The pasted URL names the genre; a run that ignored it would sweep the generic
            // all-genres feed instead of what the user was looking at.
            let tags = if req.tags.is_empty() && !url.is_empty() { urls::discover_tags(&url) } else { req.tags.clone() };
            let mut q = DiscoverQuery::new();
            q.tags = tags;
            q.genre = req.genre.clone();
            q.slice = req.slice.clone();
            q.geoname_id = req.geoname_id;
            sources::harvest_discover(client.clone(), q, limit, 60)
        }
        _ => sources::harvest_collection(client.clone(), None, Some(urls::normalise(&url)), req.kind.clone(), limit, 100, None),
    })
}

/// `fans.username_from_url`: the first path segment, else the URL itself.
fn username_from_url(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.path_segments().and_then(|mut s| s.find(|x| !x.is_empty()).map(str::to_string)))
        .unwrap_or_else(|| url.to_string())
}

impl RunService {
    /// Substitute how the source stream is built (tests).
    pub fn set_stream_builder(&self, b: StreamBuilder) {
        *self.builder.write() = b;
    }

    /// Create the `harvest` job for a run and return its id.
    pub async fn submit(&self, req: RunRequest) -> Result<String> {
        validate(&req)?;
        let label = run_label(&req);
        let nj = NewJob::new(KIND_HARVEST, vec![NewItem { source: Some(req.kind.clone()), ..Default::default() }])
            .label(format!("Harvest {label}"))
            .params(serde_json::to_value(&req).unwrap_or_default());
        let job = self.ctx.jobs.store().run(move |s| s.create_job(nj)).await?;
        Ok(job.id)
    }

    /// "in_wishlist" is the sticky "on MY wishlist" flag: walking anyone else's list through
    /// the generic route must not set it.
    async fn mark_wishlist(&self, req: &RunRequest) -> Option<bool> {
        let url = req.url.as_deref().filter(|u| !u.is_empty())?;
        if req.kind != "wishlist" {
            return None;
        }
        let theirs = username_from_url(&urls::normalise(url)).to_lowercase();
        let mine: Option<String> = self
            .ctx
            .db
            .read_async(|c| Ok(c.query_row("SELECT username FROM fans WHERE is_self = 1 ORDER BY id LIMIT 1", [], |r| r.get(0)).optional()?))
            .await
            .ok()
            .flatten();
        Some(mine.is_some_and(|m| m.to_lowercase() == theirs))
    }

    async fn run(&self, ic: &ItemCtx) -> HandlerOutcome {
        let req: RunRequest = match serde_json::from_value(ic.job.params_json()) {
            Ok(r) => r,
            Err(e) => return HandlerOutcome::Failed { error: format!("bad harvest params: {e}"), class: "internal".into(), retryable: false },
        };
        let label = run_label(&req);
        let builder = self.builder.read().clone();
        let stream = match builder(&self.ctx.client, &req) {
            Ok(s) => s,
            Err(e) => return HandlerOutcome::Failed { error: format!("harvest failed: {e}"), class: "bad_request".into(), retryable: false },
        };

        let mut opts = AbsorbOpts::new(req.kind.clone(), label.clone());
        opts.label_name = if req.kind == "label" { req.label_name.clone() } else { None };
        opts.mark_wishlist = self.mark_wishlist(&req).await;

        // Progress: absorb calls a sync callback per flushed batch; a sibling task coalesces it
        // (<= 4/s) into `harvest.progress` and the item's own progress.
        let (tx, mut rx) = mpsc::unbounded_channel::<(i64, Option<i64>)>();
        let progress = {
            let (bus, job_id, kind, mut rep) = (self.ctx.bus.clone(), ic.job.id.clone(), req.kind.clone(), ic.reporter());
            tokio::spawn(async move {
                let mut throttle = Throttle::per_sec(4);
                while let Some((seen, total)) = rx.recv().await {
                    if !throttle.ready() {
                        continue;
                    }
                    bus.publish(TOPIC_HARVEST_PROGRESS, &HarvestProgress { job_id: job_id.clone(), kind: kind.clone(), seen, total, new: 0 });
                    let frac = total.filter(|t| *t > 0).map_or(0.0, |t| (seen as f64 / t as f64).min(0.99));
                    rep.report_async(frac, &format!("{seen} seen")).await;
                }
            })
        };
        let cb = move |seen: i64, total: Option<i64>| {
            let _ = tx.send((seen, total));
        };
        let absorbed = absorb(&self.ctx.db, stream, &opts, Some(&cb), &ic.cancel).await;
        drop(cb);
        let _ = progress.await;

        // A cancelled run still flushed its partial batch ("stop fetching", not "forget").
        if absorbed.cancelled {
            return HandlerOutcome::Interrupted;
        }
        if let Some(err) = absorbed.error {
            let class = match err {
                HarvestError::IdentityExpired(_) => "identity_expired",
                HarvestError::RateLimited(_) => "rate_limited",
                _ => "network",
            };
            return HandlerOutcome::Failed { error: format!("harvest failed: {err}"), class: class.into(), retryable: false };
        }

        let c = absorbed.counts;
        let result = RunResult {
            kind: req.kind.clone(),
            label: label.clone(),
            seen: c.seen,
            new: c.new,
            already_known: c.already_known,
            in_library: c.in_library,
            queued: c.queued,
            pending_item_ids: c.pending_ids,
            errors: c.errors,
            tier_counts: c.tier_counts,
        };

        // File what just arrived. A label run is the one source that names the label for
        // everything it yields, so the shelf should reflect it at once rather than after the
        // next restart.
        if req.kind == "label" && req.label_name.as_deref().is_some_and(|n| !n.is_empty()) {
            match self.ctx.db.write_async(|tx| backfill_release_labels(tx)).await {
                Ok(n) if n > 0 => tracing::info!("filed {n} release(s) under {}", req.label_name.as_deref().unwrap_or("")),
                Ok(_) => {}
                Err(e) => tracing::warn!("label backfill after harvest failed: {e}"),
            }
        }
        self.ctx.bus.publish(
            TOPIC_HARVEST_COMPLETED,
            &HarvestCompleted { kind: req.kind.clone(), label, seen: result.seen, new: result.new },
        );
        HandlerOutcome::Done(Complete {
            message: Some(format!("{} seen, {} new", result.seen, result.new)),
            result: serde_json::to_value(&result).ok(),
            release_id: None,
        })
    }

    /// `GET /harvest/runs/{job_id}`: the stored result once the job's item finished.
    pub async fn result(&self, job_id: &str) -> RunLookup {
        let id = job_id.to_string();
        let found = self
            .ctx
            .jobs
            .store()
            .run(move |s| {
                let Some(job) = s.get_job(&id)? else { return Ok(None) };
                if job.kind != KIND_HARVEST {
                    return Ok(None);
                }
                let items = s.list_items(&id, None, &[], 0, Some(1))?;
                Ok(Some((job, items.into_iter().next())))
            })
            .await;
        match found {
            Ok(Some((job, item))) => match job.status.as_str() {
                "completed" => match item.and_then(|i| i.result).and_then(|r| serde_json::from_str::<RunResult>(&r).ok()) {
                    Some(r) => RunLookup::Done(r),
                    None => RunLookup::Pending,
                },
                "failed" => RunLookup::Failed {
                    message: item.as_ref().and_then(|i| i.last_error.clone()).or(job.error).unwrap_or_else(|| "harvest failed".into()),
                    identity_expired: item.is_some_and(|i| i.error_class.as_deref() == Some("identity_expired")),
                },
                "cancelled" => RunLookup::Cancelled,
                _ => RunLookup::Pending,
            },
            _ => RunLookup::Missing,
        }
    }
}

pub enum RunLookup {
    Done(RunResult),
    Pending,
    Failed { message: String, identity_expired: bool },
    Cancelled,
    Missing,
}

struct RunHandler {
    ctx: Arc<Ctx>,
}

#[async_trait]
impl ItemHandler for RunHandler {
    async fn run(&self, ic: ItemCtx) -> HandlerOutcome {
        self.ctx.expect::<RunService>().run(&ic).await
    }
}
