//! Workstream 1 (Data & Library): the DTOs of the library API. Request and
//! response shapes keep the legacy field names (`web/backend/.../schemas/library.py`)
//! so the UI port is mechanical. Everything is serde-only and compiles for wasm32.
//!
//! Conventions
//! * Timestamps are ISO-8601 strings in UTC (`2026-07-28T07:29:28.210257Z`).
//! * No filesystem path is ever returned to a client, except `RootOut.path` (a
//!   library root is a user-chosen location) and the move target.
//! * Query structs are `Deserialize` (the server reads them from the query string
//!   with repeated keys, e.g. `tags=a&tags=b`) and offer `to_query()` so the UI
//!   builds the same string without hand-rolling it.
//! * Topics for the WebSocket live in the `TOPIC_*` constants at the bottom.

use serde::{Deserialize, Serialize};

pub use crate::common::{Page, ScopeMode};
use crate::{ArtistId, LabelId, ReleaseId, TagId, TrackId};

pub mod maint;
pub mod metadata;
pub mod playlists;
pub mod scan;

pub use maint::*;
pub use metadata::*;
pub use playlists::*;
pub use scan::*;

// ======================================================================================
// Shared small types
// ======================================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtistRef {
    pub id: ArtistId,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ReleaseRef {
    pub id: ReleaseId,
    pub title: String,
    pub year: Option<i64>,
    /// Thumb URL with the immutable `?v=` art version, e.g. `/api/art/release/12?size=thumb&v=3a9f`.
    pub art_url: Option<String>,
    /// BlurHash of the cover for a no-layout-shift placeholder.
    #[serde(default)]
    pub art_blurhash: Option<String>,
    /// Dominant colour as `#rrggbb` (themed headers, album-art accent).
    #[serde(default)]
    pub art_color: Option<String>,
    pub label_id: Option<LabelId>,
    /// Set when the record sits on another person's shelf (a fan's wishlist).
    pub source_fan_id: Option<i64>,
    /// The record is nothing but Bandcamp teaser clips.
    #[serde(default)]
    pub snippet_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FileRef {
    pub id: i64,
    pub ext: String,
    pub codec: Option<String>,
    pub bitrate: Option<i64>,
    #[serde(default)]
    pub size_bytes: i64,
    #[serde(default)]
    pub missing: bool,
}

// ======================================================================================
// Tracks
// ======================================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TrackOut {
    pub id: TrackId,
    pub title: String,
    pub artist: Option<ArtistRef>,
    pub release: Option<ReleaseRef>,
    pub track_no: Option<i64>,
    pub disc_no: Option<i64>,
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub rating: Option<i64>,
    #[serde(default)]
    pub loved: bool,
    #[serde(default)]
    pub play_count: i64,
    pub last_played_at: Option<String>,
    pub added_at: Option<String>,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    /// Musical key name, e.g. `Am`, `C#`.
    pub key: Option<String>,
    pub energy: Option<f64>,
    pub file: Option<FileRef>,
    /// `/api/stream/{id}`
    pub stream_url: String,
    pub art_url: Option<String>,
    /// A Bandcamp teaser clip rather than the track.
    #[serde(default)]
    pub is_snippet: bool,
    /// Row id in the list this track came out of (playlist item); null elsewhere.
    #[serde(default)]
    pub item_id: Option<i64>,
}

/// A page of tracks plus how long the whole filter plays for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackPage {
    #[serde(flatten)]
    pub page: Page<TrackOut>,
    pub total_duration_ms: i64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrackSort {
    #[default]
    Added,
    Title,
    Artist,
    Album,
    Duration,
    Bpm,
    PlayCount,
    LastPlayed,
    Year,
    /// Seeded-hash shuffle: stable and pageable for a given `seed`.
    Random,
    /// Search relevance (bm25). Only meaningful with `q`; the default when `q` is set and `sort` is absent.
    Relevance,
    Key,
    Energy,
    Rating,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SortDir {
    Asc,
    #[default]
    Desc,
}

/// Every legacy `GET /tracks` parameter, plus `seed`. Paging is offset based and
/// unlimited (`limit` up to 5000; the old 500 cap is gone).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct TrackQuery {
    pub q: Option<String>,
    pub artist_id: Option<ArtistId>,
    pub label_id: Option<LabelId>,
    pub release_id: Option<ReleaseId>,
    pub release_ids: Vec<ReleaseId>,
    /// ANDed, matched on the indexed `name_key`.
    pub tags: Vec<String>,
    pub year_min: Option<i64>,
    pub year_max: Option<i64>,
    pub loved: Option<bool>,
    /// ISO-8601 (`2026-07-01` or a full timestamp); half-open window `after <= added_at < before`.
    pub added_after: Option<String>,
    pub added_before: Option<String>,
    pub played: Option<bool>,
    pub last_played_before: Option<String>,
    pub favorites: Option<bool>,
    /// `Some(false)` (default) hides tracks whose files are all missing; `None` shows both.
    pub missing: Option<bool>,
    pub bpm_min: Option<f64>,
    pub bpm_max: Option<f64>,
    pub camelot: Option<String>,
    pub sort: Option<TrackSort>,
    pub order: Option<SortDir>,
    /// Shuffle seed (1..=1_000_000) for `sort=random`.
    pub seed: Option<i64>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
    /// `mine` | `all`; absent = the saved library-scope setting.
    pub scope: Option<ScopeMode>,
    pub source_fan_id: Option<i64>,
}

