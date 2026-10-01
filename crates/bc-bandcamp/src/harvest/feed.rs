//! The follow feed: check every followed source for new releases, in one pass (port of
//! `services/harvest/feed.py`).
//!
//! A follow is a place new music appears -- a saved discover query, an artist or label page, or
//! the library's own shelves of artists and labels -- and the question asked of all of them is
//! the same: "what has shown up since I last looked?". [`FeedSweeper`] walks them all,
//! absorbing each into the inbox under `source_kind = "follow"`, and stops there. Unlike the
//! label sweep it queues nothing: the feed is a triage surface, and downloading is the user's
//! call, per item, from the Feed page.
//!
//! Explicit follows live in `harvest_sources` (one row per saved query or followed page;
//! `enabled` is the follow switch). The library shelves are followed wholesale via two settings
//! toggles rather than a row per entity -- rows would need constant syncing against library
//! churn, while "every label with a page pinned" is exactly the set the label sweep already
//! walks. A library artist or label can still be excluded: a `harvest_sources` row for its page
//! with `enabled = 0` takes that root out of the wholesale walk.
//!
//! As a job: a feed sweep is a job of kind `sweep` with `params.sweep = "feed"` (one item,
//! `source = "feed"`); the in-memory [`FeedSweepStatus`] stays the source of truth for
//! `GET /follows/sweep` and goes out on `feed.sweep`. **Wiring:** the `sweep` kind is shared with
//! the label/favourites sweeps, so there is ONE `KindWorker` for it (in `harvest::sweep`) and it
//! must hand items whose `params.sweep == "feed"` to
//! `ctx.expect::<FeedSweeper>().run_item(ictx)` ([`FeedSweepHandler`] is that as an
//! `ItemHandler`). This module starts no worker of its own for the kind.
//!
//! [`FeedScheduler`] is the standing order: a lightweight loop that starts a sweep every
//! `follows.poll_hours` hours (0 turns it off). The last poll time is stored in settings, so a
//! restart does not reset the clock.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bc_db::rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
use bc_db::Db;
use bc_jobs::{Complete, HandlerOutcome, Interrupt, ItemCtx, ItemHandler, JobHooks, NewItem, NewJob};
use bc_types::bandcamp::{FeedSweepStatus, FollowOut, FollowsOut, TOPIC_FEED_SWEEP};
use chrono::{DateTime, Utc};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;

use crate::error::{HarvestError, Result};
use crate::harvest::inbox::{self, AbsorbOpts};
use crate::service::Ctx;
use crate::sources::{self, DiscoverQuery, EventStream};
use crate::urls;

pub const SOURCE_KIND: &str = "follow";

/// Matches the label sweep: a band's /music grid is its whole catalogue.
pub const PER_BAND_LIMIT: usize = 2000;

/// A discover query never runs dry -- one tag reported 434,991 results live -- so a followed
/// query takes a slice off the top, not a walk to the end.
pub const DISCOVER_LIMIT: usize = 200;

/// Page size of a followed discover query (the legacy default).
const DISCOVER_PAGE: usize = 60;

/// Follow kinds the sweep visits. `search` rows are saved for recall only: autocomplete results
/// are not a release feed.
pub const SWEPT_KINDS: [&str; 3] = ["discover", "artist", "label"];

pub const LIBRARY_ARTISTS_KEY: &str = "follows.library_artists";
pub const LIBRARY_LABELS_KEY: &str = "follows.library_labels";
pub const POLL_HOURS_KEY: &str = "follows.poll_hours";
pub const LAST_POLL_KEY: &str = "follows.last_poll";

pub const DEFAULT_POLL_HOURS: f64 = 12.0;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FollowSettings {
    pub include_library_artists: bool,
    pub include_library_labels: bool,
    pub poll_hours: f64,
}

fn flag(c: &Connection, key: &str, default: bool) -> Result<bool> {
    Ok(match bc_db::settings::get(c, key)? {
        None => default,
        Some(v) => v.trim() == "1",
    })
}

/// The follow toggles and poll interval, with their defaults.
pub fn load_settings(c: &Connection) -> Result<FollowSettings> {
    let poll_hours = bc_db::settings::get(c, POLL_HOURS_KEY)?
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|h| h.is_finite())
        .unwrap_or(DEFAULT_POLL_HOURS);
    Ok(FollowSettings {
        include_library_artists: flag(c, LIBRARY_ARTISTS_KEY, true)?,
        include_library_labels: flag(c, LIBRARY_LABELS_KEY, true)?,
        poll_hours,
    })
}

