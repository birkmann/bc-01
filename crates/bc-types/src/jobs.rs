//! Workstream 2: the durable job queue (`bc-jobs`) as the UI and other
//! workstreams see it. Timestamps are ISO-8601 strings (UTC) so this module has
//! no chrono dependency and compiles for wasm32.

use serde::{Deserialize, Serialize};

use crate::JobId;

// -- job kinds -----------------------------------------------------------------
pub const KIND_DOWNLOAD: &str = "download";
pub const KIND_ANALYZE: &str = "analyze";
pub const KIND_SCAN: &str = "scan";
pub const KIND_METADATA_BULK: &str = "metadata_bulk";
pub const KIND_HARVEST_IMPORT: &str = "harvest_import";
// Beyond the legacy CHECK constraint (needs `req_ws2_jobs_kinds.sql`):
pub const KIND_MOVE: &str = "move";
pub const KIND_HARVEST: &str = "harvest";
pub const KIND_WALK: &str = "walk";
pub const KIND_SWEEP: &str = "sweep";
pub const KIND_ENRICH: &str = "enrich";
pub const KIND_RELINK: &str = "relink";

// -- job statuses --------------------------------------------------------------
pub const JOB_QUEUED: &str = "queued";
pub const JOB_RUNNING: &str = "running";
pub const JOB_PAUSED: &str = "paused";
pub const JOB_COMPLETED: &str = "completed";
pub const JOB_FAILED: &str = "failed";
pub const JOB_CANCELLED: &str = "cancelled";

// -- item statuses -------------------------------------------------------------
pub const ITEM_PENDING: &str = "pending";
pub const ITEM_RUNNING: &str = "running";
pub const ITEM_DONE: &str = "done";
pub const ITEM_FAILED: &str = "failed";
pub const ITEM_SKIPPED: &str = "skipped";
pub const ITEM_CANCELLED: &str = "cancelled";

// -- WS topics -----------------------------------------------------------------
pub const TOPIC_JOB_CREATED: &str = "job.created";
pub const TOPIC_JOB_PROGRESS: &str = "job.progress";
pub const TOPIC_JOB_PAUSED: &str = "job.paused";
pub const TOPIC_JOB_RESUMED: &str = "job.resumed";
pub const TOPIC_JOB_CANCELLED: &str = "job.cancelled";
pub const TOPIC_JOB_DELETED: &str = "job.deleted";
pub const TOPIC_JOB_REORDERED: &str = "job.reordered";
pub const TOPIC_JOB_RETRIED: &str = "job.retried";
pub const TOPIC_JOB_ITEM_STARTED: &str = "job.item.started";
pub const TOPIC_JOB_ITEM_PROGRESS: &str = "job.item.progress";
pub const TOPIC_JOB_ITEM_COMPLETED: &str = "job.item.completed";
pub const TOPIC_JOB_ITEM_FAILED: &str = "job.item.failed";
pub const TOPIC_JOB_ITEM_SKIPPED: &str = "job.item.skipped";
pub const TOPIC_DOWNLOADS_DISK: &str = "downloads.disk";

// -- DTOs ------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobItemOut {
    pub id: i64,
    pub seq: i64,
    pub status: String,
    pub url: Option<String>,
    pub url_kind: Option<String>,
    #[serde(default)]
    pub progress: f64,
    pub message: Option<String>,
    #[serde(default)]
    pub attempts: i64,
    #[serde(default = "d_max_attempts")]
    pub max_attempts: i64,
    pub last_error: Option<String>,
    pub error_class: Option<String>,
    pub release_id: Option<i64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}
