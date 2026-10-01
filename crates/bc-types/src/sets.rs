//! DJ-set DTOs (workstream 3): slots, transitions and summaries (`bc_music::setmath`),
//! pool sources, set suggestions and automix. CRUD storage/routers belong to workstream 1
//! and use these types; pool/suggest/automix routers belong to workstream 3.

use serde::{Deserialize, Serialize};

use crate::analysis::{Compatibility, Verdict};
use crate::suggest::{SuggestResponse, SuggestTrack};
use crate::{Page, SetId, TrackId};

pub const AUTOMIX_MAX: usize = 200;

// --- pool sources -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoolSourceKind {
    Tag,
    Loved,
    Playlist,
    Label,
    Artist,
    Tracks,
}

/// One source of a set's pool (stored as JSON in `dj_sets.pool_sources`). Dead refs
/// resolve to nothing, never an error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PoolSource {
    pub kind: PoolSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist_id: Option<i64>,
    /// At most 1000.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub track_ids: Vec<TrackId>,
    /// Display name frozen at pick time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

pub const MAX_EXPLICIT_TRACKS: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PoolSourceCountOut {
    pub index: usize,
    pub kind: String,
    pub label: String,
    pub track_count: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PoolSort {
    Bpm,
    #[default]
    Added,
    Random,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

/// `GET /sets/{id}/pool` query.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PoolQuery {
    pub q: Option<String>,
    pub sort: PoolSort,
    pub order: SortOrder,
    pub seed: i64,
    pub offset: i64,
    /// 1..500
    pub limit: i64,
}

impl Default for PoolQuery {
    fn default() -> Self {
        Self { q: None, sort: PoolSort::Added, order: SortOrder::Asc, seed: 0, offset: 0, limit: 100 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PoolPage<T = SuggestTrack> {
    #[serde(flatten)]
    pub page: Page<T>,
    pub total_duration_ms: i64,
    /// Deduped pool tracks hidden because they are already in the set.
    pub excluded_in_set: i64,
    pub sources: Vec<PoolSourceCountOut>,
    pub automix_max: usize,
}

// --- slots, transitions, summary ----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetItemOut {
    pub id: i64,
    pub index: usize,
    pub track_id: Option<TrackId>,
    pub title: String,
    pub artist: String,
    pub duration_ms: Option<i64>,
    pub played_ms: i64,
    pub start_ms: i64,
    pub bpm: Option<f64>,
    pub effective_bpm: Option<f64>,
    pub camelot: Option<String>,
    pub effective_camelot: Option<String>,
    pub energy: Option<i64>,
    pub cue_in_ms: Option<i64>,
    pub cue_out_ms: Option<i64>,
    pub tempo_adjust_pct: f64,
    pub key_lock: bool,
    pub transition_type: Option<String>,
    pub transition_beats: Option<i64>,
    pub transition_notes: Option<String>,
    pub art_url: Option<String>,
    pub missing: bool,
    /// Beat-grid anchor and gain-match hint for the arrange timeline.
    pub beat_offset_ms: Option<f64>,
    pub loudness_lufs: Option<f64>,
}

/// Verdicts for the join between two slots.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TransitionOut {
    pub from_index: usize,
    pub to_index: usize,
    pub key_verdict: Verdict,
    pub key_reason: String,
    pub key_score: f64,
    pub bpm_verdict: Verdict,
    pub bpm_reason: String,
    pub bpm_score: f64,
    pub tempo_delta_pct: f64,
    pub overlap_ms: i64,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SetSummaryOut {
    pub total_ms: i64,
    pub played_ms: i64,
    pub overlap_ms: i64,
    pub track_count: i64,
    pub avg_bpm: Option<f64>,
    pub bpm_min: Option<f64>,
    pub bpm_max: Option<f64>,
    pub target_ms: Option<i64>,
    pub over_target_ms: Option<i64>,
    pub problem_transitions: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DjSetOut {
    pub id: SetId,
    pub name: String,
    pub venue: Option<String>,
    pub event_date: Option<String>,
    pub target_minutes: Option<i64>,
    pub notes: Option<String>,
    pub status: String,
    pub summary: SetSummaryOut,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DjSetDetail {
    #[serde(flatten)]
    pub set: DjSetOut,
    pub items: Vec<SetItemOut>,
    pub transitions: Vec<TransitionOut>,
    pub pool_sources: Vec<PoolSource>,
}

/// A set as a card on the list page: no transition math.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DjSetListOut {
    pub id: SetId,
    pub name: String,
    pub venue: Option<String>,
    pub event_date: Option<String>,
    pub target_minutes: Option<i64>,
    pub status: String,
    pub track_count: i64,
    /// Cue spans summed as stored (no tempo/overlap correction).
    pub est_duration_ms: i64,
    pub avg_bpm: Option<f64>,
    pub art_urls: Vec<String>,
    pub pool_source_count: i64,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DjSetCreate {
    pub name: String,
    pub venue: Option<String>,
    pub event_date: Option<String>,
    pub target_minutes: Option<i64>,
    pub from_playlist_id: Option<i64>,
    #[serde(default)]
    pub pool_sources: Vec<PoolSource>,
}

/// `pool_sources`: `None` = untouched, `Some(vec![])` = clear.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DjSetUpdate {
    pub name: Option<String>,
    pub venue: Option<String>,
    pub event_date: Option<String>,
    pub target_minutes: Option<i64>,
    pub notes: Option<String>,
    pub status: Option<String>,
    pub pool_sources: Option<Vec<PoolSource>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SetItemUpdate {
    pub cue_in_ms: Option<i64>,
    pub cue_out_ms: Option<i64>,
    pub tempo_adjust_pct: Option<f64>,
    pub key_lock: Option<bool>,
    pub transition_type: Option<String>,
    pub transition_beats: Option<i64>,
    pub transition_notes: Option<String>,
    pub energy: Option<i64>,
}

/// Body of `POST .../tracks` and `.../items`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AddTracks {
    pub track_ids: Vec<TrackId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MoveItem {
    /// Index in the list WITHOUT the dragged item (drag-library convention).
    pub to_index: i64,
}

// --- suggestions & automix for a set ------------------------------------------------------------------

/// `POST /sets/{id}/suggest`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SetSuggestRequest {
    /// Suggest what mixes out of this slot; the tail of the set by default.
    pub after_item_id: Option<i64>,
    #[serde(flatten)]
    pub direction: crate::suggest::Direction,
    #[serde(flatten)]
    pub wishes: crate::suggest::Wishes,
    /// False ranks the whole library even when the set has pool sources.
    pub use_pool: bool,
    /// 1..100
    pub limit: i64,
}

impl Default for SetSuggestRequest {
    fn default() -> Self {
        Self {
            after_item_id: None,
            direction: Default::default(),
            wishes: Default::default(),
            use_pool: true,
            limit: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetSuggestResponse<T = SuggestTrack> {
    #[serde(flatten)]
    pub suggest: SuggestResponse<T>,
    pub after_index: Option<usize>,
    pub after_item_id: Option<i64>,
    pub pool_restricted: bool,
}

/// `POST /sets/{id}/automix`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AutomixRequest {
    /// Explicit picks to arrange; empty = the whole pool.
    pub track_ids: Vec<TrackId>,
    /// True keeps current items as the fixed head; false clears and re-arranges.
    pub keep_existing: bool,
    pub start_track_id: Option<TrackId>,
    /// Defaults to the set's own target.
    pub target_minutes: Option<i64>,
    /// Write cue in/out from each track's waveform intro/outro.
    pub write_cues: bool,
}

impl Default for AutomixRequest {
    fn default() -> Self {
        Self { track_ids: vec![], keep_existing: true, start_track_id: None, target_minutes: None, write_cues: true }
    }
}

/// Convenience for the UI: key/tempo verdict of a transition between two arbitrary slots.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JoinVerdict {
    pub key: Compatibility,
    pub bpm: Compatibility,
    pub ok: bool,
}
