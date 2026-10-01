//! Persistent page cache in its own SQLite file (`Config::cache_db_path()`).
//!
//! Re-parsing a cached page costs nothing, so an extractor fix can be replayed
//! over the cache without re-crawling ([`PageCache::iter_kind`], used by
//! `bc bandcamp replay`) -- which matters because re-crawling is the thing
//! most likely to get the IP blocked.
//!
//! This deliberately does **not** use `bc_db::Db` (that runs the library
//! migrations); it owns a separate connection (WAL, `synchronous=NORMAL`)
//! behind a mutex. Methods are blocking; the client calls them through
//! `spawn_blocking`.
//!
//! Bodies are stored zstd(3)-compressed; `size` is the compressed length and the
//! cap (default 256 MiB) is applied to the sum of those, evicting
//! least-recently-used rows (`last_used`) first.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bc_db::rusqlite::{Connection, OptionalExtension, params};
use parking_lot::Mutex;

use crate::error::{HarvestError, Result};

pub const TTL_ALBUM: f64 = 7.0 * 86400.0;
pub const TTL_MUSIC: f64 = 12.0 * 3600.0;
pub const TTL_FAN: f64 = 300.0;
/// A live feed; never served from cache.
pub const TTL_DISCOVER: f64 = 0.0;
/// Tralbum / stream pages used by explore: short, stream URLs expire.
pub const TTL_STREAM: f64 = 600.0;

/// Default cache size cap (compressed bytes).
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Kind of page; selects the default TTL and lets extractors be replayed per kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PageKind {
    /// A release page (7 days).
    #[default]
    Album,
    /// An artist/label `/music` grid (12 hours).
    Music,
    /// A fan page (5 minutes).
    Fan,
    /// Live discover feed (never cached).
    Discover,
    /// Tralbum page used by explore stream resolution (10 minutes).
    Stream,
}

impl PageKind {
    pub const ALL: [PageKind; 5] = [Self::Album, Self::Music, Self::Fan, Self::Discover, Self::Stream];

