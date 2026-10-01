//! `/library/roots` and `/library/scan` routes.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use bc_libcore::{ApiError, ApiResult, Ctx, Q};
use bc_types::Accepted;
use bc_types::library::{
    AddRootRequest, RootOut, RootPatch, ScanProgress, ScanResult, ScanStatus, TOPIC_LIBRARY_CHANGED,
    TOPIC_LIBRARY_SCAN_DONE, TOPIC_LIBRARY_SCAN_PROGRESS,
};
use serde::Deserialize;

use crate::roots;
use crate::scanner::{ScanHooks, scan_root};

/// Router with state applied; paths WITHOUT the `/api` prefix.
pub fn router(ctx: Ctx) -> Router {
    Router::new()
        .route("/library/roots", get(list_roots).post(add_root))
        .route("/library/roots/{id}", patch(patch_root).delete(delete_root))
        .route("/library/scan", post(start_scan))
        .route("/library/scan/{job_id}", get(scan_status))
        .with_state(ctx)
}

async fn list_roots(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<RootOut>>> {
    Ok(Json(ctx.read_async(roots::list_roots).await?))
}

async fn add_root(State(ctx): State<Ctx>, Json(body): Json<AddRootRequest>) -> ApiResult<Json<RootOut>> {
    let out = tokio::task::spawn_blocking(move || roots::add_root(&ctx, &body.path, &body.kind))
        .await
        .map_err(ApiError::internal)??;
    Ok(Json(out))
}

async fn delete_root(State(ctx): State<Ctx>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    tokio::task::spawn_blocking(move || roots::remove_root(&ctx, id)).await.map_err(ApiError::internal)??;
    Ok(StatusCode::NO_CONTENT)
}

async fn patch_root(State(ctx): State<Ctx>, Path(id): Path<i64>, Json(body): Json<RootPatch>) -> ApiResult<Json<RootOut>> {
    let out = tokio::task::spawn_blocking(move || roots::patch_root(&ctx, id, body)).await.map_err(ApiError::internal)??;
    Ok(Json(out))
}

#[derive(Debug, Deserialize, Default)]
struct ScanQuery {
    root_id: Option<i64>,
}

async fn start_scan(State(ctx): State<Ctx>, Q(q): Q<ScanQuery>) -> ApiResult<(StatusCode, Json<Accepted>)> {
    let job_id = spawn_scan(&ctx, q.root_id).await?;
    Ok((StatusCode::ACCEPTED, Json(Accepted { job_id })))
}

/// Start a tracked scan task (`kind = "scan"`) over one root, or every enabled root. Returns the
/// job id. Errors 404 for an unknown `root_id`.
pub async fn spawn_scan(ctx: &Ctx, root_id: Option<i64>) -> ApiResult<String> {
    let ids: Vec<i64> = ctx
        .read_async(move |c| {
            let mut st = c.prepare("SELECT id FROM library_roots WHERE (?1 IS NULL AND enabled = 1) OR id = ?1 ORDER BY id")?;
            Ok(st.query_map([root_id], |r| r.get(0))?.collect::<Result<_, _>>()?)
        })
        .await?;
    if let Some(id) = root_id
        && ids.is_empty()
    {
        return Err(ApiError::not_found(format!("root {id} not found")));
    }
    let label = match root_id {
        Some(id) => format!("scan root {id}"),
        None => "scan all roots".to_string(),
    };
    let handle = ctx.jobs.begin("scan", &label);
    let job_id = handle.id.clone();
    handle.progress(0, None, Some(&progress_msg("walk", ids.first().copied())));
    let ctx2 = ctx.clone();
    tokio::task::spawn_blocking(move || run_scan_task(ctx2, handle, ids));
    Ok(job_id)
}

fn progress_msg(phase: &str, root_id: Option<i64>) -> String {
    serde_json::json!({ "phase": phase, "root_id": root_id }).to_string()
}

/// The body of a scan task: scans `ids` in order, publishes `library.scan.progress` (<= 4/s),
/// `library.scan.done` and `library.changed`, and finishes the job with
/// `{"results": [ScanResult..]}`.
pub fn run_scan_task(ctx: Ctx, handle: bc_libcore::JobHandle, ids: Vec<i64>) {
    let mut results: Vec<ScanResult> = Vec::new();
    let never = AtomicBool::new(false);
    let _ = &never;
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let mut failure: Option<String> = None;
    for id in ids {
        if handle.cancelled() {
            break;
        }
        let mut last = Instant::now() - Duration::from_secs(1);
        let h = handle.clone();
        let bus = ctx.bus.clone();
        let flag = cancel_flag.clone();
        let mut prog = move |phase: &'static str, seen: i64, total: i64| {
            if h.cancelled() {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if phase != "done" && last.elapsed() < Duration::from_millis(250) {
                return;
            }
            last = Instant::now();
            let total_opt = (total > 0).then_some(total);
            h.progress(seen, total_opt, Some(&progress_msg(phase, Some(id))));
            bus.publish(
                TOPIC_LIBRARY_SCAN_PROGRESS,
                &ScanProgress { job_id: h.id.clone(), root_id: id, phase: phase.into(), seen, total: total_opt },
            );
        };
        match scan_root(&ctx, id, ScanHooks { progress: Some(&mut prog), cancel: &cancel_flag }) {
            Ok(r) => results.push(r),
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        }
    }
    if failure.is_none() && !handle.cancelled() {
        // Records that arrived in this scan carry no publisher tag if they came from Bandcamp, so
        // file them under the label Bandcamp stated for them (unlabelled releases only).
        match ctx.write(bc_maint::dedup::backfill_release_labels) {
            Ok(n) if n > 0 => tracing::info!("filed {n} release(s) under their label"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "label backfill failed"),
        }
    }
    ctx.bus.publish(TOPIC_LIBRARY_SCAN_DONE, &serde_json::json!({ "job_id": handle.id, "results": results }));
    ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &bc_types::library::LibraryChanged::default());
    match failure {
        Some(e) => handle.finish_err(e),
        None => handle.finish_ok(serde_json::json!({ "results": results })),
    }
}

async fn scan_status(State(ctx): State<Ctx>, Path(job_id): Path<String>) -> ApiResult<Json<ScanStatus>> {
    let t = ctx.jobs.get(&job_id).filter(|t| t.kind == "scan").ok_or_else(|| ApiError::not_found(format!("scan {job_id} not found")))?;
    let msg: serde_json::Value = t.message.as_deref().and_then(|m| serde_json::from_str(m).ok()).unwrap_or_default();
    let results: Vec<ScanResult> = t
        .result
        .as_ref()
        .and_then(|r| r.get("results"))
        .and_then(|r| serde_json::from_value(r.clone()).ok())
        .unwrap_or_default();
    let state = match t.state.as_str() {
        "failed" => "failed",
        "running" => "running",
        _ => "done",
    };
    let phase = if state == "running" { msg.get("phase").and_then(|p| p.as_str()).unwrap_or("walk") } else { "done" };
    Ok(Json(ScanStatus {
        job_id: t.id,
        state: state.into(),
        root_id: msg.get("root_id").and_then(|v| v.as_i64()).or_else(|| results.first().map(|r| r.root_id)),
        phase: phase.into(),
        seen: t.done,
        total: t.total,
        results,
    }))
}
