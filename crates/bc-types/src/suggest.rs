//! Recommender DTOs (workstream 3): suggest-next (nextup), loved/taste, similar.
//! Weights and rules are documented in `docs/api/ws3.md`.

use serde::{Deserialize, Serialize};

use crate::{ArtistId, LabelId, PlaylistId, ReleaseId, TrackId};

// --- the track shape recommenders return --------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NameRef {
    pub id: i64,
    pub name: String,
}

/// Light track shape returned by the recommenders.
///
/// TODO(ws1): once `bc_types::library::TrackOut` is published the response types below
/// switch their type parameter default to it (`Suggestion<T = TrackOut>`); the field names
/// here mirror the legacy `TrackOut` so the swap is mechanical.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TrackBrief {
    pub id: TrackId,
    pub title: String,
    pub artist: Option<NameRef>,
    pub release: Option<NameRef>,
    pub label: Option<NameRef>,
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub loved: bool,
    pub play_count: i64,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub key: Option<String>,
    pub energy: Option<f64>,
    /// `/api/tracks/{id}/stream`
    pub stream_url: String,
    pub art_url: Option<String>,
    pub is_snippet: bool,
}

/// The type the routers return: WS1's full `TrackOut` (hydrated by `bc_libcore::hydrate::tracks_out`).
/// `TrackBrief` above is kept only for in-crate lightweight uses.
pub type SuggestTrack = crate::library::TrackOut;

// --- seeds ---------------------------------------------------------------------------

/// What the client knows about a seed the server has no row for (a Bandcamp stream), or
/// wants to override.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct SeedOverride {
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub energy: Option<f64>,
    pub tags: Vec<String>,
}

// --- suggest-next ------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Tempo {
    Lower,
    #[default]
    Keep,
    Raise,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EnergyDir {
    Down,
    #[default]
    Keep,
    Up,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TagMode {
    Stick,
    #[default]
    Drift,
    Switch,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Harmonic {
    #[default]
    Strict,
    Loose,
    Off,
}

/// The direction part of a live-planner request: where the DJ wants to go.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Direction {
    pub tempo: Tempo,
    pub energy: EnergyDir,
    pub tag_mode: TagMode,
    /// Tags for `tag_mode = switch` (door choice) / drift targets.
    pub tags: Vec<String>,
    /// Hard rules: with `allow_tags` only tracks carrying at least one are candidates.
    pub allow_tags: Vec<String>,
    /// A track carrying any `deny_tags` never is. Never loosened when the crate runs dry.
    pub deny_tags: Vec<String>,
    pub harmonic: Harmonic,
    /// 0.01..0.25
    pub bpm_tolerance: f64,
}

impl Default for Direction {
    fn default() -> Self {
        Self {
            tempo: Tempo::Keep,
            energy: EnergyDir::Keep,
            tag_mode: TagMode::Drift,
            tags: vec![],
            allow_tags: vec![],
            deny_tags: vec![],
            harmonic: Harmonic::Strict,
            bpm_tolerance: 0.06,
        }
    }
}

/// Additive boosts for what the DJ wants to play.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Wishes {
    pub wish_track_ids: Vec<TrackId>,
    pub wish_artist_ids: Vec<ArtistId>,
    pub wish_label_ids: Vec<LabelId>,
    pub wish_tags: Vec<String>,
}

/// `POST /suggest/next`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SuggestRequest {
    pub seed_track_id: Option<TrackId>,
    pub seed: Option<SeedOverride>,
    #[serde(flatten)]
    pub direction: Direction,
    #[serde(flatten)]
    pub wishes: Wishes,
    pub exclude_track_ids: Vec<TrackId>,
    /// 1..100
    pub limit: i64,
    /// Pool: a playlist, the loved tracks, or (neither) the whole library.
    pub playlist_id: Option<PlaylistId>,
    pub loved: Option<bool>,
}

