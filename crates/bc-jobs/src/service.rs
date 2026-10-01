//! `JobsService`: owns the [`JobStore`], runs crash recovery and the lease
//! reaper, and serves the generic job-control routes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use bc_core::EventBus;
use bc_db::Db;
use bc_types::jobs::{
    ClearedOut, ItemSelection, JobItemGroupOut, JobItemOut, JobOut, MoveItemsRequest, MovedOut, RemovedOut,
};
use bc_types::Page;
use parking_lot::RwLock;
use serde::Deserialize;

use crate::api_error::{ApiError, ApiResult};
use crate::hooks::{Interrupt, JobHooks};
use crate::store::{JobStore, Place};

pub const REAP_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct JobsService {
    inner: Arc<Inner>,
}

struct Inner {
    store: JobStore,
    hooks: RwLock<Vec<Arc<dyn JobHooks>>>,
    recovered: AtomicBool,
    started: AtomicBool,
}

impl JobsService {
    pub fn new(db: Db, bus: Arc<EventBus>) -> Self {
        Self {
            inner: Arc::new(Inner {
                store: JobStore::new(db, Some(bus)),
                hooks: RwLock::new(Vec::new()),
                recovered: AtomicBool::new(false),
                started: AtomicBool::new(false),
            }),
        }
    }

    pub fn store(&self) -> &JobStore {
        &self.inner.store
    }

    pub fn add_hooks(&self, h: Arc<dyn JobHooks>) {
        self.inner.hooks.write().push(h);
    }

    /// Crash recovery: requeue items left `running`, settle jobs. Idempotent per
    /// process (only the first call does anything). **Call before any worker
    /// claims**; `start` does it, and workers' own `start` may call it too.
    pub async fn recover(&self) {
        if self.inner.recovered.swap(true, Ordering::SeqCst) {
            return;
        }
        match self.inner.store.run(|s| s.reconcile()).await {
            Ok(r) => tracing::debug!("job recovery: {r:?}"),
            Err(e) => tracing::error!("job recovery failed: {e}"),
        }
    }

    /// Run recovery, then spawn the lease reaper (every 30 s).
    pub async fn start(&self) {
        self.recover().await;
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let store = self.inner.store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REAP_INTERVAL);
            tick.tick().await;
            loop {
                tick.tick().await;
                match store.run(|s| s.reap_expired()).await {
                    Ok((requeued, failed)) if requeued + failed > 0 => {
                        tracing::warn!("lease reaper: requeued {requeued}, failed {failed}");
                    }
                    Ok(_) => {}
                    Err(e) => tracing::error!("lease reaper: {e}"),
                }
            }
        });
    }

    async fn interrupt(&self, job_id: &str, reason: Interrupt) {
        let hooks: Vec<_> = self.inner.hooks.read().clone();
        for h in hooks {
            h.interrupt_job(job_id, reason).await;
        }
    }
    async fn release_urls(&self, urls: &[String]) {
        if urls.is_empty() {
            return;
        }
        let hooks: Vec<_> = self.inner.hooks.read().clone();
        for h in hooks {
            h.release_urls(urls).await;
        }
    }
    fn wake(&self) {
        self.inner.store.notify();
        for h in self.inner.hooks.read().iter() {
            h.wake();
        }
    }

    /// Cancel for good: pending rows, the work in flight, the inbox. Shared by
    /// cancel and delete (order matters, see legacy `_stop_job`).
    pub async fn stop_job(&self, job_id: &str) -> ApiResult<Option<crate::store::Job>> {
        let id = job_id.to_string();
        let Some(_) = self.inner.store.run({
            let id = id.clone();
            move |s| s.cancel_job(&id)
        })
        .await?
        else {
            return Ok(None);
        };
        self.interrupt(job_id, Interrupt::Cancel).await;
        let urls = self
            .inner
            .store
            .run({
                let id = id.clone();
                move |s| s.unrun_urls(&id)
            })
            .await?;
        self.release_urls(&urls).await;
        Ok(self.inner.store.run(move |s| s.get_job(&id)).await?)
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/jobs", get(list_jobs))
            .route("/jobs/clear", post(clear_finished))
            .route("/jobs/{job_id}", get(get_job).delete(delete_job))
            .route("/jobs/{job_id}/items", get(get_job_items))
            .route("/jobs/{job_id}/groups", get(get_job_groups))
            .route("/jobs/{job_id}/items/move", post(move_items))
            .route("/jobs/{job_id}/items/remove", post(remove_items))
            .route("/jobs/{job_id}/cancel", post(cancel))
            .route("/jobs/{job_id}/pause", post(pause))
            .route("/jobs/{job_id}/resume", post(resume))
            .route("/jobs/{job_id}/retry", post(retry))
            .with_state(self.clone())
    }
}

