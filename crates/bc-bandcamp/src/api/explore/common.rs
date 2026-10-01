//! Helpers shared by the explore, stream and tracklist routes: the SSRF guard, the
//! library badges, the card builders and a tolerant query-string reader.

use std::str::FromStr;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use bc_db::Db;
use bc_jobs::ApiError;
use bc_types::bandcamp::ReleaseCardOut;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

use crate::download::dedup::{KnownIds, NameLookup, find_known_ids_async, url_key};
use crate::extract::{GridItem, HarvestedRelease, HarvestedTrack};
use crate::urls;

/// `url_key -> (reason, releases.id)` as `find_known_ids` answers it.
pub type Known = KnownIds;

pub fn unprocessable(msg: impl Into<String>) -> ApiError {
    ApiError::new(422, "Unprocessable Entity").detail(msg)
}

/// `_bandcamp_url`: accept only real Bandcamp release/band URLs from the client.
///
/// Every route here takes a URL as a parameter and hands it to an HTTP client, so an
/// unvalidated value is a server-side request forgery. Validating the host once, centrally,
/// is what keeps that from being a per-route judgement call. Custom domains are legitimate
/// Bandcamp artist sites, but cannot be told apart from arbitrary hosts, so they are out of scope.
pub fn bandcamp_url(raw: &str, field: &str) -> Result<String, ApiError> {
    let mut value = raw.trim().to_string();
    if value.is_empty() {
        return Err(ApiError::bad_request(format!("{field} is required")));
    }
    let lower = value.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        value = format!("https://{value}");
    }
    // The netloc as urlparse sees it: between `//` and the first `/`, `?` or `#`.
    let after_scheme = value.split_once("//").map(|(_, r)| r).unwrap_or("");
    let netloc = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = netloc.split(':').next().unwrap_or("").to_ascii_lowercase();
    if host.is_empty() {
        return Err(ApiError::bad_request(format!("{field} is not a URL: {raw}")));
    }
    if host != "bandcamp.com" && !host.ends_with(".bandcamp.com") {
        return Err(ApiError::bad_request(format!("not a bandcamp.com URL: {raw}")));
    }
    Ok(urls::normalise(&value))
}

// -- library badges ------------------------------------------------------------------------

/// Whether the shelf actually holds this, as opposed to refusing it. `find_known_ids` answers
/// "should this be offered for download", and a blacklisted release answers no for a
/// completely different reason than an owned one.
pub fn owned(known: &Known, url: &str) -> bool {
    known.get(&url_key(url)).is_some_and(|e| e.0 != "blacklist")
}

pub fn blacklisted(known: &Known, url: &str) -> bool {
    known.get(&url_key(url)).is_some_and(|e| e.0 == "blacklist")
}

/// The library row to play instead of streaming, where matching named one.
pub fn library_release_id(known: &Known, url: &str) -> Option<i64> {
    known.get(&url_key(url)).filter(|e| e.0 != "blacklist").and_then(|e| e.1)
}

/// What of this browse view is already on the shelf. Every caller is looking at Bandcamp pages
/// that carry an artist and a title, so the name fallback is always on.
pub async fn known_releases(db: &Db, items: &[(String, String, String)]) -> Result<Known, ApiError> {
    let urls: Vec<String> = items.iter().map(|(u, _, _)| u.clone()).collect();
    let names: NameLookup = items.iter().map(|(u, a, t)| (u.clone(), (a.clone(), t.clone()))).collect();
    Ok(find_known_ids_async(db, urls, Some(names)).await?)
}

// -- cards ---------------------------------------------------------------------------------

pub fn card(release: &HarvestedRelease, known: &Known) -> ReleaseCardOut {
    ReleaseCardOut {
        url: release.url.clone(),
        title: release.title.clone(),
        artist_name: release.artist_name.clone(),
        item_type: release.item_type.clone(),
        art_url: release.art_url.clone(),
        release_date: release.release_date.clone(),
        is_free_download: release.is_free_download,
        in_library: owned(known, &release.url),
        blacklisted: blacklisted(known, &release.url),
        library_release_id: library_release_id(known, &release.url),
    }
}

