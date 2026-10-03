//! Maintenance DTOs: cleanup, blacklist, strays, completeness/fill, delete.

use serde::{Deserialize, Serialize};

use super::ReleaseOut;
use crate::{ReleaseId, TrackId};

// --- delete -------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DeleteReleasesRequest {
    #[serde(default)]
    pub ids: Vec<ReleaseId>,
    /// Also record these as never-download-again.
    #[serde(default)]
    pub blacklist: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DeleteReleasesResult {
    pub releases: i64,
    pub tracks: i64,
    pub files: i64,
    pub blacklisted: i64,
    pub inbox_ignored: i64,
    #[serde(default)]
    pub errors: Vec<String>,
}

/// `DELETE /tracks/{id}` and `DELETE /releases/{id}`: `{ "tracks": n, "files": n }`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DeletedOut {
    pub tracks: i64,
    pub files: i64,
}

// --- remove from library (files stay on disk) -----------------------------------------

/// `POST /tracks/remove`: drop tracks from the library and keep their files; the paths are
/// excluded so the next scan does not bring them back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RemoveTracksRequest {
    #[serde(default)]
    pub track_ids: Vec<TrackId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RemovedOut {
    pub tracks: i64,
    /// Releases that lost their last track and went with it.
    pub releases: i64,
    /// File paths now excluded from scans.
    pub excluded: i64,
}

/// One row of `GET /library/excluded`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ExcludedOut {
    pub path: String,
    pub artist_name: String,
    pub title: String,
    pub added_at: Option<String>,
}

/// `POST /library/excluded/restore`: let these paths back in and ingest the ones still on disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RestoreExcludedRequest {
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RestoredOut {
    /// Exclusions lifted.
    pub restored: i64,
    /// Tracks back in the library (a path that no longer exists is lifted but adds nothing).
    pub tracks_added: i64,
    #[serde(default)]
    pub errors: Vec<String>,
}

// --- completeness / fill ------------------------------------------------------------

/// Result of queueing a re-download of one release (`POST /releases/{id}/fill`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FillResult {
    pub job_id: String,
    pub url: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FillAllResult {
    /// Releases short of tracks.
    pub missing: i64,
    pub queued: i64,
    pub already_queued: i64,
    /// Nothing ever linked them to Bandcamp.
    pub unfillable: i64,
    #[serde(default)]
    pub job_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LovedDownloadOut {
    pub queued: i64,
    pub already_owned: i64,
    pub skipped: i64,
    pub resolved: i64,
    pub job_id: Option<String>,
    pub detail: String,
}

// --- strays ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrayOut {
    pub release_id: ReleaseId,
    pub title: String,
    pub artist: Option<String>,
    pub track_no: Option<i64>,
    pub url: Option<String>,
    /// False when nothing ever linked this release to Bandcamp.
    pub resolvable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct StraysOut {
    pub total: i64,
    pub resolvable: i64,
    pub items: Vec<StrayOut>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct StraySweepStatus {
    pub phase: String,
    pub running: bool,
    pub seen: i64,
    pub total: Option<i64>,
    pub merged: i64,
    pub albums_created: i64,
    pub singles: i64,
    pub albums: i64,
    #[serde(default)]
    pub fills_queued: i64,
    pub unresolved: i64,
    pub failed: i64,
    pub current: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct StrayMergeRequest {
    /// Merge just these releases; empty = every stray in scope.
    #[serde(default)]
    pub ids: Vec<ReleaseId>,
    pub label_id: Option<i64>,
    pub limit: Option<i64>,
}

// --- cleanup / blacklist --------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CleanupCandidate {
    pub release: ReleaseOut,
    /// Longest track; null when no track has a readable duration (never flagged short).
    pub longest_ms: Option<i64>,
    pub track_count: i64,
    /// `short-tracks` and/or `title`
    #[serde(default)]
    pub reasons: Vec<String>,
    #[serde(default)]
    pub matched_phrases: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CleanupOut {
    pub items: Vec<CleanupCandidate>,
    pub total: i64,
    pub max_track_s: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct CleanupQuery {
    pub max_track_s: Option<i64>,
    pub include_titles: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct BlacklistOut {
    pub id: i64,
    pub url: Option<String>,
    #[serde(default)]
    pub artist_name: String,
    #[serde(default)]
    pub title: String,
    pub reason: Option<String>,
    pub added_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct BlacklistAdd {
    pub url: Option<String>,
    #[serde(default)]
    pub artist_name: String,
    #[serde(default)]
    pub title: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct BlacklistQuery {
    pub q: Option<String>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}
