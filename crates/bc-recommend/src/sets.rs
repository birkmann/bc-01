//! DJ-set planning: the working pool, set-aware suggestions and automix (ports of `set_pool`,
//! `suggest_for_set`, `automix_set` and the `_slot` / `_snapshot` / `_load_set` / `_detail`
//! helpers of `api/routes/playlists.py`).
//!
//! Set CRUD is WS1's; this module only reads sets (and, for automix, rewrites their items in
//! one transaction). TODO(ws1): [`load_set`] and [`detail`] are stand-ins for WS1's dj-set
//! loaders; the shape is `DjSetDetail`, identical to the legacy `_detail`.

use std::collections::{BTreeSet, HashSet};

use bc_db::rusqlite::{Connection, OptionalExtension, params};
use bc_music::automix::{self, AUTOMIX_MAX, MixTrack};
use bc_music::{ordering, setmath};
use bc_types::Page;
use bc_types::sets::{
    DjSetDetail, PoolPage, PoolQuery, PoolSort, SetSuggestRequest, SetSuggestResponse, SortOrder,
    AutomixRequest,
};
use bc_types::suggest::{SuggestRequest };

use crate::cue::{CueSource, plan_or_default};
use bc_types::library::TrackOut;
use crate::error::{RecommendError, Result};
use crate::hydrate;
use crate::nextup::{self, RunOpts, Seed};
use crate::pool;
use crate::pooling;
use crate::scope::{Scope, ScopeExt};
use crate::sqlutil::{PRESENT, in_list, iso};

// ======================================================================================
// Loading
// ======================================================================================

