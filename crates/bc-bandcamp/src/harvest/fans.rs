//! Wishlists to listen to: mine, and other people's (port of `services/harvest/fans.py`).
//!
//! A `fans` row is a Bandcamp account whose wishlist is followed here. The one with `is_self`
//! is the saved wishlist that used to be a settings key -- walking it still recognises what the
//! library already has and queues the rest straight into the library root, which is the
//! backfill the Settings page has always offered. Every other fan is someone else's list: a
//! thing to stream through album by album, shuffle across, pick records out of, or download
//! whole -- onto that fan's own shelf, apart from my library, unless asked otherwise.
//!
//! Membership lives in `fan_items`, not on the inbox row: a record that two people both wish
//! for is one `harvest_items` row, and which lists it is on cannot be a column of it. The walk
//! records each item's position as it goes, because "play the wishlist in order" means the
//! order Bandcamp shows it in -- newest wished first -- and nothing else remembers that.
//!
//! The walker is a FIFO, one walk in flight at a time: a five-figure wishlist is ~130 polite
//! API pages, minutes of work, and adding another fan meanwhile should wait its turn rather
//! than fail. Every requested walk is a job (kind `walk`, one item, params
//! `{fan_id, tabs, queue_new}`) claimed by a [`bc_jobs::KindWorker`] with concurrency 1; the
//! in-memory [`WalkState`] stays the source of truth for the GET routes and goes out on
//! `fans.walk`. Stop cancels the walk now; a stopped self walk still queues what it has seen,
//! in the same spirit as the label sweep.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use bc_db::rusqlite::types::Value as SqlValue;
use bc_db::rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
use bc_db::{Db, DbError};
use bc_jobs::{Complete, HandlerOutcome, ItemCtx, ItemHandler, KindWorker, NewItem, NewJob, WorkerSpec};
use bc_types::bandcamp::{FanNextItem, FanNextOut, FanOut, FanTabOut, TOPIC_FANS_WALK, WalkState};
use futures::StreamExt;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::error::{HarvestError, Result};
use crate::extract::HarvestedRelease;
use crate::harvest::inbox::{self, AbsorbCounts, AbsorbOpts, BatchHook, QueueOpts, QueueOutcome};
use crate::service::Ctx;
use crate::sources::{self, EventStream, SourceProbe};
use crate::urls;

/// The settings keys the single saved wishlist used to live under. Read once to create the
/// self fan; left in place as cheap insurance for a rollback.
pub const LEGACY_URL_KEY: &str = "bandcamp.wishlist_url";
pub const LEGACY_LAST_RUN_KEY: &str = "bandcamp.wishlist_last_run";

/// The two lists a fan page shows. Hidden items need the cookie and are not walked.
pub const TABS: [&str; 2] = ["wishlist", "collection"];

/// Bandcamp pages 100 items per call, so this is ~250 requests at most, and the walk always
/// restarts from the newest item -- an unfinished walk never leaves the tail unreachable.
pub const WALK_LIMIT: usize = 25_000;

/// Page size of a walk (Bandcamp's own).
const WALK_PAGE: usize = 100;

/// Knuth's multiplicative hash: an odd multiplier mod 2^32 is a bijection on ids, so a seed
/// yields a fixed, repeat-free order over the whole list. Public because it is the app's one
/// definition of "shuffled by seed".
pub const SHUFFLE_MULT: i64 = 2_654_435_761;
pub const SHUFFLE_MOD: i64 = 4_294_967_296;

/// Order keys for memberships the migration backfilled without a position: past every walked
/// item, in id order, until the next walk places them.
pub const UNPLACED: i64 = 1_000_000_000;

/// The shuffle position of an item id under `seed`.
pub fn shuffle_key(id: i64, seed: u32) -> i64 {
    ((id + i64::from(seed)) * SHUFFLE_MULT) % SHUFFLE_MOD
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FanOrder {
    /// The list's own order (newest wished first).
    #[default]
    Seq,
    /// A fixed, seeded, repeat-free order.
    Shuffle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FanTab {
    Wishlist,
    Collection,
    /// Both lists as one.
    #[default]
    All,
}

impl FanTab {
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            FanTab::Wishlist => Some("wishlist"),
            FanTab::Collection => Some("collection"),
            FanTab::All => None,
        }
    }
}

/// What to say about lists the walk could not read. A refused list is a fact about the
/// account -- Bandcamp shows a private wishlist to nobody -- so it reads as a note beside a
/// walk that otherwise finished, not as the walk's error.
pub fn notes(refused: &[String], failures: &[String]) -> Vec<String> {
    refused
        .iter()
        .map(|tab| format!("Bandcamp does not show this fan's {tab}."))
        .chain(failures.iter().cloned())
        .collect()
}

/// The tabs a request names, in walk order; nothing named means both.
pub fn coerce_tabs(raw: Option<&[String]>) -> Vec<String> {
    let all = || TABS.iter().map(|t| t.to_string()).collect::<Vec<_>>();
    let Some(raw) = raw.filter(|r| !r.is_empty()) else { return all() };
    let wanted: Vec<String> = TABS.iter().filter(|t| raw.iter().any(|r| r == *t)).map(|t| t.to_string()).collect();
    if wanted.is_empty() { all() } else { wanted }
}

// ---------------------------------------------------------------------------
// Fan rows
// ---------------------------------------------------------------------------

/// Turn whatever was pasted into a canonical fan URL. Accepts a profile or wishlist link, a
/// bare `bandcamp.com/name`, or just the username.
pub fn coerce_fan_url(raw: &str) -> Result<String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(HarvestError::other("Paste a Bandcamp profile or wishlist link, or a username."));
    }
    let text = if !text.contains("://") && !text.contains('/') && !text.contains('.') && !text.contains(' ') {
        format!("https://bandcamp.com/{text}")
    } else {
        text.to_string()
    };
    let canonical = urls::normalise(&urls::coerce(&text));
    if urls::classify(&canonical) != urls::UrlKind::Fan {
        return Err(HarvestError::other(
            "That is not a Bandcamp fan page. Use a profile or wishlist link, for example https://bandcamp.com/somebody/wishlist",
        ));
    }
    Ok(urls::fan_base_url(&canonical))
}