pub fn store_setting(tx: &Transaction<'_>, key: &str, value: &str) -> Result<()> {
    Ok(bc_db::settings::set(tx, key, value)?)
}

// ---------------------------------------------------------------------------
// Follow rows
// ---------------------------------------------------------------------------

/// A `harvest_sources` row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SourceRow {
    pub id: i64,
    pub kind: String,
    pub identifier: String,
    pub label: Option<String>,
    pub url: Option<String>,
    pub enabled: bool,
    pub config: String,
    pub last_run_at: Option<String>,
    pub last_error: Option<String>,
    pub items_seen: i64,
    pub items_new: i64,
    pub created_at: String,
}

pub const SOURCE_COLS: &str = "id, kind, identifier, label, url, enabled, config, last_run_at, last_error, items_seen, items_new, created_at";

impl SourceRow {
    pub fn from_row(r: &bc_db::rusqlite::Row<'_>) -> bc_db::rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            kind: r.get(1)?,
            identifier: r.get(2)?,
            label: r.get(3)?,
            url: r.get(4)?,
            enabled: r.get::<_, i64>(5)? != 0,
            config: r.get(6)?,
            last_run_at: r.get(7)?,
            last_error: r.get(8)?,
            items_seen: r.get(9)?,
            items_new: r.get(10)?,
            created_at: r.get(11)?,
        })
    }

    pub fn parse_config(&self) -> serde_json::Map<String, Value> {
        parse_config(&self.config)
    }

    /// What the progress line and the follows list call this source.
    pub fn display_label(&self) -> String {
        self.label
            .clone()
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| match self.url.as_deref().filter(|u| !u.is_empty()) {
                Some(u) => urls::display_name(u),
                None => self.kind.clone(),
            })
    }

    pub fn to_out(&self) -> FollowOut {
        let config = self.parse_config();
        let obj = |k: &str| -> BTreeMap<String, Value> {
            match config.get(k) {
                Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                _ => BTreeMap::new(),
            }
        };
        FollowOut {
            id: self.id,
            kind: self.kind.clone(),
            label: self.display_label(),
            url: self.url.clone(),
            enabled: self.enabled,
            explore_params: obj("explore_params"),
            api_params: obj("api_params"),
            limit: config.get("limit").and_then(|v| v.as_i64()),
            last_run_at: self.last_run_at.as_deref().map(bc_jobs::time::to_iso),
            last_error: self.last_error.clone(),
            items_seen: self.items_seen,
            items_new: self.items_new,
            created_at: Some(bc_jobs::time::to_iso(&self.created_at)),
        }
    }
}

pub fn get_source(c: &Connection, id: i64) -> Result<Option<SourceRow>> {
    Ok(c.query_row(&format!("SELECT {SOURCE_COLS} FROM harvest_sources WHERE id = ?1"), [id], SourceRow::from_row).optional()?)
}

/// Every follow, newest first (`GET /follows`).
pub fn list_sources(c: &Connection) -> Result<Vec<SourceRow>> {
    let mut st = c.prepare(&format!(
        "SELECT {SOURCE_COLS} FROM harvest_sources WHERE kind IN ('discover','search','artist','label') \
         ORDER BY created_at DESC, id DESC"
    ))?;
    Ok(st.query_map([], SourceRow::from_row)?.collect::<bc_db::rusqlite::Result<Vec<_>>>()?)
}

pub fn follows_out(c: &Connection) -> Result<FollowsOut> {
    let prefs = load_settings(c)?;
    Ok(FollowsOut {
        sources: list_sources(c)?.iter().map(SourceRow::to_out).collect(),
        include_library_artists: prefs.include_library_artists,
        include_library_labels: prefs.include_library_labels,
        poll_hours: prefs.poll_hours,
    })
}

pub fn parse_config(raw: &str) -> serde_json::Map<String, Value> {
    match serde_json::from_str::<Value>(if raw.is_empty() { "{}" } else { raw }) {
        Ok(Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    }
}

/// Python truthiness of the "empty" values a saved query drops: `None`, `""`, `[]`, `0`, `False`.
fn is_blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Object(_) => false,
    }
}

