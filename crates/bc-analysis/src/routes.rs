//! HTTP routes of the analysis service (paths without the `/api` prefix).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bc_db::rusqlite::{Connection, OptionalExtension, params};
use bc_jobs::{NewItem, NewJob};
use bc_music::beatgrid::legacy_grid;
use bc_music::camelot::{bpm_compatibility, compatible_keys, key_compatibility, key_name_opt};
use bc_music::mixpoints;
use bc_types::analysis::*;
use bc_types::{Accepted, Page};
use serde::Deserialize;

use crate::error::{ApiError, ApiResult};
use crate::persist::{ANALYZER_VERSION, cue_kind_parse, cue_kind_str};
use crate::runner::{PRIORITY_BACKFILL, PRIORITY_DECK, PRIORITY_QUEUE};
use crate::service::Inner;

type S = State<Arc<Inner>>;

pub fn router(inner: Arc<Inner>) -> Router {
    Router::new()
        .route("/analysis/status", get(status))
        .route("/analysis/queue", get(queue))
        .route("/analysis/scan", post(scan))
        .route("/analysis/prioritize", post(prioritize))
        .route("/analysis/accuracy", get(accuracy))
        .route("/analysis/waveform-cache", get(waveform_cache))
        .route("/analysis/tracks/{id}", post(analyse_track).get(get_analysis))
        .route("/analysis/compatible/{id}", get(compatible))
        .route("/analysis/compatibility/{a}/{b}", get(compatibility))
        .route("/tracks/{id}/waveform", get(waveform))
        .route("/tracks/{id}/music", get(music))
        .route("/tracks/{id}/cues", get(get_cues).put(put_cues))
        .route("/tracks/{id}/peaks", get(peaks))
        .route("/tracks/{id}/bands", get(bands))
        .with_state(inner)
}

fn one_i64(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0)
}

// --- status / queue --------------------------------------------------------------------------

async fn status(State(s): S) -> ApiResult<Json<AnalysisStatus>> {
    let sidecar = crate::sidecar::available();
    let out = s
        .db
        .read_async(move |c| {
            let total = one_i64(c, "SELECT count(*) FROM tracks");
            let analysed = one_i64(c, "SELECT count(*) FROM analysis WHERE status != 'failed'");
            let failed = one_i64(c, "SELECT count(*) FROM analysis WHERE status = 'failed'");
            let stale = one_i64(
                c,
                &format!("SELECT count(*) FROM analysis WHERE analyzer_version < {ANALYZER_VERSION} AND status != 'failed'"),
            );
            let running_jobs = one_i64(c, "SELECT count(*) FROM jobs WHERE kind='analyze' AND status IN ('queued','running')");
            let mut by_status: BTreeMap<String, i64> = BTreeMap::new();
            let mut st = c.prepare(
                "SELECT i.status, count(*) FROM job_items i JOIN jobs j ON j.id = i.job_id \
                 WHERE j.kind='analyze' AND j.status IN ('queued','running','paused') GROUP BY i.status",
            )?;
            for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                let (k, v) = row?;
                by_status.insert(k, v);
            }
            let mut by_analyzer: BTreeMap<String, i64> = BTreeMap::new();
            let mut st = c.prepare(
                "SELECT COALESCE(analyzer, CASE WHEN backend='essentia' THEN 'essentia-import' ELSE backend END), count(*) \
                 FROM analysis GROUP BY 1",
            )?;
            for row in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                let (k, v) = row?;
                by_analyzer.insert(k, v);
            }
            let batch_total: i64 = by_status.values().sum();
            let settled: i64 = ["done", "failed", "skipped", "cancelled"].iter().map(|k| by_status.get(*k).copied().unwrap_or(0)).sum();
            let mut backends = BTreeMap::new();
            backends.insert("bc-rs-1".to_string(), true);
            backends.insert("essentia-sidecar".to_string(), sidecar);
            Ok(AnalysisStatus {
                total_tracks: total,
                analysed,
                missing: (total - analysed - failed).max(0),
                failed,
                stale,
                coverage: if total > 0 { ((analysed as f64 / total as f64) * 10000.0).round() / 10000.0 } else { 0.0 },
                analyzer_version: ANALYZER_VERSION,
                backends,
                active_backend: Some("bc-rs-1".into()),
                by_analyzer,
                running_jobs,
                queued_tracks: by_status.get("pending").copied().unwrap_or(0),
                running_tracks: by_status.get("running").copied().unwrap_or(0),
                batch_total,
                batch_done: settled,
            })
        })
        .await?;
    Ok(Json(out))
}

