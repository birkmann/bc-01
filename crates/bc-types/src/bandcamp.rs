//! Workstream 2: Bandcamp harvest / explore / fans / follows / tracklists /
//! loved streams / downloads. DTOs, request bodies, WS event payloads and
//! `TOPIC_*` constants. Timestamps are ISO-8601 strings (UTC). wasm32-safe.
//!
//! Naming follows the legacy Python pydantic models (`web/backend/.../routes/*.py`)
//! so the UI port is mechanical.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// -- WS topics -----------------------------------------------------------------------
pub const TOPIC_HARVEST_COMPLETED: &str = "harvest.completed";
pub const TOPIC_HARVEST_PROGRESS: &str = "harvest.progress";
pub const TOPIC_HARVEST_ENRICH: &str = "harvest.enrich";
pub const TOPIC_HARVEST_LABELS: &str = "harvest.labels";
pub const TOPIC_HARVEST_RELINK: &str = "harvest.relink";
pub const TOPIC_LABELS_SWEEP: &str = "labels.sweep";
pub const TOPIC_FAVORITES_SWEEP: &str = "favorites.sweep";
pub const TOPIC_FEED_SWEEP: &str = "feed.sweep";
pub const TOPIC_FANS_WALK: &str = "fans.walk";
pub const TOPIC_LIBRARY_CHANGED: &str = "library.changed";

fn t() -> bool {
    true
}

// ======================================================================================
// Downloads
// ======================================================================================

/// `POST /downloads`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DownloadRequest {
    #[serde(default)]
    pub urls: Vec<String>,
    pub label: Option<String>,
    pub target_subdir: Option<String>,
    #[serde(default = "d_prio")]
    pub priority: i64,
    /// Client-supplied so a resubmit is idempotent.
    pub job_id: Option<String>,
    /// Re-download even when the URL is already in the library.
    #[serde(default)]
    pub force: bool,
    pub label_name: Option<String>,
    pub label_url: Option<String>,
    pub source_fan_id: Option<i64>,
    /// Flat layout: every file directly in `target_subdir`.
    #[serde(default)]
    pub single_folder: bool,
    /// Download a /track/ URL as that track instead of widening to its album.
    #[serde(default)]
    pub tracks_only: bool,
}
fn d_prio() -> i64 {
    100
}

/// `POST /downloads/parse` body.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ParseUrlsRequest {
    #[serde(default)]
    pub text: String,
}

/// `POST /downloads/parse` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ParsedUrls {
    pub valid: Vec<String>,
    pub invalid: Vec<String>,
    pub duplicates: i64,
    pub albums: i64,
    pub tracks: i64,
    #[serde(default)]
    pub artists: i64,
    #[serde(default)]
    pub already_have: i64,
}

// ======================================================================================
// Harvest / inbox
// ======================================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HarvestItemOut {
    pub id: i64,
    pub url: String,
    pub url_kind: String,
    pub state: String,
    pub title: String,
    pub artist_name: String,
    pub label_name: Option<String>,
    pub art_url: Option<String>,
    pub release_date: Option<String>,
    pub track_count: Option<i64>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source_kind: Option<String>,
    pub source_label: Option<String>,
    #[serde(default)]
    pub in_collection: bool,
    #[serde(default)]
    pub in_wishlist: bool,
    #[serde(default)]
    pub is_free_download: bool,
    #[serde(default = "t")]
    pub is_purchasable: bool,
    #[serde(default)]
    pub is_preorder: bool,
    #[serde(default)]
    pub in_library: bool,
    pub release_id: Option<i64>,
    /// Place in the list being listed (`fan_id`), newest first.
    pub position: Option<i64>,
    /// Which of that fan's lists the item is on: `wishlist`, `collection`.
    #[serde(default)]
    pub tabs: Vec<String>,
    pub discovered_at: Option<String>,
}

