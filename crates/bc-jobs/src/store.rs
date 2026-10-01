//! Durable job queue backed by SQLite (port of `services/jobs/store.py`).
//!
//! The DB *is* the queue: jobs and their items live in `jobs` / `job_items`
//! (legacy schema). Every write goes through [`bc_db::Db::write`], i.e. the single
//! writer thread inside `BEGIN IMMEDIATE`, so a claim is atomic without any
//! in-process locking.
//!
//! Workers of any kind (download, scan, analyze, move, harvest, ...) use the same
//! API:
//!
//! ```text
//! let claimed = store.claim_item("scan", pid, LEASE)?;     // Option<Claimed>
//! loop { store.heartbeat(item_id)?; store.update_progress(..)?; }
//! store.complete_item(item_id, Complete { message, result, release_id })?;
//! // or store.fail_item(item_id, err, "network", retryable)? / store.skip_item(..)
//! ```
//!
//! Methods are blocking (call from a plain thread, or from async code via
//! [`JobStore::run`]). The store publishes the legacy `job.*` / `job.item.*`
//! events itself, so a worker never has to.

use std::collections::HashSet;
use std::sync::Arc;

use bc_core::EventBus;
use bc_db::rusqlite::{self, Connection, OptionalExtension, params};
use bc_db::{Db, DbError, Result};
use bc_types::Page;
use bc_types::jobs as dto;
use serde::Serialize;
use tokio::sync::watch;

use crate::time;

/// Default lease. Workers must [`JobStore::heartbeat`] more often than this
/// (the reaper runs every 30 s; heartbeating every 30 s is plenty).
pub const LEASE_SECONDS: f64 = 300.0;
pub const MAX_BACKOFF_S: f64 = 600.0;
pub const DEFAULT_MAX_ATTEMPTS: i64 = 3;

pub const ITEM_STATUSES: [&str; 6] = ["pending", "running", "done", "failed", "skipped", "cancelled"];

/// 10 s, 20 s, 40 s ... capped at 10 minutes, jittered x0.8..1.2.
pub fn backoff_delay(attempts: i64) -> f64 {
    let exp = (attempts - 1).clamp(0, 20) as u32;
    let base = (10.0 * 2f64.powi(exp as i32)).min(MAX_BACKOFF_S);
    base * (0.8 + 0.4 * rand::random::<f64>())
}

/// The account a URL lives under: `https://x.bandcamp.com/album/y` -> `x.bandcamp.com`
/// (`www.` stripped). Python twin: `host_of`.
pub fn host_of(url: Option<&str>) -> String {
    let Some(url) = url else { return String::new() };
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split('/').next().unwrap_or("").to_lowercase();
    host.strip_prefix("www.").map(str::to_string).unwrap_or(host)
}

// -- rows ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub label: Option<String>,
    pub priority: i64,
    /// JSON text.
    pub params: String,
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
    pub skipped: i64,
    pub cancel_requested: bool,
    pub error: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