/// What a saved discover query may pass through to the API walk (`slice` is renamed on the way
/// in; anything else a client sends is dropped).
pub fn discover_query(config: &serde_json::Map<String, Value>) -> DiscoverQuery {
    let params = match config.get("api_params") {
        Some(Value::Object(m)) => m.clone(),
        _ => serde_json::Map::new(),
    };
    let mut q = DiscoverQuery::new();
    let present = |k: &str| params.get(k).filter(|v| !is_blank(v));
    if let Some(Value::Array(tags)) = present("tags") {
        q.tags = tags.iter().filter_map(|t| t.as_str().map(str::to_string)).collect();
    }
    if let Some(g) = present("genre").and_then(|v| v.as_str()) {
        q.genre = Some(g.to_string());
    }
    if let Some(s) = params.get("slice").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        q.slice = s.to_string();
    }
    if let Some(n) = present("category_id").and_then(|v| v.as_i64()) {
        q.category_id = n;
    }
    if let Some(n) = present("geoname_id").and_then(|v| v.as_i64()) {
        q.geoname_id = n;
    }
    if let Some(n) = present("time_facet_id").and_then(|v| v.as_i64()) {
        q.time_facet_id = Some(n);
    }
    q
}

/// `json.dumps(v, sort_keys=True, separators=(",", ":"))` (with Python's ASCII escaping).
fn canonical_json(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                ascii_string(k, out);
                out.push(':');
                canonical_json(&m[*k], out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(e, out);
            }
            out.push(']');
        }
        Value::String(s) => ascii_string(s, out),
        other => out.push_str(&other.to_string()),
    }
}

fn ascii_string(s: &str, out: &mut String) {
    let quoted = Value::String(s.to_string()).to_string();
    for ch in quoted.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else {
            let mut buf = [0u16; 2];
            for u in ch.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{:04x}", u));
            }
        }
    }
}

/// What makes two follows the same follow. A page follow is its root URL. A query follow is its
/// API parameters, serialised with sorted keys so `{a,b}` and `{b,a}` collide on the
/// `(kind, identifier)` unique constraint instead of saving twice.
pub fn canonical_identifier(kind: &str, url: Option<&str>, api_params: &BTreeMap<String, Value>) -> String {
    if kind == "artist" || kind == "label" {
        return urls::artist_root(url.unwrap_or(""));
    }
    let cleaned: serde_json::Map<String, Value> =
        api_params.iter().filter(|(_, v)| !is_blank(v)).map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut out = String::new();
    canonical_json(&Value::Object(cleaned), &mut out);
    out
}

// ---------------------------------------------------------------------------
// Sources of releases
// ---------------------------------------------------------------------------

/// What the sweep reads from Bandcamp (the live impl wraps the shared client; the Python tests
/// monkeypatched `sources.harvest_artist` / `harvest_discover`).
pub trait FeedSource: Send + Sync {
    fn harvest_artist(&self, url: &str, limit: usize) -> EventStream;
    fn harvest_discover(&self, query: DiscoverQuery, limit: usize) -> EventStream;
}

pub struct LiveFeedSource {
    ctx: Arc<Ctx>,
}

impl FeedSource for LiveFeedSource {
    fn harvest_artist(&self, url: &str, limit: usize) -> EventStream {
        sources::harvest_artist(self.ctx.client.clone(), url.to_string(), "shallow".into(), Some(limit))
    }
    fn harvest_discover(&self, query: DiscoverQuery, limit: usize) -> EventStream {
        sources::harvest_discover(self.ctx.client.clone(), query, limit, DISCOVER_PAGE)
    }
}

/// Free URLs first, same as the label sweep: every page pinned here is a shelf entity the walk
/// covers instead of skips. Wired by the service assembly to
/// `harvest::labels::backfill_label_urls` + `harvest::artists::backfill_artist_urls`.
pub type Backfill = Arc<dyn Fn(&Connection) + Send + Sync>;

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Target {
    /// What the progress line and the absorbed rows call this source.
    pub name: String,
    /// `discover` walks the query; anything else walks a band page.
    pub kind: String,
    pub url: Option<String>,
    pub discover: DiscoverQuery,
    pub limit: usize,
    /// The follow row to write stats back to; library shelves have none.
    pub source_id: Option<i64>,
    /// Set for label pages, where the grid never names its own imprint.
    pub label_name: Option<String>,
}

