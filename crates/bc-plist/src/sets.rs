//! `/sets...`: DJ-set storage CRUD.
//!
//! A set is `dj_sets` plus ordered `dj_set_items`. Each item freezes a **snapshot** of its track
//! (title, artist, duration, bpm, key, energy, beat offset, loudness, cover) when it is added, so
//! the plan survives the track or its file being deleted: the slot then renders from the
//! snapshot with `missing = true`. Live data wins while the track exists.
//!
//! Timing, key/tempo verdicts and the summary are computed by `bc_music::setmath`; this module
//! only gathers the slots. `/sets/{id}/pool|suggest|automix` and offline render are served
//! elsewhere.

use std::collections::{HashMap, HashSet};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, patch, post};
use bc_db::rusqlite::{Connection, OptionalExtension, params};
use bc_db::util::{iso, now_db};
use bc_libcore::hydrate::art_url;
use bc_libcore::{ApiError, ApiResult, Q};
use bc_music::setmath::{self, Slot};
use bc_types::library::{ExportFormat, Page, PlaylistAddTracks, TrackOut};
use bc_types::sets::{
    DjSetCreate, DjSetDetail, DjSetListOut, DjSetOut, DjSetUpdate, MAX_EXPLICIT_TRACKS, MoveItem, PoolSource,
    PoolSourceKind, SetItemOut, SetItemUpdate,
};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::lane::{self, Lane};
use crate::playlists::{hydrate_items, playlist_items, require_tracks};
use crate::{J, PlistState, export};

pub const STATUSES: [&str; 4] = ["draft", "ready", "performed", "archived"];

pub fn routes() -> Router<PlistState> {
    Router::new()
        .route("/sets", get(list).post(create))
        .route("/sets/{id}", get(get_one).patch(update).delete(delete))
        .route("/sets/{id}/items", post(add_items))
        .route("/sets/{id}/items/{item_id}", patch(update_item).delete(delete_item))
        .route("/sets/{id}/items/{item_id}/move", post(move_item))
        .route("/sets/{id}/tracks", get(tracks))
        .route("/sets/{id}/export", get(export_set))
}

// ---------------------------------------------------------------------------------------
// Pool sources (the stored JSON column)
// ---------------------------------------------------------------------------------------

fn source_valid(s: &PoolSource) -> bool {
    match s.kind {
        PoolSourceKind::Tag => s.tag.as_deref().is_some_and(|t| !t.is_empty()),
        PoolSourceKind::Playlist => s.playlist_id.is_some_and(|i| i != 0),
        PoolSourceKind::Label => s.label_id.is_some_and(|i| i != 0),
        PoolSourceKind::Artist => s.artist_id.is_some_and(|i| i != 0),
        PoolSourceKind::Tracks => !s.track_ids.is_empty() && s.track_ids.len() <= MAX_EXPLICIT_TRACKS,
        PoolSourceKind::Loved => s.track_ids.len() <= MAX_EXPLICIT_TRACKS,
    }
}

/// Tolerant read of the stored column: bad JSON or bad entries are skipped, never an error.
pub fn parse_sources(raw: Option<&str>) -> Vec<PoolSource> {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else { return vec![] };
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(raw) else { return vec![] };
    entries
        .into_iter()
        .filter_map(|e| serde_json::from_value::<PoolSource>(e).ok())
        .filter(source_valid)
        .collect()
}

/// The column's JSON (defaults omitted).
pub fn dump_sources(sources: &[PoolSource]) -> String {
    serde_json::to_string(sources).unwrap_or_else(|_| "[]".into())
}

fn checked_sources(sources: &[PoolSource]) -> ApiResult<()> {
    if sources.iter().all(source_valid) {
        Ok(())
    } else {
        Err(ApiError::bad("a pool source is missing the reference its kind needs (or lists more than 1000 tracks)"))
    }
}

// ---------------------------------------------------------------------------------------
// Snapshots
// ---------------------------------------------------------------------------------------

