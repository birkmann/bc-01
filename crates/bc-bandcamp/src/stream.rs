//! Stream resolver and CDN proxy (legacy `explore.py` "Streaming proxy", PLAN §3.4 row "CDN
//! stream proxy stalls on the scraper's token bucket").
//!
//! * The **parsed tralbum is cached in memory** (`release url -> tracks with their signed
//!   `stream_url``), so a browser's range requests never re-parse a page.
//! * Entries used in the last 30 minutes are **re-resolved ahead of time at 80 % of the TTL**
//!   ([`sources::TTL_PLAYABLE`], 15 min) by a background refresher on the **reserved token lane**
//!   ([`sources::fetch_release_reserved`]), single-flight per release, so playback does not queue
//!   behind a crawler and a long-running queue never meets an expired signature.
//! * Audio bytes travel over [`BandcampClient::cdn_client`]: a separate, **unthrottled**,
//!   cookie-less client. They never touch the scraper's bucket.
//! * `GET /explore/stream` forwards `Range` verbatim and relays 200/206/416 with a lower-cased
//!   header whitelist. A 403 means the signature expired: the entry is dropped, re-resolved once
//!   and the request retried.
//!
//! The signed CDN URL lives only in this process's memory; the player is only ever handed
//! `/api/explore/stream?release=..&track=..`.
//!
//! Rust API for the engine (no HTTP hop): [`StreamService::resolve_stream`] and
//! [`StreamService::release_tracks`]; `BandcampService` delegates to them.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Response, StatusCode};
use bc_db::Db;
use bc_jobs::ApiError;
use bc_types::bandcamp::{ExploreReleaseOut, ExploreTrackOut};
use bytes::Bytes;
use futures::StreamExt;
use parking_lot::Mutex;

use crate::api::explore::common::{blacklisted, known_releases, library_release_id, owned, proxy_url};
use crate::error::HarvestError;
use crate::extract::HarvestedRelease;
use crate::net::{BandcampClient, Lane};
use crate::service::Ctx;
use crate::{sources, urls};

/// The CDN hosts audio is served from (`https://t4.bcbits.com/stream/...`).
pub const STREAM_HOST_SUFFIX: &str = ".bcbits.com";
/// Relay chunk size.
pub const PROXY_CHUNK: usize = 64 * 1024;
/// Re-resolve at this fraction of the TTL.
pub const REFRESH_FRACTION: f64 = 0.8;
/// An entry used within this window is kept warm; an idle one is evicted.
pub const ACTIVE_WINDOW: Duration = Duration::from_secs(30 * 60);
/// How often the refresher looks for due entries.
pub const REFRESH_TICK: Duration = Duration::from_secs(30);
const MAX_ENTRIES: usize = 512;

/// Tunables (production values by default; tests shrink them).
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// How long a resolved signature is trusted ([`sources::TTL_PLAYABLE`]).
    pub ttl: Duration,
    pub refresh_fraction: f64,
    pub active_window: Duration,
    pub tick: Duration,
    /// Accept `http://` CDN URLs. **Tests only** (a local fake CDN has no TLS); the host
    /// allowlist still applies.
    pub allow_plain_http: bool,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            ttl: sources::TTL_PLAYABLE,
            refresh_fraction: REFRESH_FRACTION,
            active_window: ACTIVE_WINDOW,
            tick: REFRESH_TICK,
            allow_plain_http: false,
        }
    }
}

/// A resolved, currently valid CDN URL. Never persist it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStream {
    /// The signed CDN URL.
    pub url: String,
    /// How long until the signature is expected to lapse.
    pub expires_in: Duration,
}

struct Entry {
    release: Arc<HarvestedRelease>,
    fetched_at: Instant,
    last_used: Instant,
}

pub struct StreamService {
    client: BandcampClient,
    db: Db,
    cfg: StreamConfig,
    entries: Mutex<HashMap<String, Entry>>,
    /// Per-release single-flight locks.
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    refresher_started: AtomicBool,
}

