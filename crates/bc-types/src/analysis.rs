//! Analysis DTOs (workstream 3): status, queue, per-track results, beat grids,
//! cue points, mix points, waveform metadata and the `analysis.*` WS payloads.
//!
//! Everything here is wasm-safe (serde only). The pure logic that operates on
//! [`BeatGrid`] / [`MixPoints`] lives in `bc-music`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::TrackId;

// --- WS topics ---------------------------------------------------------------

/// A batch was claimed by the pool: `AnalysisBatchEvent`.
pub const TOPIC_ANALYSIS_BATCH: &str = "analysis.batch";
/// One track finished: `AnalysisItemEvent`.
pub const TOPIC_ANALYSIS_ITEM: &str = "analysis.item";
/// Authoritative job counters: `AnalysisProgressEvent`.
pub const TOPIC_ANALYSIS_PROGRESS: &str = "analysis.progress";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnalysisBatchEvent {
    pub tracks: Vec<TrackId>,
    /// `running` when claimed.
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnalysisItemEvent {
    pub track_id: TrackId,
    /// `ok` | `partial` | `failed`
    pub status: String,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    /// Human key name (`Am`).
    pub key: Option<String>,
    /// 0..1 legacy scale (`energy`), see [`AnalysisOut::energy_v2`] for 1..10.
    pub energy: Option<f64>,
    pub error: Option<String>,
    pub done: u32,
    pub batch: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnalysisProgressEvent {
    pub job_id: String,
    pub status: String,
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
}

// --- status / queue -----------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AnalysisStatus {
    pub total_tracks: i64,
    pub analysed: i64,
    pub missing: i64,
    pub failed: i64,
    /// Rows whose `analyzer_version` is below the current one.
    pub stale: i64,
    pub coverage: f64,
    pub analyzer_version: i64,
    /// Which analyzers exist on this machine (`bc-rs-1` always; `essentia-sidecar` when the
    /// optional Python sidecar is configured).
    pub backends: BTreeMap<String, bool>,
    pub active_backend: Option<String>,
    /// Rows per `analyzer` value (`essentia-import`, `bc-rs-1`, ...).
    #[serde(default)]
    pub by_analyzer: BTreeMap<String, i64>,
    pub running_jobs: i64,
    /// Items still waiting in an unfinished analyse job.
    pub queued_tracks: i64,
    /// Items a pool worker has claimed right now.
    pub running_tracks: i64,
    /// Items in every unfinished analyse job, settled or not.
    pub batch_total: i64,
    /// Of `batch_total`, how many have stopped moving.
    pub batch_done: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScanScope {
    #[default]
    Missing,
    Stale,
    Failed,
    All,
    Ids,
    /// Tracks that have an analysis row but no `.bcw2` waveform / beat grid yet
    /// (what the essentia import leaves behind).
    Upgrade,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ScanRequest {
    #[serde(default)]
    pub scope: ScanScope,
    #[serde(default)]
    pub track_ids: Vec<TrackId>,
    pub limit: Option<i64>,
    /// With `scope = ids`, drop tracks that already have an analysis.
    #[serde(default)]
    pub only_missing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScanResponse {
    pub queued: i64,
    pub requested: i64,
    pub job_id: Option<String>,
    pub detail: String,
}

/// A track's analysis, as stored (`GET /analysis/tracks/{id}`) or just computed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AnalysisOut {
    pub track_id: TrackId,
    pub status: String,
    /// Legacy name of `analyzer`.
    pub backend: Option<String>,
    /// `essentia-import` | `bc-rs-1` | `essentia-sidecar`.
    pub analyzer: Option<String>,
    pub bpm: Option<f64>,
    pub bpm_confidence: Option<f64>,
    #[serde(default)]
    pub bpm_candidates: Vec<f64>,
    /// First-beat position. For `essentia-import` rows this is relative to the legacy
    /// 120 s excerpt (see `bc_music::beatgrid::legacy_origin_ms`); for `bc-rs-*` rows it is
    /// absolute into the track.
    pub beat_offset_ms: Option<f64>,
    pub key: Option<String>,
    pub camelot: Option<String>,
    pub key_confidence: Option<f64>,
    pub loudness_lufs: Option<f64>,
    /// dBTP (true peak, 4x oversampled) for new rows.
    pub true_peak_dbtp: Option<f64>,
    pub lra: Option<f64>,
    pub replaygain_gain: Option<f64>,
    /// Legacy 0..1.
    pub energy: Option<f64>,
    /// Calibrated 1..10.
    pub energy_v2: Option<f64>,
    pub grid_kind: Option<GridKind>,
    pub downbeat_offset_ms: Option<f64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnalysisJobOut {
    pub id: String,
    pub status: String,
    pub label: Option<String>,
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
    pub skipped: i64,
    pub progress: f64,
    pub error: Option<String>,
    pub created_at: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// One track's place in the queue, with enough of the track to name it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnalysisItemOut {
    pub id: i64,
    pub job_id: String,
    pub seq: i64,
    pub status: String,
    pub track_id: Option<TrackId>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub message: Option<String>,
    pub last_error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AnalysisQueue {
    pub jobs: Vec<AnalysisJobOut>,
    pub items: Vec<AnalysisItemOut>,
    /// Claimed by a worker right now.
    #[serde(default)]
    pub active_track_ids: Vec<TrackId>,
    #[serde(default)]
    pub queued_track_ids: Vec<TrackId>,
    #[serde(default)]
    pub failed_track_ids: Vec<TrackId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompatibilityOut {
    pub key_score: f64,
    pub key_verdict: String,
    pub key_reason: String,
    pub bpm_score: f64,
    pub bpm_verdict: String,
    pub bpm_reason: String,
}

// --- music primitives shared by server, UI and engine ----------------------------

/// Verdict ladder shared by key and tempo compatibility.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Perfect,
    Good,
    Energy,
    Risky,
    Clash,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Perfect => "perfect",
            Verdict::Good => "good",
            Verdict::Energy => "energy",
            Verdict::Risky => "risky",
            Verdict::Clash => "clash",
        }
    }
    /// `perfect`, `good` and `energy` count as a usable mix.
    pub fn ok(self) -> bool {
        matches!(self, Verdict::Perfect | Verdict::Good | Verdict::Energy)
    }
}

/// Score + verdict + prose for one key or tempo transition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Compatibility {
    pub score: f64,
    pub verdict: Verdict,
    pub reason: String,
}

impl Compatibility {
    pub fn ok(&self) -> bool {
        self.verdict.ok()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum GridKind {
    #[default]
    Constant,
    Variable,
}

/// One constant-tempo stretch of a variable grid.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct TempoSegment {
    /// Time of the first beat of the segment (ms into the track).
    pub origin_ms: f64,
    pub bpm: f64,
    /// Number of beats in the segment (`None` = until the next segment / end).
    pub beats: Option<u32>,
}

/// Beat grid of one track. A constant grid is one segment; a variable grid is a list of
/// segments. All times are absolute milliseconds into the track.
///
/// Stored compressed in `beat_grids` (the tempo-segment list, not individual beats), so
/// even a 2 h mix is a few hundred bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BeatGrid {
    pub kind: GridKind,
    pub segments: Vec<TempoSegment>,
    /// Which beat (0..3, counted from `segments[0].origin_ms`) is the first downbeat
    /// ("the one"). `None` = unknown.
    pub downbeat_phase: Option<u8>,
    /// Beats per bar (4 unless detected otherwise).
    #[serde(default = "default_beats_per_bar")]
    pub beats_per_bar: u8,
    /// Phrase starts (ms), snapped to 8/16/32-bar boundaries. Empty = unknown.
    #[serde(default)]
    pub phrase_starts_ms: Vec<f64>,
    /// 0..1.
    pub confidence: f64,
    /// Where this grid came from: `bc-rs-1` | `essentia-import` (extrapolated, low trust).
    pub source: String,
}

fn default_beats_per_bar() -> u8 {
    4
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CueKind {
    Hot,
    Memory,
    MixIn,
    MixOut,
    Drop,
    Intro,
    Outro,
    Loop,
}

/// Hot/memory cues (user) and auto cues (mix in/out, drop). Table `cue_points`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CuePoint {
    pub id: Option<i64>,
    pub track_id: TrackId,
    pub kind: CueKind,
    pub pos_ms: f64,
    /// Loop end (`kind = loop`).
    pub end_ms: Option<f64>,
    pub label: Option<String>,
    /// `#rrggbb`.
    pub color: Option<String>,
    /// Hot-cue slot 0..7.
    pub slot: Option<u8>,
    /// Written by the analyzer (never overwritten by the user's edits).
    pub auto: bool,
}

/// Where to start a track (`cue_in_ms`) and where to start blending out
/// (`cue_out_ms`, `None` = play to the end).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct MixPoints {
    pub cue_in_ms: i64,
    pub cue_out_ms: Option<i64>,
}

/// `GET /tracks/{id}/beatgrid`: grid + cues + mix points in one fetch (deck load).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackMusicInfo {
    pub track_id: TrackId,
    pub duration_ms: Option<i64>,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub grid: Option<BeatGrid>,
    pub cues: Vec<CuePoint>,
    pub mix_points: Option<MixPoints>,
}