/// Freeze what a set needs from each track so it survives the file being deleted. Unknown ids
/// get no entry (their snapshot is `{}`).
pub fn snapshots(c: &Connection, ids: &[i64]) -> ApiResult<HashMap<i64, Value>> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let json = serde_json::to_string(ids).map_err(ApiError::internal)?;
    let mut st = c.prepare_cached(
        "SELECT t.id, t.title, COALESCE(ta.name, ra.name, ''), t.duration_ms, an.bpm, an.camelot, an.energy,
                an.beat_offset_ms, an.loudness_lufs, r.id, r.cover_path IS NOT NULL, aw.version
           FROM tracks t
           LEFT JOIN artists ta ON ta.id = t.artist_id
           LEFT JOIN releases r ON r.id = t.release_id
           LEFT JOIN artists ra ON ra.id = r.artist_id
           LEFT JOIN artwork aw ON aw.release_id = r.id
           LEFT JOIN analysis an ON an.track_id = t.id
          WHERE t.id IN (SELECT value FROM json_each(?1))",
    )?;
    let rows = st.query_map([&json], |r| {
        let energy: Option<f64> = r.get(6)?;
        let release_id: Option<i64> = r.get(9)?;
        let has_cover: bool = r.get::<_, Option<bool>>(10)?.unwrap_or(false);
        let version: Option<String> = r.get(11)?;
        let art = release_id.filter(|_| has_cover).map(|rid| art_url(rid, version.as_deref(), true));
        let mut m = Map::new();
        m.insert("title".into(), Value::from(r.get::<_, String>(1)?));
        m.insert("artist".into(), Value::from(r.get::<_, String>(2)?));
        m.insert("duration_ms".into(), json_opt(r.get::<_, Option<i64>>(3)?));
        m.insert("bpm".into(), json_opt(r.get::<_, Option<f64>>(4)?));
        m.insert("camelot".into(), json_opt(r.get::<_, Option<String>>(5)?));
        // Python's round() is banker's rounding.
        m.insert(
            "energy".into(),
            json_opt(energy.filter(|e| *e != 0.0).map(|e| (e * 10.0).round_ties_even() as i64)),
        );
        m.insert("beat_offset_ms".into(), json_opt(r.get::<_, Option<f64>>(7)?));
        m.insert("loudness_lufs".into(), json_opt(r.get::<_, Option<f64>>(8)?));
        m.insert("art_url".into(), json_opt(art));
        Ok((r.get::<_, i64>(0)?, Value::Object(m)))
    })?;
    for r in rows {
        let (id, v) = r?;
        out.insert(id, v);
    }
    Ok(out)
}

fn json_opt<T: Into<Value>>(v: Option<T>) -> Value {
    v.map_or(Value::Null, Into::into)
}

fn snapshot_text(snaps: &HashMap<i64, Value>, track_id: i64) -> String {
    snaps.get(&track_id).map_or_else(|| "{}".to_string(), Value::to_string)
}

// ---------------------------------------------------------------------------------------
// Detail
// ---------------------------------------------------------------------------------------

/// Python's `a or b` for strings: empty counts as absent.
fn or_str(a: Option<String>, b: Option<String>) -> Option<String> {
    a.filter(|s| !s.is_empty()).or(b.filter(|s| !s.is_empty()))
}
fn or_i64(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    a.filter(|v| *v != 0).or(b.filter(|v| *v != 0))
}
fn or_f64(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    a.filter(|v| *v != 0.0).or(b.filter(|v| *v != 0.0))
}

struct Header {
    set: DjSetOut,
    pool_sources: Vec<PoolSource>,
}