pub fn grid_card(item: &GridItem, known: &Known, fallback_artist: &str) -> ReleaseCardOut {
    ReleaseCardOut {
        url: item.page_url.clone(),
        title: item.title.clone(),
        // The grid omits the artist on a single-artist page; the band name is the honest
        // answer there, and an empty second line is not.
        artist_name: if item.artist.is_empty() { fallback_artist.to_string() } else { item.artist.clone() },
        item_type: item.item_type.clone(),
        // The id builds a 700px cover; the scraped URL is the grid's own thumbnail, kept for
        // pages that expose no id -- a small cover beats an empty square.
        art_url: urls::build_art_url(item.art_id).or_else(|| item.art_url.clone()),
        release_date: None,
        is_free_download: false,
        in_library: owned(known, &item.page_url),
        blacklisted: blacklisted(known, &item.page_url),
        library_release_id: library_release_id(known, &item.page_url),
    }
}

/// `quote(url, safe='')`.
const QUOTE: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// `_proxy_url`: a stable player URL for one track.
///
/// Deliberately addresses the track by *release + id* rather than carrying the CDN URL itself.
/// Bandcamp signs those with a timestamp, so an embedded one would 403 an hour later. Resolving
/// on demand means the link the player holds never goes stale, and the signature never leaves
/// the server.
pub fn proxy_url(release_url: &str, track: &HarvestedTrack, index: usize) -> Option<String> {
    track.stream_url.as_ref()?;
    let key = track.bc_track_id.map(|i| i.to_string()).unwrap_or_else(|| format!("i{index}"));
    Some(format!("/api/explore/stream?release={}&track={key}", utf8_percent_encode(release_url, QUOTE)))
}

// -- query strings ---------------------------------------------------------------------------

/// The raw query as ordered pairs (repeated keys allowed: `tags=a&tags=b`), with the typed
/// getters answering 422 like FastAPI's validation.
#[derive(Debug, Clone, Default)]
pub struct Params(pub Vec<(String, String)>);

impl<S: Send + Sync> FromRequestParts<S> for Params {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let raw = parts.uri.query().unwrap_or("");
        Ok(Params(url::form_urlencoded::parse(raw.as_bytes()).into_owned().collect()))
    }
}

impl Params {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
    pub fn all(&self, key: &str) -> Vec<String> {
        self.0.iter().filter(|(k, _)| k == key).map(|(_, v)| v.clone()).collect()
    }
    pub fn string(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or(default).to_string()
    }
    /// A required string.
    pub fn required(&self, key: &str) -> Result<String, ApiError> {
        self.get(key).map(str::to_string).ok_or_else(|| unprocessable(format!("{key}: field required")))
    }
    pub fn num<T: FromStr + PartialOrd + std::fmt::Display + Copy>(&self, key: &str, default: T, lo: T, hi: T) -> Result<T, ApiError> {
        let Some(raw) = self.get(key) else { return Ok(default) };
        let v: T = raw.trim().parse().map_err(|_| unprocessable(format!("{key}: not a valid number: {raw}")))?;
        if v < lo || v > hi {
            return Err(unprocessable(format!("{key}: must be between {lo} and {hi}")));
        }
        Ok(v)
    }
    pub fn opt_i64(&self, key: &str) -> Result<Option<i64>, ApiError> {
        match self.get(key).filter(|v| !v.is_empty()) {
            None => Ok(None),
            Some(raw) => raw.trim().parse().map(Some).map_err(|_| unprocessable(format!("{key}: not a valid integer: {raw}"))),
        }
    }
    pub fn boolean(&self, key: &str, default: bool) -> Result<bool, ApiError> {
        match self.get(key).map(|v| v.trim().to_ascii_lowercase()) {
            None => Ok(default),
            Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on" | "t" | "y") => Ok(true),
            Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off" | "f" | "n") => Ok(false),
            Some(v) => Err(unprocessable(format!("{key}: not a valid boolean: {v}"))),
        }
    }
}