    /// Default time-to-live in seconds (0 = never cached).
    pub fn ttl(self) -> f64 {
        match self {
            Self::Album => TTL_ALBUM,
            Self::Music => TTL_MUSIC,
            Self::Fan => TTL_FAN,
            Self::Discover => TTL_DISCOVER,
            Self::Stream => TTL_STREAM,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Music => "music",
            Self::Fan => "fan",
            Self::Discover => "discover",
            Self::Stream => "stream",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// One cached page, decompressed.
#[derive(Debug, Clone)]
pub struct CachedPage {
    pub url: String,
    pub kind: Option<PageKind>,
    pub body: String,
    pub fetched_at: f64,
    pub etag: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    pub entries: u64,
    /// Sum of compressed body sizes.
    pub bytes: u64,
}

struct Inner {
    conn: Connection,
    total: i64,
}

/// The persistent page cache. Cheap to share: wrap in `Arc` (see [`PageCache::shared`]).
pub struct PageCache {
    inner: Mutex<Inner>,
    max_bytes: u64,
}

impl std::fmt::Debug for PageCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageCache").field("max_bytes", &self.max_bytes).finish()
    }
}

fn cerr(e: impl std::fmt::Display) -> HarvestError {
    HarvestError::Other(format!("page cache: {e}"))
}

pub fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS pages(
    url TEXT PRIMARY KEY,
    kind TEXT,
    body BLOB NOT NULL,
    fetched_at REAL NOT NULL,
    last_used REAL NOT NULL,
    size INTEGER NOT NULL,
    etag TEXT
);
CREATE INDEX IF NOT EXISTS pages_last_used ON pages(last_used);
CREATE INDEX IF NOT EXISTS pages_kind ON pages(kind);
";

impl PageCache {
    /// Open (creating) the cache file at `path`.
    pub fn open(path: &Path, max_bytes: u64) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).map_err(cerr)?;
        Self::init(conn, max_bytes, true)
    }

    /// A throwaway in-memory cache (tests, or when the file cannot be opened).
    pub fn open_in_memory(max_bytes: u64) -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(cerr)?;
        Self::init(conn, max_bytes, false)
    }

    fn init(conn: Connection, max_bytes: u64, wal: bool) -> Result<Self> {
        if wal {
            conn.pragma_update(None, "journal_mode", "WAL").map_err(cerr)?;
        }
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(cerr)?;
        conn.busy_timeout(std::time::Duration::from_secs(5)).map_err(cerr)?;
        conn.execute_batch(SCHEMA).map_err(cerr)?;
        let total: i64 = conn
            .query_row("SELECT COALESCE(SUM(size), 0) FROM pages", [], |r| r.get(0))
            .map_err(cerr)?;
        Ok(Self { inner: Mutex::new(Inner { conn, total }), max_bytes })
    }

    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// The page if cached and younger than `max_age` seconds (a hit refreshes `last_used`).
    /// `max_age <= 0` never hits.
    pub fn get(&self, url: &str, max_age: f64) -> Result<Option<CachedPage>> {
        self.get_at(url, max_age, now_secs())
    }

    fn get_at(&self, url: &str, max_age: f64, now: f64) -> Result<Option<CachedPage>> {
        if max_age <= 0.0 {
            return Ok(None);
        }
        let g = self.inner.lock();
        let row = g
            .conn
            .query_row(
                "SELECT kind, body, fetched_at, etag FROM pages WHERE url = ?1",
                [url],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, f64>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(cerr)?;
        let Some((kind, blob, fetched_at, etag)) = row else { return Ok(None) };
        if now - fetched_at > max_age {
            return Ok(None);
        }
        let body = decode(&blob)?;
        g.conn.execute("UPDATE pages SET last_used = ?2 WHERE url = ?1", params![url, now]).map_err(cerr)?;
        Ok(Some(CachedPage {
            url: url.to_string(),
            kind: kind.as_deref().and_then(PageKind::parse),
            body,
            fetched_at,
            etag,
        }))
    }

    /// Store (or replace) a page, then evict least-recently-used rows above the size cap.
    pub fn put(&self, url: &str, kind: PageKind, body: &str, etag: Option<&str>) -> Result<()> {
        self.put_at(url, kind, body, etag, now_secs())
    }

    /// [`put`](Self::put) with an explicit `fetched_at` (tests / imports).
    pub fn put_at(&self, url: &str, kind: PageKind, body: &str, etag: Option<&str>, at: f64) -> Result<()> {
        let blob = zstd::bulk::compress(body.as_bytes(), 3).map_err(cerr)?;
        let size = blob.len() as i64;
        let mut g = self.inner.lock();
        let old: Option<i64> =
            g.conn.query_row("SELECT size FROM pages WHERE url = ?1", [url], |r| r.get(0)).optional().map_err(cerr)?;
        g.conn
            .execute(
                "INSERT INTO pages(url, kind, body, fetched_at, last_used, size, etag)
                 VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)
                 ON CONFLICT(url) DO UPDATE SET kind=excluded.kind, body=excluded.body,
                    fetched_at=excluded.fetched_at, last_used=excluded.last_used,
                    size=excluded.size, etag=excluded.etag",
                params![url, kind.as_str(), blob, at, size, etag],
            )
            .map_err(cerr)?;
        g.total += size - old.unwrap_or(0);
        Self::evict(&mut g, self.max_bytes as i64)
    }

    fn evict(g: &mut Inner, cap: i64) -> Result<()> {
        if g.total <= cap {
            return Ok(());
        }
        let victims: Vec<(String, i64)> = {
            let mut st = g
                .conn
                .prepare("SELECT url, size FROM pages ORDER BY last_used ASC, fetched_at ASC")
                .map_err(cerr)?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))).map_err(cerr)?;
            let mut total = g.total;
            let mut out = Vec::new();
            for row in rows {
                if total <= cap {
                    break;
                }
                let (u, s) = row.map_err(cerr)?;
                total -= s;
                out.push((u, s));
            }
            out
        };
        for (u, s) in victims {
            g.conn.execute("DELETE FROM pages WHERE url = ?1", [&u]).map_err(cerr)?;
            g.total -= s;
        }
        Ok(())
    }

    /// Drop one page (`Some(url)`) or everything (`None`); returns rows removed.
    pub fn invalidate(&self, url: Option<&str>) -> Result<usize> {
        let mut g = self.inner.lock();
        let n = match url {
            Some(u) => g.conn.execute("DELETE FROM pages WHERE url = ?1", [u]).map_err(cerr)?,
            None => g.conn.execute("DELETE FROM pages", []).map_err(cerr)?,
        };
        g.total = g.conn.query_row("SELECT COALESCE(SUM(size), 0) FROM pages", [], |r| r.get(0)).map_err(cerr)?;
        Ok(n)
    }

    pub fn stats(&self) -> Result<CacheStats> {
        let g = self.inner.lock();
        let (entries, bytes): (i64, i64) = g
            .conn
            .query_row("SELECT COUNT(*), COALESCE(SUM(size), 0) FROM pages", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(cerr)?;
        Ok(CacheStats { entries: entries as u64, bytes: bytes as u64 })
    }

    /// Number of cached pages (the legacy `size`).
    pub fn len(&self) -> usize {
        self.stats().map(|s| s.entries as usize).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Replay support: every cached page of `kind` regardless of age, one at a
    /// time (bodies are decompressed lazily, so memory stays bounded). Does not
    /// touch `last_used`. Pages that vanish mid-iteration are skipped.
    pub fn iter_kind(self: &Arc<Self>, kind: PageKind) -> Result<KindIter> {
        let urls: Vec<String> = {
            let g = self.inner.lock();
            let mut st = g.conn.prepare("SELECT url FROM pages WHERE kind = ?1 ORDER BY url").map_err(cerr)?;
            let rows = st.query_map([kind.as_str()], |r| r.get::<_, String>(0)).map_err(cerr)?;
            rows.collect::<std::result::Result<_, _>>().map_err(cerr)?
        };
        Ok(KindIter { cache: Arc::clone(self), urls: urls.into_iter() })
    }

    fn load_any_age(&self, url: &str) -> Result<Option<CachedPage>> {
        let g = self.inner.lock();
        let row = g
            .conn
            .query_row("SELECT kind, body, fetched_at, etag FROM pages WHERE url = ?1", [url], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .optional()
            .map_err(cerr)?;
        let Some((kind, blob, fetched_at, etag)) = row else { return Ok(None) };
        Ok(Some(CachedPage {
            url: url.to_string(),
            kind: kind.as_deref().and_then(PageKind::parse),
            body: decode(&blob)?,
            fetched_at,
            etag,
        }))
    }
}

fn decode(blob: &[u8]) -> Result<String> {
    let raw = zstd::stream::decode_all(blob).map_err(cerr)?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// Iterator returned by [`PageCache::iter_kind`].
pub struct KindIter {
    cache: Arc<PageCache>,
    urls: std::vec::IntoIter<String>,
}

impl Iterator for KindIter {
    type Item = Result<CachedPage>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let url = self.urls.next()?;
            match self.cache.load_any_age(&url) {
                Ok(Some(p)) => return Some(Ok(p)),
                Ok(None) => continue,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(max: u64) -> Arc<PageCache> {
        PageCache::open_in_memory(max).unwrap().shared()
    }

    #[test]
    fn cache_round_trip() {
        let c = cache(DEFAULT_MAX_BYTES);
        c.put("https://x/album", PageKind::Album, "<html>body</html>", Some("abc")).unwrap();
        let hit = c.get("https://x/album", 60.0).unwrap().unwrap();
        assert_eq!(hit.body, "<html>body</html>");
        assert_eq!(hit.etag.as_deref(), Some("abc"));
        assert_eq!(hit.kind, Some(PageKind::Album));
    }

    #[test]
    fn cache_respects_max_age() {
        let c = cache(DEFAULT_MAX_BYTES);
        c.put("https://x/album", PageKind::Album, "<html/>", None).unwrap();
        assert!(c.get("https://x/album", 0.0).unwrap().is_none());
        assert!(c.get("https://x/album", 60.0).unwrap().is_some());
        // aged entries miss
        c.put_at("https://x/old", PageKind::Fan, "<html/>", None, now_secs() - 400.0).unwrap();
        assert!(c.get("https://x/old", TTL_FAN).unwrap().is_none());
        assert!(c.get("https://x/old", TTL_MUSIC).unwrap().is_some());
    }

    #[test]
    fn ttl_constants_and_kinds() {
        assert_eq!(TTL_ALBUM, 604_800.0);
        assert_eq!(TTL_MUSIC, 43_200.0);
        assert_eq!(TTL_FAN, 300.0);
        assert_eq!(TTL_DISCOVER, 0.0);
        assert_eq!(PageKind::Stream.ttl(), 600.0);
        assert_eq!(PageKind::Discover.ttl(), 0.0);
        for k in PageKind::ALL {
            assert_eq!(PageKind::parse(k.as_str()), Some(k));
        }
    }

    /// Deterministic pseudo-random text that zstd cannot shrink much.
    fn noisy(seed: u64, len: usize) -> String {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (0..len)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (b'a' + ((x >> 33) % 26) as u8) as char
            })
            .collect()
    }

    #[test]
    fn lru_cap_evicts_the_least_recently_used() {
        let body = noisy(1, 4000);
        let one = zstd::bulk::compress(body.as_bytes(), 3).unwrap().len() as u64;
        let c = cache(one * 3 + one / 2); // room for three
        for n in 0..3 {
            c.put_at(&format!("https://x/{n}"), PageKind::Album, &noisy(n + 1, 4000), None, 1000.0 + n as f64).unwrap();
        }
        // touch 0 so 1 becomes the least recently used
        assert!(c.get_at("https://x/0", 1e12, 2000.0).unwrap().is_some());
        c.put_at("https://x/3", PageKind::Album, &noisy(9, 4000), None, 3000.0).unwrap();
        let st = c.stats().unwrap();
        assert!(st.bytes <= c.max_bytes());
        assert_eq!(st.entries, 3);
        assert!(c.get_at("https://x/1", 1e12, 4000.0).unwrap().is_none(), "LRU victim");
        assert!(c.get_at("https://x/0", 1e12, 4000.0).unwrap().is_some());
        assert!(c.get_at("https://x/3", 1e12, 4000.0).unwrap().is_some());
    }

    #[test]
    fn cache_invalidate() {
        let c = cache(DEFAULT_MAX_BYTES);
        c.put("https://x/a", PageKind::Album, "<html/>", None).unwrap();
        c.put("https://x/b", PageKind::Album, "<html/>", None).unwrap();
        assert_eq!(c.invalidate(Some("https://x/a")).unwrap(), 1);
        assert!(c.get("https://x/a", 60.0).unwrap().is_none());
        assert_eq!(c.invalidate(None).unwrap(), 1);
        assert_eq!(c.stats().unwrap(), CacheStats { entries: 0, bytes: 0 });
        assert!(c.is_empty());
    }

    #[test]
    fn zstd_round_trip_compresses() {
        // Album pages run ~270KB; storing them raw would bloat the cache for no gain.
        let c = cache(DEFAULT_MAX_BYTES);
        let body = format!("<html>{}</html>", "x".repeat(200_000));
        c.put("https://x/big", PageKind::Album, &body, None).unwrap();
        assert_eq!(c.get("https://x/big", 60.0).unwrap().unwrap().body, body);
        let st = c.stats().unwrap();
        assert!(st.bytes < 2_000, "compressed size {}", st.bytes);
        // unicode survives
        c.put("https://x/u", PageKind::Fan, "caf\u{e9} \u{1F3B5}", None).unwrap();
        assert_eq!(c.get("https://x/u", 60.0).unwrap().unwrap().body, "caf\u{e9} \u{1F3B5}");
    }

    #[test]
    fn replace_keeps_byte_accounting_straight() {
        let c = cache(DEFAULT_MAX_BYTES);
        c.put("https://x/a", PageKind::Album, &noisy(1, 5000), None).unwrap();
        c.put("https://x/a", PageKind::Album, "tiny", None).unwrap();
        let st = c.stats().unwrap();
        assert_eq!(st.entries, 1);
        assert_eq!(c.inner.lock().total as u64, st.bytes);
    }

    #[test]
    fn iter_kind_replays_regardless_of_age() {
        let c = cache(DEFAULT_MAX_BYTES);
        c.put_at("https://x/1", PageKind::Album, "one", None, 1.0).unwrap();
        c.put_at("https://x/2", PageKind::Album, "two", None, 2.0).unwrap();
        c.put("https://x/f", PageKind::Fan, "fan", None).unwrap();
        let bodies: Vec<String> = c.iter_kind(PageKind::Album).unwrap().map(|p| p.unwrap().body).collect();
        assert_eq!(bodies, ["one", "two"]);
        assert_eq!(c.iter_kind(PageKind::Music).unwrap().count(), 0);
    }

    #[test]
    fn persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cache.db");
        {
            let c = PageCache::open(&p, DEFAULT_MAX_BYTES).unwrap();
            c.put("https://x/a", PageKind::Music, "persisted", None).unwrap();
        }
        let c = PageCache::open(&p, DEFAULT_MAX_BYTES).unwrap();
        assert_eq!(c.get("https://x/a", 60.0).unwrap().unwrap().body, "persisted");
        assert_eq!(c.stats().unwrap().entries, 1);
    }
}