/// Header facts of a stored `.bcw2` file (table `waveform_meta`), also returned in
/// `X-BCW-*` response headers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WaveformMeta {
    pub track_id: TrackId,
    pub format_version: u16,
    /// Hash of the source file the waveform was made from (also the `ETag`).
    pub source_hash: String,
    pub sample_rate: u32,
    pub duration_ms: i64,
    pub overview_points: u32,
    pub detail_points: u32,
    pub detail_rate_hz: f32,
    pub bytes: i64,
}

// --- accuracy report / waveform cache ----------------------------------------------------

/// Settings key holding the last gate report (JSON `AccuracyReport`).
pub const SETTING_ACCURACY_REPORT: &str = "analysis.accuracy_report";

/// The PLAN 7.2 benchmark against the imported essentia values, as stored by the bench run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccuracyReport {
    /// RFC 3339 UTC.
    pub generated_at: String,
    pub n: u64,
    pub failed: u64,
    pub bpm_within_0_5_pct_octave: f64,
    pub bpm_within_0_5_pct_strict: f64,
    pub bpm_within_3_pct_octave: f64,
    pub key_exact: f64,
    pub key_exact_or_compatible: f64,
    pub tracks_per_s: f64,
    pub realtime_x: f64,
    pub threads: u64,
    /// Thresholds of the gate: 0.95 / 0.80 / 0.92.
    pub passed: bool,
}

/// `GET /analysis/accuracy`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccuracyOut {
    /// `None` until a gate/bench run has stored one.
    pub report: Option<AccuracyReport>,
    /// Whether native BPM/key currently overwrite imported essentia values.
    pub native_bpm_key: bool,
}

/// `GET /analysis/waveform-cache`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WaveformCacheOut {
    pub bytes: u64,
    pub files: u64,
    pub cap_bytes: u64,
    /// Files still holding the detail level.
    pub detail_files: u64,
    /// Files whose detail was evicted (overview only).
    pub overview_only_files: u64,
}