fn d_max_attempts() -> i64 {
    3
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobOut {
    pub id: JobId,
    pub kind: String,
    pub status: String,
    pub label: Option<String>,
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
    pub skipped: i64,
    /// `(completed + failed + skipped) / total`, 0 when `total == 0`.
    pub progress: f64,
    pub error: Option<String>,
    pub created_at: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// A job's items sharing one host (on Bandcamp: one artist or label).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobItemGroupOut {
    pub key: String,
    pub total: i64,
    /// How many of these match the status filter the list was asked with.
    pub visible: i64,
    pub pending: i64,
    pub running: i64,
    pub done: i64,
    pub failed: i64,
    pub skipped: i64,
    pub cancelled: i64,
    pub first_seq: i64,
    /// The one visible item when `visible == 1`.
    pub item: Option<JobItemOut>,
}

/// What an item-level action applies to: explicit items and/or whole groups.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ItemSelection {
    #[serde(default)]
    pub item_ids: Vec<i64>,
    #[serde(default)]
    pub groups: Vec<String>,
    /// Comma-free single status filter that narrows `groups` (legacy: a status string, e.g. "failed" or "pending,running").
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MoveItemsRequest {
    #[serde(flatten)]
    pub selection: ItemSelection,
    /// `top` | `bottom` | `before` | `after`
    #[serde(default = "d_top")]
    pub place: String,
    #[serde(default)]
    pub anchor_item_id: Option<i64>,
    #[serde(default)]
    pub anchor_group: Option<String>,
}
fn d_top() -> String {
    "top".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MovedOut {
    pub moved: i64,
    pub job: JobOut,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemovedOut {
    pub removed: i64,
    /// Items left in place because a worker is mid-download on them.
    pub kept_running: i64,
    /// `None` when the removal emptied the job and it was deleted.
    pub job: Option<JobOut>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClearedOut {
    pub deleted: i64,
}

// -- event payloads ----------------------------------------------------------------

/// `job.created`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobCreated {
    pub job_id: JobId,
    pub label: Option<String>,
    pub total: i64,
}

/// `job.progress` (also settles: status completed/failed/cancelled).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobProgress {
    pub job_id: JobId,
    pub status: String,
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
    pub skipped: i64,
}

/// `job.paused|resumed|cancelled|deleted`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobRef {
    pub job_id: JobId,
}

/// `job.reordered`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobReordered {
    pub job_id: JobId,
    pub moved: i64,
}

/// `job.retried`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobRetried {
    pub job_id: JobId,
    pub requeued: i64,
}

/// `job.item.started|skipped`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobItemStarted {
    pub job_id: JobId,
    pub item_id: i64,
    pub url: Option<String>,
}

/// `job.item.progress`, coalesced to at most 4 per second per item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobItemProgress {
    pub job_id: JobId,
    pub item_id: i64,
    pub progress: f64,
    pub message: String,
}

/// `job.item.completed`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobItemCompleted {
    pub job_id: JobId,
    pub item_id: i64,
    pub detail: String,
}

/// `job.item.failed`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobItemFailed {
    pub job_id: JobId,
    pub item_id: i64,
    pub detail: String,
    /// error class: not_found | network | timeout | partial | no_output | crash | unsupported_url | ...
    pub kind: String,
    pub will_retry: bool,
}

/// `downloads.disk` and `GET/PUT /downloads/disk`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiskOut {
    pub path: String,
    pub free_bytes: Option<i64>,
    pub min_free_bytes: i64,
    pub held: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiskIn {
    pub min_free_bytes: i64,
}

/// One format Bandcamp sells (`flac`, `mp3-320`, ...).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FormatOption {
    pub key: String,
    pub label: String,
}

/// `GET/PUT /downloads/format`: the format purchases are downloaded in.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DownloadFormatOut {
    /// `None`: everything comes from the public 128 kbps stream.
    pub format: Option<String>,
    /// The formats on offer, best first.
    pub formats: Vec<FormatOption>,
    /// Whether a Bandcamp cookie is set (purchases can only be found with one).
    pub cookie: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DownloadFormatIn {
    /// A format key, or `None` for the public stream.
    pub format: Option<String>,
}