/// Everything in scope, and how many library entities lacked a page.
///
/// Explicit follows first -- they are what the user pointed at -- then the library shelves,
/// deduped by page root so a followed label that is also on the shelf is fetched once.
pub fn targets(c: &Connection, source_ids: Option<&[i64]>) -> Result<(Vec<Target>, i64)> {
    let mut sql = format!(
        "SELECT {SOURCE_COLS} FROM harvest_sources WHERE kind IN ('discover','artist','label') AND enabled = 1"
    );
    let mut args: Vec<i64> = Vec::new();
    if let Some(ids) = source_ids {
        let ph = vec!["?"; ids.len()].join(",");
        sql.push_str(&format!(" AND id IN ({ph})"));
        args.extend_from_slice(ids);
    }
    sql.push_str(" ORDER BY label, id");
    let rows = {
        let mut st = c.prepare(&sql)?;
        st.query_map(params_from_iter(args.iter()), SourceRow::from_row)?.collect::<bc_db::rusqlite::Result<Vec<_>>>()?
    };

    let mut out: Vec<Target> = Vec::new();
    let mut seen_roots: HashSet<String> = HashSet::new();
    for row in &rows {
        let name = row.display_label();
        let config = row.parse_config();
        if row.kind == "discover" {
            let limit = config.get("limit").and_then(|v| v.as_i64()).filter(|l| *l > 0).map(|l| l as usize).unwrap_or(DISCOVER_LIMIT);
            out.push(Target {
                name,
                kind: "discover".into(),
                url: None,
                discover: discover_query(&config),
                limit,
                source_id: Some(row.id),
                label_name: None,
            });
            continue;
        }
        let Some(url) = row.url.as_deref().filter(|u| !u.is_empty()) else { continue };
        let root = urls::artist_root(url);
        seen_roots.insert(root.clone());
        out.push(Target {
            label_name: (row.kind == "label").then(|| name.clone()),
            name,
            kind: row.kind.clone(),
            url: Some(root),
            discover: DiscoverQuery::new(),
            limit: PER_BAND_LIMIT,
            source_id: Some(row.id),
        });
    }

    if source_ids.is_some() {
        return Ok((out, 0));
    }

    // A disabled page-follow row is a deliberate opt-out: it takes that root out of the
    // wholesale library walk below.
    let excluded: HashSet<String> = {
        let mut st = c.prepare(
            "SELECT url FROM harvest_sources WHERE kind IN ('artist','label') AND enabled = 0 AND url IS NOT NULL",
        )?;
        st.query_map([], |r| r.get::<_, String>(0))?.collect::<bc_db::rusqlite::Result<Vec<_>>>()?.iter().map(|u| urls::artist_root(u)).collect()
    };

    let prefs = load_settings(c)?;
    let mut no_url = 0;
    for (enabled, table, kind) in [(prefs.include_library_labels, "labels", "label"), (prefs.include_library_artists, "artists", "artist")] {
        if !enabled {
            continue;
        }
        let mut st = c.prepare(&format!("SELECT name, bandcamp_url FROM {table} ORDER BY name"))?;
        let rows = st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?
            .collect::<bc_db::rusqlite::Result<Vec<_>>>()?;
        for (name, url) in rows {
            let Some(url) = url.filter(|u| !u.is_empty()) else {
                no_url += 1;
                continue;
            };
            let root = urls::artist_root(&url);
            if seen_roots.contains(&root) || excluded.contains(&root) {
                continue;
            }
            seen_roots.insert(root.clone());
            out.push(Target {
                label_name: (kind == "label").then(|| name.clone()),
                name,
                kind: kind.into(),
                url: Some(root),
                discover: DiscoverQuery::new(),
                limit: PER_BAND_LIMIT,
                source_id: None,
            });
        }
    }
    Ok((out, no_url))
}

// ---------------------------------------------------------------------------
// The sweeper
// ---------------------------------------------------------------------------

