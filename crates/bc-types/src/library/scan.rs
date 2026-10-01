//! Scanner / ingest / art-job DTOs (owned by the bc-scan sub-agent of workstream 1).
//!
//! `ScanResult`, `ScanStatus`, `ScanProgress`, `RootOut`, `AddRootRequest` and `RootPatch`
//! live in the parent module; this file only holds what the scanner adds on top.

use serde::{Deserialize, Serialize};

/// What an ingest of explicit paths touched (`bc_library::ingest::ingest_paths` / `ingest_dir`).
/// Returned to the download worker, which uses `releases_created` to decide what a
/// fan-shelf download may be filed under.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct IngestReport {
    pub files_seen: i64,
    pub files_added: i64,
    pub files_updated: i64,
    pub tracks_added: i64,
    pub tracks_updated: i64,
    /// Every track id written or confirmed by this ingest.
    pub track_ids: Vec<i64>,
    pub release_ids: Vec<i64>,
    /// The subset of `release_ids` that did not exist before this ingest.
    pub releases_created: Vec<i64>,
    #[serde(default)]
    pub errors: Vec<String>,
}

/// Payload of `library.art.progress` (legacy cover conversion job).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ArtProgress {
    pub job_id: String,
    pub done: i64,
    pub total: i64,
    pub failed: i64,
    pub finished: bool,
}
