//! `/library/roots`, `/library/browse`, `/library/scan` and `/library/excluded` routes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use bc_libcore::{ApiError, ApiResult, Ctx, Q};
use bc_types::Accepted;
use bc_types::library::{
    AddRootRequest, BrowseOut, RootOut, RootPatch, ScanProgress, ScanResult, ScanStatus, TOPIC_LIBRARY_CHANGED,
    TOPIC_LIBRARY_SCAN_DONE, TOPIC_LIBRARY_SCAN_PROGRESS,
};
use serde::Deserialize;

use crate::roots;
use crate::scanner::{ScanHooks, scan_root};

/// Router with state applied; paths WITHOUT the `/api` prefix.
pub fn router(ctx: Ctx) -> Router {
    Router::new()
        .route("/library/roots", get(list_roots).post(add_root))
        .route("/library/browse", get(browse))
        .route("/library/roots/{id}", patch(patch_root).delete(delete_root))
        .route("/library/scan", post(start_scan))
        .route("/library/scan/{job_id}", get(scan_status))
        .route("/library/scan/{job_id}/cancel", post(cancel_scan))
        .route("/library/excluded", get(crate::excluded::list))
        .route("/library/excluded/restore", post(crate::excluded::restore_route))
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

#[derive(Debug, Deserialize, Default)]
struct BrowseQuery {
    path: Option<String>,
    #[serde(default)]
    hidden: bool,
}

async fn browse(State(ctx): State<Ctx>, Q(q): Q<BrowseQuery>) -> ApiResult<Json<BrowseOut>> {
    let out = tokio::task::spawn_blocking(move || crate::browse::browse(&ctx, q.path.as_deref(), q.hidden))
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
    let cancel_flag = AtomicBool::new(false);
    let finished = AtomicBool::new(false);
    let (results, failure) = std::thread::scope(|s| {
        // The walk reports no progress, so a cancel is relayed to the scanner's flag from here
        // rather than from the progress callback.
        s.spawn(|| {
            while !finished.load(Ordering::Relaxed) {
                if handle.cancelled() {
                    cancel_flag.store(true, Ordering::Relaxed);
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let out = scan_roots(&ctx, &handle, ids, &cancel_flag);
        finished.store(true, Ordering::Relaxed);
        out
    });
    let cancelled = handle.cancelled();
    if failure.is_none() && !cancelled {
        // Records that arrived in this scan carry no publisher tag if they came from Bandcamp, so
        // file them under the label Bandcamp stated for them (unlabelled releases only).
        match ctx.write(bc_maint::dedup::backfill_release_labels) {
            Ok(n) if n > 0 => tracing::info!("filed {n} release(s) under their label"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "label backfill failed"),
        }
    }
    ctx.bus.publish(
        TOPIC_LIBRARY_SCAN_DONE,
        &serde_json::json!({ "job_id": handle.id, "results": results, "cancelled": cancelled }),
    );
    ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &bc_types::library::LibraryChanged::default());
    match failure {
        Some(e) => handle.finish_err(e),
        None => handle.finish_ok(serde_json::json!({ "results": results })),
    }
}

/// Scan `ids` in order until one fails or `cancel` is set; the results so far and the failure.
fn scan_roots(ctx: &Ctx, handle: &bc_libcore::JobHandle, ids: Vec<i64>, cancel: &AtomicBool) -> (Vec<ScanResult>, Option<String>) {
    let mut results: Vec<ScanResult> = Vec::new();
    for id in ids {
        if cancel.load(Ordering::Relaxed) || handle.cancelled() {
            break;
        }
        let mut last = Instant::now() - Duration::from_secs(1);
        let h = handle.clone();
        let bus = ctx.bus.clone();
        let mut prog = move |phase: &'static str, seen: i64, total: i64| {
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
        match scan_root(ctx, id, ScanHooks { progress: Some(&mut prog), cancel }) {
            Ok(r) => results.push(r),
            Err(e) => return (results, Some(e.to_string())),
        }
    }
    (results, None)
}

/// Ask a running scan to stop. It stops between files (or directories, while walking); what
/// was written so far stays, nothing is marked missing, and `library.scan.done` follows with
/// `"cancelled": true`. Stopping a scan that already finished is a no-op.
async fn cancel_scan(State(ctx): State<Ctx>, Path(job_id): Path<String>) -> ApiResult<(StatusCode, Json<Accepted>)> {
    ctx.jobs.get(&job_id).filter(|t| t.kind == "scan").ok_or_else(|| ApiError::not_found(format!("scan {job_id} not found")))?;
    ctx.jobs.request_cancel(&job_id);
    Ok((StatusCode::ACCEPTED, Json(Accepted { job_id })))
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
        "cancelled" => "cancelled",
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