fn load_header(c: &Connection, id: i64) -> ApiResult<(Header, Option<i64>)> {
    c.query_row(
        "SELECT name, venue, event_date, target_minutes, notes, status, pool_sources, created_at FROM dj_sets WHERE id = ?1",
        [id],
        |r| {
            let target: Option<i64> = r.get(3)?;
            let pools: Option<String> = r.get(6)?;
            Ok((
                Header {
                    set: DjSetOut {
                        id,
                        name: r.get(0)?,
                        venue: r.get(1)?,
                        event_date: r.get(2)?,
                        target_minutes: target,
                        notes: r.get(4)?,
                        status: r.get(5)?,
                        summary: Default::default(),
                        created_at: Some(iso(&r.get::<_, String>(7)?)),
                    },
                    pool_sources: parse_sources(pools.as_deref()),
                },
                target,
            ))
        },
    )
    .optional()?
    .ok_or_else(|| ApiError::not_found(format!("set {id} not found")))
}

/// The full set: slots with timing, summary and transition verdicts (one items query).
pub fn set_detail(c: &Connection, id: i64) -> ApiResult<DjSetDetail> {
    let (header, target) = load_header(c, id)?;
    let mut st = c.prepare_cached(
        "SELECT i.id, i.track_id, i.cue_in_ms, i.cue_out_ms, i.tempo_adjust_pct, i.key_lock, i.transition_type,
                i.transition_beats, i.transition_notes, i.energy, i.snapshot,
                t.id, t.title, COALESCE(ta.name, ra.name), t.duration_ms, an.track_id, an.bpm, an.camelot,
                an.beat_offset_ms, an.loudness_lufs, r.id, r.cover_path IS NOT NULL, aw.version, t.available
           FROM dj_set_items i
           LEFT JOIN tracks t ON t.id = i.track_id
           LEFT JOIN artists ta ON ta.id = t.artist_id
           LEFT JOIN releases r ON r.id = t.release_id
           LEFT JOIN artists ra ON ra.id = r.artist_id
           LEFT JOIN artwork aw ON aw.release_id = r.id
           LEFT JOIN analysis an ON an.track_id = t.id
          WHERE i.set_id = ?1 ORDER BY i.position, i.id",
    )?;
    let mut rows = st.query([id])?;
    let mut slots: Vec<Slot> = Vec::new();
    let mut items: Vec<SetItemOut> = Vec::new();
    while let Some(r) = rows.next()? {
        let index = slots.len();
        let snap: Value = r
            .get::<_, Option<String>>(10)?
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(Value::is_object)
            .unwrap_or(Value::Null);
        let s_str = |k: &str| snap.get(k).and_then(Value::as_str).map(str::to_string);
        let s_i64 = |k: &str| snap.get(k).and_then(Value::as_i64);
        let s_f64 = |k: &str| snap.get(k).and_then(Value::as_f64);

        let item_id: i64 = r.get(0)?;
        let track_id: Option<i64> = r.get(1)?;
        let live: Option<i64> = r.get(11)?;
        let has_analysis = r.get::<_, Option<i64>>(15)?.is_some();
        let item_energy: Option<i64> = r.get(9)?;
        let release_id: Option<i64> = r.get(20)?;
        let has_cover: bool = r.get::<_, Option<bool>>(21)?.unwrap_or(false);
        let version: Option<String> = r.get(22)?;
        let available: bool = r.get::<_, Option<i64>>(23)?.unwrap_or(0) != 0;

        let slot = Slot {
            index,
            track_id,
            // Live data when the track still exists; the snapshot otherwise.
            title: or_str(r.get(12)?, s_str("title")).unwrap_or_default(),
            artist: or_str(r.get(13)?, s_str("artist")).unwrap_or_default(),
            duration_ms: or_i64(r.get(14)?, s_i64("duration_ms")),
            bpm: or_f64(r.get(16)?, s_f64("bpm")),
            camelot: or_str(r.get(17)?, s_str("camelot")),
            energy: or_i64(item_energy, s_i64("energy")),
            cue_in_ms: r.get(2)?,
            cue_out_ms: r.get(3)?,
            tempo_adjust_pct: r.get(4)?,
            key_lock: r.get(5)?,
            transition_type: r.get(6)?,
            transition_beats: r.get(7)?,
            transition_notes: r.get(8)?,
        };
        let art = if live.is_some() {
            release_id.filter(|_| has_cover).map(|rid| art_url(rid, version.as_deref(), true))
        } else {
            s_str("art_url")
        };
        items.push(SetItemOut {
            id: item_id,
            index,
            track_id,
            title: slot.title.clone(),
            artist: slot.artist.clone(),
            duration_ms: slot.duration_ms,
            played_ms: slot.played_ms(),
            start_ms: 0,
            bpm: slot.bpm,
            effective_bpm: slot.effective_bpm(),
            camelot: slot.camelot.clone(),
            effective_camelot: slot.effective_camelot(),
            energy: slot.energy,
            cue_in_ms: slot.cue_in_ms,
            cue_out_ms: slot.cue_out_ms,
            tempo_adjust_pct: slot.tempo_adjust_pct,
            key_lock: slot.key_lock,
            transition_type: slot.transition_type.clone(),
            transition_beats: slot.transition_beats,
            transition_notes: slot.transition_notes.clone(),
            art_url: art,
            // The track is gone, or none of its files is on disk.
            missing: live.is_none() || !available,
            beat_offset_ms: if has_analysis { r.get(18)? } else { s_f64("beat_offset_ms") },
            loudness_lufs: if has_analysis { r.get(19)? } else { s_f64("loudness_lufs") },
        });
        slots.push(slot);
    }
    let summary = setmath::summarise(&slots, target);
    for (item, start) in items.iter_mut().zip(setmath::start_times(&slots)) {
        item.start_ms = start;
    }
    let mut set = header.set;
    set.summary = setmath::summary_out(&summary);
    Ok(DjSetDetail {
        set,
        items,
        transitions: summary.transitions.iter().map(setmath::transition_out).collect(),
        pool_sources: header.pool_sources,
    })
}