impl Job {
    pub fn params_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.params).unwrap_or(serde_json::Value::Null)
    }
    pub fn param_str(&self, key: &str) -> Option<String> {
        self.params_json().get(key).and_then(|v| v.as_str()).map(str::to_string)
    }
    pub fn progress(&self) -> f64 {
        if self.total > 0 {
            (self.completed + self.failed + self.skipped) as f64 / self.total as f64
        } else {
            0.0
        }
    }
    pub fn is_finished(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "failed" | "cancelled")
    }
    pub fn to_out(&self) -> dto::JobOut {
        dto::JobOut {
            id: self.id.clone(),
            kind: self.kind.clone(),
            status: self.status.clone(),
            label: self.label.clone(),
            total: self.total,
            completed: self.completed,
            failed: self.failed,
            skipped: self.skipped,
            progress: self.progress(),
            error: self.error.clone(),
            created_at: Some(time::to_iso(&self.created_at)),
            started_at: self.started_at.as_deref().map(time::to_iso),
            finished_at: self.finished_at.as_deref().map(time::to_iso),
        }
    }
    pub fn progress_event(&self) -> dto::JobProgress {
        dto::JobProgress {
            job_id: self.id.clone(),
            status: self.status.clone(),
            total: self.total,
            completed: self.completed,
            failed: self.failed,
            skipped: self.skipped,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobItem {
    pub id: i64,
    pub job_id: String,
    pub seq: i64,
    pub status: String,
    pub url: Option<String>,
    pub url_kind: Option<String>,
    pub source: Option<String>,
    pub target_dir: Option<String>,
    pub track_id: Option<i64>,
    pub attempts: i64,
    pub max_attempts: i64,
    pub next_attempt_at: Option<String>,
    pub lease_expires_at: Option<String>,
    pub worker_pid: Option<i64>,
    pub progress: f64,
    pub message: Option<String>,
    pub last_error: Option<String>,
    pub error_class: Option<String>,
    /// JSON text.
    pub result: Option<String>,
    pub release_id: Option<i64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

impl JobItem {
    pub fn to_out(&self) -> dto::JobItemOut {
        dto::JobItemOut {
            id: self.id,
            seq: self.seq,
            status: self.status.clone(),
            url: self.url.clone(),
            url_kind: self.url_kind.clone(),
            progress: self.progress,
            message: self.message.clone(),
            attempts: self.attempts,
            max_attempts: self.max_attempts,
            last_error: self.last_error.clone(),
            error_class: self.error_class.clone(),
            release_id: self.release_id,
            started_at: self.started_at.as_deref().map(time::to_iso),
            finished_at: self.finished_at.as_deref().map(time::to_iso),
        }
    }
}

/// An item taken from the queue together with its job.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub item: JobItem,
    pub job: Job,
}

/// What to enqueue.
#[derive(Debug, Clone, Default)]
pub struct NewItem {
    pub url: Option<String>,
    pub url_kind: Option<String>,
    pub source: Option<String>,
    pub target_dir: Option<String>,
    pub track_id: Option<i64>,
    /// The release this item exists to repair, for a fill. Set up front so a
    /// fill of an album already on disk still has a release to record against.
    pub release_id: Option<i64>,
}

impl NewItem {
    pub fn url(url: impl Into<String>, kind: impl Into<String>) -> Self {
        Self { url: Some(url.into()), url_kind: Some(kind.into()), ..Default::default() }
    }
    pub fn track(track_id: i64) -> Self {
        Self { track_id: Some(track_id), ..Default::default() }
    }
}

#[derive(Debug, Clone)]
pub struct NewJob {
    pub kind: String,
    pub label: Option<String>,
    pub priority: i64,
    pub params: serde_json::Value,
    /// Client-supplied id (idempotent resubmits are the caller's job: check `get_job` first).
    pub job_id: Option<String>,
    pub items: Vec<NewItem>,
    pub max_attempts: i64,
}

impl NewJob {
    pub fn new(kind: impl Into<String>, items: Vec<NewItem>) -> Self {
        Self {
            kind: kind.into(),
            label: None,
            priority: 100,
            params: serde_json::json!({}),
            job_id: None,
            items,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }
    pub fn label(mut self, l: impl Into<String>) -> Self {
        self.label = Some(l.into());
        self
    }
    pub fn priority(mut self, p: i64) -> Self {
        self.priority = p;
        self
    }
    pub fn params(mut self, p: serde_json::Value) -> Self {
        self.params = p;
        self
    }
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.job_id = Some(id.into());
        self
    }
}

/// Arguments for [`JobStore::complete_item`].
#[derive(Debug, Clone, Default)]
pub struct Complete {
    pub message: Option<String>,
    pub result: Option<serde_json::Value>,
    pub release_id: Option<i64>,
}

impl Complete {
    pub fn msg(m: impl Into<String>) -> Self {
        Self { message: Some(m.into()), ..Default::default() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Top,
    Bottom,
    Before,
    After,
}

impl Place {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "top" => Self::Top,
            "bottom" => Self::Bottom,
            "before" => Self::Before,
            "after" => Self::After,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemGroup {
    pub key: String,
    pub total: i64,
    pub visible: i64,
    pub pending: i64,
    pub running: i64,
    pub done: i64,
    pub failed: i64,
    pub skipped: i64,
    pub cancelled: i64,
    pub first_seq: i64,
    pub item: Option<JobItem>,
}

impl ItemGroup {
    pub fn to_out(&self) -> dto::JobItemGroupOut {
        dto::JobItemGroupOut {
            key: self.key.clone(),
            total: self.total,
            visible: self.visible,
            pending: self.pending,
            running: self.running,
            done: self.done,
            failed: self.failed,
            skipped: self.skipped,
            cancelled: self.cancelled,
            first_seq: self.first_seq,
            item: self.item.as_ref().map(JobItem::to_out),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RemoveReport {
    pub removed: i64,
    pub kept_running: i64,
    /// URLs of removed items that never got downloaded, for the caller to hand
    /// back to the inbox (their rows there still say "queued").
    pub unrun_urls: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub requeued: i64,
    pub failed: i64,
    pub jobs_settled: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailOutcome {
    /// The item went back to `pending` with a backoff.
    pub will_retry: bool,
    /// `false` when the item was no longer `running` (reaped / cancelled): nothing changed.
    pub applied: bool,
}

// -- SQL ----------------------------------------------------------------------------

const JOB_COLS: &str = "id, kind, status, label, priority, params, total, completed, failed, \
    skipped, cancel_requested, error, created_at, started_at, finished_at";

const ITEM_COLS: &str = "id, job_id, seq, status, url, url_kind, source, target_dir, track_id, \
    attempts, max_attempts, next_attempt_at, lease_expires_at, worker_pid, progress, message, \
    last_error, error_class, result, release_id, started_at, finished_at";

/// Same claim as `store.py::_CLAIM_SQL`.
const CLAIM_SQL: &str = "\
UPDATE job_items
   SET status = 'running',
       attempts = attempts + 1,
       started_at = ?1,
       lease_expires_at = ?2,
       worker_pid = ?3
 WHERE id = (
   SELECT ji.id
     FROM job_items ji
     JOIN jobs j ON j.id = ji.job_id
    WHERE ji.status = 'pending'
      AND j.status IN ('queued', 'running')
      AND j.kind = ?4
      AND j.cancel_requested = 0
      AND (ji.next_attempt_at IS NULL OR ji.next_attempt_at <= ?1)
    ORDER BY j.priority, j.created_at, ji.seq
    LIMIT 1
 )
RETURNING id";

/// CTE chain `a, b, c` over one job's items (`?1` = job id) yielding
/// `c(id, seq, status, grp)` where `grp` is the host (SQL twin of [`host_of`]).
const HOSTS_CTE: &str = "\
WITH a AS (SELECT id, seq, status,
                  CASE WHEN instr(url, '://') > 0 THEN substr(url, instr(url, '://') + 3) ELSE url END AS rest
             FROM job_items WHERE job_id = ?1),
     b AS (SELECT id, seq, status,
                  lower(CASE WHEN instr(rest, '/') > 0 THEN substr(rest, 1, instr(rest, '/') - 1) ELSE rest END) AS h
             FROM a),
     c AS (SELECT id, seq, status,
                  coalesce(CASE WHEN substr(h, 1, 4) = 'www.' THEN substr(h, 5) ELSE h END, '') AS grp
             FROM b)";

fn job_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        kind: r.get(1)?,
        status: r.get(2)?,
        label: r.get(3)?,
        priority: r.get(4)?,
        params: r.get(5)?,
        total: r.get(6)?,
        completed: r.get(7)?,
        failed: r.get(8)?,
        skipped: r.get(9)?,
        cancel_requested: r.get::<_, i64>(10)? != 0,
        error: r.get(11)?,
        created_at: r.get(12)?,
        started_at: r.get(13)?,
        finished_at: r.get(14)?,
    })
}

fn item_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobItem> {
    Ok(JobItem {
        id: r.get(0)?,
        job_id: r.get(1)?,
        seq: r.get(2)?,
        status: r.get(3)?,
        url: r.get(4)?,
        url_kind: r.get(5)?,
        source: r.get(6)?,
        target_dir: r.get(7)?,
        track_id: r.get(8)?,
        attempts: r.get(9)?,
        max_attempts: r.get(10)?,
        next_attempt_at: r.get(11)?,
        lease_expires_at: r.get(12)?,
        worker_pid: r.get(13)?,
        progress: r.get(14)?,
        message: r.get(15)?,
        last_error: r.get(16)?,
        error_class: r.get(17)?,
        result: r.get(18)?,
        release_id: r.get(19)?,
        started_at: r.get(20)?,
        finished_at: r.get(21)?,
    })
}

pub(crate) fn load_job(c: &Connection, id: &str) -> Result<Option<Job>> {
    Ok(c.query_row(&format!("SELECT {JOB_COLS} FROM jobs WHERE id = ?1"), [id], job_from_row).optional()?)
}

pub(crate) fn load_item(c: &Connection, id: i64) -> Result<Option<JobItem>> {
    Ok(c.query_row(&format!("SELECT {ITEM_COLS} FROM job_items WHERE id = ?1"), [id], item_from_row).optional()?)
}

/// Close a job once no item can still run. Returns true when its status changed.
fn settle_in(c: &Connection, job_id: &str) -> Result<bool> {
    let Some(job) = load_job(c, job_id)? else { return Ok(false) };
    if job.is_finished() {
        return Ok(false);
    }
    let pending: i64 = c.query_row(
        "SELECT count(*) FROM job_items WHERE job_id = ?1 AND status IN ('pending','running')",
        [job_id],
        |r| r.get(0),
    )?;
    if pending > 0 {
        return Ok(false);
    }
    let status = if job.cancel_requested {
        "cancelled"
    } else if job.failed > 0 && job.completed == 0 {
        "failed"
    } else {
        "completed"
    };
    c.execute("UPDATE jobs SET status = ?2, finished_at = ?3 WHERE id = ?1", params![job_id, status, time::now()])?;
    Ok(true)
}

fn validate_statuses(statuses: &[String]) -> Result<Vec<&'static str>> {
    let mut out = Vec::new();
    for s in statuses {
        match ITEM_STATUSES.iter().find(|k| **k == s.as_str()) {
            Some(k) => out.push(*k),
            None => return Err(DbError::Other(format!("Unknown item status: {s}"))),
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// `AND <col> IN ('a','b')`, or empty for "no filter". Values are validated
/// against [`ITEM_STATUSES`], so inlining them is safe.
fn status_clause(col: &str, statuses: &[&str]) -> String {
    if statuses.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = statuses.iter().map(|s| format!("'{s}'")).collect();
        format!(" AND {col} IN ({})", list.join(","))
    }
}

/// Insert a job and its items inside the caller's transaction (so the caller can change other
/// rows atomically with the enqueue, e.g. the harvest inbox flipping `new` -> `queued`).
/// Call [`JobStore::announce_created`] after the transaction commits.
pub fn create_job_in(tx: &rusqlite::Transaction<'_>, nj: &NewJob) -> Result<Job> {
    let id = nj.job_id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    tx.execute(
        "INSERT INTO jobs (id, kind, status, label, priority, params, total, completed, failed, skipped, \
                           cancel_requested, created_at) \
         VALUES (?1, ?2, 'queued', ?3, ?4, ?5, ?6, 0, 0, 0, 0, ?7)",
        params![id, nj.kind, nj.label, nj.priority, nj.params.to_string(), nj.items.len() as i64, time::now()],
    )?;
    {
        let mut st = tx.prepare(
            "INSERT INTO job_items (job_id, seq, status, url, url_kind, source, target_dir, track_id, \
                                    release_id, attempts, max_attempts, progress) \
             VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, 0.0)",
        )?;
        for (seq, it) in nj.items.iter().enumerate() {
            st.execute(params![
                id, seq as i64, it.url, it.url_kind, it.source, it.target_dir, it.track_id, it.release_id, nj.max_attempts
            ])?;
        }
    }
    load_job(tx, &id)?.ok_or(DbError::NotFound)
}

// -- the store ----------------------------------------------------------------------

#[derive(Clone)]
pub struct JobStore {
    db: Db,
    bus: Option<Arc<EventBus>>,
    wake: Arc<watch::Sender<u64>>,
}

impl JobStore {
    pub fn new(db: Db, bus: Option<Arc<EventBus>>) -> Self {
        let (tx, _rx) = watch::channel(0u64);
        Self { db, bus, wake: Arc::new(tx) }
    }

    /// Run a blocking store call off the async runtime:
    /// `store.run(|s| s.claim_item("scan", pid, LEASE_SECONDS)).await`.
    pub async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&JobStore) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let s = self.clone();
        tokio::task::spawn_blocking(move || f(&s)).await.map_err(|e| DbError::Other(e.to_string()))?
    }

    pub fn db(&self) -> &Db {
        &self.db
    }
    pub fn bus(&self) -> Option<&Arc<EventBus>> {
        self.bus.as_ref()
    }

    fn publish<P: Serialize>(&self, topic: &str, payload: &P) {
        if let Some(b) = &self.bus {
            b.publish(topic, payload);
        }
    }

    fn publish_progress(&self, job: &Job) {
        self.publish(dto::TOPIC_JOB_PROGRESS, &job.progress_event());
    }

    /// Wake workers: new work may be claimable. Subscribe with [`JobStore::subscribe_wake`].
    pub fn notify(&self) {
        self.wake.send_modify(|v| *v = v.wrapping_add(1));
    }

    /// A receiver that changes whenever new work may be claimable; a worker loop is
    /// `select! { _ = rx.changed() => {}, _ = sleep(poll) => {} }` then `claim_item`.
    pub fn subscribe_wake(&self) -> watch::Receiver<u64> {
        self.wake.subscribe()
    }

    // ---- create -------------------------------------------------------------------

    /// Create a job with its items in one transaction. Publishes `job.created`
    /// and wakes workers.
    pub fn create_job(&self, nj: NewJob) -> Result<Job> {
        let job = self.db.write(move |tx| create_job_in(tx, &nj))?;
        self.announce_created(&job);
        Ok(job)
    }

    /// Publish `job.created` and wake workers. Call after committing a job made with
    /// [`create_job_in`] inside your own transaction.
    pub fn announce_created(&self, job: &Job) {
        self.publish(
            dto::TOPIC_JOB_CREATED,
            &dto::JobCreated { job_id: job.id.clone(), label: job.label.clone(), total: job.total },
        );
        self.invalidate_jobs();
        self.notify();
    }

    /// `invalidate("job", [])`: stable entity name for the UI cache.
    fn invalidate_jobs(&self) {
        if let Some(b) = &self.bus {
            b.invalidate("job", vec![]);
        }
    }

    // ---- claim / worker side --------------------------------------------------------

    /// Atomically take the next runnable item of `kind` (ordered by job priority,
    /// job age, then `seq`). The job flips to `running`; the item's lease is
    /// `lease_secs` (use [`LEASE_SECONDS`]) and must be renewed with [`heartbeat`](Self::heartbeat).
    pub fn claim_item(&self, kind: &str, pid: i64, lease_secs: f64) -> Result<Option<Claimed>> {
        let kind = kind.to_string();
        let out = self.db.write(move |tx| {
            let now = time::now();
            let lease = time::plus_secs(lease_secs);
            let id: Option<i64> =
                tx.query_row(CLAIM_SQL, params![now, lease, pid, kind], |r| r.get(0)).optional()?;
            let Some(id) = id else { return Ok(None) };
            let item = load_item(tx, id)?.ok_or(DbError::NotFound)?;
            let mut job = load_job(tx, &item.job_id)?.ok_or(DbError::NotFound)?;
            let mut started = false;
            if job.status == "queued" {
                tx.execute(
                    "UPDATE jobs SET status = 'running', started_at = coalesce(started_at, ?2) WHERE id = ?1",
                    params![job.id, now],
                )?;
                job = load_job(tx, &item.job_id)?.ok_or(DbError::NotFound)?;
                started = true;
            }
            Ok(Some((Claimed { item, job }, started)))
        })?;
        match out {
            None => Ok(None),
            Some((c, started)) => {
                if started {
                    self.publish_progress(&c.job);
                }
                self.publish(
                    dto::TOPIC_JOB_ITEM_STARTED,
                    &dto::JobItemStarted { job_id: c.job.id.clone(), item_id: c.item.id, url: c.item.url.clone() },
                );
                Ok(Some(c))
            }
        }
    }

    /// Renew the lease of a running item. Returns false when the item is no
    /// longer running (reaped, cancelled): the worker should stop.
    pub fn heartbeat(&self, item_id: i64, lease_secs: f64) -> Result<bool> {
        self.db.write(move |tx| {
            let n = tx.execute(
                "UPDATE job_items SET lease_expires_at = ?2 WHERE id = ?1 AND status = 'running'",
                params![item_id, time::plus_secs(lease_secs)],
            )?;
            Ok(n > 0)
        })
    }

    /// Persist progress/message of a running item (does *not* publish: workers
    /// coalesce progress events themselves, see [`crate::throttle::ProgressThrottle`]).
    pub fn update_progress(&self, item_id: i64, progress: f64, message: &str) -> Result<()> {
        let message = message.to_string();
        self.db.write(move |tx| {
            tx.execute(
                "UPDATE job_items SET progress = ?2, message = ?3 WHERE id = ?1",
                params![item_id, progress, message],
            )?;
            Ok(())
        })
    }

    /// Settle a running item as `done`. Returns false when it was no longer running.
    pub fn complete_item(&self, item_id: i64, c: Complete) -> Result<bool> {
        let (ok, job, detail) = self.db.write(move |tx| {
            let Some(item) = load_item(tx, item_id)? else { return Ok((false, None, String::new())) };
            if item.status != "running" {
                return Ok((false, None, String::new()));
            }
            tx.execute(
                "UPDATE job_items SET status = 'done', progress = 1.0, message = ?2, result = ?3, \
                        release_id = coalesce(?4, release_id), finished_at = ?5, lease_expires_at = NULL \
                 WHERE id = ?1",
                params![
                    item_id,
                    c.message,
                    c.result.as_ref().map(|v| v.to_string()),
                    c.release_id,
                    time::now()
                ],
            )?;
            tx.execute("UPDATE jobs SET completed = completed + 1 WHERE id = ?1", [&item.job_id])?;
            settle_in(tx, &item.job_id)?;
            Ok((true, load_job(tx, &item.job_id)?, c.message.unwrap_or_default()))
        })?;
        if let Some(job) = job.filter(|_| ok) {
            self.publish(
                dto::TOPIC_JOB_ITEM_COMPLETED,
                &dto::JobItemCompleted { job_id: job.id.clone(), item_id, detail },
            );
            self.publish_progress(&job);
        }
        Ok(ok)
    }

    /// Settle a pending or running item as `skipped` (nothing fetched).
    pub fn skip_item(&self, item_id: i64, message: &str) -> Result<bool> {
        let message = message.to_string();
        let (ok, job, url) = self.db.write(move |tx| {
            let Some(item) = load_item(tx, item_id)? else { return Ok((false, None, None)) };
            if !matches!(item.status.as_str(), "pending" | "running") {
                return Ok((false, None, None));
            }
            tx.execute(
                "UPDATE job_items SET status = 'skipped', progress = 1.0, message = ?2, finished_at = ?3, \
                        lease_expires_at = NULL WHERE id = ?1",
                params![item_id, message, time::now()],
            )?;
            tx.execute("UPDATE jobs SET skipped = skipped + 1 WHERE id = ?1", [&item.job_id])?;
            settle_in(tx, &item.job_id)?;
            Ok((true, load_job(tx, &item.job_id)?, item.url))
        })?;
        if let Some(job) = job.filter(|_| ok) {
            self.publish(
                dto::TOPIC_JOB_ITEM_SKIPPED,
                &dto::JobItemStarted { job_id: job.id.clone(), item_id, url },
            );
            self.publish_progress(&job);
        }
        Ok(ok)
    }

    /// Bulk [`skip_item`](Self::skip_item): one transaction for all items, one
    /// settle per job. Only `pending` items are touched. Returns how many.
    pub fn skip_items(&self, item_ids: &[i64], message: &str) -> Result<i64> {
        if item_ids.is_empty() {
            return Ok(0);
        }
        let ids = item_ids.to_vec();
        let message = message.to_string();
        let jobs = self.db.write(move |tx| {
            let now = time::now();
            let mut touched: Vec<String> = Vec::new();
            let mut n = 0i64;
            for chunk in ids.chunks(500) {
                for id in chunk {
                    let Some(item) = load_item(tx, *id)? else { continue };
                    if item.status != "pending" {
                        continue;
                    }
                    tx.execute(
                        "UPDATE job_items SET status = 'skipped', progress = 1.0, message = ?2, finished_at = ?3 \
                         WHERE id = ?1",
                        params![id, message, now],
                    )?;
                    tx.execute("UPDATE jobs SET skipped = skipped + 1 WHERE id = ?1", [&item.job_id])?;
                    if !touched.contains(&item.job_id) {
                        touched.push(item.job_id.clone());
                    }
                    n += 1;
                }
            }
            let mut jobs = Vec::new();
            for j in &touched {
                settle_in(tx, j)?;
                if let Some(job) = load_job(tx, j)? {
                    jobs.push(job);
                }
            }
            Ok((n, jobs))
        })?;
        for j in &jobs.1 {
            self.publish_progress(j);
        }
        Ok(jobs.0)
    }

    /// Replace one item with the several it stood for, inside the same job. The
    /// stand-in is settled `skipped`; new items are appended after everything
    /// queued. One transaction, so the job cannot close in between.
    pub fn expand_item(&self, item_id: i64, items: Vec<NewItem>, message: &str) -> Result<i64> {
        let message = message.to_string();
        let n = items.len() as i64;
        let job = self.db.write(move |tx| {
            let item = load_item(tx, item_id)?.ok_or(DbError::NotFound)?;
            let next_seq: i64 = tx.query_row(
                "SELECT coalesce(max(seq), 0) + 1 FROM job_items WHERE job_id = ?1",
                [&item.job_id],
                |r| r.get(0),
            )?;
            {
                let mut st = tx.prepare(
                    "INSERT INTO job_items (job_id, seq, status, url, url_kind, source, target_dir, track_id, \
                                            release_id, attempts, max_attempts, progress) \
                     VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, 0.0)",
                )?;
                for (off, it) in items.iter().enumerate() {
                    st.execute(params![
                        item.job_id,
                        next_seq + off as i64,
                        it.url,
                        it.url_kind,
                        it.source,
                        it.target_dir,
                        it.track_id,
                        it.release_id,
                        item.max_attempts
                    ])?;
                }
            }
            tx.execute("UPDATE jobs SET total = total + ?2 WHERE id = ?1", params![item.job_id, items.len() as i64])?;
            tx.execute(
                "UPDATE job_items SET status = 'skipped', progress = 1.0, message = ?2, finished_at = ?3, \
                        lease_expires_at = NULL WHERE id = ?1",
                params![item_id, message, time::now()],
            )?;
            tx.execute("UPDATE jobs SET skipped = skipped + 1 WHERE id = ?1", [&item.job_id])?;
            settle_in(tx, &item.job_id)?;
            load_job(tx, &item.job_id)?.ok_or(DbError::NotFound)
        })?;
        self.publish_progress(&job);
        self.notify();
        Ok(n)
    }

    /// Mark an attempt failed. With `retryable` and attempts left the item goes
    /// back to `pending` with a jittered backoff; otherwise it is `failed`.
    pub fn fail_item(&self, item_id: i64, error: &str, error_class: &str, retryable: bool) -> Result<FailOutcome> {
        let error: String = error.chars().take(4000).collect();
        let error_class = error_class.to_string();
        let kind = error_class.clone();
        let (outcome, job, detail) = self.db.write(move |tx| {
            let Some(item) = load_item(tx, item_id)? else {
                return Ok((FailOutcome { will_retry: false, applied: false }, None, String::new()));
            };
            if item.status != "running" {
                return Ok((FailOutcome { will_retry: false, applied: false }, None, String::new()));
            }
            let will_retry = retryable && item.attempts < item.max_attempts;
            if will_retry {
                tx.execute(
                    "UPDATE job_items SET status = 'pending', last_error = ?2, error_class = ?3, \
                            lease_expires_at = NULL, worker_pid = NULL, next_attempt_at = ?4, message = ?5 \
                     WHERE id = ?1",
                    params![
                        item_id,
                        error,
                        error_class,
                        time::plus_secs(backoff_delay(item.attempts)),
                        format!("retrying (attempt {}/{})", item.attempts + 1, item.max_attempts)
                    ],
                )?;
            } else {
                tx.execute(
                    "UPDATE job_items SET status = 'failed', last_error = ?2, error_class = ?3, \
                            lease_expires_at = NULL, worker_pid = NULL, finished_at = ?4 WHERE id = ?1",
                    params![item_id, error, error_class, time::now()],
                )?;
                tx.execute("UPDATE jobs SET failed = failed + 1 WHERE id = ?1", [&item.job_id])?;
                settle_in(tx, &item.job_id)?;
            }
            Ok((FailOutcome { will_retry, applied: true }, load_job(tx, &item.job_id)?, error))
        })?;
        if let (true, Some(job)) = (outcome.applied, job) {
            self.publish(
                dto::TOPIC_JOB_ITEM_FAILED,
                &dto::JobItemFailed {
                    job_id: job.id.clone(),
                    item_id,
                    detail,
                    kind,
                    will_retry: outcome.will_retry,
                },
            );
            self.publish_progress(&job);
        }
        if outcome.will_retry {
            self.notify();
        }
        Ok(outcome)
    }

    /// Settle an in-flight item as `cancelled` -- the user stopped the job. Not a
    /// failure: no counter moves, no retry.
    pub fn cancel_item(&self, item_id: i64, message: &str) -> Result<()> {
        let message = message.to_string();
        let job = self.db.write(move |tx| {
            let Some(item) = load_item(tx, item_id)? else { return Ok(None) };
            tx.execute(
                "UPDATE job_items SET status = 'cancelled', message = ?2, finished_at = ?3, \
                        lease_expires_at = NULL, worker_pid = NULL WHERE id = ?1",
                params![item_id, message, time::now()],
            )?;
            settle_in(tx, &item.job_id)?;
            load_job(tx, &item.job_id)
        })?;
        if let Some(j) = job {
            self.publish_progress(&j);
        }
        Ok(())
    }

    /// Hand an in-flight item back to the queue without it counting as a try (a
    /// pause or shutdown): the attempt the claim charged is refunded and the item
    /// is claimable again with no backoff.
    pub fn release_item(&self, item_id: i64, message: &str) -> Result<()> {
        let message = message.to_string();
        self.db.write(move |tx| {
            tx.execute(
                "UPDATE job_items SET status = 'pending', attempts = max(0, attempts - 1), next_attempt_at = NULL, \
                        lease_expires_at = NULL, worker_pid = NULL, message = ?2 WHERE id = ?1",
                params![item_id, message],
            )?;
            Ok(())
        })?;
        self.notify();
        Ok(())
    }

    // ---- reads ------------------------------------------------------------------------

    pub fn get_job(&self, id: &str) -> Result<Option<Job>> {
        let id = id.to_string();
        self.db.read(move |c| load_job(c, &id))
    }

    pub fn get_item(&self, id: i64) -> Result<Option<JobItem>> {
        self.db.read(move |c| load_item(c, id))
    }

    /// Jobs newest first; `status` / `kind` filters.
    pub fn list_jobs(&self, status: Option<&str>, kind: Option<&str>, offset: i64, limit: i64) -> Result<Page<Job>> {
        let status = status.map(str::to_string);
        let kind = kind.map(str::to_string);
        self.db.read(move |c| {
            let (s, k) = (status.as_deref(), kind.as_deref());
            let total: i64 = c.query_row(
                "SELECT count(*) FROM jobs WHERE (?1 IS NULL OR status = ?1) AND (?2 IS NULL OR kind = ?2)",
                params![s, k],
                |r| r.get(0),
            )?;
            let mut st = c.prepare(&format!(
                "SELECT {JOB_COLS} FROM jobs WHERE (?1 IS NULL OR status = ?1) AND (?2 IS NULL OR kind = ?2) \
                 ORDER BY created_at DESC, id LIMIT ?3 OFFSET ?4"
            ))?;
            let items = st
                .query_map(params![s, k, limit, offset], job_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(Page { items, total, offset, limit })
        })
    }

    /// Count of items by status for a job.
    pub fn item_counts(&self, job_id: &str) -> Result<std::collections::BTreeMap<String, i64>> {
        let id = job_id.to_string();
        self.db.read(move |c| {
            let mut st = c.prepare("SELECT status, count(*) FROM job_items WHERE job_id = ?1 GROUP BY status")?;
            let rows = st
                .query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
    }

    /// A job's items in queue order; optionally one host group and/or statuses.
    pub fn list_items(
        &self,
        job_id: &str,
        group: Option<&str>,
        statuses: &[String],
        offset: i64,
        limit: Option<i64>,
    ) -> Result<Vec<JobItem>> {
        let st = validate_statuses(statuses)?;
        let job_id = job_id.to_string();
        let group = group.map(str::to_string);
        self.db.read(move |c| {
            let sql = format!(
                "{HOSTS_CTE} SELECT {} FROM job_items ji JOIN c ON c.id = ji.id \
                 WHERE (?2 IS NULL OR c.grp = ?2){} ORDER BY ji.seq LIMIT ?3 OFFSET ?4",
                ITEM_COLS.split(", ").map(|c| format!("ji.{}", c.trim())).collect::<Vec<_>>().join(", "),
                status_clause("ji.status", &st),
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map(params![job_id, group, limit.unwrap_or(-1), offset], item_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// A job's items clustered by host, in queue order. `statuses` narrows which
    /// items count as visible (groups with none are dropped, the rest are ordered by
    /// their earliest visible item); counts always describe the whole group.
    pub fn list_groups(
        &self,
        job_id: &str,
        statuses: &[String],
        offset: i64,
        limit: i64,
    ) -> Result<(Vec<ItemGroup>, i64)> {
        let st = validate_statuses(statuses)?;
        let job_id = job_id.to_string();
        self.db.read(move |c| {
            let vis = if st.is_empty() {
                "1".to_string()
            } else {
                format!("status IN ({})", st.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(","))
            };
            let base = format!(
                "{HOSTS_CTE}, g AS (SELECT grp, count(*) AS total, \
                    sum(CASE WHEN {vis} THEN 1 ELSE 0 END) AS visible, \
                    sum(status = 'pending') AS pending, sum(status = 'running') AS running, \
                    sum(status = 'done') AS done, sum(status = 'failed') AS failed, \
                    sum(status = 'skipped') AS skipped, sum(status = 'cancelled') AS cancelled, \
                    min(seq) AS first_seq, min(CASE WHEN {vis} THEN seq END) AS order_seq \
                  FROM c GROUP BY grp)"
            );
            let having = if st.is_empty() { "" } else { " WHERE order_seq IS NOT NULL" };
            let total: i64 =
                c.query_row(&format!("{base} SELECT count(*) FROM g{having}"), [&job_id], |r| r.get(0))?;
            let mut stmt = c.prepare(&format!(
                "{base} SELECT grp, total, visible, pending, running, done, failed, skipped, cancelled, first_seq \
                 FROM g{having} ORDER BY order_seq, grp LIMIT ?2 OFFSET ?3"
            ))?;
            let mut groups = stmt
                .query_map(params![job_id, limit, offset], |r| {
                    Ok(ItemGroup {
                        key: r.get(0)?,
                        total: r.get(1)?,
                        visible: r.get(2)?,
                        pending: r.get(3)?,
                        running: r.get(4)?,
                        done: r.get(5)?,
                        failed: r.get(6)?,
                        skipped: r.get(7)?,
                        cancelled: r.get(8)?,
                        first_seq: r.get(9)?,
                        item: None,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            // One query for every singleton on the page.
            let singles: HashSet<String> = groups.iter().filter(|g| g.visible == 1).map(|g| g.key.clone()).collect();
            if !singles.is_empty() {
                let sql = format!(
                    "{HOSTS_CTE} SELECT c.grp, {} FROM job_items ji JOIN c ON c.id = ji.id WHERE 1=1{}",
                    ITEM_COLS.split(", ").map(|c| format!("ji.{}", c.trim())).collect::<Vec<_>>().join(", "),
                    status_clause("ji.status", &st)
                );
                let mut stmt = c.prepare(&sql)?;
                let mut rows = stmt.query([&job_id])?;
                while let Some(r) = rows.next()? {
                    let grp: String = r.get(0)?;
                    if singles.contains(&grp) {
                        let item = item_from_row_offset(r, 1)?;
                        if let Some(g) = groups.iter_mut().find(|g| g.key == grp) {
                            g.item = Some(item);
                        }
                    }
                }
            }
            Ok((groups, total))
        })
    }

    /// Resolve a selection -- explicit items plus whole groups -- to item ids in
    /// queue order. Explicit ids need only be members of the job; the status filter
    /// applies to groups.
    pub fn select_item_ids(
        &self,
        job_id: &str,
        item_ids: &[i64],
        groups: &[String],
        statuses: &[String],
    ) -> Result<Vec<i64>> {
        if item_ids.is_empty() && groups.is_empty() {
            return Ok(vec![]);
        }
        let st = validate_statuses(statuses)?;
        let job_id = job_id.to_string();
        let item_ids = item_ids.to_vec();
        let groups = groups.to_vec();
        self.db.read(move |c| {
            let mut stmt = c.prepare(&format!("{HOSTS_CTE} SELECT id, seq, status, grp FROM c ORDER BY seq"))?;
            let rows = stmt
                .query_map([&job_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let want_ids: HashSet<i64> = item_ids.into_iter().collect();
            let want_groups: HashSet<&str> = groups.iter().map(String::as_str).collect();
            Ok(rows
                .into_iter()
                .filter(|(id, status, grp)| {
                    want_ids.contains(id)
                        || (want_groups.contains(grp.as_str()) && (st.is_empty() || st.contains(&status.as_str())))
                })
                .map(|(id, _, _)| id)
                .collect())
        })
    }

    /// The id of a group's first (`last == false`) or last visible item, as an anchor.
    pub fn group_edge(&self, job_id: &str, group: &str, last: bool, statuses: &[String]) -> Result<Option<i64>> {
        let st = validate_statuses(statuses)?;
        let (job_id, group) = (job_id.to_string(), group.to_string());
        self.db.read(move |c| {
            let sql = format!(
                "{HOSTS_CTE} SELECT id FROM c WHERE grp = ?2{} ORDER BY seq {} LIMIT 1",
                status_clause("status", &st),
                if last { "DESC" } else { "ASC" }
            );
            Ok(c.query_row(&sql, params![job_id, group], |r| r.get(0)).optional()?)
        })
    }

    /// URLs of a job's items that never got downloaded (everything not done/skipped).
    pub fn unrun_urls(&self, job_id: &str) -> Result<Vec<String>> {
        let id = job_id.to_string();
        self.db.read(move |c| {
            let mut st = c.prepare(
                "SELECT url FROM job_items WHERE job_id = ?1 AND url IS NOT NULL AND status NOT IN ('done','skipped')",
            )?;
            let rows = st.query_map([id], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
    }

    /// True if cancel was requested for the job (workers poll this mid-item).
    pub fn is_cancel_requested(&self, job_id: &str) -> Result<bool> {
        let id = job_id.to_string();
        self.db.read(move |c| {
            Ok(c.query_row("SELECT cancel_requested FROM jobs WHERE id = ?1", [id], |r| r.get::<_, i64>(0))
                .optional()?
                .map(|v| v != 0)
                .unwrap_or(true))
        })
    }

    // ---- job control --------------------------------------------------------------------

    /// Re-run settle for one job (public for workers that edit counters themselves).
    pub fn settle_job(&self, job_id: &str) -> Result<Option<Job>> {
        let id = job_id.to_string();
        let (changed, job) = self.db.write(move |tx| Ok((settle_in(tx, &id)?, load_job(tx, &id)?)))?;
        if let (true, Some(j)) = (changed, &job) {
            self.publish_progress(j);
        }
        Ok(job)
    }

    /// Set a job-level error string (e.g. a harvest job's fatal error).
    pub fn set_job_error(&self, job_id: &str, error: &str) -> Result<()> {
        let (id, e) = (job_id.to_string(), error.to_string());
        self.db.write(move |tx| {
            tx.execute("UPDATE jobs SET error = ?2 WHERE id = ?1", params![id, e])?;
            Ok(())
        })
    }

    /// Stop a job: nothing pending will run, and it closes as `cancelled` once its
    /// in-flight items are settled by their workers. One UPDATE for the pending
    /// rows, however many.
    pub fn cancel_job(&self, job_id: &str) -> Result<Option<Job>> {
        let id = job_id.to_string();
        let job = self.db.write(move |tx| {
            if load_job(tx, &id)?.is_none() {
                return Ok(None);
            }
            tx.execute("UPDATE jobs SET cancel_requested = 1 WHERE id = ?1", [&id])?;
            tx.execute(
                "UPDATE job_items SET status = 'cancelled', finished_at = ?2, lease_expires_at = NULL \
                 WHERE job_id = ?1 AND status = 'pending'",
                params![id, time::now()],
            )?;
            settle_in(tx, &id)?;
            load_job(tx, &id)
        })?;
        if let Some(j) = &job {
            self.publish_progress(j);
        }
        Ok(job)
    }

    /// Hold a job: nothing more is claimed from it until [`resume_job`](Self::resume_job).
    /// A finished job is returned unchanged.
    pub fn pause_job(&self, job_id: &str) -> Result<Option<Job>> {
        let id = job_id.to_string();
        self.db.write(move |tx| {
            let Some(job) = load_job(tx, &id)? else { return Ok(None) };
            if matches!(job.status.as_str(), "queued" | "running") {
                tx.execute("UPDATE jobs SET status = 'paused' WHERE id = ?1", [&id])?;
            }
            load_job(tx, &id)
        })
    }

    /// Let a paused job run again; anything else is returned unchanged. A job
    /// paused with nothing left to do closes now.
    pub fn resume_job(&self, job_id: &str) -> Result<Option<Job>> {
        let id = job_id.to_string();
        let job = self.db.write(move |tx| {
            let Some(job) = load_job(tx, &id)? else { return Ok(None) };
            if job.status == "paused" {
                tx.execute("UPDATE jobs SET status = 'queued' WHERE id = ?1", [&id])?;
                settle_in(tx, &id)?;
            }
            load_job(tx, &id)
        })?;
        self.notify();
        Ok(job)
    }

    /// Requeue every failed item in a job. Returns how many were reset.
    pub fn retry_failed(&self, job_id: &str) -> Result<i64> {
        let id = job_id.to_string();
        let n = self.db.write(move |tx| {
            if load_job(tx, &id)?.is_none() {
                return Ok(0);
            }
            let n = tx.execute(
                "UPDATE job_items SET status = 'pending', attempts = 0, next_attempt_at = NULL, last_error = NULL, \
                        error_class = NULL, finished_at = NULL WHERE job_id = ?1 AND status = 'failed'",
                [&id],
            )? as i64;
            if n > 0 {
                tx.execute(
                    "UPDATE jobs SET failed = max(0, failed - ?2), status = 'queued', finished_at = NULL, \
                            cancel_requested = 0 WHERE id = ?1",
                    params![id, n],
                )?;
            }
            Ok(n)
        })?;
        if n > 0 {
            self.notify();
        }
        Ok(n)
    }

    /// Delete a job and (by cascade) its items. Callers cancel first.
    pub fn delete_job(&self, job_id: &str) -> Result<bool> {
        let id = job_id.to_string();
        let n = self.db.write(move |tx| Ok(tx.execute("DELETE FROM jobs WHERE id = ?1", [&id])?))?;
        if n > 0 {
            self.publish(dto::TOPIC_JOB_DELETED, &dto::JobRef { job_id: job_id.to_string() });
            self.invalidate_jobs();
        }
        Ok(n > 0)
    }

    /// Delete jobs that finished clean (`completed` with no failures). Returns their ids.
    pub fn clear_finished(&self) -> Result<Vec<String>> {
        let ids = self.db.write(move |tx| {
            let mut st = tx.prepare("SELECT id FROM jobs WHERE status = 'completed' AND failed = 0")?;
            let ids: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            drop(st);
            for id in &ids {
                tx.execute("DELETE FROM jobs WHERE id = ?1", [id])?;
            }
            Ok(ids)
        })?;
        for id in &ids {
            self.publish(dto::TOPIC_JOB_DELETED, &dto::JobRef { job_id: id.clone() });
        }
        Ok(ids)
    }

    /// Delete items from a job, keeping its counters honest. Running items stay.
    pub fn remove_items(&self, job_id: &str, item_ids: &[i64]) -> Result<RemoveReport> {
        let id = job_id.to_string();
        let wanted: Vec<i64> = item_ids.to_vec();
        let (report, job) = self.db.write(move |tx| {
            let mut report = RemoveReport::default();
            if wanted.is_empty() || load_job(tx, &id)?.is_none() {
                return Ok((report, None));
            }
            let mut list = wanted.clone();
            list.sort_unstable();
            list.dedup();
            for iid in list {
                let Some(item) = load_item(tx, iid)? else { continue };
                if item.job_id != id {
                    continue;
                }
                if item.status == "running" {
                    report.kept_running += 1;
                    continue;
                }
                match item.status.as_str() {
                    "done" => tx.execute("UPDATE jobs SET completed = max(0, completed - 1) WHERE id = ?1", [&id])?,
                    "failed" => tx.execute("UPDATE jobs SET failed = max(0, failed - 1) WHERE id = ?1", [&id])?,
                    "skipped" => tx.execute("UPDATE jobs SET skipped = max(0, skipped - 1) WHERE id = ?1", [&id])?,
                    _ => 0,
                };
                if !matches!(item.status.as_str(), "done" | "skipped") {
                    if let Some(u) = &item.url {
                        report.unrun_urls.push(u.clone());
                    }
                }
                tx.execute("UPDATE jobs SET total = max(0, total - 1) WHERE id = ?1", [&id])?;
                tx.execute("DELETE FROM job_items WHERE id = ?1", [iid])?;
                report.removed += 1;
            }
            if report.removed > 0 {
                settle_in(tx, &id)?;
            }
            Ok((report, load_job(tx, &id)?))
        })?;
        if report.removed > 0 {
            if let Some(j) = &job {
                self.publish_progress(j);
            }
        }
        Ok(report)
    }

    /// Reorder: put `item_ids` at the top or bottom, or right before/after
    /// `anchor_id`. Returns how many rows changed position. `seq` is the order the
    /// claim walks; gaps are preserved and the moved block keeps its internal order.
    pub fn move_items(&self, job_id: &str, item_ids: &[i64], place: Place, anchor_id: Option<i64>) -> Result<i64> {
        if item_ids.is_empty() {
            return Ok(0);
        }
        let id = job_id.to_string();
        let wanted: HashSet<i64> = item_ids.iter().copied().collect();
        let moved = self.db.write(move |tx| {
            let mut st = tx.prepare("SELECT id, seq FROM job_items WHERE job_id = ?1 ORDER BY seq")?;
            let rows: Vec<(i64, i64)> =
                st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            drop(st);
            let ordered: Vec<i64> = rows.iter().map(|r| r.0).collect();
            let seqs: Vec<i64> = rows.iter().map(|r| r.1).collect();
            let old: std::collections::HashMap<i64, i64> = rows.iter().copied().collect();
            let moving: Vec<i64> = ordered.iter().copied().filter(|i| wanted.contains(i)).collect();
            if moving.is_empty() {
                return Ok(0);
            }
            let rest: Vec<i64> = ordered.iter().copied().filter(|i| !wanted.contains(i)).collect();
            let new: Vec<i64> = match place {
                Place::Top => moving.iter().chain(rest.iter()).copied().collect(),
                Place::Bottom => rest.iter().chain(moving.iter()).copied().collect(),
                Place::Before | Place::After => {
                    let Some(a) = anchor_id else { return Ok(0) };
                    if wanted.contains(&a) || !old.contains_key(&a) {
                        return Ok(0);
                    }
                    let pos = rest.iter().position(|x| *x == a).unwrap_or(0) + usize::from(place == Place::After);
                    rest[..pos].iter().chain(moving.iter()).chain(rest[pos..].iter()).copied().collect()
                }
            };
            let changed: Vec<(i64, i64)> =
                new.iter().zip(seqs.iter()).filter(|(i, s)| old[*i] != **s).map(|(i, s)| (*i, *s)).collect();
            if changed.is_empty() {
                return Ok(0);
            }
            let park = *seqs.last().unwrap_or(&0) + 1;
            for (i, _) in &changed {
                tx.execute("UPDATE job_items SET seq = seq + ?2 WHERE id = ?1", params![i, park])?;
            }
            for (i, s) in &changed {
                tx.execute("UPDATE job_items SET seq = ?2 WHERE id = ?1", params![i, s])?;
            }
            Ok(changed.len() as i64)
        })?;
        if moved > 0 {
            self.publish(dto::TOPIC_JOB_REORDERED, &dto::JobReordered { job_id: job_id.to_string(), moved });
        }
        Ok(moved)
    }

    // ---- recovery -------------------------------------------------------------------------

    /// Startup recovery. Nothing can legitimately be `running` right after a boot,
    /// so anything in that state was interrupted: requeue (or fail when attempts are
    /// exhausted), put `running` jobs back to `queued` and settle open jobs. Must run
    /// **before any worker starts** (see `JobsService::start`).
    pub fn reconcile(&self) -> Result<ReconcileReport> {
        let (report, jobs) = self.db.write(move |tx| {
            let mut report = ReconcileReport::default();
            let now = time::now();
            let mut st = tx.prepare("SELECT id, job_id, attempts, max_attempts FROM job_items WHERE status = 'running'")?;
            let stuck: Vec<(i64, String, i64, i64)> = st
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            drop(st);
            for (id, job_id, attempts, max) in stuck {
                if attempts < max {
                    tx.execute(
                        "UPDATE job_items SET status = 'pending', error_class = 'crash', \
                                last_error = 'Interrupted by server restart', next_attempt_at = NULL, \
                                lease_expires_at = NULL, worker_pid = NULL WHERE id = ?1",
                        [id],
                    )?;
                    report.requeued += 1;
                } else {
                    tx.execute(
                        "UPDATE job_items SET status = 'failed', error_class = 'crash', \
                                last_error = 'Interrupted by server restart; attempts exhausted', \
                                lease_expires_at = NULL, worker_pid = NULL, finished_at = ?2 WHERE id = ?1",
                        params![id, now],
                    )?;
                    tx.execute("UPDATE jobs SET failed = failed + 1 WHERE id = ?1", [&job_id])?;
                    report.failed += 1;
                }
            }
            tx.execute("UPDATE jobs SET status = 'queued' WHERE status = 'running'", [])?;
            let mut st = tx.prepare("SELECT id, status FROM jobs WHERE status IN ('queued','running','paused')")?;
            let open: Vec<(String, String)> =
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            drop(st);
            let mut jobs = Vec::new();
            for (jid, before) in open {
                settle_in(tx, &jid)?;
                if let Some(j) = load_job(tx, &jid)? {
                    if j.status != before {
                        report.jobs_settled += 1;
                    }
                    jobs.push(j);
                }
            }
            Ok((report, jobs))
        })?;
        if report.requeued > 0 || report.failed > 0 {
            tracing::info!(
                "recovery: requeued {} item(s), failed {}, settled {} job(s)",
                report.requeued,
                report.failed,
                report.jobs_settled
            );
        }
        for j in &jobs {
            self.publish_progress(j);
        }
        self.notify();
        Ok(report)
    }

    /// Lease reaper: requeue `running` items whose lease expired (worker died or
    /// wedged); fail them when attempts are exhausted. Returns `(requeued, failed)`.
    pub fn reap_expired(&self) -> Result<(i64, i64)> {
        let (counts, jobs) = self.db.write(move |tx| {
            let now = time::now();
            let mut st = tx.prepare(
                "SELECT id, job_id, attempts, max_attempts FROM job_items \
                 WHERE status = 'running' AND lease_expires_at IS NOT NULL AND lease_expires_at < ?1",
            )?;
            let stuck: Vec<(i64, String, i64, i64)> = st
                .query_map([&now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            drop(st);
            let (mut requeued, mut failed) = (0, 0);
            let mut touched: Vec<String> = Vec::new();
            for (id, job_id, attempts, max) in stuck {
                if attempts < max {
                    tx.execute(
                        "UPDATE job_items SET status = 'pending', error_class = 'lease_expired', \
                                last_error = 'Worker lease expired', next_attempt_at = NULL, \
                                lease_expires_at = NULL, worker_pid = NULL, message = 'requeued (lease expired)' \
                         WHERE id = ?1",
                        [id],
                    )?;
                    requeued += 1;
                } else {
                    tx.execute(
                        "UPDATE job_items SET status = 'failed', error_class = 'lease_expired', \
                                last_error = 'Worker lease expired; attempts exhausted', \
                                lease_expires_at = NULL, worker_pid = NULL, finished_at = ?2 WHERE id = ?1",
                        params![id, now],
                    )?;
                    tx.execute("UPDATE jobs SET failed = failed + 1 WHERE id = ?1", [&job_id])?;
                    failed += 1;
                }
                if !touched.contains(&job_id) {
                    touched.push(job_id);
                }
            }
            let mut jobs = Vec::new();
            for j in touched {
                settle_in(tx, &j)?;
                if let Some(job) = load_job(tx, &j)? {
                    jobs.push(job);
                }
            }
            Ok(((requeued, failed), jobs))
        })?;
        for j in &jobs {
            self.publish_progress(j);
        }
        if counts.0 > 0 {
            self.notify();
        }
        Ok(counts)
    }
}

fn item_from_row_offset(r: &rusqlite::Row<'_>, o: usize) -> rusqlite::Result<JobItem> {
    Ok(JobItem {
        id: r.get(o)?,
        job_id: r.get(o + 1)?,
        seq: r.get(o + 2)?,
        status: r.get(o + 3)?,
        url: r.get(o + 4)?,
        url_kind: r.get(o + 5)?,
        source: r.get(o + 6)?,
        target_dir: r.get(o + 7)?,
        track_id: r.get(o + 8)?,
        attempts: r.get(o + 9)?,
        max_attempts: r.get(o + 10)?,
        next_attempt_at: r.get(o + 11)?,
        lease_expires_at: r.get(o + 12)?,
        worker_pid: r.get(o + 13)?,
        progress: r.get(o + 14)?,
        message: r.get(o + 15)?,
        last_error: r.get(o + 16)?,
        error_class: r.get(o + 17)?,
        result: r.get(o + 18)?,
        release_id: r.get(o + 19)?,
        started_at: r.get(o + 20)?,
        finished_at: r.get(o + 21)?,
    })
}