fn push_opt<T: ToString>(out: &mut Vec<(String, String)>, k: &str, v: &Option<T>) {
    if let Some(v) = v {
        out.push((k.to_string(), v.to_string()));
    }
}

impl TrackQuery {
    /// Key/value pairs in query-string form (repeated keys for lists). The caller percent-encodes.
    pub fn to_pairs(&self) -> Vec<(String, String)> {
        let mut o = Vec::new();
        push_opt(&mut o, "q", &self.q);
        push_opt(&mut o, "artist_id", &self.artist_id);
        push_opt(&mut o, "label_id", &self.label_id);
        push_opt(&mut o, "release_id", &self.release_id);
        for r in &self.release_ids {
            o.push(("release_ids".into(), r.to_string()));
        }
        for t in &self.tags {
            o.push(("tags".into(), t.clone()));
        }
        push_opt(&mut o, "year_min", &self.year_min);
        push_opt(&mut o, "year_max", &self.year_max);
        push_opt(&mut o, "loved", &self.loved);
        push_opt(&mut o, "added_after", &self.added_after);
        push_opt(&mut o, "added_before", &self.added_before);
        push_opt(&mut o, "played", &self.played);
        push_opt(&mut o, "last_played_before", &self.last_played_before);
        push_opt(&mut o, "favorites", &self.favorites);
        push_opt(&mut o, "missing", &self.missing);
        push_opt(&mut o, "bpm_min", &self.bpm_min);
        push_opt(&mut o, "bpm_max", &self.bpm_max);
        push_opt(&mut o, "camelot", &self.camelot);
        if let Some(s) = self.sort {
            o.push(("sort".into(), enum_name(&s)));
        }
        if let Some(s) = self.order {
            o.push(("order".into(), enum_name(&s)));
        }
        push_opt(&mut o, "seed", &self.seed);
        push_opt(&mut o, "offset", &self.offset);
        push_opt(&mut o, "limit", &self.limit);
        if let Some(s) = self.scope {
            o.push(("scope".into(), enum_name(&s)));
        }
        push_opt(&mut o, "source_fan_id", &self.source_fan_id);
        o
    }
}

fn enum_name<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    #[default]
    M3u8,
    Csv,
    Zip,
}

/// `GET /tracks/export?format=m3u8|csv|zip` takes the `TrackQuery` filters (no paging)
/// plus this `format` parameter (parsed separately from the same query string).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct ExportFormatQuery {
    pub format: ExportFormat,
}

/// `POST /tracks/love`: assign (not toggle) loved on many tracks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetLoved {
    pub track_ids: Vec<TrackId>,
    pub loved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ChangedOut {
    pub changed: i64,
}

/// `PUT /tracks/{id}/rating`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetRating {
    /// 0..=5, null clears.
    pub rating: Option<i64>,
}