/// A feed sweep is in flight; a second would re-fetch every page.
#[derive(Debug, thiserror::Error)]
#[error("a feed sweep is already running")]
pub struct AlreadyRunning;

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    AlreadyRunning(#[from] AlreadyRunning),
    #[error(transparent)]
    Other(#[from] HarvestError),
}

fn idle_status() -> FeedSweepStatus {
    FeedSweepStatus { phase: "idle".into(), scope: "all".into(), ..Default::default() }
}

fn iso_now() -> String {
    bc_jobs::time::to_iso(&bc_jobs::time::now())
}

pub(crate) async fn dbw<T: Send + 'static>(db: &Db, f: impl FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static) -> Result<T> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || db.write_with::<T, HarvestError>(f))
        .await
        .map_err(|e| HarvestError::other(e.to_string()))?
}

pub(crate) async fn dbr<T: Send + 'static>(db: &Db, f: impl FnOnce(&Connection) -> Result<T> + Send + 'static) -> Result<T> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || db.read_with::<T, HarvestError>(f))
        .await
        .map_err(|e| HarvestError::other(e.to_string()))?
}

struct SweepInner {
    state: FeedSweepStatus,
    /// The job carrying the sweep in flight (or waiting).
    job_id: Option<String>,
    /// Whether the worker has picked that job up.
    claimed: bool,
}

/// Runs at most one feed sweep at a time and reports where it has got to.
pub struct FeedSweeper {
    ctx: Arc<Ctx>,
    inner: Mutex<SweepInner>,
    source: RwLock<Arc<dyn FeedSource>>,
    backfill: RwLock<Option<Backfill>>,
}

impl FeedSweeper {
    pub fn new(ctx: Arc<Ctx>) -> Arc<Self> {
        let src: Arc<dyn FeedSource> = Arc::new(LiveFeedSource { ctx: ctx.clone() });
        Arc::new(Self {
            ctx,
            inner: Mutex::new(SweepInner { state: idle_status(), job_id: None, claimed: false }),
            source: RwLock::new(src),
            backfill: RwLock::new(None),
        })
    }

    /// Replace where the sweep reads Bandcamp from (tests).
    pub fn set_source(&self, s: Arc<dyn FeedSource>) {
        *self.source.write() = s;
    }

    /// Install the "pin free page URLs first" step (see [`Backfill`]).
    pub fn set_backfill(&self, b: Backfill) {
        *self.backfill.write() = Some(b);
    }

    pub fn state(&self) -> FeedSweepStatus {
        let mut s = self.inner.lock().state.clone();
        s.running = s.phase == "harvesting";
        s.errors.truncate(10);
        s
    }

    pub fn running(&self) -> bool {
        self.inner.lock().state.phase == "harvesting"
    }

    fn publish(&self, f: impl FnOnce(&mut FeedSweepStatus)) {
        {
            let mut g = self.inner.lock();
            f(&mut g.state);
        }
        self.ctx.bus.publish(TOPIC_FEED_SWEEP, &self.state());
    }

    /// Stop the walk, keeping whatever it has already absorbed.
    pub async fn stop(&self) {
        let (job, claimed) = {
            let g = self.inner.lock();
            (g.job_id.clone(), g.claimed)
        };
        let Some(job) = job else { return };
        if !self.running() {
            return;
        }
        if let Err(e) = self.ctx.jobs.stop_job(&job).await {
            tracing::warn!("could not stop feed sweep job {job}: {e:?}");
        }
        if !claimed {
            self.finish_unclaimed(&job);
        }
    }

    /// A job cancelled before any worker picked it up never runs its handler: close the state.
    fn finish_unclaimed(&self, job_id: &str) {
        let waiting = {
            let g = self.inner.lock();
            g.job_id.as_deref() == Some(job_id) && !g.claimed && g.state.phase == "harvesting"
        };
        if waiting {
            self.publish(|s| {
                s.phase = "done".into();
                s.current = None;
                s.error = Some("Stopped".into());
                s.finished_at = Some(iso_now());
            });
        }
    }