#[derive(Deserialize)]
struct QueueQ {
    limit: Option<i64>,
    jobs_limit: Option<i64>,
}

fn job_out(r: &bc_db::rusqlite::Row<'_>) -> bc_db::rusqlite::Result<AnalysisJobOut> {
    let (total, completed, failed, skipped): (i64, i64, i64, i64) = (r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?);
    let done = completed + failed + skipped;
    Ok(AnalysisJobOut {
        id: r.get(0)?,
        status: r.get(1)?,
        label: r.get(2)?,
        total,
        completed,
        failed,
        skipped,
        progress: if total > 0 { done as f64 / total as f64 } else { 0.0 },
        error: r.get(8)?,
        created_at: r.get(9)?,
        started_at: r.get(10)?,
        finished_at: r.get(11)?,
    })
}

const JOB_COLS: &str = "id, status, label, 0, total, completed, failed, skipped, error, created_at, started_at, finished_at";

async fn queue(State(s): S, Query(q): Query<QueueQ>) -> ApiResult<Json<AnalysisQueue>> {
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let jobs_limit = q.jobs_limit.unwrap_or(20).clamp(1, 100);
    let out = s
        .db
        .read_async(move |c| {
            let active_sql = format!(
                "SELECT {JOB_COLS} FROM jobs WHERE kind='analyze' AND status IN ('queued','running','paused') \
                 ORDER BY created_at DESC, id LIMIT ?1"
            );
            let active: Vec<AnalysisJobOut> =
                c.prepare(&active_sql)?.query_map([jobs_limit], job_out)?.collect::<Result<_, _>>()?;
            let recent_sql = format!(
                "SELECT {JOB_COLS} FROM jobs WHERE kind='analyze' AND status NOT IN ('queued','running','paused') \
                 ORDER BY created_at DESC, id LIMIT ?1"
            );
            let recent: Vec<AnalysisJobOut> = c
                .prepare(&recent_sql)?
                .query_map([(jobs_limit - active.len() as i64).max(1)], job_out)?
                .collect::<Result<_, _>>()?;
            let mut jobs = active.clone();
            jobs.extend(recent);
            if jobs.is_empty() {
                return Ok(AnalysisQueue::default());
            }
            let item_jobs: Vec<String> = if active.is_empty() { vec![jobs[0].id.clone()] } else { active.iter().map(|j| j.id.clone()).collect() };
            let ph = item_jobs.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT i.id, i.job_id, i.seq, i.status, i.track_id, t.title, a.name, i.message, i.last_error, i.started_at, i.finished_at \
                 FROM job_items i LEFT JOIN tracks t ON t.id = i.track_id LEFT JOIN artists a ON a.id = t.artist_id \
                 WHERE i.job_id IN ({ph}) \
                 ORDER BY CASE i.status WHEN 'running' THEN 0 WHEN 'failed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END, i.seq LIMIT {limit}"
            );
            let mut st = c.prepare(&sql)?;
            let items: Vec<AnalysisItemOut> = st
                .query_map(bc_db::rusqlite::params_from_iter(item_jobs.iter()), |r| {
                    Ok(AnalysisItemOut {
                        id: r.get(0)?,
                        job_id: r.get(1)?,
                        seq: r.get(2)?,
                        status: r.get(3)?,
                        track_id: r.get(4)?,
                        title: r.get(5)?,
                        artist: r.get(6)?,
                        message: r.get(7)?,
                        last_error: r.get(8)?,
                        started_at: r.get(9)?,
                        finished_at: r.get(10)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
            let ids = |statuses: &[&str]| -> Vec<i64> {
                items.iter().filter(|i| statuses.contains(&i.status.as_str())).filter_map(|i| i.track_id).collect()
            };
            Ok(AnalysisQueue {
                active_track_ids: ids(&["running"]),
                queued_track_ids: ids(&["pending"]),
                failed_track_ids: ids(&["failed"]),
                jobs,
                items,
            })
        })
        .await?;
    Ok(Json(out))
}

// --- scan ---------------------------------------------------------------------------------------

fn pending_ids(c: &Connection, scope: ScanScope) -> Result<Vec<i64>, bc_db::DbError> {
    let present = "EXISTS (SELECT 1 FROM files f WHERE f.track_id = t.id AND f.missing_since IS NULL)";
    let sql = match scope {
        ScanScope::Missing => format!("SELECT t.id FROM tracks t LEFT JOIN analysis a ON a.track_id = t.id WHERE {present} AND a.track_id IS NULL ORDER BY t.added_at DESC, t.id"),
        ScanScope::Stale => format!(
            "SELECT t.id FROM tracks t LEFT JOIN analysis a ON a.track_id = t.id WHERE {present} AND (a.track_id IS NULL OR (a.analyzer_version < {ANALYZER_VERSION} AND a.status != 'failed')) ORDER BY t.added_at DESC, t.id"
        ),
        ScanScope::Failed => format!("SELECT t.id FROM tracks t JOIN analysis a ON a.track_id = t.id WHERE {present} AND a.status = 'failed' ORDER BY t.added_at DESC, t.id"),
        ScanScope::All => format!("SELECT t.id FROM tracks t WHERE {present} ORDER BY t.added_at DESC, t.id"),
        ScanScope::Upgrade => format!(
            "SELECT t.id FROM tracks t JOIN analysis a ON a.track_id = t.id WHERE {present} AND a.status != 'failed' \
             AND NOT EXISTS (SELECT 1 FROM waveform_meta w WHERE w.track_id = t.id) ORDER BY t.added_at DESC, t.id"
        ),
        ScanScope::Ids => return Ok(vec![]),
    };
    let mut st = c.prepare(&sql)?;
    let v = st.query_map([], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok(v)
}

fn dedup(ids: Vec<i64>) -> Vec<i64> {
    let mut seen = std::collections::HashSet::new();
    ids.into_iter().filter(|i| seen.insert(*i)).collect()
}

fn unanalysed(c: &Connection, ids: &[i64]) -> Result<Vec<i64>, bc_db::DbError> {
    let mut done = std::collections::HashSet::new();
    for chunk in ids.chunks(500) {
        let ph = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut st = c.prepare(&format!("SELECT track_id FROM analysis WHERE status != 'failed' AND track_id IN ({ph})"))?;
        for r in st.query_map(bc_db::rusqlite::params_from_iter(chunk.iter()), |r| r.get::<_, i64>(0))? {
            done.insert(r?);
        }
    }
    Ok(ids.iter().copied().filter(|i| !done.contains(i)).collect())
}

pub fn enqueue(s: &Inner, ids: Vec<i64>, priority: i64, label: String) -> Result<String, bc_db::DbError> {
    let job = s
        .jobs
        .create_job(NewJob::new("analyze", ids.into_iter().map(NewItem::track).collect()).label(label).priority(priority))?;
    Ok(job.id)
}

async fn scan(State(s): S, Json(body): Json<ScanRequest>) -> ApiResult<Response> {
    let (ids, requested) = {
        let body = body.clone();
        s.db.read_async(move |c| {
            if body.scope == ScanScope::Ids {
                let ids = dedup(body.track_ids.clone());
                let requested = ids.len() as i64;
                let ids = if body.only_missing { unanalysed(c, &ids)? } else { ids };
                Ok((ids, requested))
            } else {
                let ids = dedup(pending_ids(c, body.scope)?);
                let requested = ids.len() as i64;
                Ok((ids, requested))
            }
        })
        .await?
    };
    let ids: Vec<i64> = match body.limit {
        Some(l) if l > 0 => ids.into_iter().take(l as usize).collect(),
        _ => ids,
    };
    if ids.is_empty() {
        let r = ScanResponse {
            queued: 0,
            requested,
            job_id: None,
            detail: if requested > 0 { "already analysed".into() } else { "nothing to analyse".into() },
        };
        return Ok(Json(r).into_response());
    }
    let n = ids.len() as i64;
    let sc = s.clone();
    let prio = if body.scope == ScanScope::Ids { PRIORITY_QUEUE } else { PRIORITY_BACKFILL };
    let job_id = s.jobs.run(move |_| enqueue(&sc, ids, prio, format!("analyse {n} track(s)"))).await?;
    let r = ScanResponse { queued: n, requested, job_id: Some(job_id), detail: "queued".into() };
    Ok((StatusCode::ACCEPTED, Json(r)).into_response())
}

#[derive(Deserialize)]
struct PrioritizeBody {
    track_ids: Vec<i64>,
    #[serde(default)]
    level: Option<String>,
}

/// Bump tracks to the front of the queue: skip their pending items in lower-priority jobs and
/// enqueue a fresh high-priority job (deck 0 / queue-or-set 100).
async fn prioritize(State(s): S, Json(body): Json<PrioritizeBody>) -> ApiResult<StatusCode> {
    let prio = if body.level.as_deref() == Some("deck") { PRIORITY_DECK } else { PRIORITY_QUEUE };
    let ids = dedup(body.track_ids);
    if ids.is_empty() {
        return Ok(StatusCode::NO_CONTENT);
    }
    let sc = s.clone();
    let todo = ids.clone();
    let pending: Vec<i64> = s
        .db
        .read_async(move |c| {
            let todo = unanalysed(c, &todo)?;
            let mut out = Vec::new();
            for chunk in todo.chunks(400) {
                let ph = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let sql = format!(
                    "SELECT i.id FROM job_items i JOIN jobs j ON j.id = i.job_id WHERE j.kind='analyze' AND i.status='pending' \
                     AND j.priority > {prio} AND i.track_id IN ({ph})"
                );
                let mut st = c.prepare(&sql)?;
                for r in st.query_map(bc_db::rusqlite::params_from_iter(chunk.iter()), |r| r.get::<_, i64>(0))? {
                    out.push(r?);
                }
            }
            Ok(out)
        })
        .await?;
    // tracks that still need analysis (or a re-run when the user explicitly asked)
    let to_run = {
        let ids2 = ids.clone();
        s.db.read_async(move |c| unanalysed(c, &ids2)).await?
    };
    if to_run.is_empty() {
        return Ok(StatusCode::NO_CONTENT);
    }
    s.jobs
        .run(move |st| {
            if !pending.is_empty() {
                st.skip_items(&pending, "moved to a higher-priority job")?;
            }
            enqueue(&sc, to_run, prio, "analyse (priority)".into())?;
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- single track ------------------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct WaitQ {
    #[serde(default)]
    wait: bool,
}

pub fn analysis_out(c: &Connection, track_id: i64) -> Result<Option<AnalysisOut>, bc_db::DbError> {
    let r = c
        .query_row(
            "SELECT status, backend, analyzer, bpm, bpm_confidence, bpm_candidates, beat_offset_ms, key_root, key_mode, camelot, \
                    key_confidence, loudness_lufs, true_peak_dbtp, lra, replaygain_gain, energy, energy_v2, grid_kind, \
                    downbeat_offset_ms, error FROM analysis WHERE track_id = ?1",
            [track_id],
            |r| {
                let key_root: Option<i64> = r.get(7)?;
                let key_mode: Option<String> = r.get(8)?;
                let cands: Option<String> = r.get(5)?;
                let grid_kind: Option<String> = r.get(17)?;
                let backend: String = r.get(1)?;
                let analyzer: Option<String> = r.get(2)?;
                Ok(AnalysisOut {
                    track_id,
                    status: r.get(0)?,
                    analyzer: analyzer.clone().or_else(|| Some(if backend == "essentia" { "essentia-import".into() } else { backend.clone() })),
                    backend: Some(backend),
                    bpm: r.get(3)?,
                    bpm_confidence: r.get(4)?,
                    bpm_candidates: cands.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
                    beat_offset_ms: r.get(6)?,
                    key: key_name_opt(key_root.map(|k| k as i32), key_mode.as_deref()),
                    camelot: r.get(9)?,
                    key_confidence: r.get(10)?,
                    loudness_lufs: r.get(11)?,
                    true_peak_dbtp: r.get(12)?,
                    lra: r.get(13)?,
                    replaygain_gain: r.get(14)?,
                    energy: r.get(15)?,
                    energy_v2: r.get(16)?,
                    grid_kind: grid_kind.map(|g| if g == "variable" { GridKind::Variable } else { GridKind::Constant }),
                    downbeat_offset_ms: r.get(18)?,
                    error: r.get(19)?,
                })
            },
        )
        .optional()?;
    Ok(r)
}

async fn get_analysis(State(s): S, Path(id): Path<i64>) -> ApiResult<Json<AnalysisOut>> {
    let r = s.db.read_async(move |c| analysis_out(c, id)).await?;
    r.map(Json).ok_or_else(|| ApiError::not_found(format!("track {id} has not been analysed")))
}

async fn analyse_track(State(s): S, Path(id): Path<i64>, Query(q): Query<WaitQ>) -> ApiResult<Response> {
    let exists = s.db.read_async(move |c| Ok(c.query_row("SELECT 1 FROM tracks WHERE id = ?1", [id], |_| Ok(())).optional()?.is_some())).await?;
    if !exists {
        return Err(ApiError::not_found(format!("track {id} not found")));
    }
    if q.wait {
        let sc = s.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<Option<AnalysisOut>, ApiError> {
            let path = file_path(&sc.db, id).ok_or_else(|| ApiError::not_found("no available file"))?;
            let opts = sc.runner.opts.read().clone();
            let sidecar = sc.runner.wants_sidecar(id);
            let res = crate::runner::work(id, &path, &opts, &sc.waveforms, sidecar.as_deref());
            let policy = crate::persist::read_policy(&sc.db);
            crate::persist::persist_batch(&sc.db, vec![(id, res.clone())], policy)?;
            if let Err(e) = res {
                return Ok(Some(AnalysisOut { track_id: id, status: "failed".into(), error: Some(e), ..Default::default() }));
            }
            Ok(sc.db.read(|c| analysis_out(c, id))?)
        })
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;
        return Ok(Json(out).into_response());
    }
    let sc = s.clone();
    let job_id = s.jobs.run(move |_| enqueue(&sc, vec![id], PRIORITY_DECK, format!("analyse track {id}"))).await?;
    Ok((StatusCode::ACCEPTED, Json(Accepted { job_id })).into_response())
}

pub fn file_path(db: &bc_db::Db, track_id: i64) -> Option<std::path::PathBuf> {
    db.read(|c| {
        Ok(c.query_row(
            "SELECT path FROM files WHERE track_id = ?1 AND missing_since IS NULL ORDER BY id LIMIT 1",
            [track_id],
            |r| r.get::<_, String>(0),
        )
        .ok())
    })
    .ok()
    .flatten()
    .map(std::path::PathBuf::from)
}

// --- harmonic mixing ------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct CompatQ {
    bpm_tolerance: Option<f64>,
    #[serde(default)]
    include_risky: bool,
    limit: Option<i64>,
}

/// Full `TrackOut` rows via WS1's hydrator, in the order of `ids`.
fn hydrate(c: &Connection, ids: &[i64]) -> Result<Vec<bc_types::library::TrackOut>, bc_db::DbError> {
    let mut rows = bc_libcore::hydrate::tracks_out(c, ids).map_err(|e| bc_db::DbError::Other(e.to_string()))?;
    let pos: std::collections::HashMap<i64, usize> = ids.iter().enumerate().map(|(i, v)| (*v, i)).collect();
    rows.sort_by_key(|b| pos.get(&b.id).copied().unwrap_or(usize::MAX));
    Ok(rows)
}

async fn compatible(State(s): S, Path(id): Path<i64>, Query(q): Query<CompatQ>) -> ApiResult<Json<Page<bc_types::library::TrackOut>>> {
    let tol = q.bpm_tolerance.unwrap_or(0.06).clamp(0.0, 0.25);
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let risky = q.include_risky;
    let page = s
        .db
        .read_async(move |c| {
            let seed: Option<(Option<String>, Option<f64>)> = c
                .query_row("SELECT camelot, bpm FROM analysis WHERE track_id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            let Some((Some(camelot), bpm)) = seed else {
                return Err(bc_db::DbError::Other("seed track has no key analysis yet".into()));
            };
            let keys = compatible_keys(Some(&camelot), risky);
            let ph = keys.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut sql = format!(
                "SELECT t.id, a.camelot, a.bpm FROM tracks t JOIN analysis a ON a.track_id = t.id \
                 WHERE a.camelot IN ({ph}) AND t.id != ?{} AND EXISTS (SELECT 1 FROM files f WHERE f.track_id = t.id AND f.missing_since IS NULL)",
                keys.len() + 1
            );
            let mut args: Vec<bc_db::rusqlite::types::Value> = keys.iter().map(|k| k.clone().into()).collect();
            args.push(id.into());
            if let Some(b) = bpm.filter(|b| *b > 0.0) {
                let (lo, hi) = (b * (1.0 - tol), b * (1.0 + tol));
                let n = args.len();
                sql.push_str(&format!(
                    " AND ((a.bpm BETWEEN ?{} AND ?{}) OR (a.bpm BETWEEN ?{} AND ?{}) OR (a.bpm BETWEEN ?{} AND ?{}))",
                    n + 1, n + 2, n + 3, n + 4, n + 5, n + 6
                ));
                for v in [lo, hi, lo * 2.0, hi * 2.0, lo / 2.0, hi / 2.0] {
                    args.push(v.into());
                }
            }
            sql.push_str(&format!(" ORDER BY t.id LIMIT {limit}"));
            let mut st = c.prepare(&sql)?;
            let mut rows: Vec<(i64, Option<String>, Option<f64>)> = st
                .query_map(bc_db::rusqlite::params_from_iter(args.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<Result<_, _>>()?;
            rows.sort_by(|a, b| {
                let ka = key_compatibility(Some(&camelot), a.1.as_deref()).score;
                let kb = key_compatibility(Some(&camelot), b.1.as_deref()).score;
                let ba = bpm_compatibility(bpm, a.2, 0.06).score;
                let bb = bpm_compatibility(bpm, b.2, 0.06).score;
                kb.total_cmp(&ka).then(bb.total_cmp(&ba))
            });
            let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
            let briefs = hydrate(c, &ids)?;
            let total = briefs.len() as i64;
            Ok(Page { items: briefs, total, offset: 0, limit })
        })
        .await;
    match page {
        Ok(p) => Ok(Json(p)),
        Err(bc_db::DbError::Other(m)) if m.starts_with("seed track") => Err(ApiError::bad_request(m)),
        Err(e) => Err(e.into()),
    }
}

async fn compatibility(State(s): S, Path((a, b)): Path<(i64, i64)>) -> ApiResult<Json<CompatibilityOut>> {
    let r = s
        .db
        .read_async(move |c| {
            let get = |id: i64| -> Result<Option<(Option<String>, Option<f64>)>, bc_db::DbError> {
                Ok(c.query_row("SELECT camelot, bpm FROM analysis WHERE track_id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
            };
            Ok((get(a)?, get(b)?))
        })
        .await?;
    let (Some(x), Some(y)) = r else { return Err(ApiError::bad_request("both tracks must be analysed")) };
    let key = key_compatibility(x.0.as_deref(), y.0.as_deref());
    let bpm = bpm_compatibility(x.1, y.1, 0.06);
    let r3 = |v: f64| (v * 1000.0).round() / 1000.0;
    Ok(Json(CompatibilityOut {
        key_score: r3(key.score),
        key_verdict: key.verdict.as_str().into(),
        key_reason: key.reason,
        bpm_score: r3(bpm.score),
        bpm_verdict: bpm.verdict.as_str().into(),
        bpm_reason: bpm.reason,
    }))
}

// --- waveforms ----------------------------------------------------------------------------------------------

#[derive(Deserialize)]
struct WfQ {
    level: Option<String>,
    v: Option<String>,
    /// Format version the client understands (`bc_waveform::format::VERSION`); part of the
    /// immutable-cache key so a format bump cannot be shadowed by an old cached response.
    f: Option<String>,
}

/// Overview + detail (single-flight): analyse for the waveform only when it is not stored.
async fn ensure_waveform(s: &Arc<Inner>, id: i64, need_detail: bool) -> ApiResult<bc_waveform::Waveform> {
    let sc = s.clone();
    let found = tokio::task::spawn_blocking(move || {
        if need_detail { sc.waveforms.get(id) } else { sc.waveforms.get_overview_only(id) }
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    if let Some(w) = found {
        if !need_detail || w.detail.is_some() {
            return Ok(w);
        }
    }
    let lock = s.building.lock().entry(id).or_default().clone();
    let sc = s.clone();
    let res = tokio::task::spawn_blocking(move || -> ApiResult<bc_waveform::Waveform> {
        let _g = lock.lock();
        // someone else may have built it while we waited
        if let Some(w) = sc.waveforms.get(id) {
            if w.detail.is_some() || !need_detail {
                return Ok(w);
            }
        }
        let path = file_path(&sc.db, id).ok_or_else(|| ApiError::not_found(format!("no audio for track {id}")))?;
        let opts = crate::pipeline::AnalyzeOptions { loudness: false, tempo: false, key: false, waveform: true, ..Default::default() };
        let mut a = crate::pipeline::analyze_file(&path, &opts).map_err(|e| ApiError::not_found(e.to_string()))?;
        let wf = a.waveform.take().ok_or_else(|| ApiError::internal("no waveform produced"))?;
        sc.waveforms.put(id, &wf).map_err(|e| ApiError::internal(e.to_string()))?;
        Ok(wf)
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    s.building.lock().remove(&id);
    res
}

async fn waveform(State(s): S, Path(id): Path<i64>, Query(q): Query<WfQ>, headers: HeaderMap) -> ApiResult<Response> {
    let detail = match q.level.as_deref().unwrap_or("overview") {
        "overview" => false,
        "detail" => true,
        other => return Err(ApiError::bad_request(format!("level must be overview or detail, got {other}"))),
    };
    let wf = ensure_waveform(&s, id, detail).await?;
    let hash = wf.source_hash_hex();
    let etag = format!("\"{hash}-v{}-{}\"", bc_waveform::format::VERSION, if detail { "detail" } else { "overview" });
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == format!("W/{etag}"))) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }
    let bytes = wf.to_bytes(&if detail { bc_waveform::EncodeOpts::wire_detail() } else { bc_waveform::EncodeOpts::wire_overview() });
    let immutable = q.v.as_deref() == Some(hash.as_str()) && q.f.as_deref() == Some(bc_waveform::format::VERSION.to_string().as_str());
    let cache = if immutable { "public, max-age=31536000, immutable" } else { "no-cache" };
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, cache.to_string()),
            (header::HeaderName::from_static("x-bcw-duration-ms"), wf.duration_ms().to_string()),
            (header::HeaderName::from_static("x-bcw-sample-rate"), wf.sample_rate.to_string()),
        ],
        bytes,
    )
        .into_response())
}


#[derive(Deserialize)]
struct PointsQ {
    points: Option<usize>,
}

async fn peaks(State(s): S, Path(id): Path<i64>, Query(q): Query<PointsQ>) -> ApiResult<Response> {
    let points = q.points.unwrap_or(200).clamp(20, 2000);
    let wf = match s.waveforms.get_overview_only(id) {
        Some(w) => w,
        None => return Err(ApiError::not_found(format!("no waveform for track {id}; analyse it first"))),
    };
    let peaks = bc_waveform::legacy::peaks_json(&wf, points);
    Ok(([(header::CACHE_CONTROL, "public, max-age=86400")], Json(serde_json::json!({ "track_id": id, "peaks": peaks }))).into_response())
}

async fn bands(State(s): S, Path(id): Path<i64>, Query(q): Query<PointsQ>) -> ApiResult<Response> {
    let points = q.points.unwrap_or(400).clamp(20, 2000);
    let wf = ensure_waveform(&s, id, false).await?;
    let b = bc_waveform::legacy::bands_json(&wf, points);
    let json: Vec<[f32; 3]> = b;
    Ok(([(header::CACHE_CONTROL, "public, max-age=86400")], Json(serde_json::json!({ "track_id": id, "bands": json }))).into_response())
}

// --- grid, cues, mix points ------------------------------------------------------------------------------------

fn load_cues(c: &Connection, id: i64) -> Result<Vec<CuePoint>, bc_db::DbError> {
    let mut st = c.prepare("SELECT id, kind, pos_ms, end_ms, label, color, slot, auto FROM cue_points WHERE track_id = ?1 ORDER BY pos_ms, id")?;
    let rows = st
        .query_map([id], |r| {
            let kind: String = r.get(1)?;
            Ok((r.get::<_, i64>(0)?, kind, r.get::<_, f64>(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get::<_, Option<i64>>(6)?, r.get::<_, i64>(7)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(cid, kind, pos, end, label, color, slot, auto)| {
            Some(CuePoint {
                id: Some(cid),
                track_id: id,
                kind: cue_kind_parse(&kind)?,
                pos_ms: pos,
                end_ms: end,
                label,
                color,
                slot: slot.map(|s| s as u8),
                auto: auto != 0,
            })
        })
        .collect())
}

async fn get_cues(State(s): S, Path(id): Path<i64>) -> ApiResult<Json<Vec<CuePoint>>> {
    Ok(Json(s.db.read_async(move |c| load_cues(c, id)).await?))
}

async fn put_cues(State(s): S, Path(id): Path<i64>, Json(cues): Json<Vec<CuePoint>>) -> ApiResult<Json<Vec<CuePoint>>> {
    let now = crate::time_now();
    s.db
        .write_async(move |tx| {
            tx.execute("DELETE FROM cue_points WHERE track_id = ?1 AND auto = 0", [id])?;
            for q in cues.iter().filter(|q| !q.auto) {
                tx.execute(
                    "INSERT INTO cue_points (track_id, kind, pos_ms, end_ms, label, color, slot, auto, created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,0,?8)",
                    params![id, cue_kind_str(q.kind), q.pos_ms, q.end_ms, q.label, q.color, q.slot.map(|v| v as i64), now],
                )?;
            }
            Ok(())
        })
        .await?;
    Ok(Json(s.db.read_async(move |c| load_cues(c, id)).await?))
}

pub fn music_info(c: &Connection, id: i64) -> Result<Option<TrackMusicInfo>, bc_db::DbError> {
    let base: Option<(Option<i64>, Option<f64>, Option<String>, Option<f64>, Option<f64>)> = c
        .query_row(
            "SELECT t.duration_ms, a.bpm, a.camelot, a.beat_offset_ms, a.bpm_confidence FROM tracks t LEFT JOIN analysis a ON a.track_id = t.id WHERE t.id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((dur, bpm, camelot, offset, conf)) = base else { return Ok(None) };
    let stored: Option<String> = c.query_row("SELECT grid FROM beat_grids WHERE track_id = ?1", [id], |r| r.get(0)).optional()?;
    let grid = stored
        .and_then(|j| serde_json::from_str::<BeatGrid>(&j).ok())
        .or_else(|| legacy_grid(bpm, offset, conf, dur.unwrap_or(0) as f64 / 1000.0));
    let cues = load_cues(c, id)?;
    let find = |k: CueKind| cues.iter().find(|q| q.kind == k).map(|q| q.pos_ms as i64);
    let mut mix = mixpoints::defaults(dur, bpm);
    if let Some(i) = find(CueKind::MixIn) {
        mix.cue_in_ms = i;
    }
    if let Some(o) = find(CueKind::MixOut) {
        mix.cue_out_ms = Some(o);
    }
    Ok(Some(TrackMusicInfo { track_id: id, duration_ms: dur, bpm, camelot, grid, cues, mix_points: Some(mix) }))
}

async fn music(State(s): S, Path(id): Path<i64>) -> ApiResult<Json<TrackMusicInfo>> {
    s.db.read_async(move |c| music_info(c, id)).await?.map(Json).ok_or_else(|| ApiError::not_found(format!("track {id} not found")))
}

// --- accuracy report / waveform cache ------------------------------------------------------------

async fn accuracy(State(s): S) -> ApiResult<Json<AccuracyOut>> {
    let out = s
        .db
        .read_async(|c| {
            let rep = bc_db::settings::get(c, SETTING_ACCURACY_REPORT)?.and_then(|j| serde_json::from_str::<AccuracyReport>(&j).ok());
            let native = matches!(bc_db::settings::get(c, crate::persist::SETTING_NATIVE_BPM_KEY)?.as_deref(), Some("1") | Some("true"));
            Ok(AccuracyOut { report: rep, native_bpm_key: native })
        })
        .await?;
    Ok(Json(out))
}

async fn waveform_cache(State(s): S) -> ApiResult<Json<WaveformCacheOut>> {
    let sc = s.clone();
    let st = tokio::task::spawn_blocking(move || sc.waveforms.stats()).await.map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(WaveformCacheOut {
        bytes: st.bytes,
        files: st.files,
        cap_bytes: st.cap_bytes,
        detail_files: st.files.saturating_sub(st.overview_only_files),
        overview_only_files: st.overview_only_files,
    }))
}