// ======================================================================================
// Releases
// ======================================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ReleaseOut {
    pub id: ReleaseId,
    pub title: String,
    pub artist: Option<ArtistRef>,
    pub label: Option<String>,
    pub label_id: Option<LabelId>,
    pub year: Option<i64>,
    pub kind: String,
    #[serde(default)]
    pub track_count: i64,
    /// Bandcamp's count (or the files' own numbering) when known; more than
    /// `track_count` means the album is partly here and can be filled.
    pub expected_track_count: Option<i64>,
    #[serde(default)]
    pub duration_ms: i64,
    /// Full-size art URL with `?v=`.
    pub art_url: Option<String>,
    #[serde(default)]
    pub art_blurhash: Option<String>,
    #[serde(default)]
    pub art_color: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub added_at: Option<String>,
    pub bandcamp_url: Option<String>,
    pub source_fan_id: Option<i64>,
    #[serde(default)]
    pub snippet_only: bool,
    /// Still a pre-order: Bandcamp has the record on sale but only some tracks are out, and the
    /// release date is in the future (or unknown). Cached knowledge only; false when never checked.
    #[serde(default)]
    pub is_preorder: bool,
    /// When the full record comes out (`YYYY-MM-DD`), while `is_preorder`.
    #[serde(default)]
    pub release_date: Option<String>,
    /// Tracks the library lacks because Bandcamp has not released them yet.
    #[serde(default)]
    pub unreleased_count: i64,
    /// Tracks the library lacks that a fill can actually fetch (`missing - unreleased`).
    #[serde(default)]
    pub fillable_missing: i64,
    /// The unreleased tracks (number, title, duration), in record order.
    #[serde(default)]
    pub unreleased_tracks: Vec<TrackAvailability>,
}

/// One track of a record as Bandcamp lists it, with whether it can be fetched yet.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TrackAvailability {
    pub track_num: Option<i64>,
    pub title: String,
    pub duration_sec: Option<f64>,
    pub available: bool,
}

/// `GET /releases/{id}/availability`: what Bandcamp has out for a release right now.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ReleaseAvailability {
    #[serde(default)]
    pub release_id: ReleaseId,
    /// UTC `YYYY-MM-DD HH:MM:SS`; empty until stored.
    #[serde(default)]
    pub checked_at: String,
    pub is_preorder: bool,
    pub release_date: Option<String>,
    pub tracks: Vec<TrackAvailability>,
    /// True when this response came from a live Bandcamp read (the release data should be refetched).
    #[serde(default)]
    pub fetched: bool,
}

impl ReleaseAvailability {
    /// Tracks Bandcamp has not released yet (only a pre-order has any).
    pub fn unreleased(&self) -> impl Iterator<Item = &TrackAvailability> {
        self.tracks.iter().filter(move |t| self.is_preorder && !t.available)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseSort {
    #[default]
    Added,
    Title,
    Year,
    Artist,
    Random,
}

/// `GET /releases`, `/releases/ids`, `/releases/{id}/next` share these filters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct ReleaseQuery {
    pub q: Option<String>,
    pub artist_id: Option<ArtistId>,
    pub label_id: Option<LabelId>,
    pub tags: Vec<String>,
    pub loved: Option<bool>,
    /// Only releases short of tracks.
    pub missing: Option<bool>,
    pub added_after: Option<String>,
    pub added_before: Option<String>,
    pub sort: Option<ReleaseSort>,
    pub order: Option<SortDir>,
    pub seed: Option<i64>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
    pub scope: Option<ScopeMode>,
    pub source_fan_id: Option<i64>,
}

impl ReleaseQuery {
    pub fn to_pairs(&self) -> Vec<(String, String)> {
        let mut o = Vec::new();
        push_opt(&mut o, "q", &self.q);
        push_opt(&mut o, "artist_id", &self.artist_id);
        push_opt(&mut o, "label_id", &self.label_id);
        for t in &self.tags {
            o.push(("tags".into(), t.clone()));
        }
        push_opt(&mut o, "loved", &self.loved);
        push_opt(&mut o, "missing", &self.missing);
        push_opt(&mut o, "added_after", &self.added_after);
        push_opt(&mut o, "added_before", &self.added_before);
        if let Some(s) = self.sort {
            o.push(("sort".into(), enum_name(&s)));
        }
        if let Some(s) = self.order {
            o.push(("order".into(), enum_name(&s)));
        }
        push_opt(&mut o, "seed", &self.seed);
        push_opt(&mut o, "offset", &self.offset);
        push_opt(&mut o, "limit", &self.limit);
        if let Some(s) = self.scope {
            o.push(("scope".into(), enum_name(&s)));
        }
        push_opt(&mut o, "source_fan_id", &self.source_fan_id);
        o
    }
}

/// A release reduced to what a selection needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReleaseStub {
    pub id: ReleaseId,
    pub track_count: i64,
}