    /// Kick off a sweep and return immediately.
    ///
    /// `source_ids` narrows the walk to those follow rows -- "check this one, now" -- and skips
    /// the library shelves; `None` sweeps everything. An empty slice is the same as `None`, so
    /// a stray press with the selection cleared does what the button says.
    pub async fn start(&self, source_ids: Option<&[i64]>) -> std::result::Result<FeedSweepStatus, StartError> {
        let ids: Option<Vec<i64>> = source_ids.filter(|s| !s.is_empty()).map(|s| {
            let mut v = s.to_vec();
            v.sort_unstable();
            v.dedup();
            v
        });
        let job_id = uuid::Uuid::new_v4().to_string();
        {
            let mut g = self.inner.lock();
            if g.state.phase == "harvesting" {
                return Err(AlreadyRunning.into());
            }
            g.state = FeedSweepStatus {
                phase: "harvesting".into(),
                scope: if ids.is_none() { "all" } else { "selection" }.into(),
                started_at: Some(iso_now()),
                ..Default::default()
            };
            g.job_id = Some(job_id.clone());
            g.claimed = false;
        }
        self.ctx.bus.publish(TOPIC_FEED_SWEEP, &self.state());

        let nj = NewJob::new(
            bc_types::jobs::KIND_SWEEP,
            vec![NewItem { source: Some("feed".into()), ..Default::default() }],
        )
        .id(job_id)
        .label(if ids.is_none() { "Check follows" } else { "Check selected follows" })
        .params(serde_json::json!({"sweep": "feed", "source_ids": ids}));
        let store = self.ctx.jobs.store().clone();
        if let Err(e) = store.run(move |s| s.create_job(nj)).await {
            self.publish(|s| {
                s.phase = "failed".into();
                s.error = Some(e.to_string());
                s.finished_at = Some(iso_now());
            });
            return Err(HarvestError::from(e).into());
        }
        Ok(self.state())
    }

    fn record_run(c: &Connection, source_id: i64, seen: Option<(i64, i64)>, error: Option<&str>) -> Result<()> {
        c.execute(
            "UPDATE harvest_sources SET last_run_at = ?2, last_error = ?3, \
                    items_seen = items_seen + ?4, items_new = items_new + ?5 WHERE id = ?1",
            params![source_id, bc_jobs::time::now(), error, seen.map(|s| s.0).unwrap_or(0), seen.map(|s| s.1).unwrap_or(0)],
        )?;
        Ok(())
    }

