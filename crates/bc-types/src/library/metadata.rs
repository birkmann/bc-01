//! Metadata editing DTOs: DJ-field write-back from analysis (preview, bulk write with a
//! mandatory dry-run, JSONL undo journal).

use serde::{Deserialize, Serialize};

use crate::TrackId;

/// Which logical tag groups to write: `bpm`, `key`, `camelot`, `energy`, `replaygain`.
pub type FieldGroup = String;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MetadataStatus {
    pub total_tracks: i64,
    pub analysed: i64,
    pub writable_extensions: Vec<String>,
    pub default_groups: Vec<FieldGroup>,
    pub write_mode: String,
    pub running_jobs: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FieldPlanOut {
    pub field: String,
    /// `gap` | `kept` | `conflict` | `no_source`
    pub status: String,
    pub existing: Option<String>,
    pub proposed: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackPlanOut {
    pub track_id: TrackId,
    /// `written` | `no_gaps` | `not_analysed` | `unsupported` | `missing` | `failed`
    pub status: String,
    pub message: String,
    #[serde(default)]
    pub would_write: Vec<String>,
    #[serde(default)]
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub fields: Vec<FieldPlanOut>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PlanSummary {
    pub tracks: i64,
    pub would_write: i64,
    pub no_gaps: i64,
    pub conflicts: i64,
    pub not_analysed: i64,
    pub unsupported: i64,
    #[serde(default)]
    pub per_field: std::collections::BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MetadataScope {
    #[default]
    Analysed,
    All,
    Ids,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PreviewRequest {
    pub scope: MetadataScope,
    pub track_ids: Vec<TrackId>,
    pub groups: Vec<FieldGroup>,
    pub limit: i64,
}
impl Default for PreviewRequest {
    fn default() -> Self {
        Self { scope: MetadataScope::Analysed, track_ids: vec![], groups: vec![], limit: 50 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreviewOut {
    pub summary: PlanSummary,
    pub items: Vec<TrackPlanOut>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WriteRequest {
    pub scope: MetadataScope,
    pub track_ids: Vec<TrackId>,
    pub groups: Vec<FieldGroup>,
    pub limit: Option<i64>,
    /// Default true. A real write needs `dry_run=false` AND `confirm=true`.
    pub dry_run: bool,
    pub confirm: bool,
    /// `auto` | `atomic` (this implementation always copy-edits atomically).
    pub mode: Option<String>,
}
impl Default for WriteRequest {
    fn default() -> Self {
        Self {
            scope: MetadataScope::Analysed,
            track_ids: vec![],
            groups: vec![],
            limit: None,
            dry_run: true,
            confirm: false,
            mode: None,
        }
    }
}

/// `POST /metadata/write` response (202): the work runs as a tracked job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct WriteQueued {
    pub queued: i64,
    pub job_id: Option<String>,
    pub dry_run: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UndoOut {
    pub job_id: String,
    pub entries: i64,
    pub restored: i64,
    pub skipped_changed: i64,
    pub missing: i64,
    pub failed: i64,
}

/// Payload of `metadata.progress`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MetadataProgress {
    pub job_id: String,
    pub done: i64,
    pub total: i64,
    pub written: i64,
    pub failed: i64,
    pub finished: bool,
}
