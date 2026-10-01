//! The Bandcamp HTTP layer: token buckets, persistent page cache, client.

pub mod bucket;
pub mod cache;
pub mod client;

pub use bucket::{TokenBucket, RESERVED_BURST, RESERVED_RATE_PER_SEC};
pub use cache::{
    CacheStats, CachedPage, KindIter, PageCache, PageKind, DEFAULT_MAX_BYTES, TTL_ALBUM, TTL_DISCOVER, TTL_FAN, TTL_MUSIC,
    TTL_STREAM,
};
pub use client::{
    BandcampClient, ClientOptions, ClientStats, GetOpts, Lane, check_api_error, is_bandcamp_host, is_certificate_error,
};

/// Build the client from the app config: harvest rate/burst/concurrency/UA,
/// the persistent page cache at `Config::cache_db_path()` (an in-memory cache
/// if the file cannot be opened) and the optional identity cookie.
pub fn client_from_config(cfg: &bc_core::Config, cookie: Option<String>) -> BandcampClient {
    let cache = match PageCache::open(&cfg.cache_db_path(), DEFAULT_MAX_BYTES) {
        Ok(c) => Some(c.shared()),
        Err(e) => {
            tracing::warn!("page cache unavailable at {} ({e}); using an in-memory cache", cfg.cache_db_path().display());
            PageCache::open_in_memory(DEFAULT_MAX_BYTES).ok().map(PageCache::shared)
        }
    };
    BandcampClient::new(ClientOptions {
        rate_per_sec: cfg.harvest_rate_per_sec,
        burst: cfg.harvest_burst,
        concurrency: cfg.harvest_concurrency,
        user_agent: Some(cfg.harvest_user_agent.clone()),
        cookie,
        cache,
        ..ClientOptions::default()
    })
}