impl StreamService {
    pub fn new(client: BandcampClient, db: Db) -> Self {
        Self::with_config(client, db, StreamConfig::default())
    }

    pub fn with_config(client: BandcampClient, db: Db, cfg: StreamConfig) -> Self {
        Self {
            client,
            db,
            cfg,
            entries: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
            refresher_started: AtomicBool::new(false),
        }
    }

    pub fn config(&self) -> &StreamConfig {
        &self.cfg
    }

    /// Number of releases currently held in memory.
    pub fn cached_releases(&self) -> usize {
        self.entries.lock().len()
    }

    // -- resolving -------------------------------------------------------------------------

    fn fresh(&self, key: &str, newer_than: Option<Instant>, touch: bool) -> Option<(Arc<HarvestedRelease>, Instant)> {
        let mut map = self.entries.lock();
        let e = map.get_mut(key)?;
        if e.fetched_at.elapsed() >= self.cfg.ttl || newer_than.is_some_and(|t| e.fetched_at < t) {
            return None;
        }
        if touch {
            e.last_used = Instant::now();
        }
        Some((e.release.clone(), e.fetched_at))
    }

    /// The parsed release, from memory when fresh, else fetched single-flight on `lane`.
    /// `newer_than`: only an entry fetched after this instant will do (a forced re-resolve; the
    /// page cache is invalidated first so the fetch really reaches Bandcamp). Callers that queue
    /// up behind the same re-resolve share its result.
    async fn entry(
        &self,
        url: &str,
        lane: Lane,
        newer_than: Option<Instant>,
        touch: bool,
    ) -> Result<(Arc<HarvestedRelease>, Instant), HarvestError> {
        let key = urls::normalise(url);
        if let Some(hit) = self.fresh(&key, newer_than, touch) {
            return Ok(hit);
        }
        let lock = self.flights.lock().entry(key.clone()).or_default().clone();
        let result = {
            let _guard = lock.lock().await;
            if let Some(hit) = self.fresh(&key, newer_than, touch) {
                Ok(hit)
            } else {
                if newer_than.is_some()
                    && let Some(cache) = self.client.cache()
                {
                    let _ = cache.invalidate(Some(&key));
                }
                let fetched = match lane {
                    Lane::Reserved => sources::fetch_release_reserved(&self.client, &key).await,
                    Lane::Normal => sources::fetch_release(&self.client, &key).await,
                };
                fetched.map(|release| {
                    let now = Instant::now();
                    let release = Arc::new(release);
                    let mut map = self.entries.lock();
                    // A refresh keeps the use history: the entry was in use, that is why it is
                    // being refreshed.
                    let last_used = if touch { now } else { map.get(&key).map(|e| e.last_used).unwrap_or(now) };
                    map.insert(key.clone(), Entry { release: release.clone(), fetched_at: now, last_used });
                    if map.len() > MAX_ENTRIES
                        && let Some(oldest) = map.iter().min_by_key(|(_, e)| e.last_used).map(|(k, _)| k.clone())
                    {
                        map.remove(&oldest);
                    }
                    (release, now)
                })
            }
        };
        // Drop the flight slot when nobody else is waiting on it.
        {
            let mut flights = self.flights.lock();
            if Arc::strong_count(&lock) <= 2 {
                flights.remove(&key);
            }
        }
        result
    }

    /// Drop the cached signature for a release (the CDN answered 403 for it).
    pub fn forget(&self, release_url: &str) {
        self.entries.lock().remove(&urls::normalise(release_url));
    }

