//! The harvest inbox: absorb a source stream into it, and queue out of it
//! (port of `services/harvest/inbox.py`).
//!
//! Lifted out of the route module so every caller (harvest run, fan walks, sweeps, the
//! feed) shares the same rules: how an existing row is refreshed, when an item counts as
//! already in the library, and what may be queued.
//!
//! Everything here is blocking over `&Connection`/`&Transaction` (so a caller can compose it
//! inside its own `Db::write`), except [`absorb`] which drives an async stream and writes in
//! batches of [`FLUSH_EVERY`].

use std::collections::BTreeMap;
use std::sync::Arc;

use bc_db::rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use bc_db::{Db, util::name_key};
use bc_jobs::{JobStore, NewItem, NewJob};
use bc_types::bandcamp::HarvestItemOut;
use futures::StreamExt;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::download::dedup::{ReleaseIndex, build_release_index, take, url_key};
use crate::error::{HarvestError, Result};
use crate::extract::HarvestedRelease;
use crate::service::Ctx;
use crate::sources::EventStream;
use crate::urls::normalise;

pub const FLUSH_EVERY: usize = 50;

pub fn init(ctx: &Arc<Ctx>) {
    // Unrun URLs of cancelled/removed/deleted jobs go back to `new` in the inbox.
    ctx.jobs.add_hooks(Arc::new(InboxHooks { ctx: ctx.clone() }));
}
pub async fn start(_ctx: &Arc<Ctx>) {}

/// A `harvest_items` row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HarvestRow {
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
    /// JSON array text.
    pub tags: String,
    pub bc_item_id: Option<i64>,
    pub band_id: Option<i64>,
    pub source_kind: Option<String>,
    pub source_label: Option<String>,
    pub in_collection: bool,
    pub in_wishlist: bool,
    pub is_free_download: bool,
    pub is_purchasable: bool,
    pub is_preorder: bool,
    pub extract_tier: Option<String>,
    pub release_id: Option<i64>,
    pub discovered_at: String,
    pub resolved_at: Option<String>,
}

pub const ROW_COLS: &str = "id, url, url_kind, state, title, artist_name, label_name, art_url, release_date, \
    track_count, tags, bc_item_id, band_id, source_kind, source_label, in_collection, in_wishlist, \
    is_free_download, is_purchasable, is_preorder, extract_tier, release_id, discovered_at, resolved_at";

impl HarvestRow {
    pub fn from_row(r: &Row<'_>) -> bc_db::rusqlite::Result<Self> {
        let b = |i: usize| -> bc_db::rusqlite::Result<bool> { Ok(r.get::<_, i64>(i)? != 0) };
        Ok(Self {
            id: r.get(0)?,
            url: r.get(1)?,
            url_kind: r.get(2)?,
            state: r.get(3)?,
            title: r.get(4)?,
            artist_name: r.get(5)?,
            label_name: r.get(6)?,
            art_url: r.get(7)?,
            release_date: r.get(8)?,
            track_count: r.get(9)?,
            tags: r.get(10)?,
            bc_item_id: r.get(11)?,
            band_id: r.get(12)?,
            source_kind: r.get(13)?,
            source_label: r.get(14)?,
            in_collection: b(15)?,
            in_wishlist: b(16)?,
            is_free_download: b(17)?,
            is_purchasable: b(18)?,
            is_preorder: b(19)?,
            extract_tier: r.get(20)?,
            release_id: r.get(21)?,
            discovered_at: r.get(22)?,
            resolved_at: r.get(23)?,
        })
    }

    pub fn tag_list(&self) -> Vec<String> {
        match serde_json::from_str::<serde_json::Value>(&self.tags) {
            Ok(serde_json::Value::Array(a)) => a.iter().map(|t| t.as_str().map(str::to_string).unwrap_or_else(|| t.to_string())).collect(),
            _ => Vec::new(),
        }
    }