/// `GET /harvest/items` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HarvestItemsQuery {
    pub state: Option<String>,
    pub source_kind: Option<String>,
    pub source_label: Option<String>,
    pub in_wishlist: Option<bool>,
    pub fan_id: Option<i64>,
    /// `wishlist` | `collection` | `all`
    pub tab: Option<String>,
    /// `discovered` | `position`
    pub order: Option<String>,
    pub q: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub offset: i64,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TagCount {
    pub tag: String,
    pub count: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResolveRequest {
    pub input: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ResolveResult {
    pub kind: String,
    pub label: String,
    pub url: Option<String>,
    pub total_hint: Option<i64>,
    #[serde(default)]
    pub requires_auth: bool,
    #[serde(default = "t")]
    pub auth_ok: bool,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
}

/// `POST /harvest/run` body. The run is a job (`202 Accepted {job_id}`); the
/// [`RunResult`] arrives in `harvest.completed` and `GET /harvest/runs/{job_id}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunRequest {
    /// url_list | artist | label | collection | wishlist | hidden | discover
    pub kind: String,
    pub url: Option<String>,
    pub text: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub label_name: Option<String>,
    #[serde(default = "d_new")]
    pub slice: String,
    pub genre: Option<String>,
    #[serde(default)]
    pub geoname_id: i64,
    #[serde(default = "d_limit200")]
    pub limit: i64,
    /// `shallow` | `full`
    #[serde(default = "d_shallow")]
    pub depth: String,
}
fn d_new() -> String {
    "new".into()
}
fn d_limit200() -> i64 {
    200
}
fn d_shallow() -> String {
    "shallow".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RunResult {
    pub kind: String,
    pub label: String,
    pub seen: i64,
    pub new: i64,
    pub already_known: i64,
    pub in_library: i64,
    #[serde(default)]
    pub queued: i64,
    #[serde(default)]
    pub pending_item_ids: Vec<i64>,
    #[serde(default)]
    pub errors: Vec<String>,
    /// Extraction tier counts: `blob` | `json_ld` | `css` | `api` ...
    #[serde(default)]
    pub tier_counts: BTreeMap<String, i64>,
}

/// `POST /harvest/items/queue` body.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueueRequest {
    #[serde(default)]
    pub item_ids: Vec<i64>,
    #[serde(default)]
    pub all_matching: bool,
    #[serde(default = "d_new")]
    pub state: String,
    #[serde(default)]
    pub in_wishlist: bool,
    /// `None` groups under the source's label; `Some("")` writes into the downloads root.
    pub target_subdir: Option<String>,
    #[serde(default)]
    pub allow_unowned: bool,
    #[serde(default)]
    pub include_in_library: bool,
    #[serde(default)]
    pub single_folder: bool,
    pub fan_id: Option<i64>,
    pub tab: Option<String>,
    pub source_kind: Option<String>,
    pub source_label: Option<String>,
    pub source_fan_id: Option<i64>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueueResult {
    pub queued: i64,
    pub skipped_in_library: i64,
    /// Up to 50 unowned, non-free items awaiting `allow_unowned`.
    #[serde(default)]
    pub needs_confirmation: Vec<Value>,
    pub job_id: Option<String>,
    #[serde(default)]
    pub adopted: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EnrichRequest {
    #[serde(default)]
    pub item_ids: Vec<i64>,
}

/// `GET/POST/DELETE /harvest/enrich` and `harvest.enrich` event.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EnrichState {
    /// idle | running | done | failed
    pub phase: String,
    pub running: bool,
    pub done: i64,
    pub total: i64,
    pub tagged: i64,
    pub current: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// `GET/POST /harvest/labels[/resolve]` and `harvest.labels` event.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LabelResolveStatus {
    pub phase: String,
    pub running: bool,
    pub seen: i64,
    pub total: Option<i64>,
    pub resolved: i64,
    pub labelled: i64,
    pub filed: i64,
    /// Releases whose artist was the label itself, re-credited to the artist on their tracks.
    #[serde(default)]
    pub artists_fixed: i64,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// `GET/POST/DELETE /harvest/relink` and `harvest.relink` event: finding each release's
/// Bandcamp page again by search.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RelinkStatus {
    /// idle | running | done | failed
    pub phase: String,
    pub running: bool,
    /// Releases searched so far this run.
    pub seen: i64,
    /// Releases this run set out to search.
    pub total: i64,
    /// Releases that got their Bandcamp page back.
    pub linked: i64,
    /// Releases without a confident match (left as they were).
    pub unmatched: i64,
    pub current: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

/// Label sweep (`labels.sweep`) and favourites sweep (`favorites.sweep`) status.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SweepStatus {
    /// idle | harvesting | queueing | done | failed
    pub phase: String,
    /// `all` | `selection`
    #[serde(default = "d_all")]
    pub scope: String,
    pub running: bool,
    pub done: i64,
    pub total: Option<i64>,
    pub current: Option<String>,
    /// Sources skipped because no Bandcamp page is pinned to them.
    #[serde(default)]
    pub no_url: i64,
    #[serde(default)]
    pub seen: i64,
    #[serde(default)]
    pub new: i64,
    #[serde(default)]
    pub in_library: i64,
    #[serde(default)]
    pub queued: i64,
    pub job_id: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
}
fn d_all() -> String {
    "all".into()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LabelSweepRequest {
    /// Empty = the whole shelf.
    #[serde(default)]
    pub label_ids: Vec<i64>,
}

/// `harvest.completed`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HarvestCompleted {
    pub kind: String,
    pub label: String,
    pub seen: i64,
    pub new: i64,
}

/// `harvest.progress` (new: progress of a harvest-run job)
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HarvestProgress {
    pub job_id: String,
    pub kind: String,
    pub seen: i64,
    pub total: Option<i64>,
    pub new: i64,
}

// -- identity (cookie) -----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CookieRequest {
    pub cookie: String,
}

/// The cookie itself is never returned, only a fingerprint.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IdentityStatus {
    pub configured: bool,
    pub valid: Option<bool>,
    pub fingerprint: Option<String>,
    pub username: Option<String>,
    pub fan_id: Option<i64>,
    #[serde(default)]
    pub detail: String,
}