    /// The current signed CDN URL for a track, from the in-memory tralbum.
    ///
    /// `track_key` is the `bc_track_id` or `i<index>`. `refresh = true` (after a 403) drops the
    /// cached signature and the page cache and re-resolves once on the reserved lane; concurrent
    /// refreshes of one release share a single fetch. The signed URL is never stored.
    pub async fn resolve_stream(&self, release_url: &str, track_key: &str, refresh: bool) -> Result<ResolvedStream, HarvestError> {
        let newer_than = refresh.then(Instant::now);
        let (release, fetched_at) = self.entry(release_url, Lane::Reserved, newer_than, true).await?;
        for (index, t) in release.tracks.iter().enumerate() {
            let matches = if track_key.starts_with('i') {
                format!("i{index}") == track_key
            } else {
                t.bc_track_id.is_some_and(|id| id.to_string() == track_key)
            };
            if matches {
                let Some(url) = t.stream_url.clone() else {
                    return Err(HarvestError::other("stream not found: this track has no stream on Bandcamp"));
                };
                return Ok(ResolvedStream { url, expires_in: self.cfg.ttl.saturating_sub(fetched_at.elapsed()) });
            }
        }
        Err(HarvestError::other(format!("track {track_key} not found on {release_url}")))
    }

    /// A release for the browse view and for the engine's explore queue source: tracks carry
    /// `bc_track_id` and the same-origin proxy `stream_url`, library badges included. Writes the
    /// page's tags back to the inbox row and leaves the tralbum in the stream cache.
    pub async fn release_tracks(&self, release_url: &str) -> Result<ExploreReleaseOut, HarvestError> {
        let (found, _) = self.entry(release_url, Lane::Normal, None, true).await?;
        if !found.tags.is_empty()
            && let Err(e) = self.write_back_tags(&found.url, &found.tags).await
        {
            tracing::debug!("could not write tags back for {}: {e}", found.url);
        }
        let known = known_releases(&self.db, &[(found.url.clone(), found.artist_name.clone(), found.title.clone())])
            .await
            .map_err(|e| HarvestError::other(e.problem.detail.unwrap_or_default()))?;
        Ok(ExploreReleaseOut {
            url: found.url.clone(),
            title: found.title.clone(),
            artist_name: found.artist_name.clone(),
            item_type: found.item_type.clone(),
            art_url: found.art_url.clone(),
            label_name: found.label_name.clone(),
            release_date: found.release_date.clone(),
            about: found.about.clone(),
            credits: found.credits.clone(),
            tags: found.tags.clone(),
            price: found.price,
            currency: found.currency.clone(),
            is_free_download: found.is_free_download,
            is_purchasable: found.is_purchasable,
            is_preorder: found.is_preorder,
            in_library: owned(&known, &found.url),
            blacklisted: blacklisted(&known, &found.url),
            library_release_id: library_release_id(&known, &found.url),
            band_url: Some(urls::artist_root(&found.url)),
            tracks: found
                .tracks
                .iter()
                .enumerate()
                .map(|(index, t)| ExploreTrackOut {
                    title: t.title.clone(),
                    track_num: t.track_num,
                    duration_sec: t.duration_sec,
                    artist: t.artist.clone(),
                    url: t.url.clone(),
                    bc_track_id: t.bc_track_id,
                    stream_url: proxy_url(&found.url, t, index),
                })
                .collect(),
        })
    }

    /// The page carries what grid harvests could not see -- tags above all -- so write them back
    /// to the inbox row (and its normalised tag table) while they are in hand.
    async fn write_back_tags(&self, url: &str, tags: &[String]) -> Result<(), bc_db::DbError> {
        let (url, tags) = (url.to_string(), tags.to_vec());
        self.db
            .write_async(move |tx| {
                use bc_db::rusqlite::OptionalExtension;
                let row: Option<(i64, String)> = tx
                    .query_row("SELECT id, tags FROM harvest_items WHERE url = ?1", [&url], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?;
                let Some((id, current)) = row else { return Ok(()) };
                let same = serde_json::from_str::<Vec<String>>(&current).map(|c| c == tags).unwrap_or(false);
                if !same {
                    tx.execute("UPDATE harvest_items SET tags = ?2 WHERE id = ?1", bc_db::rusqlite::params![id, serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into())])?;
                    crate::harvest::inbox::sync_tags(tx, id, &tags).map_err(|e| bc_db::DbError::Other(e.to_string()))?;
                }
                Ok(())
            })
            .await
    }