    /// `_item_out`.
    pub fn to_out(&self, in_library: bool, position: Option<i64>, tabs: Vec<String>) -> HarvestItemOut {
        HarvestItemOut {
            id: self.id,
            url: self.url.clone(),
            url_kind: self.url_kind.clone(),
            state: self.state.clone(),
            title: self.title.clone(),
            artist_name: self.artist_name.clone(),
            label_name: self.label_name.clone(),
            art_url: self.art_url.clone(),
            release_date: self.release_date.clone(),
            track_count: self.track_count,
            tags: self.tag_list(),
            source_kind: self.source_kind.clone(),
            source_label: self.source_label.clone(),
            in_collection: self.in_collection,
            in_wishlist: self.in_wishlist,
            is_free_download: self.is_free_download,
            is_purchasable: self.is_purchasable,
            is_preorder: self.is_preorder,
            in_library: in_library || self.state == "in_library",
            release_id: self.release_id,
            position,
            tabs,
            discovered_at: Some(bc_jobs::time::to_iso(&self.discovered_at)),
        }
    }
}

pub fn load_row(c: &Connection, id: i64) -> Result<Option<HarvestRow>> {
    Ok(c.query_row(&format!("SELECT {ROW_COLS} FROM harvest_items WHERE id = ?1"), [id], HarvestRow::from_row).optional()?)
}