pub fn username_from_url(url: &str) -> String {
    let path = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/').map(|(_, p)| p).unwrap_or(""),
        None => url,
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    path.split('/').find(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(|| url.to_string())
}

/// The folder a fan's downloads go under: `fan-<username>`.
pub fn shelf_name(fan: &FanRow) -> String {
    let n = urls::display_name(&fan.url);
    if n.is_empty() { format!("fan-{}", fan.username) } else { n }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FanRow {
    pub id: i64,
    pub bc_fan_id: Option<i64>,
    pub username: String,
    pub url: String,
    pub display_name: Option<String>,
    pub is_self: bool,
    pub wishlist_count: Option<i64>,
    pub collection_count: Option<i64>,
    pub last_walk_at: Option<String>,
    pub last_walk: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
}

const FAN_COLS: &str = "id, bc_fan_id, username, url, display_name, is_self, wishlist_count, collection_count, \
                        last_walk_at, last_walk, last_error, created_at";

impl FanRow {
    fn from_row(r: &bc_db::rusqlite::Row<'_>) -> bc_db::rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            bc_fan_id: r.get(1)?,
            username: r.get(2)?,
            url: r.get(3)?,
            display_name: r.get(4)?,
            is_self: r.get::<_, i64>(5)? != 0,
            wishlist_count: r.get(6)?,
            collection_count: r.get(7)?,
            last_walk_at: r.get(8)?,
            last_walk: r.get(9)?,
            last_error: r.get(10)?,
            created_at: r.get(11)?,
        })
    }
}

pub fn get_fan(c: &Connection, fan_id: i64) -> Result<Option<FanRow>> {
    Ok(c.query_row(&format!("SELECT {FAN_COLS} FROM fans WHERE id = ?1"), [fan_id], FanRow::from_row).optional()?)
}

pub fn fan_by_username(c: &Connection, username: &str) -> Result<Option<FanRow>> {
    Ok(c.query_row(
        &format!("SELECT {FAN_COLS} FROM fans WHERE lower(username) = lower(?1)"),
        [username],
        FanRow::from_row,
    )
    .optional()?)
}

pub fn fan_by_bc_id(c: &Connection, bc_fan_id: i64) -> Result<Option<FanRow>> {
    Ok(c.query_row(&format!("SELECT {FAN_COLS} FROM fans WHERE bc_fan_id = ?1"), [bc_fan_id], FanRow::from_row).optional()?)
}

pub fn self_fan(c: &Connection) -> Result<Option<FanRow>> {
    Ok(c.query_row(&format!("SELECT {FAN_COLS} FROM fans WHERE is_self = 1 ORDER BY id LIMIT 1"), [], FanRow::from_row).optional()?)
}