    // -- resolving ahead ---------------------------------------------------------------------

    /// Resolve a release in the background (on the reserved lane) so that pressing play does
    /// not wait for a page fetch. Call it when a release is shown or queued.
    pub fn warm(self: &Arc<Self>, release_url: &str) {
        let me = self.clone();
        let url = release_url.to_string();
        tokio::spawn(async move {
            if let Err(e) = me.entry(&url, Lane::Reserved, None, false).await {
                tracing::debug!("stream warm-up failed for {url}: {e}");
            }
        });
    }

    /// Re-resolve every entry used recently whose signature is at 80 % of its TTL, and evict the
    /// idle ones. One pass of the background refresher.
    pub async fn refresh_due(&self) {
        let threshold = self.cfg.ttl.mul_f64(self.cfg.refresh_fraction);
        let due: Vec<String> = {
            let mut map = self.entries.lock();
            map.retain(|_, e| e.last_used.elapsed() < self.cfg.active_window);
            map.iter().filter(|(_, e)| e.fetched_at.elapsed() >= threshold).map(|(k, _)| k.clone()).collect()
        };
        for key in due {
            // Single-flight with any on-demand resolve of the same release; the reserved lane
            // paces the batch.
            if let Err(e) = self.entry(&key, Lane::Reserved, Some(Instant::now()), false).await {
                tracing::debug!("stream refresh failed for {key}: {e}");
            }
        }
    }