pub fn load_rows(c: &Connection, ids: &[i64]) -> Result<Vec<HarvestRow>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(400) {
        let ph = vec!["?"; chunk.len()].join(",");
        let mut st = c.prepare(&format!("SELECT {ROW_COLS} FROM harvest_items WHERE id IN ({ph}) ORDER BY id"))?;
        let rows = st.query_map(bc_db::rusqlite::params_from_iter(chunk.iter()), HarvestRow::from_row)?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

/// Keep the normalised tag table (`harvest_item_tags`) in step with `harvest_items.tags`.
pub fn sync_tags(c: &Connection, item_id: i64, tags: &[String]) -> Result<()> {
    c.execute("DELETE FROM harvest_item_tags WHERE item_id = ?1", [item_id])?;
    let mut st = c.prepare_cached("INSERT OR IGNORE INTO harvest_item_tags (item_id, tag_key, tag) VALUES (?1, ?2, ?3)")?;
    for t in tags {
        let key = name_key(t);
        if !key.is_empty() {
            st.execute(params![item_id, key, t])?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct AbsorbCounts {
    pub seen: i64,
    pub new: i64,
    pub already_known: i64,
    pub in_library: i64,
    /// Already in a download job and not yet on disk ("seen before" and "you own this" are
    /// different answers).
    pub queued: i64,
    /// Items from this run still in state `new`: what a "download what this source has" button acts on.
    pub pending_ids: Vec<i64>,
    pub errors: Vec<String>,
    pub tier_counts: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Default)]
pub struct QueueOutcome {
    pub queued: i64,
    pub skipped_in_library: i64,
    pub skipped_blacklisted: i64,
    pub needs_confirmation: Vec<String>,
    pub job_id: Option<String>,
    /// Releases moved into my library because they were asked for personally and sat on a shelf.
    pub adopted: i64,
}

#[derive(Debug, Clone)]
pub struct UpsertOpts<'a> {
    pub source_kind: &'a str,
    pub source_label: &'a str,
    pub label_name: Option<&'a str>,
    pub in_collection: bool,
    pub in_wishlist: bool,
    /// Whether this harvest may overwrite the row's recorded provenance. A new row always takes it;
    /// a walk of someone else's wishlist passes `false` so a record on my own wishlist keeps saying so.
    pub claim_source: bool,
}

/// Result of one [`upsert`].
#[derive(Debug, Clone)]
pub struct Upserted {
    pub id: i64,
    pub is_new: bool,
    pub state: String,
}

fn ae(e: bc_libcore::ApiError) -> HarvestError {
    HarvestError::Other(e.to_string())
}

/// Insert or refresh one inbox row (`upsert`). A shallow record never blanks out fields a full
/// fetch already filled.
pub fn upsert(c: &Connection, release: &HarvestedRelease, o: &UpsertOpts<'_>, index: Option<&mut ReleaseIndex>) -> Result<Upserted> {
    let existing: Option<HarvestRow> = c
        .query_row(&format!("SELECT {ROW_COLS} FROM harvest_items WHERE url = ?1"), [&release.url], HarvestRow::from_row)
        .optional()?;
    let is_new = existing.is_none();
    let mut item = existing.unwrap_or_else(|| HarvestRow {
        url: release.url.clone(),
        state: "new".into(),
        tags: "[]".into(),
        is_purchasable: true,
        discovered_at: bc_jobs::time::now(),
        ..Default::default()
    });

    item.url_kind = release.item_type.clone();
    // A shallow record must never blank out fields a full fetch already filled.
    if !release.title.is_empty() {
        item.title = release.title.clone();
    }
    if !release.artist_name.is_empty() {
        item.artist_name = release.artist_name.clone();
    }
    // The caller's label wins over the page's own silence, but never over a label the release
    // itself states.
    item.label_name = release
        .label_name
        .clone()
        .filter(|l| !l.is_empty())
        .or_else(|| item.label_name.clone().filter(|l| !l.is_empty()))
        .or_else(|| o.label_name.map(str::to_string));
    item.art_url = release.art_url.clone().filter(|a| !a.is_empty()).or(item.art_url);
    item.release_date = release.release_date.clone().filter(|a| !a.is_empty()).or(item.release_date);
    if release.track_count() > 0 {
        item.track_count = Some(release.track_count() as i64);
    }
    let mut tags_changed = false;
    if !release.tags.is_empty() && (!release.shallow || matches!(item.tags.as_str(), "" | "[]")) {
        // A shallow record's tags are a stamp from the query that found it -- good enough to fill a
        // blank row, never to overwrite the release page's own tags.
        item.tags = serde_json::to_string(&release.tags).unwrap_or_else(|_| "[]".into());
        tags_changed = true;
    }
    item.bc_item_id = release.bc_item_id.or(item.bc_item_id);
    item.band_id = release.band_id.or(item.band_id);
    if is_new || o.claim_source {
        item.source_kind = Some(o.source_kind.to_string());
        item.source_label = Some(o.source_label.to_string());
    }
    item.in_collection |= o.in_collection;
    item.in_wishlist |= o.in_wishlist;
    item.is_free_download |= release.is_free_download;
    item.is_purchasable = release.is_purchasable;
    item.is_preorder = release.is_preorder;
    item.extract_tier = Some(release.tier.as_str().to_string());

    // Already downloaded? Match on the canonical URL first -- that is exact. `queued` is re-resolved
    // too: it is the one state nothing else can correct.
    if is_new || matches!(item.state.as_str(), "new" | "in_library" | "queued" | "downloaded") {
        let mut matched: Option<i64> =
            c.query_row("SELECT id FROM releases WHERE bandcamp_url = ?1", [&release.url], |r| r.get(0)).optional()?;
        // Nothing recorded a URL for releases scanned in off disk: fall back to (artist, title).
        if matched.is_none() {
            if let Some(index) = index {
                if let Some(rid) = take(index, &item.artist_name, &item.title) {
                    let has_url: Option<Option<String>> =
                        c.query_row("SELECT bandcamp_url FROM releases WHERE id = ?1", [rid], |r| r.get(0)).optional()?;
                    if let Some(url) = has_url {
                        matched = Some(rid);
                        if url.is_none() {
                            // Repair the row while we know the answer so the exact path covers it from here on.
                            c.execute("UPDATE releases SET bandcamp_url = ?2 WHERE id = ?1", params![rid, release.url])?;
                        }
                    }
                }
            }
        }
        if let Some(rid) = matched {
            item.state = "in_library".into();
            item.release_id = Some(rid);
        } else if item.state == "in_library" {
            item.state = "new".into();
        }
    }

    let id = if is_new {
        c.execute(
            "INSERT INTO harvest_items (url, url_kind, state, title, artist_name, label_name, art_url, release_date, \
                track_count, tags, bc_item_id, band_id, source_kind, source_label, in_collection, in_wishlist, \
                is_free_download, is_purchasable, is_preorder, extract_tier, release_id, discovered_at, resolved_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
            params![
                item.url, item.url_kind, item.state, item.title, item.artist_name, item.label_name, item.art_url,
                item.release_date, item.track_count, item.tags, item.bc_item_id, item.band_id, item.source_kind,
                item.source_label, item.in_collection, item.in_wishlist, item.is_free_download, item.is_purchasable,
                item.is_preorder, item.extract_tier, item.release_id, item.discovered_at, item.resolved_at
            ],
        )?;
        c.last_insert_rowid()
    } else {
        c.execute(
            "UPDATE harvest_items SET url_kind=?2, state=?3, title=?4, artist_name=?5, label_name=?6, art_url=?7, \
                release_date=?8, track_count=?9, tags=?10, bc_item_id=?11, band_id=?12, source_kind=?13, source_label=?14, \
                in_collection=?15, in_wishlist=?16, is_free_download=?17, is_purchasable=?18, is_preorder=?19, \
                extract_tier=?20, release_id=?21 WHERE id=?1",
            params![
                item.id, item.url_kind, item.state, item.title, item.artist_name, item.label_name, item.art_url,
                item.release_date, item.track_count, item.tags, item.bc_item_id, item.band_id, item.source_kind,
                item.source_label, item.in_collection, item.in_wishlist, item.is_free_download, item.is_purchasable,
                item.is_preorder, item.extract_tier, item.release_id
            ],
        )?;
        item.id
    };
    if is_new || tags_changed {
        sync_tags(c, id, &item.tag_list())?;
    }
    Ok(Upserted { id, is_new, state: item.state })
}

/// Called inside the flush transaction with `(release, item id, is_new)` for every row the batch
/// upserted, after the rows have ids. What a caller that records something *about* each row
/// (which wishlist it is on) hangs off, so that bookkeeping commits atomically with the rows.
pub type BatchHook = Arc<dyn Fn(&Connection, &[(HarvestedRelease, i64, bool)]) -> Result<()> + Send + Sync>;

#[derive(Clone)]
pub struct AbsorbOpts {
    pub source_kind: String,
    pub source_label: String,
    /// The imprint every release in this stream belongs to (a label run).
    pub label_name: Option<String>,
    /// `in_wishlist` is the sticky "on MY wishlist" flag. `None` = `source_kind == "wishlist"`.
    pub mark_wishlist: Option<bool>,
    pub claim_source: bool,
    pub on_batch: Option<BatchHook>,
}

impl AbsorbOpts {
    pub fn new(source_kind: impl Into<String>, source_label: impl Into<String>) -> Self {
        Self {
            source_kind: source_kind.into(),
            source_label: source_label.into(),
            label_name: None,
            mark_wishlist: None,
            claim_source: true,
            on_batch: None,
        }
    }
}

/// What [`absorb`] hands back: the counts plus how the stream ended. A stream that raised or was
/// cancelled still has its partial batch flushed -- stopping a walk means "stop fetching", not
/// "forget what you fetched".
#[derive(Debug, Default)]
pub struct Absorbed {
    pub counts: AbsorbCounts,
    pub error: Option<HarvestError>,
    pub cancelled: bool,
}

type Progress<'a> = Option<&'a (dyn Fn(i64, Option<i64>) + Send + Sync)>;

/// Drain a source stream into the inbox, flushing in batches of [`FLUSH_EVERY`]. `on_progress` is
/// called with `(seen, total)` as batches land.
pub async fn absorb(db: &Db, mut stream: EventStream, opts: &AbsorbOpts, on_progress: Progress<'_>, cancel: &CancellationToken) -> Absorbed {
    let mut out = Absorbed::default();
    // Built once per run, not per flush: it also carries "already claimed" state between batches.
    let index = match db.read_async(build_release_index).await {
        Ok(i) => Arc::new(Mutex::new(i)),
        Err(e) => {
            out.error = Some(e.into());
            return out;
        }
    };
    let mut pending: Vec<HarvestedRelease> = Vec::new();
    let mut total: Option<i64> = None;

    loop {
        let next = tokio::select! {
            ev = stream.next() => ev,
            _ = cancel.cancelled() => { out.cancelled = true; None }
        };
        let Some(ev) = next else { break };
        let ev = match ev {
            Ok(e) => e,
            Err(e) => {
                out.error = Some(e);
                break;
            }
        };
        if ev.total.is_some() {
            total = ev.total;
        }
        if let Some(err) = ev.error {
            out.counts.errors.push(err.chars().take(300).collect());
            continue;
        }
        let Some(release) = ev.release else { continue };
        out.counts.seen += 1;
        *out.counts.tier_counts.entry(release.tier.as_str().to_string()).or_insert(0) += 1;
        pending.push(release);
        if pending.len() >= FLUSH_EVERY {
            if let Err(e) = flush(db, std::mem::take(&mut pending), &mut out.counts, opts, &index).await {
                out.error = Some(e);
                break;
            }
            if let Some(p) = on_progress {
                p(out.counts.seen, total);
            }
        }
    }
    // Always flush the partial batch (cancellation, error or end of stream).
    if !pending.is_empty() {
        if let Err(e) = flush(db, std::mem::take(&mut pending), &mut out.counts, opts, &index).await {
            out.error.get_or_insert(e);
        }
    }
    if let Some(p) = on_progress {
        p(out.counts.seen, total);
    }
    out
}

/// Upsert one batch in a single write transaction and fold it into `counts`.
pub async fn flush(
    db: &Db,
    batch: Vec<HarvestedRelease>,
    counts: &mut AbsorbCounts,
    opts: &AbsorbOpts,
    index: &Arc<Mutex<ReleaseIndex>>,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let wishlist_flag = opts.mark_wishlist.unwrap_or(opts.source_kind == "wishlist");
    let opts = opts.clone();
    let index = index.clone();
    let (rows, delta) = db
        .write_async(move |tx| {
            let mut idx = index.lock();
            let mut rows: Vec<(HarvestedRelease, i64, bool)> = Vec::with_capacity(batch.len());
            let mut d = AbsorbCounts::default();
            let mut pending_ids = Vec::new();
            for release in batch {
                let u = upsert(
                    tx,
                    &release,
                    &UpsertOpts {
                        source_kind: &opts.source_kind,
                        source_label: &opts.source_label,
                        label_name: opts.label_name.as_deref(),
                        in_collection: opts.source_kind == "collection",
                        in_wishlist: wishlist_flag,
                        claim_source: opts.claim_source,
                    },
                    Some(&mut idx),
                )
                .map_err(|e| bc_db::DbError::Other(e.to_string()))?;
                if u.is_new {
                    d.new += 1;
                } else {
                    d.already_known += 1;
                }
                match u.state.as_str() {
                    "in_library" => d.in_library += 1,
                    "queued" => d.queued += 1,
                    "new" => pending_ids.push(u.id),
                    _ => {}
                }
                rows.push((release, u.id, u.is_new));
            }
            d.pending_ids = pending_ids;
            if let Some(hook) = &opts.on_batch {
                hook(tx, &rows).map_err(|e| bc_db::DbError::Other(e.to_string()))?;
            }
            Ok((rows.len(), d))
        })
        .await?;
    let _ = rows;
    counts.new += delta.new;
    counts.already_known += delta.already_known;
    counts.in_library += delta.in_library;
    counts.queued += delta.queued;
    counts.pending_ids.extend(delta.pending_ids);
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct QueueOpts {
    pub allow_unowned: bool,
    /// `None` groups the batch under the source label; `Some("")` writes straight into the downloads root.
    pub target_subdir: Option<String>,
    /// Queue rows the library already has and force the download so the worker's preflight does not
    /// re-skip them (building a self-contained folder).
    pub include_in_library: bool,
    /// Download the batch flat, without the `<artist>/<album>` nesting.
    pub single_folder: bool,
    /// File what this job downloads on that fan's shelf rather than in my library.
    pub source_fan_id: Option<i64>,
    pub label: Option<String>,
}

/// Create a download job for whatever in `candidates` is in scope (`queue`). Owned or free
/// releases are always in scope; anything else needs `allow_unowned` (listed in
/// `needs_confirmation`). The enqueue and the `new` -> `queued` flips are one transaction.
pub fn queue(db: &Db, jobs: &JobStore, candidates: &[HarvestRow], o: &QueueOpts) -> Result<QueueOutcome> {
    let candidates = candidates.to_vec();
    let o = o.clone();
    let (outcome, job) = db.write(move |tx| queue_in(tx, &candidates, &o).map_err(|e| bc_db::DbError::Other(e.to_string())))?;
    if let Some(j) = &job {
        jobs.announce_created(j);
    }
    Ok(outcome)
}

fn queue_in(tx: &Transaction<'_>, candidates: &[HarvestRow], o: &QueueOpts) -> Result<(QueueOutcome, Option<bc_jobs::Job>)> {
    let mut outcome = QueueOutcome::default();
    let mut queued: Vec<&HarvestRow> = Vec::new();
    let mut held_elsewhere: Vec<i64> = Vec::new();

    // One pair of queries for the batch rather than one per item.
    let keys: Vec<String> = candidates.iter().filter(|i| !i.url.is_empty()).map(|i| url_key(&i.url)).collect();
    let blocked_urls = bc_maint::blacklist::blocked_url_keys(tx, &keys).map_err(ae)?;
    let pairs: Vec<(String, String)> =
        candidates.iter().map(|i| (name_key(&i.artist_name), name_key(&i.title))).collect();
    let blocked_pairs = bc_maint::blacklist::blocked_names(tx, &pairs).map_err(ae)?;

    for item in candidates {
        if item.state == "in_library" && !o.include_in_library {
            outcome.skipped_in_library += 1;
            if let Some(r) = item.release_id {
                held_elsewhere.push(r);
            }
            continue;
        }
        // Thrown away on purpose once already: the only gate applied before ownership.
        let pair = (name_key(&item.artist_name), name_key(&item.title));
        if blocked_urls.contains(&url_key(&item.url)) || (!pair.0.is_empty() && !pair.1.is_empty() && blocked_pairs.contains(&pair)) {
            outcome.skipped_blacklisted += 1;
            continue;
        }
        // Owned or freely offered is always in scope; anything else needs an explicit flag.
        let in_scope = item.in_collection || item.is_free_download;
        if !in_scope && !o.allow_unowned {
            outcome.needs_confirmation.push(item.url.clone());
            continue;
        }
        queued.push(item);
    }

    // "I already have this" is only half an answer when the copy sits on another person's shelf.
    if o.source_fan_id.is_none() && !held_elsewhere.is_empty() {
        outcome.adopted = bc_maint::adopt::adopt_releases(tx, &held_elsewhere).map_err(ae)? as i64;
    }
    if queued.is_empty() {
        return Ok((outcome, None));
    }

    let subdir = o
        .target_subdir
        .clone()
        .or_else(|| queued[0].source_label.clone())
        .filter(|s| !s.is_empty());

    let mut params = serde_json::json!({"source": "harvest"});
    if o.single_folder {
        params["layout"] = "flat".into();
    }
    if o.include_in_library {
        // Without force the worker's preflight would skip every in-library URL right back out.
        params["force"] = true.into();
    }
    if let Some(f) = o.source_fan_id {
        params["source_fan_id"] = f.into();
    }
    let label = o.label.clone().or_else(|| subdir.clone()).unwrap_or_else(|| format!("{} from inbox", queued.len()));
    let nj = NewJob::new(
        bc_types::jobs::KIND_DOWNLOAD,
        queued
            .iter()
            .map(|i| NewItem {
                url: Some(i.url.clone()),
                url_kind: Some(i.url_kind.clone()),
                source: i.source_kind.clone(),
                target_dir: subdir.clone(),
                ..Default::default()
            })
            .collect(),
    )
    .label(label)
    .params(params);
    let job = bc_jobs::create_job_in(tx, &nj)?;

    let now = bc_jobs::time::now();
    for item in &queued {
        tx.execute("UPDATE harvest_items SET state = 'queued', resolved_at = ?2 WHERE id = ?1", params![item.id, now])?;
    }
    outcome.queued = queued.len() as i64;
    outcome.job_id = Some(job.id.clone());
    Ok((outcome, Some(job)))
}

/// Take the inbox row for `url` out of `queued` -- its download is over (`settle`). Returns whether
/// a row was found, so callers can stay quiet about URLs that never came through the inbox.
pub fn settle(db: &Db, url: &str, release_id: Option<i64>) -> Result<bool> {
    let url = url.to_string();
    Ok(db.write(move |tx| settle_in(tx, &url, release_id).map_err(|e| bc_db::DbError::Other(e.to_string())))?)
}

pub fn settle_in(tx: &Transaction<'_>, url: &str, release_id: Option<i64>) -> Result<bool> {
    let canonical = normalise(url);
    let item: Option<(i64, Option<String>)> = tx
        .query_row(
            "SELECT id, label_name FROM harvest_items WHERE url IN (?1, ?2) ORDER BY (url = ?1) DESC LIMIT 1",
            params![url, canonical],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((item_id, label_name)) = item else { return Ok(false) };

    // An item can finish without ingesting anything (a re-run of something on disk): the release is
    // then already there under this URL.
    let release_id = match release_id {
        Some(r) => Some(r),
        None => tx
            .query_row("SELECT id FROM releases WHERE bandcamp_url IN (?1, ?2) LIMIT 1", params![url, canonical], |r| r.get(0))
            .optional()?,
    };
    let now = bc_jobs::time::now();
    match release_id {
        Some(rid) => {
            tx.execute("UPDATE harvest_items SET state = 'in_library', release_id = ?2, resolved_at = ?3 WHERE id = ?1", params![item_id, rid, now])?;
            // File the fresh release under the label its page stated, now rather than at the next
            // restart or label sweep. Fills an empty label_id only.
            let name = label_name.unwrap_or_default();
            let name = name.trim();
            if !name.is_empty() {
                let fresh_label: Option<Option<i64>> =
                    tx.query_row("SELECT label_id FROM releases WHERE id = ?1", [rid], |r| r.get(0)).optional()?;
                if let Some(None) = fresh_label {
                    if let Some(lid) = crate::download::dedup::get_or_create_label(tx, name)? {
                        tx.execute("UPDATE releases SET label_id = ?2 WHERE id = ?1", params![rid, lid])?;
                    }
                }
            }
        }
        None => {
            tx.execute("UPDATE harvest_items SET state = 'downloaded', resolved_at = ?2 WHERE id = ?1", params![item_id, now])?;
        }
    }
    Ok(true)
}

/// Put queued inbox rows back to `new` -- nothing is going to fetch them (`release`). For the
/// URLs of a job that was cancelled or deleted before it ran them.
pub fn release(c: &Connection, urls: &[String]) -> Result<usize> {
    let mut wanted: Vec<String> = urls.iter().filter(|u| !u.is_empty()).flat_map(|u| [u.clone(), normalise(u)]).collect();
    wanted.sort();
    wanted.dedup();
    let mut count = 0;
    // Well under SQLite's 999-parameter limit; a cancelled wishlist job carries five figures of URLs.
    for chunk in wanted.chunks(400) {
        let ph = vec!["?"; chunk.len()].join(",");
        count += c.execute(
            &format!("UPDATE harvest_items SET state = 'new', resolved_at = NULL WHERE state = 'queued' AND url IN ({ph})"),
            bc_db::rusqlite::params_from_iter(chunk.iter()),
        )?;
    }
    Ok(count)
}

/// Async wrapper for [`release`] (single transaction).
pub async fn release_async(db: &Db, urls: Vec<String>) -> Result<usize> {
    Ok(db.write_async(move |tx| release(tx, &urls).map_err(|e| bc_db::DbError::Other(e.to_string()))).await?)
}

/// `JobHooks` glue: when jobs are removed/cancelled/deleted their unrun URLs go back to the inbox.
pub struct InboxHooks {
    pub ctx: Arc<Ctx>,
}

#[async_trait::async_trait]
impl bc_jobs::JobHooks for InboxHooks {
    async fn release_urls(&self, urls: &[String]) {
        if let Err(e) = release_async(&self.ctx.db, urls.to_vec()).await {
            tracing::warn!("could not release inbox rows: {e}");
        }
    }
}