pub fn list_fans(c: &Connection) -> Result<Vec<FanRow>> {
    let mut st = c.prepare(&format!("SELECT {FAN_COLS} FROM fans ORDER BY is_self DESC, created_at, id"))?;
    let rows = st.query_map([], FanRow::from_row)?.collect::<bc_db::rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Add a fan by URL, returning the existing row if it is already followed.
pub fn create_fan(tx: &Transaction<'_>, url: &str, is_self: bool, probe: Option<&SourceProbe>) -> Result<FanRow> {
    let canonical = coerce_fan_url(url)?;
    let username = username_from_url(&canonical);
    if let Some(existing) = fan_by_username(tx, &username)? {
        if let Some(p) = probe {
            record_probe(tx, existing.id, p)?;
        }
        return Ok(get_fan(tx, existing.id)?.unwrap_or(existing));
    }
    tx.execute(
        "INSERT INTO fans (username, url, display_name, is_self, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![username, canonical, username, is_self, bc_jobs::time::now()],
    )?;
    let id = tx.last_insert_rowid();
    if let Some(p) = probe {
        record_probe(tx, id, p)?;
    }
    get_fan(tx, id)?.ok_or_else(|| HarvestError::other("fan vanished after insert"))
}

/// Keep what the fan page says: Bandcamp's id, the name, the list sizes.
pub fn record_probe(c: &Connection, fan_id: i64, probe: &SourceProbe) -> Result<()> {
    let p = &probe.params;
    if let Some(bc_id) = p.get("fan_id").and_then(|v| v.as_i64()) {
        // A username can change; the numeric id cannot. Only claim it when nobody else
        // already has (two URLs for one account).
        let taken: Option<i64> =
            c.query_row("SELECT id FROM fans WHERE bc_fan_id = ?1 AND id != ?2", params![bc_id, fan_id], |r| r.get(0)).optional()?;
        if taken.is_none() {
            c.execute("UPDATE fans SET bc_fan_id = ?2 WHERE id = ?1", params![fan_id, bc_id])?;
        }
    }
    if let Some(name) = p.get("username").and_then(|v| v.as_str()).filter(|n| !n.is_empty()) {
        c.execute("UPDATE fans SET display_name = ?2 WHERE id = ?1", params![fan_id, name])?;
    }
    if let Some(w) = p.get("wishlist_count").and_then(|v| v.as_i64()) {
        c.execute("UPDATE fans SET wishlist_count = ?2 WHERE id = ?1", params![fan_id, w])?;
    } else if probe.kind == "wishlist" {
        if let Some(h) = probe.total_hint {
            c.execute("UPDATE fans SET wishlist_count = ?2 WHERE id = ?1", params![fan_id, h])?;
        }
    }
    if let Some(n) = p.get("collection_count").and_then(|v| v.as_i64()) {
        c.execute("UPDATE fans SET collection_count = ?2 WHERE id = ?1", params![fan_id, n])?;
    }
    Ok(())
}

pub fn delete_fan(c: &Connection, fan_id: i64) -> Result<()> {
    c.execute("DELETE FROM fans WHERE id = ?1", [fan_id])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Migration from the single saved wishlist
// ---------------------------------------------------------------------------

/// Create the self fan from the legacy settings key, once.
///
/// Every inbox row flagged `in_wishlist` becomes a member (no position yet; the next walk
/// places them). Idempotent: a self fan that already exists is returned untouched -- and
/// deliberately not re-backfilled, because a full walk prunes items no longer on the list
/// while the old flag is sticky.
pub fn ensure_self_fan(tx: &Transaction<'_>) -> Result<Option<FanRow>> {
    if let Some(existing) = self_fan(tx)? {
        return Ok(Some(existing));
    }
    let Some(raw) = bc_db::settings::get(tx, LEGACY_URL_KEY)? else { return Ok(None) };
    let value = unquote(&raw);
    if value.is_empty() {
        return Ok(None);
    }
    let fan = match create_fan(tx, &value, true, None) {
        Ok(f) => f,
        Err(_) => {
            tracing::warn!("saved wishlist URL {value:?} is not a fan page; not migrating it");
            return Ok(None);
        }
    };
    if !fan.is_self {
        tx.execute("UPDATE fans SET is_self = 1 WHERE id = ?1", [fan.id])?;
    }
    let now = bc_jobs::time::now();
    for (tab, col) in [("wishlist", "in_wishlist"), ("collection", "in_collection")] {
        tx.execute(
            &format!(
                "INSERT OR IGNORE INTO fan_items (fan_id, item_id, tab, position, first_seen_at, last_seen_at) \
                 SELECT ?1, id, ?2, NULL, ?3, ?3 FROM harvest_items WHERE {col} = 1"
            ),
            params![fan.id, tab, now],
        )?;
    }
    if let Some(last) = bc_db::settings::get(tx, LEGACY_LAST_RUN_KEY)?.map(|v| unquote(&v)).filter(|v| !v.is_empty()) {
        tx.execute("UPDATE fans SET last_walk = ?2 WHERE id = ?1 AND last_walk IS NULL", params![fan.id, last])?;
    }
    tracing::info!("migrated the saved wishlist into fan {}", fan.username);
    get_fan(tx, fan.id)
}

/// A settings value is raw text; tolerate one that was JSON-quoted on its way in.
fn unquote(v: &str) -> String {
    let t = v.trim();
    if t.starts_with('"') {
        if let Ok(s) = serde_json::from_str::<String>(t) {
            return s;
        }
    }
    t.to_string()
}

// ---------------------------------------------------------------------------
// Membership and play order
// ---------------------------------------------------------------------------

/// SQL of one fan's list(s) as a subquery of `(item_id, position)`, one row per item. `tab`
/// picks the wishlist or the collection; `None` is both together -- a record on both lists is
/// still one row, at the earlier of its two positions, which is what keeps the unified view
/// and its counts honest. Parameters: `?1` = fan id, then `?2` = tab when given.
pub fn members_sql(tab: Option<&str>) -> String {
    let t = if tab.is_some() { " AND tab = ?2" } else { "" };
    format!("(SELECT item_id, MIN(position) AS position FROM fan_items WHERE fan_id = ?1{t} GROUP BY item_id)")
}

/// Bind values for [`members_sql`].
pub fn member_params(fan_id: i64, tab: Option<&str>) -> Vec<SqlValue> {
    let mut v = vec![SqlValue::Integer(fan_id)];
    if let Some(t) = tab {
        v.push(SqlValue::Text(t.to_string()));
    }
    v
}

/// Items of one fan's list by inbox state.
pub fn fan_counts(c: &Connection, fan_id: i64, tab: Option<&str>) -> Result<BTreeMap<String, i64>> {
    let sql = format!(
        "SELECT h.state, COUNT(h.id) FROM harvest_items h JOIN {} m ON m.item_id = h.id GROUP BY h.state",
        members_sql(tab)
    );
    let mut st = c.prepare(&sql)?;
    let rows = st
        .query_map(params_from_iter(member_params(fan_id, tab)), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<bc_db::rusqlite::Result<Vec<_>>>()?;
    Ok(rows.into_iter().collect())
}

/// Per fan: how many items on each list, and how many distinct across both (`"all"`).
pub fn fan_item_totals(c: &Connection) -> Result<HashMap<i64, BTreeMap<String, i64>>> {
    let mut totals: HashMap<i64, BTreeMap<String, i64>> = HashMap::new();
    let mut st = c.prepare("SELECT fan_id, tab, COUNT(item_id) FROM fan_items GROUP BY fan_id, tab")?;
    for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))? {
        let (fan, tab, n) = r?;
        totals.entry(fan).or_default().insert(tab, n);
    }
    let mut st = c.prepare("SELECT fan_id, COUNT(DISTINCT item_id) FROM fan_items GROUP BY fan_id")?;
    for r in st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
        let (fan, n) = r?;
        totals.entry(fan).or_default().insert("all".into(), n);
    }
    Ok(totals)
}

/// Which of the fan's lists each item is on -- the card's little badge.
pub fn tabs_of(c: &Connection, fan_id: i64, item_ids: &[i64]) -> Result<HashMap<i64, Vec<String>>> {
    let mut out: HashMap<i64, Vec<String>> = HashMap::new();
    let mut ids: Vec<i64> = item_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    for chunk in ids.chunks(400) {
        let ph = vec!["?"; chunk.len()].join(",");
        let mut st = c.prepare(&format!(
            "SELECT item_id, tab FROM fan_items WHERE fan_id = ? AND item_id IN ({ph}) ORDER BY item_id, tab DESC"
        ))?;
        let mut p: Vec<i64> = vec![fan_id];
        p.extend_from_slice(chunk);
        for r in st.query_map(params_from_iter(p.iter()), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (item, tab) = r?;
            out.entry(item).or_default().push(tab);
        }
    }
    Ok(out)
}