/// Cards for the list page: counts, cue-span durations, mean tempo and four covers per set in
/// three aggregate queries however many sets exist (never a per-set item load).
pub fn list_sets(c: &Connection) -> ApiResult<Vec<DjSetListOut>> {
    struct Agg {
        n: i64,
        dur: i64,
        bpm: Option<f64>,
    }
    let mut aggs: HashMap<i64, Agg> = HashMap::new();
    {
        // Cue span per item, falling back through the live track to the snapshot for tracks whose
        // file was deleted. Two-argument max() clamps a cue_out that sits before cue_in.
        let mut st = c.prepare_cached(
            "SELECT i.set_id, COUNT(*),
                    COALESCE(SUM(MAX(0, COALESCE(i.cue_out_ms, t.duration_ms, json_extract(i.snapshot, '$.duration_ms'), 0)
                                        - COALESCE(i.cue_in_ms, 0))), 0),
                    AVG(an.bpm)
               FROM dj_set_items i
               LEFT JOIN tracks t ON t.id = i.track_id
               LEFT JOIN analysis an ON an.track_id = t.id
              GROUP BY i.set_id",
        )?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, Agg { n: r.get(1)?, dur: r.get(2)?, bpm: r.get(3)? })))?;
        for r in rows {
            let (id, a) = r?;
            aggs.insert(id, a);
        }
    }
    // First few covers per set in one window query: numbering inside the partition keeps this from
    // dragging back every item of a 100-track set to use four covers of it.
    let mut covers: HashMap<i64, Vec<String>> = HashMap::new();
    {
        let mut seen: HashMap<i64, HashSet<i64>> = HashMap::new();
        let mut st = c.prepare_cached(
            "SELECT set_id, release_id, version FROM (
                SELECT i.set_id AS set_id, r.id AS release_id, aw.version AS version,
                       ROW_NUMBER() OVER (PARTITION BY i.set_id ORDER BY i.position, i.id) AS rn
                  FROM dj_set_items i
                  JOIN tracks t ON t.id = i.track_id
                  JOIN releases r ON r.id = t.release_id
                  LEFT JOIN artwork aw ON aw.release_id = r.id
                 WHERE r.cover_path IS NOT NULL)
              WHERE rn <= 8 ORDER BY set_id, rn",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let (set_id, rid, version): (i64, i64, Option<String>) = (r.get(0)?, r.get(1)?, r.get(2)?);
            let bucket = covers.entry(set_id).or_default();
            if bucket.len() >= 4 || !seen.entry(set_id).or_default().insert(rid) {
                continue;
            }
            bucket.push(art_url(rid, version.as_deref(), true));
        }
    }
    let mut st = c.prepare_cached(
        "SELECT id, name, venue, event_date, target_minutes, status, pool_sources, created_at, updated_at
           FROM dj_sets ORDER BY created_at DESC, id DESC",
    )?;
    let rows = st.query_map([], |r| {
        let id: i64 = r.get(0)?;
        let pools: Option<String> = r.get(6)?;
        let agg = aggs.get(&id);
        Ok(DjSetListOut {
            id,
            name: r.get(1)?,
            venue: r.get(2)?,
            event_date: r.get(3)?,
            target_minutes: r.get(4)?,
            status: r.get(5)?,
            track_count: agg.map_or(0, |a| a.n),
            est_duration_ms: agg.map_or(0, |a| a.dur),
            avg_bpm: agg.and_then(|a| a.bpm).filter(|b| *b != 0.0).map(|b| setmath::round_to(b, 1)),
            art_urls: covers.get(&id).cloned().unwrap_or_default(),
            pool_source_count: parse_sources(pools.as_deref()).len() as i64,
            created_at: Some(iso(&r.get::<_, String>(7)?)),
            updated_at: Some(iso(&r.get::<_, String>(8)?)),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn require_set(c: &Connection, id: i64) -> ApiResult<()> {
    c.query_row("SELECT 1 FROM dj_sets WHERE id = ?1", [id], |_| Ok(()))
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("set {id} not found")))
}

fn touch(c: &Connection, id: i64) -> ApiResult<()> {
    c.execute("UPDATE dj_sets SET updated_at = ?1 WHERE id = ?2", params![now_db(), id])?;
    Ok(())
}

fn require_item(c: &Connection, set_id: i64, item_id: i64) -> ApiResult<()> {
    if lane::SET.contains(c, set_id, item_id)? {
        Ok(())
    } else {
        Err(ApiError::not_found(format!("item {item_id} not in set {set_id}")))
    }
}

// ---------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------

async fn list(State(st): State<PlistState>) -> ApiResult<axum::Json<Vec<DjSetListOut>>> {
    Ok(axum::Json(st.ctx.read_async(list_sets).await?))
}

async fn get_one(State(st): State<PlistState>, Path(id): Path<i64>) -> ApiResult<axum::Json<DjSetDetail>> {
    Ok(axum::Json(st.ctx.read_async(move |c| set_detail(c, id)).await?))
}

async fn create(State(st): State<PlistState>, J(body): J<DjSetCreate>) -> ApiResult<axum::Json<DjSetDetail>> {
    checked_sources(&body.pool_sources)?;
    let resolver = st.resolver.clone();
    let name = body.name.trim();
    let name = if name.is_empty() { "Untitled set".to_string() } else { name.to_string() };
    let detail = st
        .ctx
        .write_async(move |t| {
            let now = now_db();
            t.execute(
                "INSERT INTO dj_sets(name, venue, event_date, target_minutes, notes, status, created_at, updated_at, pool_sources)
                 VALUES (?1, ?2, ?3, ?4, NULL, 'draft', ?5, ?5, ?6)",
                params![name, body.venue, body.event_date, body.target_minutes, now, dump_sources(&body.pool_sources)],
            )?;
            let id = t.last_insert_rowid();
            if let Some(pid) = body.from_playlist_id.filter(|p| *p != 0) {
                // a playlist that is not there seeds nothing, as in the legacy app
                let scope = bc_libcore::Scope::resolve(t, None, None)?;
                let tids: Vec<i64> = match playlist_items(t, resolver.as_ref(), pid, &scope) {
                    Ok(v) => v.into_iter().map(|(_, tid)| tid).collect(),
                    Err(ApiError::NotFound(_)) => vec![],
                    Err(e) => return Err(e),
                };
                let snaps = snapshots(t, &tids)?;
                let mut ins = t.prepare_cached(
                    "INSERT INTO dj_set_items(set_id, track_id, position, tempo_adjust_pct, key_lock, snapshot)
                     VALUES (?1, ?2, ?3, 0.0, 1, ?4)",
                )?;
                for (i, tid) in tids.iter().enumerate() {
                    ins.execute(params![
                        id,
                        tid,
                        bc_music::ordering::STEP * (i as f64 + 1.0),
                        snapshot_text(&snaps, *tid)
                    ])?;
                }
            }
            set_detail(t, id)
        })
        .await?;
    st.ctx.bus.invalidate("set", vec![detail.set.id]);
    Ok(axum::Json(detail))
}

/// Keys present in a JSON object body: a PATCH distinguishes "absent" (untouched) from an
/// explicit `null` (clear), which the typed DTOs cannot.
fn object_keys(v: &Value) -> ApiResult<HashSet<String>> {
    match v {
        Value::Object(m) => Ok(m.keys().cloned().collect()),
        _ => Err(ApiError::bad("expected a JSON object")),
    }
}

fn typed<T: serde::de::DeserializeOwned>(v: &Value) -> ApiResult<T> {
    serde_json::from_value(v.clone()).map_err(|e| ApiError::bad(e.to_string()))
}

async fn update(
    State(st): State<PlistState>,
    Path(id): Path<i64>,
    J(raw): J<Value>,
) -> ApiResult<axum::Json<DjSetDetail>> {
    let keys = object_keys(&raw)?;
    let body: DjSetUpdate = typed(&raw)?;
    if let Some(p) = &body.pool_sources {
        checked_sources(p)?;
    }
    if let Some(s) = &body.status
        && !STATUSES.contains(&s.as_str())
    {
        return Err(ApiError::bad(format!("status must be one of {}", STATUSES.join(", "))));
    }
    if let Some(n) = &body.name
        && n.trim().is_empty()
    {
        return Err(ApiError::bad("a set needs a name"));
    }
    let detail = st
        .ctx
        .write_async(move |t| {
            require_set(t, id)?;
            let has = |k: &str| keys.contains(k);
            if let Some(n) = &body.name {
                t.execute("UPDATE dj_sets SET name = ?1 WHERE id = ?2", params![n.trim(), id])?;
            }
            if let Some(s) = &body.status {
                t.execute("UPDATE dj_sets SET status = ?1 WHERE id = ?2", params![s, id])?;
            }
            // nullable: an explicit null clears
            if has("venue") {
                t.execute("UPDATE dj_sets SET venue = ?1 WHERE id = ?2", params![body.venue, id])?;
            }
            if has("event_date") {
                t.execute("UPDATE dj_sets SET event_date = ?1 WHERE id = ?2", params![body.event_date, id])?;
            }
            if has("target_minutes") {
                t.execute("UPDATE dj_sets SET target_minutes = ?1 WHERE id = ?2", params![body.target_minutes, id])?;
            }
            if has("notes") {
                t.execute("UPDATE dj_sets SET notes = ?1 WHERE id = ?2", params![body.notes, id])?;
            }
            // None = untouched; [] = clear the pool
            if let Some(p) = &body.pool_sources {
                t.execute("UPDATE dj_sets SET pool_sources = ?1 WHERE id = ?2", params![dump_sources(p), id])?;
            }
            touch(t, id)?;
            set_detail(t, id)
        })
        .await?;
    st.ctx.bus.invalidate("set", vec![id]);
    Ok(axum::Json(detail))
}

async fn delete(State(st): State<PlistState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    st.ctx
        .write_async(move |t| {
            require_set(t, id)?;
            t.execute("DELETE FROM dj_sets WHERE id = ?1", [id])?;
            Ok(())
        })
        .await?;
    st.ctx.bus.invalidate("set", vec![id]);
    Ok(StatusCode::NO_CONTENT)
}

async fn add_items(
    State(st): State<PlistState>,
    Path(id): Path<i64>,
    J(body): J<PlaylistAddTracks>,
) -> ApiResult<axum::Json<DjSetDetail>> {
    if body.track_ids.is_empty() {
        st.ctx.read_async(move |c| require_set(c, id)).await?;
        return Err(ApiError::bad("no tracks supplied"));
    }
    let detail = st
        .ctx
        .write_async(move |t| {
            require_set(t, id)?;
            require_tracks(t, &body.track_ids)?;
            let existing: Vec<f64> = lane::SET.items(t, id)?.into_iter().map(|(_, p)| p).collect();
            let positions = Lane::plan_insert(&existing, body.track_ids.len(), body.at_index);
            let snaps = snapshots(t, &body.track_ids)?;
            {
                let mut ins = t.prepare_cached(
                    "INSERT INTO dj_set_items(set_id, track_id, position, tempo_adjust_pct, key_lock, snapshot)
                     VALUES (?1, ?2, ?3, 0.0, 1, ?4)",
                )?;
                for (tid, p) in body.track_ids.iter().zip(&positions) {
                    ins.execute(params![id, tid, p, snapshot_text(&snaps, *tid)])?;
                }
            }
            touch(t, id)?;
            lane::SET.renormalise_if_needed(t, id)?;
            set_detail(t, id)
        })
        .await?;
    st.ctx.bus.invalidate("set", vec![id]);
    Ok(axum::Json(detail))
}

async fn update_item(
    State(st): State<PlistState>,
    Path((id, item_id)): Path<(i64, i64)>,
    J(raw): J<Value>,
) -> ApiResult<axum::Json<DjSetDetail>> {
    let keys = object_keys(&raw)?;
    let body: SetItemUpdate = typed(&raw)?;
    if let Some(e) = body.energy
        && !(1..=10).contains(&e)
    {
        return Err(ApiError::bad("energy is 1..10"));
    }
    if body.tempo_adjust_pct.is_some_and(|p| !p.is_finite()) {
        return Err(ApiError::bad("tempo_adjust_pct must be a number"));
    }
    let detail = st
        .ctx
        .write_async(move |t| {
            require_item(t, id, item_id)?;
            let has = |k: &str| keys.contains(k);
            // nullable columns: an explicit null clears (a cue point is removed that way)
            if has("cue_in_ms") {
                t.execute("UPDATE dj_set_items SET cue_in_ms = ?1 WHERE id = ?2", params![body.cue_in_ms, item_id])?;
            }
            if has("cue_out_ms") {
                t.execute("UPDATE dj_set_items SET cue_out_ms = ?1 WHERE id = ?2", params![body.cue_out_ms, item_id])?;
            }
            if has("transition_type") {
                t.execute(
                    "UPDATE dj_set_items SET transition_type = ?1 WHERE id = ?2",
                    params![body.transition_type, item_id],
                )?;
            }
            if has("transition_beats") {
                t.execute(
                    "UPDATE dj_set_items SET transition_beats = ?1 WHERE id = ?2",
                    params![body.transition_beats, item_id],
                )?;
            }
            if has("transition_notes") {
                t.execute(
                    "UPDATE dj_set_items SET transition_notes = ?1 WHERE id = ?2",
                    params![body.transition_notes, item_id],
                )?;
            }
            if has("energy") {
                t.execute("UPDATE dj_set_items SET energy = ?1 WHERE id = ?2", params![body.energy, item_id])?;
            }
            // NOT NULL columns: null means untouched
            if let Some(p) = body.tempo_adjust_pct {
                t.execute("UPDATE dj_set_items SET tempo_adjust_pct = ?1 WHERE id = ?2", params![p, item_id])?;
            }
            if let Some(k) = body.key_lock {
                t.execute("UPDATE dj_set_items SET key_lock = ?1 WHERE id = ?2", params![k, item_id])?;
            }
            touch(t, id)?;
            set_detail(t, id)
        })
        .await?;
    st.ctx.bus.invalidate("set", vec![id]);
    Ok(axum::Json(detail))
}

async fn move_item(
    State(st): State<PlistState>,
    Path((id, item_id)): Path<(i64, i64)>,
    J(body): J<MoveItem>,
) -> ApiResult<axum::Json<DjSetDetail>> {
    let detail = st
        .ctx
        .write_async(move |t| {
            require_set(t, id)?;
            lane::SET.move_item(t, id, item_id, body.to_index)?;
            touch(t, id)?;
            set_detail(t, id)
        })
        .await?;
    st.ctx.bus.invalidate("set", vec![id]);
    Ok(axum::Json(detail))
}

async fn delete_item(
    State(st): State<PlistState>,
    Path((id, item_id)): Path<(i64, i64)>,
) -> ApiResult<axum::Json<DjSetDetail>> {
    let detail = st
        .ctx
        .write_async(move |t| {
            require_item(t, id, item_id)?;
            t.execute("DELETE FROM dj_set_items WHERE id = ?1", [item_id])?;
            touch(t, id)?;
            set_detail(t, id)
        })
        .await?;
    st.ctx.bus.invalidate("set", vec![id]);
    Ok(axum::Json(detail))
}

/// Track ids of the playable slots in order: slots whose track or file is gone are skipped (the
/// plan still shows them).
fn playable_ids(c: &Connection, id: i64, only_available: bool) -> ApiResult<Vec<i64>> {
    require_set(c, id)?;
    let sql = format!(
        "SELECT i.track_id FROM dj_set_items i JOIN tracks t ON t.id = i.track_id
          WHERE i.set_id = ?1 {} ORDER BY i.position, i.id",
        if only_available { "AND t.available = 1" } else { "" }
    );
    let mut st = c.prepare_cached(&sql)?;
    let rows = st.query_map([id], |r| r.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The planned order as playable tracks: what "play this set" queues.
async fn tracks(State(st): State<PlistState>, Path(id): Path<i64>) -> ApiResult<axum::Json<Page<TrackOut>>> {
    let page = st
        .ctx
        .read_async(move |c| {
            let ids = playable_ids(c, id, true)?;
            let items: Vec<(Option<i64>, i64)> = ids.into_iter().map(|t| (None, t)).collect();
            let tracks = hydrate_items(c, &items)?;
            let n = tracks.len() as i64;
            Ok(Page { items: tracks, total: n, offset: 0, limit: n.max(1) })
        })
        .await?;
    Ok(axum::Json(page))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SetExportQuery {
    format: Option<String>,
}

async fn export_set(
    State(st): State<PlistState>,
    Path(id): Path<i64>,
    Q(q): Q<SetExportQuery>,
) -> ApiResult<Response> {
    let format = match q.format.as_deref().unwrap_or("m3u8") {
        "m3u8" => ExportFormat::M3u8,
        "csv" => ExportFormat::Csv,
        "zip" => ExportFormat::Zip,
        "mp3" | "wav" => {
            return Err(ApiError::bad("rendered mixes (mp3/wav) are produced by the render service, not this route"));
        }
        other => return Err(ApiError::bad(format!("unknown export format {other:?}"))),
    };
    let (name, tracks) = st
        .ctx
        .read_async(move |c| {
            let name: String = c
                .query_row("SELECT name FROM dj_sets WHERE id = ?1", [id], |r| r.get(0))
                .optional()?
                .ok_or_else(|| ApiError::not_found(format!("set {id} not found")))?;
            let ids = playable_ids(c, id, false)?;
            Ok((name, export::load_export_tracks(c, &ids)?))
        })
        .await?;
    export::export_response(format, &tracks, &name)
}