/// One "more like this" shelf on a release or artist page.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelatedGroup {
    /// `artist` | `label` | `tag`
    pub kind: String,
    pub key: String,
    pub title: String,
    pub id: Option<i64>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub items: Vec<ReleaseOut>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AdoptRequest {
    #[serde(default)]
    pub ids: Vec<ReleaseId>,
    /// Or every release on this fan's shelf.
    pub fan_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdoptResult {
    pub adopted: i64,
}

// ======================================================================================
// Artists
// ======================================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ArtistOut {
    pub id: ArtistId,
    pub name: String,
    #[serde(default)]
    pub release_count: i64,
    #[serde(default)]
    pub track_count: i64,
    /// Summed plays; only populated for `sort=plays`.
    #[serde(default)]
    pub play_count: i64,
    pub art_url: Option<String>,
    #[serde(default)]
    pub art_blurhash: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtistSort {
    #[default]
    Name,
    Plays,
    Releases,
    Tracks,
    Added,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct ArtistQuery {
    pub q: Option<String>,
    pub sort: Option<ArtistSort>,
    /// Defaults per sort: A-Z ascending, every count descending.
    pub order: Option<SortDir>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
    pub scope: Option<ScopeMode>,
    pub source_fan_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtistLabelOut {
    pub id: LabelId,
    pub name: String,
    pub release_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArtistTagOut {
    pub id: TagId,
    pub name: String,
    /// How many of this artist's tracks carry the tag.
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ArtistDetailOut {
    pub id: ArtistId,
    pub name: String,
    pub bandcamp_url: Option<String>,
    pub location: Option<String>,
    #[serde(default)]
    pub release_count: i64,
    #[serde(default)]
    pub track_count: i64,
    #[serde(default)]
    pub play_count: i64,
    pub year_min: Option<i64>,
    pub year_max: Option<i64>,
    pub art_url: Option<String>,
    #[serde(default)]
    pub labels: Vec<ArtistLabelOut>,
    #[serde(default)]
    pub tags: Vec<ArtistTagOut>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SimilarArtistOut {
    pub id: ArtistId,
    pub name: String,
    pub art_url: Option<String>,
    pub release_count: i64,
    #[serde(default)]
    pub shared_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ArtistRelatedOut {
    #[serde(default)]
    pub similar_artists: Vec<SimilarArtistOut>,
    #[serde(default)]
    pub shelves: Vec<RelatedGroup>,
}

/// `PATCH /artists/{id}`: `bandcamp_url` absent = untouched, `""` = clear. `name` renames the artist;
/// a name whose `name_key` belongs to another artist is a 409 (merge is not offered).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ArtistPatch {
    pub name: Option<String>,
    pub bandcamp_url: Option<String>,
}

// ======================================================================================
// Labels
// ======================================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LabelOut {
    pub id: LabelId,
    pub name: String,
    pub bandcamp_url: Option<String>,
    #[serde(default)]
    pub release_count: i64,
    #[serde(default)]
    pub track_count: i64,
    #[serde(default)]
    pub size_bytes: i64,
    /// Up to four covers (a label is a folder, not a record).
    #[serde(default)]
    pub art_urls: Vec<String>,
    pub last_added_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LabelSort {
    #[default]
    Releases,
    Tracks,
    Name,
    Added,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct LabelQuery {
    pub q: Option<String>,
    pub sort: Option<LabelSort>,
    pub order: Option<SortDir>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
    pub scope: Option<ScopeMode>,
    pub source_fan_id: Option<i64>,
    /// `/labels/random`: skip this label.
    pub exclude_id: Option<LabelId>,
}

/// `PATCH /labels/{id}`: renaming onto an existing name merges the two folders.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LabelPatch {
    pub name: Option<String>,
    /// absent = untouched, `""` = clear.
    pub bandcamp_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DeleteLabelResult {
    pub releases: i64,
    pub tracks: i64,
    pub files: i64,
}

// ======================================================================================
// Tags, facets, favorites
// ======================================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TagOut {
    pub id: TagId,
    pub name: String,
    pub track_count: i64,
}

/// `GET /tags`: with any track filter the cloud is narrowed to tracks matching it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct TagsQuery {
    pub q: Option<String>,
    pub tags: Vec<String>,
    pub loved: Option<bool>,
    pub added_after: Option<String>,
    pub added_before: Option<String>,
    pub min_count: Option<i64>,
    pub limit: Option<i64>,
    pub scope: Option<ScopeMode>,
    pub source_fan_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct YearFacet {
    pub year: i64,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Facets {
    pub tags: Vec<TagOut>,
    pub artists: Vec<ArtistOut>,
    pub years: Vec<YearFacet>,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct FavoritesOut {
    #[serde(default)]
    pub artists: Vec<ArtistOut>,
    #[serde(default)]
    pub labels: Vec<LabelOut>,
    #[serde(default)]
    pub tags: Vec<TagOut>,
}

// ======================================================================================
// Stats, roots, scope, snippets
// ======================================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiskUsage {
    pub total_bytes: i64,
    pub used_bytes: i64,
    pub free_bytes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RootOut {
    pub id: i64,
    pub path: String,
    /// `library` | `downloads`
    pub kind: String,
    pub enabled: bool,
    #[serde(default)]
    pub watch: bool,
    pub last_scan_at: Option<String>,
    pub last_scan_ms: Option<i64>,
    #[serde(default)]
    pub track_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AddRootRequest {
    /// The one place a client may send a filesystem path (besides the move target).
    pub path: String,
    #[serde(default = "d_kind_library")]
    pub kind: String,
}
fn d_kind_library() -> String {
    "library".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RootPatch {
    pub enabled: Option<bool>,
    pub watch: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LibraryStats {
    pub tracks: i64,
    pub releases: i64,
    pub artists: i64,
    pub tags: i64,
    pub total_bytes: i64,
    pub total_duration_ms: i64,
    pub missing_files: i64,
    pub analyzed: i64,
    #[serde(default)]
    pub plays: i64,
    #[serde(default)]
    pub listened_ms: i64,
    pub roots: Vec<RootOut>,
    pub disk: Option<DiskUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LibraryScopeOut {
    /// Whether listings show what was downloaded from other people's wishlists.
    pub unified: bool,
    pub foreign_releases: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LibraryScopeIn {
    pub unified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SnippetSettingOut {
    pub hidden: bool,
    #[serde(default)]
    pub snippet_tracks: i64,
    #[serde(default)]
    pub snippet_releases: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnippetSettingIn {
    pub hidden: bool,
}

// ======================================================================================
// Scan, move (long work: 202 Accepted { job_id } + `library.*` events)
// ======================================================================================

/// `POST /library/scan?root_id=` returns `202 Accepted { job_id }`; the final report
/// arrives as `library.scan.done` and as `GET /library/scan/{job_id}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ScanResult {
    pub root_id: i64,
    pub root_path: String,
    pub files_seen: i64,
    pub files_added: i64,
    pub files_updated: i64,
    pub files_missing: i64,
    pub files_unchanged: i64,
    pub tracks_added: i64,
    #[serde(default)]
    pub errors: Vec<String>,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ScanStatus {
    pub job_id: String,
    /// `running` | `done` | `failed`
    pub state: String,
    pub root_id: Option<i64>,
    pub phase: String,
    pub seen: i64,
    pub total: Option<i64>,
    pub results: Vec<ScanResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MoveRequest {
    /// The one other path a client may send: where the root should move to.
    pub target_path: String,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MovePlanOut {
    pub source: String,
    pub target: String,
    pub file_count: i64,
    pub total_bytes: i64,
    pub free_bytes: i64,
    pub same_filesystem: bool,
    pub target_exists: bool,
    pub target_empty: bool,
    pub fits: bool,
    pub ok: bool,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MoveResult {
    pub moved: i64,
    pub total: i64,
    pub bytes_moved: i64,
    #[serde(default)]
    pub errors: Vec<String>,
    pub new_path: String,
}

// ======================================================================================
// History
// ======================================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlayEvent {
    pub track_id: TrackId,
    #[serde(default)]
    pub ms_played: i64,
    #[serde(default)]
    pub completed: bool,
    #[serde(default)]
    pub skipped: bool,
    /// Free text origin (`library`, `playlist`, `set`, `dj`, ...). Default `library`.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryTopItem {
    pub track: TrackOut,
    /// Plays within the window.
    pub plays: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryTop {
    pub days: i64,
    pub items: Vec<HistoryTopItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryReset {
    pub days: Option<i64>,
    pub events: i64,
    pub tracks: i64,
}

/// One row of `GET /history/recent` (recently played, newest first).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HistoryEntry {
    pub id: i64,
    pub started_at: String,
    pub ms_played: i64,
    pub completed: bool,
    pub skipped: bool,
    pub source: String,
    pub track: TrackOut,
}

// ======================================================================================
// Loved streams (Bandcamp tracks loved straight off the page)
// ======================================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LovedStreamIn {
    pub page_url: String,
    pub track_key: String,
    pub bc_track_id: Option<i64>,
    pub track_index: Option<i64>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist_name: String,
    #[serde(default)]
    pub release_title: String,
    pub art_url: Option<String>,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LovedStreamOut {
    #[serde(flatten)]
    pub stream: LovedStreamIn,
    pub id: i64,
    pub added_at: Option<String>,
    /// `/api/explore/stream?release=..&track=..` (WS2's proxy).
    #[serde(default)]
    pub stream_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LovedAutoOut {
    pub enabled: bool,
}

// ======================================================================================
// Home shelves (snapshot; one request instead of eight)
// ======================================================================================

/// `GET /library/home?seed=`: the Home page's shelves. `seed` re-rolls the random
/// shelves; the same seed returns the same snapshot until the library changes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HomeShelves {
    pub seed: i64,
    pub generated_at: String,
    pub stats: LibraryStats,
    /// Newest releases.
    pub new_in_library: Vec<ReleaseOut>,
    /// Random releases from the top tags.
    pub crate_dig: Vec<ReleaseOut>,
    pub top_tags: Vec<TagOut>,
    pub recently_played: Vec<HistoryEntry>,
    pub top_artists: Vec<ArtistOut>,
    /// Loved tracks not played for 90 days.
    pub rediscover: Vec<TrackOut>,
    /// Random releases ("dust off a record").
    pub dust_off: Vec<ReleaseOut>,
    /// Never-played tracks added more than 30 days ago (any never-played track as a fallback).
    pub buried_treasure: Vec<TrackOut>,
    /// Top 10 by all-time play count.
    pub top_ten: Vec<TrackOut>,
    pub favorites: FavoritesOut,
}

// ======================================================================================
// Import (Settings > Import)
// ======================================================================================

/// `POST /library/import`: `202 Accepted { job_id }`. Allowed only when the library is empty, or with `force`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct ImportRequest {
    /// The legacy `library.db` or data dir (a third place a client may send a path); default `BC_LEGACY_DB`.
    pub from: Option<String>,
    pub force: bool,
    pub skip_repairs: bool,
}

/// Payload of `library.import.progress`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ImportProgress {
    pub job_id: String,
    pub message: String,
    pub done: bool,
    pub ok: Option<bool>,
}

// ======================================================================================
// Events (WebSocket topics, payloads)
// ======================================================================================

pub const TOPIC_LIBRARY_CHANGED: &str = "library.changed";
pub const TOPIC_LIBRARY_SCAN_PROGRESS: &str = "library.scan.progress";
pub const TOPIC_LIBRARY_SCAN_DONE: &str = "library.scan.done";
pub const TOPIC_LIBRARY_MOVE_PROGRESS: &str = "library.move.progress";
pub const TOPIC_LIBRARY_STRAYS: &str = "library.strays";
pub const TOPIC_LOVED_RECONCILED: &str = "loved.reconciled";
pub const TOPIC_METADATA_PROGRESS: &str = "metadata.progress";
pub const TOPIC_ART_PROGRESS: &str = "library.art.progress";
pub const TOPIC_IMPORT_PROGRESS: &str = "library.import.progress";

/// Payload of `library.changed`. Every field optional; clients combine them with
/// the entity-level `invalidate` events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LibraryChanged {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added_tracks: Vec<TrackId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_tracks: Vec<TrackId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed_tracks: Vec<TrackId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippets: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_root: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ScanProgress {
    pub job_id: String,
    pub root_id: i64,
    /// `walk` | `read` | `write` | `done`
    pub phase: String,
    pub seen: i64,
    pub total: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MoveProgress {
    pub root_id: i64,
    pub moved: i64,
    pub total: i64,
    pub bytes_moved: i64,
    pub current: Option<String>,
    pub done: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_query_roundtrip_pairs() {
        let q = TrackQuery {
            q: Some("dub techno".into()),
            tags: vec!["Techno".into(), "Dub".into()],
            sort: Some(TrackSort::PlayCount),
            order: Some(SortDir::Asc),
            offset: Some(74_000),
            ..Default::default()
        };
        let p = q.to_pairs();
        assert!(p.contains(&("sort".into(), "play_count".into())));
        assert!(p.contains(&("order".into(), "asc".into())));
        assert_eq!(p.iter().filter(|(k, _)| k == "tags").count(), 2);
    }

    #[test]
    fn track_page_flattens() {
        let page = TrackPage {
            page: Page { items: vec![], total: 3, offset: 0, limit: 100 },
            total_duration_ms: 9,
        };
        let v = serde_json::to_value(&page).unwrap();
        assert_eq!(v["total"], 3);
        assert_eq!(v["total_duration_ms"], 9);
        assert!(v.get("items").is_some());
    }
}