    /// Spawn the background refresher (idempotent; it ends when the service is dropped).
    pub fn start_refresher(self: &Arc<Self>) {
        if self.refresher_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        let tick = self.cfg.tick;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let Some(me) = weak.upgrade() else { break };
                me.refresh_due().await;
            }
        });
    }

    // -- the proxy ---------------------------------------------------------------------------

    /// The single place a URL becomes an outbound audio request: https and a `*.bcbits.com`
    /// host only, so a page that starts redirecting elsewhere cannot make this an open proxy.
    fn check_cdn_url(&self, raw: &str) -> Result<(), ApiError> {
        let bad = || ApiError::bad_request("stream URL must be an https bcbits.com address");
        let parsed = url::Url::parse(raw).map_err(|_| bad())?;
        let scheme_ok = parsed.scheme() == "https" || (self.cfg.allow_plain_http && parsed.scheme() == "http");
        let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
        if !scheme_ok || !host.ends_with(STREAM_HOST_SUFFIX) {
            return Err(bad());
        }
        Ok(())
    }

    /// Open the CDN response without reading the body. `None` = unreachable.
    async fn open_upstream(&self, url: &str, range: Option<&str>) -> Result<Option<reqwest::Response>, ApiError> {
        self.check_cdn_url(url)?;
        let mut req = self.client.cdn_client().get(url).header(reqwest::header::ACCEPT_ENCODING, "identity");
        if let Some(r) = range {
            // Forward the browser's Range verbatim; rewriting it is how seeking breaks.
            req = req.header(reqwest::header::RANGE, r);
        }
        match req.send().await {
            Ok(resp) => {
                // The client follows redirects: the final hop must pass the allowlist too.
                self.check_cdn_url(resp.url().as_str())?;
                Ok(Some(resp))
            }
            Err(e) => {
                // reqwest errors can carry the signed URL: log the kind only.
                tracing::warn!("stream proxy request failed: {}", e.without_url());
                Ok(None)
            }
        }
    }

    /// `GET /explore/stream`: relay a Bandcamp stream, preserving `Range` in both directions.
    ///
    /// Proxied rather than linked because the CDN sets no CORS headers the page can rely on, and
    /// because a signed URL in the DOM is a credential written into browser history.
    pub async fn proxy(&self, release_url: &str, track: &str, range: Option<&str>) -> Result<Response<Body>, ApiError> {
        let resolved = self.resolve_stream(release_url, track, false).await?;
        let mut upstream = self.open_upstream(&resolved.url, range).await?;

        if upstream.as_ref().is_some_and(|u| u.status() == reqwest::StatusCode::FORBIDDEN) {
            // The cached page handed us a signature that has since expired. Drop it and resolve
            // once more before giving up -- a silent retry here is the difference between "seek
            // works" and "seek kills playback".
            drop(upstream);
            self.forget(release_url);
            let again = self.resolve_stream(release_url, track, true).await?;
            upstream = self.open_upstream(&again.url, range).await?;
        }

        let Some(upstream) = upstream else {
            return Err(ApiError::bad_request("could not reach Bandcamp's audio CDN"));
        };
        let status = upstream.status();
        // 416 is a legitimate answer to a Range request: relay it with its Content-Range.
        if status.as_u16() >= 400 && status != reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(ApiError::not_found(format!("Bandcamp returned {} for this stream", status.as_u16())));
        }

        // Lower-cased whitelist: mixing "accept-ranges" from upstream with an "Accept-Ranges"
        // default produces two of the header, and a duplicated Accept-Ranges is enough to stop
        // Chrome issuing range requests at all.
        let mut builder = Response::builder().status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK));
        let mut has_accept_ranges = false;
        let mut has_type = false;
        for (name, value) in upstream.headers() {
            let n = name.as_str().to_ascii_lowercase();
            if !matches!(n.as_str(), "content-length" | "content-range" | "accept-ranges" | "content-type") {
                continue;
            }
            has_accept_ranges |= n == "accept-ranges";
            has_type |= n == "content-type";
            if let (Ok(hn), Ok(hv)) = (HeaderName::from_bytes(n.as_bytes()), HeaderValue::from_bytes(value.as_bytes())) {
                builder = builder.header(hn, hv);
            }
        }
        if !has_accept_ranges {
            builder = builder.header("accept-ranges", "bytes");
        }
        if !has_type {
            builder = builder.header("content-type", "audio/mpeg");
        }
        // Audio the app does not own: cached briefly so a replay or a scrub backwards costs
        // nothing, but never long enough to outlive the release.
        builder = builder.header("cache-control", "private, max-age=600");

        let body = Body::from_stream(upstream.bytes_stream().flat_map(|chunk| {
            futures::stream::iter(match chunk {
                Ok(bytes) => rechunk(bytes).into_iter().map(Ok).collect::<Vec<_>>(),
                Err(e) => vec![Err(std::io::Error::other(e.without_url()))],
            })
        }));
        builder.body(body).map_err(|e| ApiError::internal(e.to_string()))
    }
}

/// Split a network chunk into at most [`PROXY_CHUNK`]-sized pieces (cheap: `Bytes` slicing).
fn rechunk(mut b: Bytes) -> Vec<Bytes> {
    let mut out = Vec::with_capacity(b.len() / PROXY_CHUNK + 1);
    while b.len() > PROXY_CHUNK {
        out.push(b.split_to(PROXY_CHUNK));
    }
    if !b.is_empty() {
        out.push(b);
    }
    out
}

/// Register the service (from `api::explore::init`).
pub(crate) fn register(ctx: &Arc<Ctx>) -> Arc<StreamService> {
    let svc = Arc::new(StreamService::new(ctx.client.clone(), ctx.db.clone()));
    ctx.put(svc.clone());
    svc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rechunk_splits_at_64k() {
        let parts = rechunk(Bytes::from(vec![0u8; PROXY_CHUNK * 2 + 5]));
        assert_eq!(parts.iter().map(Bytes::len).collect::<Vec<_>>(), vec![PROXY_CHUNK, PROXY_CHUNK, 5]);
        assert!(rechunk(Bytes::new()).is_empty());
    }
}