/// The next items of a fan's list in play order, after `after`.
///
/// `tab` is the wishlist, the collection, or (`None`) both as one list. `seq` is the list's own
/// order (newest first); `shuffle` is a fixed pseudo-random order keyed by `seed`, so a
/// continuation can pick up where it left off after a reload and never repeats until it wraps.
/// `states` narrows to inbox states (empty means everything but ignored). An exhausted list
/// returns an empty result, never wraps.
#[allow(clippy::too_many_arguments)]
pub fn next_items(
    c: &Connection,
    fan_id: i64,
    after: Option<i64>,
    order: FanOrder,
    seed: u32,
    states: &[String],
    limit: usize,
    tab: Option<&str>,
) -> Result<Vec<FanNextItem>> {
    let members = members_sql(tab);
    let mut args = member_params(fan_id, tab);
    let key = match order {
        FanOrder::Shuffle => {
            args.push(SqlValue::Integer(i64::from(seed)));
            format!("(((h.id + ?{}) * {SHUFFLE_MULT}) % {SHUFFLE_MOD})", args.len())
        }
        FanOrder::Seq => format!("COALESCE(m.position, {UNPLACED})"),
    };
    let mut wher = String::new();
    if states.is_empty() {
        wher.push_str(" AND h.state != 'ignored'");
    } else {
        let ph: Vec<String> = states
            .iter()
            .map(|s| {
                args.push(SqlValue::Text(s.clone()));
                format!("?{}", args.len())
            })
            .collect();
        wher.push_str(&format!(" AND h.state IN ({})", ph.join(",")));
    }
    if let Some(after) = after {
        let after_key: i64 = match order {
            FanOrder::Shuffle => shuffle_key(after, seed),
            FanOrder::Seq => {
                let mut p = member_params(fan_id, tab);
                p.push(SqlValue::Integer(after));
                let sql = format!("SELECT COALESCE(position, {UNPLACED}) FROM {members} WHERE item_id = ?{}", p.len());
                c.query_row(&sql, params_from_iter(p), |r| r.get(0)).optional()?.unwrap_or(UNPLACED)
            }
        };
        args.push(SqlValue::Integer(after_key));
        let ak = args.len();
        args.push(SqlValue::Integer(after));
        let ai = args.len();
        wher.push_str(&format!(" AND ({key} > ?{ak} OR ({key} = ?{ak} AND h.id > ?{ai}))"));
    }
    args.push(SqlValue::Integer(limit as i64));
    let sql = format!(
        "SELECT h.id, h.url, h.url_kind, h.title, h.artist_name, h.art_url, h.state, h.release_id, m.position \
         FROM harvest_items h JOIN {members} m ON m.item_id = h.id WHERE 1 = 1{wher} \
         ORDER BY {key}, h.id LIMIT ?{}",
        args.len()
    );
    let mut st = c.prepare(&sql)?;
    let rows = st
        .query_map(params_from_iter(args), |r| {
            Ok(FanNextItem {
                item_id: r.get(0)?,
                url: r.get(1)?,
                url_kind: r.get(2)?,
                title: r.get(3)?,
                artist_name: r.get(4)?,
                art_url: r.get(5)?,
                state: r.get(6)?,
                release_id: r.get(7)?,
                position: r.get(8)?,
            })
        })?
        .collect::<bc_db::rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// What the player's continuation asks for: the next records of a fan's list, in its own order
/// or a seeded shuffle (`GET /fans/{id}/next` is a thin wrapper). `states` empty = everything
/// but ignored; `"all"` entries are dropped like the route does.
#[allow(clippy::too_many_arguments)]
pub async fn fan_next(
    ctx: &Arc<Ctx>,
    fan_id: i64,
    after: Option<i64>,
    order: FanOrder,
    seed: u32,
    states: &[String],
    tab: FanTab,
    limit: usize,
) -> Result<FanNextOut> {
    let limit = limit.max(1);
    let states: Vec<String> = states.iter().filter(|s| !s.is_empty() && *s != "all").cloned().collect();
    let rows = ctx
        .db
        .read_async(move |c| {
            next_items(c, fan_id, after, order, seed, &states, limit + 1, tab.as_str()).map_err(|e| DbError::Other(e.to_string()))
        })
        .await?;
    let exhausted = rows.len() <= limit;
    Ok(FanNextOut { items: rows.into_iter().take(limit).collect(), exhausted })
}

// ---------------------------------------------------------------------------
// Sources (the seam the tests stand a fake Bandcamp behind)
// ---------------------------------------------------------------------------

/// What the walker reads from Bandcamp. The live impl is [`LiveFanSource`]; tests swap in a
/// fixed list (the Python tests monkeypatched `sources.harvest_collection` / `probe_fan`: a
/// fan page is fetched from `https://bandcamp.com/<user>`, which no local server can pose as,
/// because `urls::fan_base_url` drops any port).
#[async_trait]
pub trait FanSource: Send + Sync {
    async fn probe_fan(&self, url: &str) -> Result<SourceProbe>;
    fn harvest_collection(&self, url: &str, which: &str, limit: usize) -> EventStream;
}

pub struct LiveFanSource {
    ctx: Arc<Ctx>,
}

#[async_trait]
impl FanSource for LiveFanSource {
    async fn probe_fan(&self, url: &str) -> Result<SourceProbe> {
        sources::probe_fan(&self.ctx.client, url).await
    }
    fn harvest_collection(&self, url: &str, which: &str, limit: usize) -> EventStream {
        sources::harvest_collection(self.ctx.client.clone(), None, Some(url.to_string()), which.to_string(), limit, WALK_PAGE, None)
    }
}

// ---------------------------------------------------------------------------
// The walker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Pending {
    fan_id: i64,
    job_id: String,
    tabs: Vec<String>,
}

#[derive(Default)]
struct Inner {
    state: WalkState,
    pending: Vec<Pending>,
    last: HashMap<i64, WalkState>,
    current_job: Option<String>,
}

fn idle_state() -> WalkState {
    WalkState { phase: "idle".into(), tabs: TABS.iter().map(|t| t.to_string()).collect(), ..Default::default() }
}

fn is_running(phase: &str) -> bool {
    matches!(phase, "harvesting" | "queueing")
}

fn out_state(s: &WalkState) -> WalkState {
    let mut o = s.clone();
    o.running = is_running(&o.phase);
    o.errors.truncate(10);
    o
}

/// `Db::write_with` off the async runtime.
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

/// Walks wishlists one at a time, in the order asked, and reports progress.
pub struct FanWalker {
    ctx: Arc<Ctx>,
    inner: Mutex<Inner>,
    source: RwLock<Arc<dyn FanSource>>,
    worker: Mutex<Option<Arc<KindWorker>>>,
}

struct WalkHandler {
    walker: Arc<FanWalker>,
}

#[async_trait]
impl ItemHandler for WalkHandler {
    async fn run(&self, ctx: ItemCtx) -> HandlerOutcome {
        self.walker.run_walk(ctx).await
    }
}

enum WalkEnd {
    Done(String),
    Failed(String),
    Interrupted,
}