#[derive(Debug, Clone)]
pub struct SetRow {
    pub id: i64,
    pub name: String,
    pub venue: Option<String>,
    pub event_date: Option<String>,
    pub target_minutes: Option<i64>,
    pub notes: Option<String>,
    pub status: String,
    pub created_at: Option<String>,
    pub pool_sources: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TrackFacts {
    pub title: String,
    pub duration_ms: Option<i64>,
    pub artist_name: Option<String>,
    pub release_id: Option<i64>,
    pub cover_path: Option<String>,
    pub has_analysis: bool,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub energy: Option<f64>,
    pub beat_offset_ms: Option<f64>,
    pub loudness_lufs: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct ItemRow {
    pub id: i64,
    pub track_id: Option<i64>,
    pub position: f64,
    pub cue_in_ms: Option<i64>,
    pub cue_out_ms: Option<i64>,
    pub tempo_adjust_pct: f64,
    pub key_lock: bool,
    pub transition_type: Option<String>,
    pub transition_beats: Option<i64>,
    pub transition_notes: Option<String>,
    pub energy: Option<i64>,
    pub snapshot: serde_json::Map<String, serde_json::Value>,
    /// `None` when the track row is gone (the slot falls back to its snapshot).
    pub track: Option<TrackFacts>,
}

fn snapshot_of(raw: Option<String>) -> serde_json::Map<String, serde_json::Value> {
    match raw.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
        Some(serde_json::Value::Object(m)) => m,
        _ => Default::default(),
    }
}

/// One DJ set with its items in plan order. 404 when the set does not exist.
pub fn load_set(conn: &Connection, set_id: i64) -> Result<(SetRow, Vec<ItemRow>)> {
    let set = conn
        .query_row(
            "SELECT id, name, venue, event_date, target_minutes, notes, status, created_at, pool_sources FROM dj_sets WHERE id = ?1",
            [set_id],
            |r| {
                Ok(SetRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    venue: r.get(2)?,
                    event_date: r.get(3)?,
                    target_minutes: r.get(4)?,
                    notes: r.get(5)?,
                    status: r.get(6)?,
                    created_at: iso(r.get(7)?),
                    pool_sources: r.get(8)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| RecommendError::not_found(format!("set {set_id} not found")))?;

    let mut st = conn.prepare(
        "SELECT i.id, i.track_id, i.position, i.cue_in_ms, i.cue_out_ms, i.tempo_adjust_pct, i.key_lock, i.transition_type, \
                i.transition_beats, i.transition_notes, i.energy, i.snapshot, \
                t.id, t.title, t.duration_ms, ar.name, r.id, r.cover_path, a.track_id, a.bpm, a.camelot, a.energy, a.beat_offset_ms, a.loudness_lufs \
         FROM dj_set_items i \
         LEFT JOIN tracks t ON t.id = i.track_id \
         LEFT JOIN releases r ON r.id = t.release_id \
         LEFT JOIN artists ar ON ar.id = COALESCE(t.artist_id, r.artist_id) \
         LEFT JOIN analysis a ON a.track_id = t.id \
         WHERE i.set_id = ?1 ORDER BY i.position, i.id",
    )?;
    let rows = st.query_map([set_id], |r| {
        let track_exists = r.get::<_, Option<i64>>(12)?.is_some();
        Ok(ItemRow {
            id: r.get(0)?,
            track_id: r.get(1)?,
            position: r.get(2)?,
            cue_in_ms: r.get(3)?,
            cue_out_ms: r.get(4)?,
            tempo_adjust_pct: r.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
            key_lock: r.get::<_, Option<bool>>(6)?.unwrap_or(true),
            transition_type: r.get(7)?,
            transition_beats: r.get(8)?,
            transition_notes: r.get(9)?,
            energy: r.get(10)?,
            snapshot: snapshot_of(r.get(11)?),
            track: if track_exists {
                Some(TrackFacts {
                    title: r.get(13)?,
                    duration_ms: r.get(14)?,
                    artist_name: r.get(15)?,
                    release_id: r.get(16)?,
                    cover_path: r.get(17)?,
                    has_analysis: r.get::<_, Option<i64>>(18)?.is_some(),
                    bpm: r.get(19)?,
                    camelot: r.get(20)?,
                    energy: r.get(21)?,
                    beat_offset_ms: r.get(22)?,
                    loudness_lufs: r.get(23)?,
                })
            } else {
                None
            },
        })
    })?;
    let items = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    Ok((set, items))
}

fn snap_f64(item: &ItemRow, key: &str) -> Option<f64> {
    item.snapshot.get(key).and_then(|v| v.as_f64())
}

fn snap_str(item: &ItemRow, key: &str) -> Option<String> {
    item.snapshot.get(key).and_then(|v| v.as_str()).map(String::from)
}

/// Legacy `_slot`: live data when the track still exists, the snapshot otherwise.
pub fn slot(item: &ItemRow, index: usize) -> setmath::Slot {
    let t = item.track.as_ref();
    let analysis_bpm = t.and_then(|t| t.bpm).filter(|b| *b != 0.0);
    let analysis_cam = t.and_then(|t| t.camelot.clone()).filter(|c| !c.is_empty());
    setmath::Slot {
        index,
        track_id: item.track_id,
        title: t.map(|t| t.title.clone()).filter(|s| !s.is_empty()).or_else(|| snap_str(item, "title")).unwrap_or_default(),
        artist: t.and_then(|t| t.artist_name.clone()).filter(|s| !s.is_empty()).or_else(|| snap_str(item, "artist")).unwrap_or_default(),
        duration_ms: t.and_then(|t| t.duration_ms).filter(|d| *d != 0).or_else(|| snap_f64(item, "duration_ms").map(|d| d as i64)),
        bpm: analysis_bpm.or_else(|| snap_f64(item, "bpm")),
        camelot: analysis_cam.or_else(|| snap_str(item, "camelot")),
        energy: item.energy.filter(|e| *e != 0).or_else(|| snap_f64(item, "energy").map(|e| e as i64)),
        cue_in_ms: item.cue_in_ms,
        cue_out_ms: item.cue_out_ms,
        tempo_adjust_pct: item.tempo_adjust_pct,
        key_lock: item.key_lock,
        transition_type: item.transition_type.clone(),
        transition_beats: item.transition_beats,
        transition_notes: item.transition_notes.clone(),
    }
}

/// The set as `GET /sets/{id}` shows it: WS1's detail builder (`bc_plist::sets::set_detail`).
pub fn detail(conn: &Connection, set_id: i64) -> Result<DjSetDetail> {
    Ok(bc_plist::sets::set_detail(conn, set_id)?)
}

// ======================================================================================
// Pool
// ======================================================================================

/// FTS5 `MATCH` expression for free user text: every term quoted, the last one a prefix.
pub fn fts_escape(query: &str) -> String {
    let terms: Vec<String> = query.replace('"', "\"\"").split_whitespace().map(String::from).collect();
    let Some((last, head)) = terms.split_last() else { return String::new() };
    let mut quoted: Vec<String> = head.iter().map(|t| format!("\"{t}\"")).collect();
    quoted.push(format!("\"{last}\"*"));
    quoted.join(" ")
}

/// `GET /sets/{id}/pool`: the deduped union of the pool sources, minus what is already in the
/// set. Empty sources give an empty page (adding sources is the UI's move, not a silent
/// whole-library fallback).
pub fn pool_page(conn: &Connection, scope: &Scope, set_id: i64, q: &PoolQuery) -> Result<PoolPage<TrackOut>> {
    let (set, items) = load_set(conn, set_id)?;
    let sources = pool::parse_sources(set.pool_sources.as_deref());
    let chips = pool::source_chips(conn, &sources, scope)?;
    let limit = q.limit.clamp(1, 500);
    let offset = q.offset.max(0);
    let empty = |sources| PoolPage {
        page: Page { items: vec![], total: 0, offset, limit },
        total_duration_ms: 0,
        excluded_in_set: 0,
        sources,
        automix_max: AUTOMIX_MAX,
    };
    if sources.is_empty() {
        return Ok(empty(chips));
    }

    let in_set: Vec<i64> = items.iter().filter_map(|i| i.track_id).collect::<BTreeSet<_>>().into_iter().collect();
    let base = format!("FROM tracks t WHERE t.id IN ({}) AND {PRESENT} AND {}", pool::pool_track_ids(&sources), scope.tp());

    let mut excluded_in_set = 0;
    let mut where_extra = String::new();
    if !in_set.is_empty() {
        excluded_in_set = conn.query_row(&format!("SELECT count(*) {base} AND t.id IN ({})", in_list(&in_set)), [], |r| r.get::<_, i64>(0))?;
        where_extra.push_str(&format!(" AND t.id NOT IN ({})", in_list(&in_set)));
    }
    if let Some(text) = q.q.as_deref().filter(|s| !s.is_empty()) {
        let expr = fts_escape(text);
        if !expr.is_empty() {
            let mut st = conn.prepare(
                "SELECT track_id FROM search_index WHERE search_index MATCH ?1 \
                 ORDER BY bm25(search_index, 10.0, 6.0, 4.0, 1.0, 2.0) LIMIT 5000",
            )?;
            let matched: Vec<i64> = st
                .query_map([expr], |r| r.get::<_, i64>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let matched = if matched.is_empty() { vec![-1] } else { matched };
            where_extra.push_str(&format!(" AND t.id IN ({})", in_list(&matched)));
        }
    }
    let (total, total_duration_ms): (i64, i64) = conn.query_row(
        &format!("SELECT count(*), COALESCE(sum(t.duration_ms), 0) {base}{where_extra}"),
        [],
        |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
    )?;

    let dir = if q.order == SortOrder::Desc { "DESC" } else { "ASC" };
    let sql = match q.sort {
        PoolSort::Bpm => format!(
            "SELECT t.id FROM tracks t LEFT JOIN analysis a ON a.track_id = t.id WHERE t.id IN ({}) AND {PRESENT} AND {}{where_extra} \
             ORDER BY (a.bpm IS NULL), a.bpm {dir}, t.id LIMIT {limit} OFFSET {offset}",
            pool::pool_track_ids(&sources),
            scope.tp()
        ),
        // Stable per-seed pseudo-random order, so pages of one shuffle agree.
        PoolSort::Random => format!(
            "SELECT t.id {base}{where_extra} ORDER BY {} LIMIT {limit} OFFSET {offset}",
            pooling::shuffled(q.seed.max(0))
        ),
        PoolSort::Added => format!("SELECT t.id {base}{where_extra} ORDER BY t.added_at {dir}, t.id LIMIT {limit} OFFSET {offset}"),
    };
    let mut st = conn.prepare(&sql)?;
    let ids: Vec<i64> = st.query_map([], |r| r.get::<_, i64>(0))?.collect::<std::result::Result<Vec<_>, _>>()?;
    let tracks = hydrate::briefs_ordered(conn, &ids)?;

    Ok(PoolPage {
        page: Page { items: tracks, total, offset, limit },
        total_duration_ms,
        excluded_in_set,
        sources: chips,
        automix_max: AUTOMIX_MAX,
    })
}

/// The pool statement(s) for `EXPLAIN QUERY PLAN` tests: `(label, sql)`.
#[doc(hidden)]
pub fn pool_statements(set_sources: &[bc_types::sets::PoolSource], scope: &Scope) -> Vec<(String, String)> {
    let mut out = vec![(
        "pool/page".to_string(),
        format!(
            "SELECT t.id FROM tracks t WHERE t.id IN ({}) AND {PRESENT} AND {} ORDER BY t.added_at, t.id LIMIT 100",
            pool::pool_track_ids(set_sources),
            scope.tp()
        ),
    )];
    for (i, s) in set_sources.iter().enumerate() {
        out.push((
            format!("pool/count[{i}]"),
            format!(
                "SELECT count(DISTINCT t.id) FROM tracks t WHERE t.id IN ({}) AND {PRESENT} AND {}",
                pool::source_track_ids(s),
                scope.tp()
            ),
        ));
    }
    out
}

// ======================================================================================
// Suggest
// ======================================================================================

/// `POST /sets/{id}/suggest`: candidates that mix out of a slot, seeded at the slot's
/// *effective* tempo and key and restricted to the set's pool when it has one. Never a 400 for
/// missing analysis.
pub fn suggest(conn: &Connection, scope: &Scope, set_id: i64, body: &SetSuggestRequest) -> Result<SetSuggestResponse<TrackOut>> {
    let (set, items) = load_set(conn, set_id)?;
    let mut index: Option<usize> = None;
    let mut item_id: Option<i64> = None;
    let mut seed = Seed::default();
    let mut seed_track_id: Option<i64> = None;
    if !items.is_empty() {
        let idx = match body.after_item_id {
            Some(after) => items
                .iter()
                .position(|i| i.id == after)
                .ok_or_else(|| RecommendError::not_found(format!("item {after} not in set {set_id}")))?,
            None => items.len() - 1,
        };
        index = Some(idx);
        let item = &items[idx];
        item_id = Some(item.id);
        seed_track_id = item.track_id;
        let s = slot(item, idx);
        seed = Seed { bpm: s.effective_bpm(), camelot: s.effective_camelot(), ..Default::default() };
        if let Some(tid) = item.track_id
            && let Some(row) = nextup::load_seed_row(conn, tid)?
        {
            seed.energy = row.energy;
            seed.tags = row.tags;
            seed.artist_id = row.artist_id;
            seed.release_id = row.release_id;
        }
    }

    let sources = pool::parse_sources(set.pool_sources.as_deref());
    let restrict = if body.use_pool && !sources.is_empty() { Some(pool::pool_track_ids(&sources)) } else { None };

    let request = SuggestRequest {
        direction: body.direction.clone(),
        wishes: body.wishes.clone(),
        exclude_track_ids: items.iter().filter_map(|i| i.track_id).collect::<BTreeSet<_>>().into_iter().collect(),
        limit: body.limit,
        ..Default::default()
    };
    let response = nextup::run(conn, scope, &request, RunOpts { seed: Some(seed), seed_track_id, restrict_sql: restrict.as_deref() })?;
    Ok(SetSuggestResponse { suggest: response, after_index: index, after_item_id: item_id, pool_restricted: restrict.is_some() })
}

// ======================================================================================
// Automix
// ======================================================================================

fn snapshot_json(conn: &Connection, track_id: i64) -> Result<String> {
    let snap = conn
        .query_row(
            "SELECT t.title, ar.name, t.duration_ms, a.bpm, a.camelot, a.energy, a.beat_offset_ms, a.loudness_lufs \
             FROM tracks t LEFT JOIN releases r ON r.id = t.release_id \
             LEFT JOIN artists ar ON ar.id = COALESCE(t.artist_id, r.artist_id) \
             LEFT JOIN analysis a ON a.track_id = t.id WHERE t.id = ?1",
            [track_id],
            |r| {
                let energy: Option<f64> = r.get(5)?;
                Ok(serde_json::json!({
                    "title": r.get::<_, String>(0)?,
                    "artist": r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    "duration_ms": r.get::<_, Option<i64>>(2)?,
                    "bpm": r.get::<_, Option<f64>>(3)?,
                    "camelot": r.get::<_, Option<String>>(4)?,
                    "energy": energy.filter(|e| *e != 0.0).map(|e| setmath::round_to(e * 10.0, 0) as i64),
                    "beat_offset_ms": r.get::<_, Option<f64>>(6)?,
                    "loudness_lufs": r.get::<_, Option<f64>>(7)?,
                }))
            },
        )
        .optional()?
        .unwrap_or_else(|| serde_json::json!({}));
    Ok(snap.to_string())
}

/// `POST /sets/{id}/automix` inside an open write transaction (the connection is the
/// transaction): order candidates for harmonic and tempo flow, write default transitions and
/// cue points, and append them to the set (or rebuild it when `keep_existing` is off).
/// 400 for an empty pool or more than [`AUTOMIX_MAX`] tracks.
pub fn automix_in(conn: &Connection, scope: &Scope, cues: &dyn CueSource, set_id: i64, body: &AutomixRequest) -> Result<DjSetDetail> {
    let (set, items) = load_set(conn, set_id)?;
    let sources = pool::parse_sources(set.pool_sources.as_deref());
    let in_set: BTreeSet<i64> = items.iter().filter_map(|i| i.track_id).collect();

    let mut candidate_ids: BTreeSet<i64> = if !body.track_ids.is_empty() {
        body.track_ids.iter().copied().collect()
    } else if !sources.is_empty() {
        let sql = format!(
            "SELECT t.id FROM tracks t WHERE t.id IN ({}) AND {PRESENT} AND {}",
            pool::pool_track_ids(&sources),
            scope.tp()
        );
        let mut st = conn.prepare(&sql)?;
        st.query_map([], |r| r.get::<_, i64>(0))?.collect::<std::result::Result<BTreeSet<_>, _>>()?
    } else {
        BTreeSet::new()
    };
    if body.keep_existing {
        candidate_ids = candidate_ids.difference(&in_set).copied().collect();
    } else {
        candidate_ids.extend(in_set.iter().copied());
    }
    if candidate_ids.is_empty() {
        return Err(RecommendError::bad_request("nothing to arrange: give track_ids or add pool sources"));
    }
    if candidate_ids.len() > AUTOMIX_MAX {
        return Err(RecommendError::bad_request(format!(
            "automix arranges up to {AUTOMIX_MAX} tracks; narrow the pool or select tracks"
        )));
    }

    let ids: Vec<i64> = candidate_ids.iter().copied().collect();
    let mut mix_tracks: Vec<MixTrack> = vec![];
    for chunk in ids.chunks(crate::sqlutil::IN_CHUNK) {
        let sql = format!(
            "SELECT t.id, t.duration_ms, t.artist_id, r.artist_id, a.bpm, a.camelot FROM tracks t \
             LEFT JOIN releases r ON r.id = t.release_id LEFT JOIN analysis a ON a.track_id = t.id \
             WHERE t.id IN ({}) ORDER BY t.id",
            in_list(chunk)
        );
        let mut st = conn.prepare(&sql)?;
        let rows = st.query_map([], |r| {
            let (ta, ra): (Option<i64>, Option<i64>) = (r.get(2)?, r.get(3)?);
            Ok(MixTrack { track_id: r.get(0)?, duration_ms: r.get(1)?, artist_id: ta.or(ra), bpm: r.get(4)?, camelot: r.get(5)? })
        })?;
        for t in rows {
            mix_tracks.push(t?);
        }
    }

    let kept: &[ItemRow] = if body.keep_existing { &items } else { &[] };
    let anchor = kept.last().map(|last| {
        let tail = slot(last, kept.len() - 1);
        MixTrack {
            track_id: last.track_id.unwrap_or(-1),
            bpm: tail.effective_bpm(),
            camelot: tail.effective_camelot(),
            duration_ms: tail.duration_ms,
            artist_id: None,
        }
    });

    let ordered_ids = automix::arrange(&mix_tracks, anchor.as_ref(), body.start_track_id);
    let by_id: std::collections::HashMap<i64, &MixTrack> = mix_tracks.iter().map(|t| (t.track_id, t)).collect();
    let ordered: Vec<MixTrack> = ordered_ids.iter().filter_map(|id| by_id.get(id).map(|t| (*t).clone())).collect();
    let planned = automix::plan_transitions(&ordered, anchor.as_ref());

    let mut mix_points = std::collections::HashMap::new();
    if body.write_cues {
        for t in &ordered {
            mix_points.insert(t.track_id, plan_or_default(cues, t.track_id, t.duration_ms, t.bpm));
        }
    }

    // Trim to the target: stop after the slot that reaches it. With a kept head the clock starts
    // at the head's real total; without one, at zero, and at least one track always lands.
    let target_minutes = body.target_minutes.or(set.target_minutes);
    let mut keep_n = ordered.len();
    if let Some(minutes) = target_minutes.filter(|m| *m != 0) {
        let target_ms = minutes * 60_000;
        let mut clock = 0i64;
        if !kept.is_empty() {
            let head: Vec<setmath::Slot> = kept.iter().enumerate().map(|(n, i)| slot(i, n)).collect();
            clock = setmath::summarise(&head, None).total_ms;
        }
        keep_n = 0;
        for (i, t) in ordered.iter().enumerate() {
            if clock >= target_ms {
                break;
            }
            let mp = mix_points.get(&t.track_id);
            let cue_in = mp.map(|m| m.cue_in_ms).unwrap_or(0);
            let cue_out = mp.and_then(|m| m.cue_out_ms).unwrap_or(t.duration_ms.unwrap_or(0));
            let overlap = setmath::overlap_ms(planned[i].transition_beats, t.bpm);
            clock += (cue_out - cue_in).max(0) - overlap;
            keep_n = i + 1;
        }
        if kept.is_empty() {
            keep_n = keep_n.max(1);
        }
    }
    let chosen = &ordered[..keep_n];

    // Everything below runs in the caller's transaction: a failure part-way must not leave the
    // set half-deleted.
    if !body.keep_existing {
        conn.execute("DELETE FROM dj_set_items WHERE set_id = ?1", [set_id])?;
    }
    let mut position = kept.last().map(|i| i.position);
    for (i, t) in chosen.iter().enumerate() {
        let p = ordering::append_position(position);
        position = Some(p);
        let mp = mix_points.get(&t.track_id);
        let snapshot = snapshot_json(conn, t.track_id)?;
        conn.execute(
            "INSERT INTO dj_set_items(set_id, track_id, position, cue_in_ms, cue_out_ms, tempo_adjust_pct, key_lock, \
                                      transition_type, transition_beats, snapshot) \
             VALUES (?1, ?2, ?3, ?4, ?5, 0.0, 1, ?6, ?7, ?8)",
            params![
                set_id,
                t.track_id,
                p,
                mp.and_then(|m| (m.cue_in_ms != 0).then_some(m.cue_in_ms)),
                mp.and_then(|m| m.cue_out_ms),
                planned[i].transition_type,
                planned[i].transition_beats,
                snapshot
            ],
        )?;
    }
    conn.execute("UPDATE dj_sets SET updated_at = strftime('%Y-%m-%d %H:%M:%f', 'now') WHERE id = ?1", [set_id])?;

    detail(conn, set_id)
}

#[allow(dead_code)]
fn _hs(_: HashSet<i64>) {}