impl Default for SuggestRequest {
    fn default() -> Self {
        Self {
            seed_track_id: None,
            seed: None,
            direction: Direction::default(),
            wishes: Wishes::default(),
            exclude_track_ids: vec![],
            limit: 20,
            playlist_id: None,
            loved: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuggestionOut<T = SuggestTrack> {
    pub track: T,
    pub score: f64,
    pub key_verdict: String,
    pub key_reason: String,
    pub bpm_verdict: String,
    pub bpm_reason: String,
    pub why: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SuggestResponse<T = SuggestTrack> {
    pub seed_track_id: Option<TrackId>,
    pub seed_bpm: Option<f64>,
    pub seed_camelot: Option<String>,
    pub seed_energy: Option<f64>,
    pub target_bpm: Option<f64>,
    pub items: Vec<SuggestionOut<T>>,
}

// --- loved / taste -----------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TagWeightOut {
    pub name: String,
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LovedProfileOut {
    pub loved_count: i64,
    pub tags: Vec<TagWeightOut>,
    pub bpm: Option<f64>,
    pub energy: Option<f64>,
    pub artists: i64,
    pub labels: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LovedSuggestionOut<T = SuggestTrack> {
    pub track: T,
    pub score: f64,
    pub why: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LovedSuggestResponse<T = SuggestTrack> {
    pub profile: LovedProfileOut,
    pub items: Vec<LovedSuggestionOut<T>>,
}

/// `GET /suggest/loved?limit=24&seed=0&tags=a,b`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LovedQuery {
    pub limit: i64,
    pub seed: i64,
    /// Comma-separated: only tracks carrying one of these.
    pub tags: String,
}

impl Default for LovedQuery {
    fn default() -> Self {
        Self { limit: 24, seed: 0, tags: String::new() }
    }
}

// --- similar --------------------------------------------------------------------------------

/// Which signals may count: the panel's toggle chips. `tempo` covers energy too.
/// The enabled weights are renormalised, so one chip alone becomes the whole score.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Signals {
    pub tags: bool,
    pub artist: bool,
    pub label: bool,
    pub tempo: bool,
    pub key: bool,
    pub loved: bool,
}

impl Default for Signals {
    fn default() -> Self {
        Self { tags: true, artist: true, label: true, tempo: true, key: true, loved: true }
    }
}

/// `POST /suggest/similar`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SimilarRequest {
    pub seed_track_id: Option<TrackId>,
    pub seed: Option<SeedOverride>,
    pub signals: Signals,
    pub exclude_track_ids: Vec<TrackId>,
    /// 1..100
    pub limit: i64,
    /// Paging over the scored list (0..500), so "load more" never repeats a row.
    pub offset: i64,
    /// Reroll: a different slice of the same taste. Same seed, same page.
    pub shuffle_seed: i64,
}

impl Default for SimilarRequest {
    fn default() -> Self {
        Self {
            seed_track_id: None,
            seed: None,
            signals: Signals::default(),
            exclude_track_ids: vec![],
            limit: 30,
            offset: 0,
            shuffle_seed: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SimilarItem<T = SuggestTrack> {
    pub track: T,
    pub score: f64,
    pub why: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SimilarResponse<T = SuggestTrack> {
    pub seed_track_id: Option<TrackId>,
    pub seed_bpm: Option<f64>,
    pub seed_camelot: Option<String>,
    pub seed_tags: Vec<String>,
    pub seed_artist: Option<String>,
    pub seed_label: Option<String>,
    /// The rare tags the pool was actually drawn from.
    pub pool_tags: Vec<String>,
    /// How many candidates were considered.
    pub pool_size: i64,
    pub items: Vec<SimilarItem<T>>,
}

/// `POST /playlists/{id}/similar` body: playlist -> a new playlist of similar tracks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct SimilarPlaylistRequest {
    pub name: Option<String>,
    /// Unset = as many as the source has.
    pub limit: Option<i64>,
    /// Reroll; the client sends a fresh one per press.
    pub shuffle_seed: i64,
}

#[allow(dead_code)]
fn _type_uses(_: ReleaseId) {}

/// Response of `POST /playlists/{id}/similar`. Wire-identical to the playlist list row
/// (`PlaylistOut` of workstream 1): the new playlist with its counts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SimilarPlaylistOut {
    pub id: PlaylistId,
    pub name: String,
    pub description: Option<String>,
    pub kind: String,
    pub track_count: i64,
    pub duration_ms: i64,
    pub art_url: Option<String>,
    pub created_at: Option<String>,
}