impl FanWalker {
    pub fn new(ctx: Arc<Ctx>) -> Arc<Self> {
        let src: Arc<dyn FanSource> = Arc::new(LiveFanSource { ctx: ctx.clone() });
        Arc::new(Self {
            ctx,
            inner: Mutex::new(Inner { state: idle_state(), ..Default::default() }),
            source: RwLock::new(src),
            worker: Mutex::new(None),
        })
    }

    /// Replace where walks read Bandcamp from (tests).
    pub fn set_source(&self, s: Arc<dyn FanSource>) {
        *self.source.write() = s;
    }

    pub fn source(&self) -> Arc<dyn FanSource> {
        self.source.read().clone()
    }

    // -- reporting ----------------------------------------------------------

    /// The current (or last) walk, whoever's it is.
    pub fn state(&self) -> WalkState {
        out_state(&self.inner.lock().state)
    }

    /// Live state for a fan: running, waiting its turn, or last finished.
    pub fn state_for(&self, fan_id: i64) -> Option<WalkState> {
        let g = self.inner.lock();
        if let Some(pos) = g.pending.iter().position(|p| p.fan_id == fan_id) {
            let p = &g.pending[pos];
            // First in line with nothing running: the worker is about to pick it up.
            let phase = if pos == 0 && !is_running(&g.state.phase) { "harvesting" } else { "queued" };
            return Some(WalkState {
                fan_id: Some(fan_id),
                phase: phase.into(),
                running: phase == "harvesting",
                tabs: p.tabs.clone(),
                ..Default::default()
            });
        }
        if g.state.fan_id == Some(fan_id) && g.state.phase != "idle" {
            return Some(out_state(&g.state));
        }
        g.last.get(&fan_id).map(out_state)
    }

    fn publish(&self, f: impl FnOnce(&mut WalkState)) {
        let snap = {
            let mut g = self.inner.lock();
            f(&mut g.state);
            g.state.running = is_running(&g.state.phase);
            out_state(&g.state)
        };
        self.ctx.bus.publish(TOPIC_FANS_WALK, &snap);
    }

    // -- control ------------------------------------------------------------

    /// Ask for a walk; returns at once with the state it will report under.
    ///
    /// `queue_new` is whether to queue what the walk finds missing; it defaults to the fan's
    /// own nature (the self fan backfills, others do not). `tabs` names the lists to walk
    /// (both when absent). A fan already walking or waiting is not queued twice.
    pub async fn request(&self, fan_id: i64, queue_new: Option<bool>, tabs: Option<&[String]>) -> Result<WalkState> {
        let wanted = coerce_tabs(tabs);
        let job_id = uuid::Uuid::new_v4().to_string();
        let ahead = {
            let mut g = self.inner.lock();
            if g.state.fan_id == Some(fan_id) && is_running(&g.state.phase) {
                return Ok(out_state(&g.state));
            }
            if g.pending.iter().any(|p| p.fan_id == fan_id) {
                return Ok(WalkState { fan_id: Some(fan_id), phase: "queued".into(), tabs: wanted, ..Default::default() });
            }
            let ahead = is_running(&g.state.phase) || !g.pending.is_empty();
            // Reserved before the job exists, so a claim that races the insert finds its entry.
            g.pending.push(Pending { fan_id, job_id: job_id.clone(), tabs: wanted.clone() });
            ahead
        };
        let label = match dbr(&self.ctx.db, move |c| get_fan(c, fan_id)).await {
            Ok(Some(f)) => format!("Walk {}", f.display_name.unwrap_or(f.username)),
            _ => "Walk wishlist".to_string(),
        };
        let nj = NewJob::new(bc_types::jobs::KIND_WALK, vec![NewItem::default()])
            .id(job_id.clone())
            .label(label)
            .params(serde_json::json!({"fan_id": fan_id, "tabs": wanted, "queue_new": queue_new}));
        let store = self.ctx.jobs.store().clone();
        if let Err(e) = store.run(move |s| s.create_job(nj)).await {
            self.inner.lock().pending.retain(|p| p.job_id != job_id);
            return Err(e.into());
        }
        Ok(if ahead {
            WalkState { fan_id: Some(fan_id), phase: "queued".into(), tabs: wanted, ..Default::default() }
        } else {
            WalkState {
                fan_id: Some(fan_id),
                phase: "harvesting".into(),
                running: true,
                tabs: wanted,
                started_at: Some(bc_jobs::time::to_iso(&bc_jobs::time::now())),
                ..Default::default()
            }
        })
    }

    /// Stop a fan's walk (or every walk) now, keeping what it has found.
    pub async fn stop(&self, fan_id: Option<i64>) {
        let mut jobs: Vec<String> = Vec::new();
        {
            let mut g = self.inner.lock();
            g.pending.retain(|p| {
                if fan_id.is_none_or(|f| f == p.fan_id) {
                    jobs.push(p.job_id.clone());
                    false
                } else {
                    true
                }
            });
            if is_running(&g.state.phase) && fan_id.is_none_or(|f| g.state.fan_id == Some(f)) {
                if let Some(j) = g.current_job.clone() {
                    jobs.push(j);
                }
            }
        }
        for j in jobs {
            if let Err(e) = self.ctx.jobs.stop_job(&j).await {
                tracing::warn!("could not stop walk job {j}: {e:?}");
            }
        }
    }