/// `GET /desktop`: what the desktop app around the server can do for this caller.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DesktopInfo {
    /// `POST /desktop/bandcamp-login` opens Bandcamp's sign-in page in a window on this machine.
    pub bandcamp_login: bool,
}

/// Asks the desktop app to open the sign-in window (server to desktop, payload `{}`).
pub const TOPIC_BANDCAMP_LOGIN_REQUEST: &str = "desktop.bandcamp_login";
/// Progress of the sign-in window (desktop to UI), payload [`BandcampLoginEvent`].
pub const TOPIC_BANDCAMP_LOGIN: &str = "bandcamp.login";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BandcampLoginState {
    /// The window is open and waiting for the user to sign in.
    Open,
    /// Signed in; the cookie is stored (the `identity` query refetches).
    SignedIn,
    /// The window was closed before signing in.
    Closed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BandcampLoginEvent {
    pub state: BandcampLoginState,
    #[serde(default)]
    pub detail: String,
}

/// `GET /harvest/health`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HarvestHealth {
    pub rate_limit: RateLimitHealth,
    pub cache: CacheHealth,
    pub requests: i64,
    pub cache_hits: i64,
    pub errors: i64,
    pub last_errors: Vec<String>,
    pub extract_tiers: BTreeMap<String, i64>,
    pub blob_ratio: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RateLimitHealth {
    pub current_rate: f64,
    pub penalised_for_s: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CacheHealth {
    pub entries: i64,
    #[serde(default)]
    pub bytes: i64,
}

// ======================================================================================
// Fans (followed wishlists)
// ======================================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FanTabOut {
    pub items: i64,
    /// Items by inbox state.
    #[serde(default)]
    pub counts: BTreeMap<String, i64>,
    /// What the fan page reports the list holds.
    pub reported: Option<i64>,
}

/// State of the fan walker (`fans.walk` event; also embedded in [`FanOut::walk`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WalkState {
    pub fan_id: Option<i64>,
    /// idle | queued | harvesting | queueing | done | failed
    pub phase: String,
    pub running: bool,
    #[serde(default)]
    pub tabs: Vec<String>,
    pub tab: Option<String>,
    pub seen: i64,
    pub total: Option<i64>,
    pub new: i64,
    pub in_library: i64,
    pub queued: i64,
    pub job_id: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FanOut {
    pub id: i64,
    pub username: String,
    pub display_name: Option<String>,
    pub url: String,
    pub wishlist_url: String,
    pub bc_fan_id: Option<i64>,
    #[serde(default)]
    pub is_self: bool,
    pub wishlist_count: Option<i64>,
    pub collection_count: Option<i64>,
    /// Folder under the downloads root for this fan's downloads.
    pub shelf: String,
    #[serde(default)]
    pub items: i64,
    #[serde(default)]
    pub counts: BTreeMap<String, i64>,
    #[serde(default)]
    pub tabs: BTreeMap<String, FanTabOut>,
    #[serde(default)]
    pub downloaded: i64,
    pub last_walk_at: Option<String>,
    pub last_error: Option<String>,
    pub walk: Option<WalkState>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AddFanRequest {
    pub url: String,
    /// Start a walk straight away.
    #[serde(default = "t")]
    pub walk: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WalkRequest {
    pub queue_new: Option<bool>,
    /// Subset of `wishlist` | `collection`; default both.
    pub tabs: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FanNextItem {
    pub item_id: i64,
    pub url: String,
    pub url_kind: String,
    pub title: String,
    pub artist_name: String,
    pub art_url: Option<String>,
    pub state: String,
    pub release_id: Option<i64>,
    pub position: Option<i64>,
}

/// `GET /fans/{id}/next?after=&order=seq|shuffle&seed=&state=&tab=&limit=`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FanNextOut {
    pub items: Vec<FanNextItem>,
    pub exhausted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PeekItem {
    pub url: String,
    pub title: String,
    pub artist_name: String,
    pub art_url: Option<String>,
    #[serde(default = "d_album")]
    pub item_type: String,
    #[serde(default)]
    pub in_library: bool,
}
fn d_album() -> String {
    "album".into()
}

/// `GET /fans/peek?url=`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FanPeekOut {
    pub url: String,
    pub username: String,
    pub display_name: String,
    pub bc_fan_id: Option<i64>,
    pub wishlist_count: i64,
    pub collection_count: i64,
    pub followed_id: Option<i64>,
    pub collection: Vec<PeekItem>,
    pub wishlist: Vec<PeekItem>,
}

/// `GET /fans/peek/items?url=&which=&cursor=&fan_id=&count=`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FanPeekPageOut {
    pub items: Vec<PeekItem>,
    pub cursor: Option<String>,
    #[serde(default)]
    pub more: bool,
    pub total: Option<i64>,
}

// ======================================================================================
// Follows (Feed)
// ======================================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FollowOut {
    pub id: i64,
    /// discover | search | artist | label
    pub kind: String,
    pub label: String,
    pub url: Option<String>,
    pub enabled: bool,
    /// The Explore page's own URL params, verbatim.
    #[serde(default)]
    pub explore_params: BTreeMap<String, Value>,
    #[serde(default)]
    pub api_params: BTreeMap<String, Value>,
    pub limit: Option<i64>,
    pub last_run_at: Option<String>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub items_seen: i64,
    #[serde(default)]
    pub items_new: i64,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FollowsOut {
    pub sources: Vec<FollowOut>,
    pub include_library_artists: bool,
    pub include_library_labels: bool,
    pub poll_hours: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FollowCreate {
    pub kind: String,
    pub label: String,
    pub url: Option<String>,
    #[serde(default)]
    pub explore_params: BTreeMap<String, Value>,
    #[serde(default)]
    pub api_params: BTreeMap<String, Value>,
    pub limit: Option<i64>,
    #[serde(default = "t")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FollowPatch {
    pub label: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FollowSettingsIn {
    pub include_library_artists: Option<bool>,
    pub include_library_labels: Option<bool>,
    pub poll_hours: Option<f64>,
}

/// `feed.sweep` event and `GET/POST/DELETE /follows/sweep`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FeedSweepStatus {
    pub phase: String,
    #[serde(default = "d_all")]
    pub scope: String,
    pub running: bool,
    pub done: i64,
    pub total: Option<i64>,
    pub current: Option<String>,
    #[serde(default)]
    pub no_url: i64,
    #[serde(default)]
    pub seen: i64,
    #[serde(default)]
    pub new: i64,
    #[serde(default)]
    pub in_library: i64,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FeedSweepRequest {
    #[serde(default)]
    pub source_ids: Vec<i64>,
}

// ======================================================================================
// Explore
// ======================================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SearchHitOut {
    /// artist | label | album | track | fan
    pub kind: String,
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub subtitle: String,
    pub art_url: Option<String>,
    pub band_id: Option<i64>,
    pub item_id: Option<i64>,
    #[serde(default)]
    pub in_library: bool,
    #[serde(default)]
    pub blacklisted: bool,
    pub library_release_id: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReleaseCardOut {
    pub url: String,
    pub title: String,
    pub artist_name: String,
    #[serde(default = "d_album")]
    pub item_type: String,
    pub art_url: Option<String>,
    pub release_date: Option<String>,
    #[serde(default)]
    pub is_free_download: bool,
    #[serde(default)]
    pub in_library: bool,
    #[serde(default)]
    pub blacklisted: bool,
    pub library_release_id: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CollectorOut {
    pub username: String,
    pub name: String,
    pub url: String,
    pub bc_fan_id: Option<i64>,
    pub image_url: Option<String>,
    /// Their review text.
    pub why: Option<String>,
    pub fav_track: Option<String>,
    /// Our `fans.id` if we follow them.
    pub followed_id: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CollectorsOut {
    pub url: String,
    pub supporters: Vec<CollectorOut>,
    pub reviews: Vec<CollectorOut>,
    pub more: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiscoverOut {
    #[serde(default)]
    pub items: Vec<ReleaseCardOut>,
    pub cursor: Option<String>,
    pub total: Option<i64>,
}

/// One artist or label page met in the best-selling feed (`GET /explore/spotlight`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotlightBandOut {
    pub name: String,
    pub url: String,
    pub location: Option<String>,
    /// The page's own photo, else the cover of its best-selling release.
    pub image_url: Option<String>,
    /// How many of the feed's releases came from this page.
    #[serde(default)]
    pub releases: i64,
}

/// Who is selling on Bandcamp right now, split into artists and labels: a starting point for
/// a library with nothing in it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotlightOut {
    #[serde(default)]
    pub artists: Vec<SpotlightBandOut>,
    #[serde(default)]
    pub labels: Vec<SpotlightBandOut>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RosterArtistOut {
    pub name: String,
    pub url: String,
    pub location: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BandOut {
    pub url: String,
    pub name: String,
    /// artist | label
    pub kind: String,
    pub location: Option<String>,
    pub bio: Option<String>,
    pub image_url: Option<String>,
    #[serde(default)]
    pub links: Vec<BTreeMap<String, String>>,
    #[serde(default)]
    pub releases: Vec<ReleaseCardOut>,
    #[serde(default)]
    pub roster: Vec<RosterArtistOut>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExploreTrackOut {
    pub title: String,
    pub track_num: Option<i64>,
    pub duration_sec: Option<f64>,
    pub artist: Option<String>,
    pub url: Option<String>,
    pub bc_track_id: Option<i64>,
    /// Same-origin proxy URL: `/api/explore/stream?release=<url>&track=<key>`.
    pub stream_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExploreReleaseOut {
    pub url: String,
    pub title: String,
    pub artist_name: String,
    #[serde(default = "d_album")]
    pub item_type: String,
    pub art_url: Option<String>,
    pub label_name: Option<String>,
    pub release_date: Option<String>,
    pub about: Option<String>,
    pub credits: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    #[serde(default)]
    pub is_free_download: bool,
    #[serde(default = "t")]
    pub is_purchasable: bool,
    #[serde(default)]
    pub is_preorder: bool,
    #[serde(default)]
    pub in_library: bool,
    #[serde(default)]
    pub blacklisted: bool,
    pub library_release_id: Option<i64>,
    pub band_url: Option<String>,
    #[serde(default)]
    pub tracks: Vec<ExploreTrackOut>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RelatedSectionOut {
    pub key: String,
    pub title: String,
    /// recommended | band | tag
    pub source: String,
    #[serde(default)]
    pub items: Vec<ReleaseCardOut>,
    pub tag: Option<String>,
    pub url: Option<String>,
    pub cursor: Option<String>,
    pub total: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RelatedOut {
    #[serde(default)]
    pub sections: Vec<RelatedSectionOut>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DownloadReleasesRequest {
    #[serde(default)]
    pub urls: Vec<String>,
    pub target_subdir: Option<String>,
    pub label: Option<String>,
    pub label_name: Option<String>,
    pub label_url: Option<String>,
    pub source_fan_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DownloadCatalogRequest {
    pub url: String,
    #[serde(default = "d_500")]
    pub limit: i64,
    #[serde(default)]
    pub free_only: bool,
    pub target_subdir: Option<String>,
}
fn d_500() -> i64 {
    500
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CatalogResult {
    pub job: Option<crate::jobs::JobOut>,
    pub band: String,
    pub found: i64,
    pub queued: i64,
    #[serde(default)]
    pub skipped_in_library: i64,
    #[serde(default)]
    pub skipped_not_free: i64,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub detail: String,
}

/// `GET /explore/genres`: Bandcamp's own filter vocabulary, passed through
/// as the discover page embeds it (genres, subgenres, locations, formats, time facets).
pub type GenresOut = Value;

// ======================================================================================
// Tracklists
// ======================================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TrackRowOut {
    pub seq: i64,
    pub artist: String,
    pub title: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub start: String,
    #[serde(default)]
    pub end: String,
    #[serde(default)]
    pub source_file: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ParsedFileOut {
    pub filename: String,
    pub title: String,
    pub rows: i64,
    #[serde(default)]
    pub skipped: i64,
    pub error: Option<String>,
}

/// `POST /tracklists/parse` (multipart `files`) response.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ParsedTracklists {
    pub suggested_title: String,
    pub files: Vec<ParsedFileOut>,
    pub rows: Vec<TrackRowOut>,
    pub duplicates: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MatchRequest {
    pub artist: String,
    pub title: String,
    #[serde(default)]
    pub label: String,
    pub query: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CandidateOut {
    #[serde(flatten)]
    pub hit: SearchHitOut,
    pub score: f64,
    /// strong | likely | weak
    pub tier: String,
    #[serde(default)]
    pub label_match: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MatchOut {
    pub query: String,
    pub candidates: Vec<CandidateOut>,
    pub best_index: Option<i64>,
    /// How many Bandcamp searches the match cost.
    pub searches: i64,
}

/// `POST /artists/{id}/locate` and `POST /labels/{id}/locate`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LocateOut {
    pub url: Option<String>,
    /// Library releases found on the located page: the proof.
    #[serde(default)]
    pub matched: i64,
    #[serde(default)]
    pub detail: String,
}

// Loved streams (DTOs, routes, reconcile): owned by WS1 (`bc_types::library`, `bc_maint::loved`).
// The `loved.reconciled` topic is WS1's too.

pub mod logic;