    /// The body of the `sweep` job with `params.sweep == "feed"`.
    pub async fn run_item(&self, ictx: ItemCtx) -> HandlerOutcome {
        let params = ictx.job.params_json();
        let ids: Option<Vec<i64>> = params
            .get("source_ids")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_i64()).collect::<Vec<_>>())
            .filter(|v| !v.is_empty());
        {
            // A job re-claimed after a restart (or created by someone else) starts from a
            // fresh in-memory state.
            let mut g = self.inner.lock();
            g.job_id = Some(ictx.job.id.clone());
            g.claimed = true;
            if g.state.phase != "harvesting" {
                g.state = FeedSweepStatus {
                    phase: "harvesting".into(),
                    scope: if ids.is_none() { "all" } else { "selection" }.into(),
                    started_at: Some(iso_now()),
                    ..Default::default()
                };
            }
        }
        self.ctx.bus.publish(TOPIC_FEED_SWEEP, &self.state());

        match self.run_inner(&ictx, ids).await {
            Ok(End::Done(msg)) => HandlerOutcome::Done(Complete::msg(msg)),
            Ok(End::Interrupted) => HandlerOutcome::Interrupted,
            Err(e) => {
                tracing::warn!("feed sweep failed: {e}");
                let msg: String = e.to_string().chars().take(500).collect();
                let m = msg.clone();
                self.publish(move |s| {
                    s.phase = "failed".into();
                    s.error = Some(m);
                    s.finished_at = Some(iso_now());
                });
                HandlerOutcome::Failed { error: msg, class: "sweep".into(), retryable: false }
            }
        }
    }

    async fn run_inner(&self, ictx: &ItemCtx, ids: Option<Vec<i64>>) -> Result<End> {
        let db = self.ctx.db.clone();
        let backfill = if ids.is_none() { self.backfill.read().clone() } else { None };
        let wanted = ids.clone();
        let (targets, no_url) = dbw(&db, move |tx| {
            if let Some(b) = backfill {
                b(tx);
            }
            targets(tx, wanted.as_deref())
        })
        .await?;
        let total = targets.len() as i64;
        self.publish(move |s| {
            s.total = Some(total);
            s.no_url = no_url;
        });
        let src = self.source.read().clone();

        let mut stopped = false;
        for (i, target) in targets.iter().enumerate() {
            let done = i as i64 + 1;
            let name = target.name.clone();
            self.publish(|s| s.current = Some(name));
            let stream = if target.kind == "discover" {
                src.harvest_discover(target.discover.clone(), target.limit)
            } else {
                src.harvest_artist(target.url.as_deref().unwrap_or(""), target.limit)
            };
            let mut opts = AbsorbOpts::new(SOURCE_KIND, target.name.clone());
            opts.label_name = target.label_name.clone();
            let absorbed = inbox::absorb(&db, stream, &opts, None, &ictx.cancel).await;
            let counts = absorbed.counts;

            if absorbed.cancelled {
                if let Some(sid) = target.source_id {
                    let (seen, new) = (counts.seen, counts.new);
                    let _ = dbw(&db, move |tx| Self::record_run(tx, sid, Some((seen, new)), None)).await;
                }
                self.publish(|s| {
                    s.seen += counts.seen;
                    s.new += counts.new;
                    s.in_library += counts.in_library;
                });
                stopped = true;
                break;
            }
            // One unreachable page must not kill the whole sweep; the source just stays
            // unchecked this run.
            let error: Option<String> = absorbed.error.map(|e| {
                tracing::info!("feed sweep: {} failed: {e}", target.name);
                e.to_string().chars().take(300).collect()
            });
            if let Some(sid) = target.source_id {
                let (seen, new) = (counts.seen, counts.new);
                let has_counts = error.is_none();
                let err = error.clone();
                let _ = dbw(&db, move |tx| Self::record_run(tx, sid, has_counts.then_some((seen, new)), err.as_deref())).await;
            }
            match error {
                Some(e) => {
                    let line: String = format!("{}: {e}", target.name).chars().take(300).collect();
                    self.publish(move |s| {
                        s.done = done;
                        s.errors.push(line);
                    });
                }
                None => {
                    self.publish(|s| {
                        s.done = done;
                        s.seen += counts.seen;
                        s.new += counts.new;
                        s.in_library += counts.in_library;
                        s.errors.extend(counts.errors.iter().take(3).cloned());
                    });
                }
            }
        }

        if stopped {
            let user_stop = {
                let jid = ictx.job.id.clone();
                ictx.store.run(move |s| s.is_cancel_requested(&jid)).await.unwrap_or(true)
            };
            if !user_stop {
                self.publish(|s| {
                    s.phase = "failed".into();
                    s.error = Some("Cancelled".into());
                    s.finished_at = Some(iso_now());
                });
                return Ok(End::Interrupted);
            }
            // Swallowed on purpose so the run still reports what it found.
            self.publish(|s| {
                s.phase = "done".into();
                s.current = None;
                s.error = Some("Stopped".into());
                s.finished_at = Some(iso_now());
            });
            return Ok(End::Done("Stopped".into()));
        }
        self.publish(|s| {
            s.phase = "done".into();
            s.current = None;
            s.finished_at = Some(iso_now());
        });
        let st = self.state();
        Ok(End::Done(format!("{} follows, {} seen, {} new", total, st.seen, st.new)))
    }
}

enum End {
    Done(String),
    Interrupted,
}

/// `ItemHandler` for feed items, for a `sweep` worker that dispatches on `params.sweep`.
pub struct FeedSweepHandler(pub Arc<FeedSweeper>);

#[async_trait]
impl ItemHandler for FeedSweepHandler {
    async fn run(&self, ctx: ItemCtx) -> HandlerOutcome {
        self.0.run_item(ctx).await
    }
}

/// Closes the status when the generic job routes cancel a feed sweep nobody has claimed yet.
struct FeedHooks(Arc<FeedSweeper>);

#[async_trait]
impl JobHooks for FeedHooks {
    async fn interrupt_job(&self, job_id: &str, reason: Interrupt) {
        if reason == Interrupt::Cancel {
            self.0.finish_unclaimed(job_id);
        }
    }
}

/// Is a label/favourites sweep job open? (`POST /follows/sweep` refuses while one runs.)
pub fn label_sweep_running(c: &Connection) -> Result<bool> {
    Ok(c.query_row(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE kind = 'sweep' AND status IN ('queued','running') \
                AND json_extract(params, '$.sweep') IN ('labels','favorites'))",
        [],
        |r| r.get::<_, i64>(0),
    )? != 0)
}