    /// Re-adopt walks that were waiting when the process went down (after crash recovery).
    async fn restore_pending(&self) {
        let rows = self
            .ctx
            .db
            .read_async(|c| {
                let mut st = c.prepare(
                    "SELECT id, params FROM jobs WHERE kind = 'walk' AND status IN ('queued','running') \
                     AND cancel_requested = 0 ORDER BY created_at",
                )?;
                let rows = st
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<bc_db::rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap_or_default();
        let mut g = self.inner.lock();
        for (job_id, params) in rows {
            let v: serde_json::Value = serde_json::from_str(&params).unwrap_or_default();
            let Some(fan_id) = v.get("fan_id").and_then(|x| x.as_i64()) else { continue };
            if g.pending.iter().any(|p| p.job_id == job_id) {
                continue;
            }
            let tabs: Vec<String> = v
                .get("tabs")
                .and_then(|t| t.as_array())
                .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            g.pending.push(Pending { fan_id, job_id, tabs: coerce_tabs(Some(&tabs)) });
        }
    }

    // -- the walk itself ----------------------------------------------------

    async fn run_walk(&self, ictx: ItemCtx) -> HandlerOutcome {
        let params = ictx.job.params_json();
        let Some(fan_id) = params.get("fan_id").and_then(|v| v.as_i64()) else {
            return HandlerOutcome::Failed { error: "walk job without a fan".into(), class: "bad_params".into(), retryable: false };
        };
        let queue_new = params.get("queue_new").and_then(|v| v.as_bool());
        let tabs: Vec<String> = params
            .get("tabs")
            .and_then(|t| t.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let tabs = coerce_tabs(Some(&tabs));
        let job_id = ictx.job.id.clone();

        let started = bc_jobs::time::to_iso(&bc_jobs::time::now());
        {
            let mut g = self.inner.lock();
            g.pending.retain(|p| p.job_id != job_id);
            g.current_job = Some(job_id.clone());
            g.state = WalkState {
                fan_id: Some(fan_id),
                phase: "harvesting".into(),
                running: true,
                tabs: tabs.clone(),
                started_at: Some(started),
                ..Default::default()
            };
        }
        self.ctx.bus.publish(TOPIC_FANS_WALK, &self.state());

        let end = match self.walk(&ictx, fan_id, queue_new, tabs).await {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("walk of fan {fan_id} failed: {e}");
                let msg: String = e.to_string().chars().take(500).collect();
                let m = msg.clone();
                self.finish(Some(fan_id), move |s| {
                    s.phase = "failed".into();
                    s.error = Some(m);
                })
                .await;
                WalkEnd::Failed(msg)
            }
        };
        self.inner.lock().current_job = None;
        match end {
            WalkEnd::Done(m) => HandlerOutcome::Done(Complete::msg(m)),
            WalkEnd::Failed(e) => HandlerOutcome::Failed { error: e, class: "walk".into(), retryable: false },
            WalkEnd::Interrupted => HandlerOutcome::Interrupted,
        }
    }

    async fn walk(&self, ictx: &ItemCtx, fan_id: i64, queue_new: Option<bool>, tabs: Vec<String>) -> Result<WalkEnd> {
        let db = self.ctx.db.clone();
        let Some(fan) = dbr(&db, move |c| get_fan(c, fan_id)).await? else {
            let msg = "That wishlist is no longer followed".to_string();
            let m = msg.clone();
            self.finish(None, move |s| {
                s.phase = "failed".into();
                s.error = Some(m);
            })
            .await;
            return Ok(WalkEnd::Failed(msg));
        };
        let (url, is_self, username) = (fan.url.clone(), fan.is_self, fan.username.clone());
        let should_queue = queue_new.unwrap_or(is_self);
        let source = self.source();

        let mut stopped = false;
        let mut outcome: Option<QueueOutcome> = None;
        let mut walked: Vec<String> = Vec::new();
        // Lists this walk could not read: refused by Bandcamp (private), or failed outright.
        // Either way the walk goes on to the next list.
        let mut refused: Vec<String> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        let mut totals = AbsorbCounts::default();

        // The page first: it names the account, counts both lists, and is the same fetch the
        // walks below will hit in cache.
        let probe = source.probe_fan(&url).await.map_err(|e| HarvestError::other(format!("could not open {url}: {e}")))?;
        {
            let probe = probe.clone();
            dbw(&db, move |tx| record_probe(tx, fan_id, &probe)).await?;
        }
        let size = |k: &str| probe.params.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        let total: i64 = tabs.iter().map(|t| size(&format!("{t}_count"))).sum();
        self.publish(|s| s.total = if total > 0 { Some(total) } else { None });

        for tab in &tabs {
            self.publish(|s| s.tab = Some(tab.clone()));
            let positions: Arc<Mutex<HashMap<String, i64>>> = Arc::default();
            let tab_started = bc_jobs::time::now();
            let before = totals.seen;

            let tap = {
                let positions = positions.clone();
                source
                    .harvest_collection(&url, tab, WALK_LIMIT)
                    .inspect(move |ev| {
                        // Remember each release's place in the walk: that is the list's order.
                        if let Ok(e) = ev {
                            if let Some(r) = &e.release {
                                positions.lock().insert(r.url.clone(), e.seen);
                            }
                        }
                    })
                    .boxed()
            };
            let mut opts = AbsorbOpts::new(
                // My own lists keep their meaning on the row (`in_wishlist` / `in_collection`);
                // another person's are provenance "fan" and flag nothing.
                if is_self { tab.clone() } else { "fan".to_string() },
                if is_self { urls::display_name(&url) } else { username.clone() },
            );
            opts.mark_wishlist = Some(is_self && tab == "wishlist");
            opts.claim_source = is_self;
            opts.on_batch = Some(record_hook(fan_id, tab.clone(), positions.clone()));
            let progress = |seen: i64, _total: Option<i64>| self.publish(|s| s.seen = before + seen);
            let absorbed = inbox::absorb(&db, tap, &opts, Some(&progress), &ictx.cancel).await;
            let counts = absorbed.counts;

            if absorbed.cancelled {
                // Kept on purpose, so the queueing below still happens: the walk is the slow
                // half, and what it has absorbed is worth keeping.
                totals.seen += counts.seen;
                totals.new += counts.new;
                totals.in_library += counts.in_library;
                totals.errors.extend(counts.errors);
                walked.push(tab.clone());
                stopped = true;
                break;
            }
            match absorbed.error {
                Some(HarvestError::ListUnavailable(m)) => {
                    // A private wishlist, say. Their other list is still readable, and the
                    // walk is worth finishing for it.
                    tracing::info!("fan {fan_id}: {m}");
                    refused.push(tab.clone());
                    continue;
                }
                Some(HarvestError::Cancelled) => {
                    stopped = true;
                    break;
                }
                Some(e) => {
                    // One list failing is not the account failing: note it and walk the next.
                    // Only a walk that read no list at all is a failed walk (below).
                    tracing::warn!("walk of fan {fan_id} failed on the {tab}: {e}");
                    failures.push(format!("{tab}: {e}"));
                    continue;
                }
                None => {}
            }
            totals.seen += counts.seen;
            totals.new += counts.new;
            totals.in_library += counts.in_library;
            let clean = counts.errors.is_empty();
            totals.errors.extend(counts.errors);
            walked.push(tab.clone());
            // Membership is pruned only after a walk that saw the whole list: a stopped or
            // capped walk has no opinion about what it did not reach.
            if clean && (counts.seen as usize) < WALK_LIMIT {
                let tab = tab.clone();
                dbw(&db, move |tx| prune(tx, fan_id, &tab, &tab_started, is_self)).await?;
            }
        }

        // Cancelled for a reason other than "stop" (shutdown, pause): nothing more to do here;
        // the runner requeues or releases the item.
        let user_stop = if stopped {
            let jid = ictx.job.id.clone();
            ictx.store.run(move |s| s.is_cancel_requested(&jid)).await.unwrap_or(true)
        } else {
            false
        };
        if stopped && !user_stop {
            self.finish(Some(fan_id), |s| {
                s.phase = "failed".into();
                s.error = Some("Cancelled".into());
            })
            .await;
            return Ok(WalkEnd::Interrupted);
        }

        let all_errors: Vec<String> = totals.errors.iter().cloned().chain(notes(&refused, &failures)).collect();
        {
            let (n, l, e) = (totals.new, totals.in_library, all_errors.clone());
            self.publish(move |s| {
                s.phase = "queueing".into();
                s.tab = None;
                s.new = n;
                s.in_library = l;
                s.errors = e;
            });
        }

        if should_queue && !walked.is_empty() {
            outcome = Some(self.queue_missing(fan_id, &walked, is_self).await?);
        }

        let mut phase = "done";
        let mut error: Option<String> = None;
        if stopped {
            phase = "failed";
            error = Some("Stopped".into());
        } else if walked.is_empty() {
            // Nothing was read at all -- that is a failure, and the reason belongs on the fan.
            phase = "failed";
            error = Some(if !failures.is_empty() {
                failures.join("; ")
            } else if !refused.is_empty() {
                format!("Bandcamp does not show this fan's {}", refused.join(" or "))
            } else {
                "Nothing to walk".to_string()
            });
        }
        let error: Option<String> = error.map(|e| e.chars().take(500).collect());
        let (queued, job) = outcome.as_ref().map(|o| (o.queued, o.job_id.clone())).unwrap_or((0, None));
        {
            let error = error.clone();
            self.finish(Some(fan_id), move |s| {
                s.phase = phase.into();
                s.error = error;
                s.errors = all_errors;
                s.queued = queued;
                s.job_id = job;
            })
            .await;
        }
        if queued > 0 {
            self.ctx.notify_downloads();
        }
        Ok(match error {
            Some(e) if e == "Stopped" => WalkEnd::Done("Stopped".into()),
            Some(e) => WalkEnd::Failed(e),
            None => WalkEnd::Done(format!("{} seen, {} new, {} queued", totals.seen, totals.new, queued)),
        })
    }

    async fn queue_missing(&self, fan_id: i64, tabs: &[String], is_self: bool) -> Result<QueueOutcome> {
        let db = self.ctx.db.clone();
        let store = self.ctx.jobs.store().clone();
        let tabs = tabs.to_vec();
        tokio::task::spawn_blocking(move || -> Result<QueueOutcome> {
            let fan = db.read_with::<_, HarvestError>(|c| get_fan(c, fan_id))?;
            let rows = db.read_with::<_, HarvestError>(|c| {
                let mut ids: Vec<i64> = Vec::new();
                let mut seen: HashSet<i64> = HashSet::new();
                for tab in &tabs {
                    let sql = format!(
                        "SELECT h.id FROM harvest_items h JOIN {} m ON m.item_id = h.id WHERE h.state = 'new' \
                         ORDER BY COALESCE(m.position, {UNPLACED}), h.id",
                        members_sql(Some(tab))
                    );
                    let mut st = c.prepare(&sql)?;
                    for r in st.query_map(params_from_iter(member_params(fan_id, Some(tab))), |r| r.get::<_, i64>(0))? {
                        let id = r?;
                        if seen.insert(id) {
                            ids.push(id);
                        }
                    }
                }
                // `load_rows` sorts by id; restore the walk order for the job.
                let by_id: HashMap<i64, inbox::HarvestRow> = inbox::load_rows(c, &ids)?.into_iter().map(|r| (r.id, r)).collect();
                Ok(ids.into_iter().filter_map(|i| by_id.get(&i).cloned()).collect::<Vec<_>>())
            })?;
            match (is_self, fan) {
                // Straight into the downloads root: the library already uses bandcamp-dl's
                // <artist>/<album> layout, and a subfolder would both duplicate artists and
                // hide the existing files from bandcamp-dl's own already-have check.
                (true, _) | (_, None) => inbox::queue(
                    &db,
                    &store,
                    &rows,
                    &QueueOpts { allow_unowned: true, target_subdir: Some(String::new()), ..Default::default() },
                ),
                (false, Some(f)) => inbox::queue(
                    &db,
                    &store,
                    &rows,
                    &QueueOpts {
                        allow_unowned: true,
                        target_subdir: Some(shelf_name(&f)),
                        source_fan_id: Some(f.id),
                        label: Some(format!("{}'s wishlist", f.display_name.clone().unwrap_or_else(|| f.username.clone()))),
                        ..Default::default()
                    },
                ),
            }
        })
        .await
        .map_err(|e| HarvestError::other(e.to_string()))?
    }

    /// Close the walk: publish the final state, remember it in memory and on the fan row.
    async fn finish(&self, fan_id: Option<i64>, f: impl FnOnce(&mut WalkState)) {
        let finished = bc_jobs::time::now();
        let iso = bc_jobs::time::to_iso(&finished);
        self.publish(|s| {
            f(s);
            s.finished_at = Some(iso);
        });
        let fin = self.state();
        let Some(fid) = fin.fan_id.or(fan_id) else { return };
        self.inner.lock().last.insert(fid, fin.clone());
        let json = serde_json::to_string(&fin).unwrap_or_default();
        let err = fin.error.clone();
        let res = dbw(&self.ctx.db, move |tx| {
            tx.execute(
                "UPDATE fans SET last_walk = ?2, last_walk_at = ?3, last_error = ?4 WHERE id = ?1",
                params![fid, json, finished, err],
            )?;
            Ok(())
        })
        .await;
        if let Err(e) = res {
            tracing::warn!("could not record the walk on fan {fid}: {e}");
        }
    }
}

/// Records `fan_items(fan_id,item_id,tab,position,first_seen_at,last_seen_at)` atomically with
/// the rows the batch upserted.
fn record_hook(fan_id: i64, tab: String, positions: Arc<Mutex<HashMap<String, i64>>>) -> BatchHook {
    Arc::new(move |c: &Connection, batch: &[(HarvestedRelease, i64, bool)]| {
        let now = bc_jobs::time::now();
        let mut st = c.prepare_cached(
            "INSERT INTO fan_items (fan_id, item_id, tab, position, first_seen_at, last_seen_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
             ON CONFLICT(fan_id, item_id, tab) DO UPDATE SET position = excluded.position, last_seen_at = excluded.last_seen_at",
        )?;
        let pos = positions.lock();
        for (release, item_id, _) in batch {
            st.execute(params![fan_id, item_id, tab, pos.get(&release.url).copied(), now])?;
        }
        Ok(())
    })
}

/// Drop memberships the walk did not see: those items left the list.
fn prune(tx: &Transaction<'_>, fan_id: i64, tab: &str, started: &str, is_self: bool) -> Result<()> {
    let gone: Vec<i64> = {
        let mut st = tx.prepare("SELECT item_id FROM fan_items WHERE fan_id = ?1 AND tab = ?2 AND last_seen_at < ?3")?;
        st.query_map(params![fan_id, tab, started], |r| r.get::<_, i64>(0))?.collect::<bc_db::rusqlite::Result<_>>()?
    };
    if gone.is_empty() {
        return Ok(());
    }
    tx.execute("DELETE FROM fan_items WHERE fan_id = ?1 AND tab = ?2 AND last_seen_at < ?3", params![fan_id, tab, started])?;
    if is_self {
        // The sticky flags mean "on MY wishlist" / "I own this"; they are no longer true for
        // what left the list.
        let col = if tab == "wishlist" { "in_wishlist" } else { "in_collection" };
        for chunk in gone.chunks(400) {
            let ph = vec!["?"; chunk.len()].join(",");
            tx.execute(&format!("UPDATE harvest_items SET {col} = 0 WHERE id IN ({ph})"), params_from_iter(chunk.iter()))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// DTO assembly
// ---------------------------------------------------------------------------

/// The walk to show for a fan: the live/queued/last-finished one, else the one stored on the row.
pub fn walk_of(walker: &FanWalker, fan: &FanRow) -> Option<WalkState> {
    walker.state_for(fan.id).or_else(|| fan.last_walk.as_deref().and_then(|j| serde_json::from_str::<WalkState>(j).ok()))
}

/// Build a [`FanOut`]. `totals` / `downloaded` are precomputed for list calls.
pub fn fan_out(
    c: &Connection,
    fan: &FanRow,
    walker: &FanWalker,
    totals: Option<&HashMap<i64, BTreeMap<String, i64>>>,
    downloaded: Option<&HashMap<i64, i64>>,
) -> Result<FanOut> {
    let per_fan = match totals {
        Some(t) => t.get(&fan.id).cloned().unwrap_or_default(),
        None => fan_item_totals(c)?.remove(&fan.id).unwrap_or_default(),
    };
    let reported = [("wishlist", fan.wishlist_count), ("collection", fan.collection_count)];
    let held = match downloaded {
        Some(d) => d.get(&fan.id).copied().unwrap_or(0),
        None => c.query_row("SELECT COUNT(*) FROM releases WHERE source_fan_id = ?1", [fan.id], |r| r.get(0))?,
    };
    let mut tabs = BTreeMap::new();
    for (tab, rep) in reported {
        tabs.insert(
            tab.to_string(),
            FanTabOut { items: per_fan.get(tab).copied().unwrap_or(0), counts: fan_counts(c, fan.id, Some(tab))?, reported: rep },
        );
    }
    Ok(FanOut {
        id: fan.id,
        username: fan.username.clone(),
        display_name: fan.display_name.clone(),
        url: fan.url.clone(),
        wishlist_url: format!("{}/wishlist", fan.url),
        bc_fan_id: fan.bc_fan_id,
        is_self: fan.is_self,
        wishlist_count: fan.wishlist_count,
        collection_count: fan.collection_count,
        shelf: shelf_name(fan),
        items: per_fan.get("all").copied().unwrap_or(0),
        counts: fan_counts(c, fan.id, None)?,
        tabs,
        downloaded: held,
        last_walk_at: fan.last_walk_at.as_deref().map(bc_jobs::time::to_iso),
        last_error: fan.last_error.clone(),
        walk: walk_of(walker, fan),
        created_at: Some(bc_jobs::time::to_iso(&fan.created_at)),
    })
}

// ---------------------------------------------------------------------------
// Service wiring
// ---------------------------------------------------------------------------

/// The walker service; the router and WS4's engine find it with `ctx.expect::<FanWalker>()`.
pub fn init(ctx: &Arc<Ctx>) {
    let walker = FanWalker::new(ctx.clone());
    let worker = KindWorker::new(
        ctx.jobs.store().clone(),
        WorkerSpec::new(bc_types::jobs::KIND_WALK, 1),
        Arc::new(WalkHandler { walker: walker.clone() }),
    );
    ctx.jobs.add_hooks(worker.clone());
    *walker.worker.lock() = Some(worker);
    ctx.put(walker);
}

/// Create the self fan from the legacy settings key, re-adopt waiting walks, start the worker.
pub async fn start(ctx: &Arc<Ctx>) {
    if let Err(e) = dbw(&ctx.db, |tx| ensure_self_fan(tx).map(|_| ())).await {
        tracing::warn!("could not migrate the saved wishlist into a fan: {e}");
    }
    let walker = ctx.expect::<FanWalker>();
    walker.restore_pending().await;
    let worker = walker.worker.lock().clone();
    if let Some(w) = worker {
        w.start();
    }
}