#[derive(Debug, Deserialize)]
struct ListQ {
    status: Option<String>,
    kind: Option<String>,
    #[serde(default)]
    offset: i64,
    limit: Option<i64>,
}

async fn list_jobs(State(s): State<JobsService>, Query(q): Query<ListQ>) -> ApiResult<Json<Page<JobOut>>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.max(0);
    let page = s
        .store()
        .run(move |st| st.list_jobs(q.status.as_deref(), q.kind.as_deref(), offset, limit))
        .await?;
    Ok(Json(Page {
        items: page.items.iter().map(|j| j.to_out()).collect(),
        total: page.total,
        offset: page.offset,
        limit: page.limit,
    }))
}

async fn load(s: &JobsService, id: &str) -> ApiResult<crate::store::Job> {
    let id2 = id.to_string();
    s.store()
        .run(move |st| st.get_job(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("job {id} not found")))
}

async fn get_job(State(s): State<JobsService>, Path(id): Path<String>) -> ApiResult<Json<JobOut>> {
    Ok(Json(load(&s, &id).await?.to_out()))
}

fn statuses(s: &Option<String>) -> ApiResult<Vec<String>> {
    let v: Vec<String> = s
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    let unknown: Vec<&String> = v.iter().filter(|x| !crate::store::ITEM_STATUSES.contains(&x.as_str())).collect();
    if !unknown.is_empty() {
        let mut names: Vec<&str> = unknown.iter().map(|s| s.as_str()).collect();
        names.sort();
        return Err(ApiError::bad_request(format!("Unknown item status: {}", names.join(", "))));
    }
    Ok(v)
}

#[derive(Debug, Deserialize)]
struct ItemsQ {
    group: Option<String>,
    status: Option<String>,
    #[serde(default)]
    offset: i64,
    limit: Option<i64>,
}

async fn get_job_items(
    State(s): State<JobsService>,
    Path(id): Path<String>,
    Query(q): Query<ItemsQ>,
) -> ApiResult<Json<Vec<JobItemOut>>> {
    load(&s, &id).await?;
    let st = statuses(&q.status)?;
    let limit = q.limit.map(|l| l.clamp(1, 1000));
    let offset = q.offset.max(0);
    let items = s
        .store()
        .run(move |store| store.list_items(&id, q.group.as_deref(), &st, offset, limit))
        .await?;
    Ok(Json(items.iter().map(|i| i.to_out()).collect()))
}

#[derive(Debug, Deserialize)]
struct GroupsQ {
    status: Option<String>,
    #[serde(default)]
    offset: i64,
    limit: Option<i64>,
}

async fn get_job_groups(
    State(s): State<JobsService>,
    Path(id): Path<String>,
    Query(q): Query<GroupsQ>,
) -> ApiResult<Json<Page<JobItemGroupOut>>> {
    load(&s, &id).await?;
    let st = statuses(&q.status)?;
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let offset = q.offset.max(0);
    let (groups, total) = s.store().run(move |store| store.list_groups(&id, &st, offset, limit)).await?;
    Ok(Json(Page { items: groups.iter().map(|g| g.to_out()).collect(), total, offset, limit }))
}

async fn selected_ids(s: &JobsService, job_id: &str, sel: &ItemSelection) -> ApiResult<Vec<i64>> {
    let st = statuses(&sel.status)?;
    let (id, items, groups) = (job_id.to_string(), sel.item_ids.clone(), sel.groups.clone());
    Ok(s.store().run(move |store| store.select_item_ids(&id, &items, &groups, &st)).await?)
}

async fn move_items(
    State(s): State<JobsService>,
    Path(id): Path<String>,
    Json(body): Json<MoveItemsRequest>,
) -> ApiResult<Json<MovedOut>> {
    let job = load(&s, &id).await?;
    let place = Place::parse(&body.place).ok_or_else(|| ApiError::bad_request("bad place"))?;
    let mut anchor = body.anchor_item_id;
    if matches!(place, Place::Before | Place::After) {
        if anchor.is_none() {
            if let Some(g) = body.anchor_group.clone() {
                // Both edges anchor on the group's *first* visible item: the list
                // orders groups by that item.
                let st = statuses(&body.selection.status)?;
                let id2 = id.clone();
                anchor = s.store().run(move |store| store.group_edge(&id2, &g, false, &st)).await?;
            }
        }
        if anchor.is_none() {
            return Err(ApiError::bad_request("before/after needs an anchor item or group."));
        }
    }
    let ids = selected_ids(&s, &id, &body.selection).await?;
    let id2 = id.clone();
    let moved = s.store().run(move |store| store.move_items(&id2, &ids, place, anchor)).await?;
    let job = if moved > 0 { load(&s, &id).await? } else { job };
    Ok(Json(MovedOut { moved, job: job.to_out() }))
}