// ---------------------------------------------------------------------------
// The standing order
// ---------------------------------------------------------------------------

/// Whether a poll is due: polling is on (`poll_hours > 0`) and either nothing was ever polled,
/// the stored time is unreadable, or the interval has elapsed.
pub fn poll_due(poll_hours: f64, last_poll: Option<&str>, now: DateTime<Utc>) -> bool {
    if poll_hours <= 0.0 {
        return false;
    }
    let Some(raw) = last_poll else { return true };
    let Some(last) = bc_jobs::time::parse_db(raw) else { return true };
    (now - last).num_milliseconds() as f64 / 1000.0 >= poll_hours * 3600.0
}

/// Starts a feed sweep every `follows.poll_hours` hours.
///
/// The interval is re-read every tick, so changing it (or setting it to 0 to turn polling off)
/// applies without a restart. The last poll time is stored in settings, so a restart resumes
/// the clock rather than resetting it -- a machine rebooted daily would otherwise never poll a
/// 25-hour interval. The first check comes a randomised short delay after boot, not at boot.
pub struct FeedScheduler {
    ctx: Arc<Ctx>,
    sweeper: Arc<FeedSweeper>,
    clock: RwLock<Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl FeedScheduler {
    pub const TICK: Duration = Duration::from_secs(60);

    pub fn new(ctx: Arc<Ctx>, sweeper: Arc<FeedSweeper>) -> Arc<Self> {
        Arc::new(Self { ctx, sweeper, clock: RwLock::new(Arc::new(Utc::now)), task: Mutex::new(None) })
    }

    /// Inject the clock (tests).
    pub fn set_clock(&self, clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>) {
        *self.clock.write() = clock;
    }

    /// The randomised delay before the first check after boot.
    pub fn initial_delay() -> Duration {
        Duration::from_secs(rand::random_range(20..=90u64))
    }

    pub fn start(self: &Arc<Self>, initial_delay: Duration, tick: Duration) {
        let mut slot = self.task.lock();
        if slot.is_some() {
            return;
        }
        let me = self.clone();
        *slot = Some(tokio::spawn(async move {
            tokio::time::sleep(initial_delay).await;
            loop {
                let now = (me.clock.read().clone())();
                if let Err(e) = me.tick(now).await {
                    // The loop is the feature; one bad tick must not end it.
                    tracing::warn!("feed scheduler tick failed: {e}");
                }
                tokio::time::sleep(tick).await;
            }
        }));
    }

    pub fn stop(&self) {
        if let Some(t) = self.task.lock().take() {
            t.abort();
        }
    }

    /// One check at `now`. Returns whether a sweep was started.
    pub async fn tick(&self, now: DateTime<Utc>) -> Result<bool> {
        if self.sweeper.running() {
            return Ok(false);
        }
        let due = dbr(&self.ctx.db, move |c| {
            let hours = load_settings(c)?.poll_hours;
            Ok(poll_due(hours, bc_db::settings::get(c, LAST_POLL_KEY)?.as_deref(), now))
        })
        .await?;
        if !due {
            return Ok(false);
        }
        let stamp = now.naive_utc().format("%Y-%m-%dT%H:%M:%S%.6f").to_string();
        dbw(&self.ctx.db, move |tx| store_setting(tx, LAST_POLL_KEY, &stamp)).await?;
        match self.sweeper.start(None).await {
            Ok(_) => Ok(true),
            Err(StartError::AlreadyRunning(_)) => Ok(false),
            Err(StartError::Other(e)) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Service wiring
// ---------------------------------------------------------------------------

pub fn init(ctx: &Arc<Ctx>) {
    let sweeper = FeedSweeper::new(ctx.clone());
    ctx.jobs.add_hooks(Arc::new(FeedHooks(sweeper.clone())));
    ctx.put(sweeper.clone());
    ctx.put(FeedScheduler::new(ctx.clone(), sweeper));
}

/// Start the scheduler (first check after a randomised short delay, then every minute).
pub async fn start(ctx: &Arc<Ctx>) {
    ctx.expect::<FeedScheduler>().start(FeedScheduler::initial_delay(), FeedScheduler::TICK);
}
