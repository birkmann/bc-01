//! The Bandcamp operations this crate needs, as a trait. WS2's client implements it; this crate
//! never depends on the Bandcamp crate. Routes that need it (`/releases/strays/merge`,
//! `/loved-streams/download`) answer `409 "the Bandcamp client is not available"` when the router
//! was built without one.

use async_trait::async_trait;

/// A page that could not be read: a removed track, a private stream, a network failure. One dead
/// page must not end a sweep of hundreds, so callers count it and go on.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct LookupError(pub String);

#[derive(Debug, Clone, PartialEq)]
pub struct AlbumTrack {
    pub title: String,
    /// 1-based position on the record, when the page states it.
    pub track_num: Option<i64>,
    pub duration_sec: Option<f64>,
    /// Bandcamp can serve it now. False on a pre-order's tracks that are not out yet.
    pub available: bool,
}

impl Default for AlbumTrack {
    fn default() -> Self {
        Self { title: String::new(), track_num: None, duration_sec: None, available: true }
    }
}

/// What an album page settles (the subset of `HarvestedRelease` the merge reads).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AlbumInfo {
    pub url: String,
    pub title: String,
    pub artist_name: String,
    /// `YYYY-MM-DD`
    pub release_date: Option<String>,
    /// The record is on pre-order: some tracks are out, the rest come on `release_date`.
    pub is_preorder: bool,
    pub label_name: Option<String>,
    pub about: Option<String>,
    pub credits: Option<String>,
    pub tracks: Vec<AlbumTrack>,
}

#[async_trait]
pub trait BandcampLookup: Send + Sync {
    /// The album URL behind a page: a `/track/` URL resolves to its parent album, a standalone
    /// single (or an album URL) resolves to itself. Errors when the page cannot be read.
    async fn resolve_album_url(&self, url: &str) -> Result<String, LookupError>;

    /// Fetch and parse an album page.
    async fn fetch_album(&self, url: &str) -> Result<AlbumInfo, LookupError>;
}