async fn remove_items(
    State(s): State<JobsService>,
    Path(id): Path<String>,
    Json(body): Json<ItemSelection>,
) -> ApiResult<Json<RemovedOut>> {
    load(&s, &id).await?;
    let ids = selected_ids(&s, &id, &body).await?;
    let id2 = id.clone();
    let report = s.store().run(move |store| store.remove_items(&id2, &ids)).await?;
    s.release_urls(&report.unrun_urls).await;
    let mut job = s.store().run({
        let id = id.clone();
        move |store| store.get_job(&id)
    })
    .await?;
    if report.removed > 0 && job.as_ref().is_some_and(|j| j.total == 0) {
        // An empty card is noise, not a record.
        let id2 = id.clone();
        s.store().run(move |store| store.delete_job(&id2)).await?;
        job = None;
    }
    Ok(Json(RemovedOut {
        removed: report.removed,
        kept_running: report.kept_running,
        job: job.map(|j| j.to_out()),
    }))
}

async fn cancel(State(s): State<JobsService>, Path(id): Path<String>) -> ApiResult<Json<JobOut>> {
    let job = s.stop_job(&id).await?.ok_or_else(|| ApiError::not_found(format!("job {id} not found")))?;
    if let Some(b) = s.store().bus() {
        b.publish(bc_types::jobs::TOPIC_JOB_CANCELLED, &bc_types::jobs::JobRef { job_id: id });
    }
    Ok(Json(job.to_out()))
}

async fn pause(State(s): State<JobsService>, Path(id): Path<String>) -> ApiResult<Json<JobOut>> {
    let id2 = id.clone();
    let job = s
        .store()
        .run(move |st| st.pause_job(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("job {id} not found")))?;
    if job.status != "paused" {
        return Err(ApiError::bad_request(format!("A {} job cannot be paused.", job.status)));
    }
    s.interrupt(&id, Interrupt::Pause).await;
    let job = load(&s, &id).await?;
    if let Some(b) = s.store().bus() {
        b.publish(bc_types::jobs::TOPIC_JOB_PAUSED, &bc_types::jobs::JobRef { job_id: id });
    }
    Ok(Json(job.to_out()))
}

async fn resume(State(s): State<JobsService>, Path(id): Path<String>) -> ApiResult<Json<JobOut>> {
    let id2 = id.clone();
    let job = s
        .store()
        .run(move |st| st.resume_job(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("job {id} not found")))?;
    s.wake();
    if let Some(b) = s.store().bus() {
        b.publish(bc_types::jobs::TOPIC_JOB_RESUMED, &bc_types::jobs::JobRef { job_id: id });
    }
    Ok(Json(job.to_out()))
}

async fn retry(State(s): State<JobsService>, Path(id): Path<String>) -> ApiResult<Json<JobOut>> {
    let id2 = id.clone();
    let count = s.store().run(move |st| st.retry_failed(&id2)).await?;
    let job = load(&s, &id).await?;
    if count > 0 {
        s.wake();
    }
    if let Some(b) = s.store().bus() {
        b.publish(bc_types::jobs::TOPIC_JOB_RETRIED, &bc_types::jobs::JobRetried { job_id: id, requeued: count });
    }
    Ok(Json(job.to_out()))
}

async fn clear_finished(State(s): State<JobsService>) -> ApiResult<Json<ClearedOut>> {
    // Release inbox rows of unrun items first (cancelled siblings of a job that
    // otherwise completed), as the single delete does.
    let (jobs, _) = {
        let page = s.store().run(|st| st.list_jobs(Some("completed"), None, 0, 100_000)).await?;
        (page.items, ())
    };
    for j in &jobs {
        if j.failed == 0 {
            let id = j.id.clone();
            let urls = s.store().run(move |st| st.unrun_urls(&id)).await?;
            s.release_urls(&urls).await;
        }
    }
    let ids = s.store().run(|st| st.clear_finished()).await?;
    Ok(Json(ClearedOut { deleted: ids.len() as i64 }))
}

async fn delete_job(State(s): State<JobsService>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let job = load(&s, &id).await?;
    if matches!(job.status.as_str(), "queued" | "running" | "paused") {
        s.stop_job(&id).await?.ok_or_else(|| ApiError::not_found(format!("job {id} not found")))?;
    } else {
        let id2 = id.clone();
        let urls = s.store().run(move |st| st.unrun_urls(&id2)).await?;
        s.release_urls(&urls).await;
    }
    let id2 = id.clone();
    s.store().run(move |st| st.delete_job(&id2)).await?;
    Ok(StatusCode::NO_CONTENT)
}
