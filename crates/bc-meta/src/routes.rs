//! `/metadata/*` routes.

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bc_libcore::{ApiError, ApiResult, Ctx, Q};
use bc_types::library::*;
use serde::Deserialize;

use crate::{journal, runner};

pub fn router(ctx: Ctx) -> Router {
    Router::new()
        .route("/metadata/status", get(status))
        .route("/metadata/preview", post(preview))
        .route("/metadata/write", post(write))
        .route("/metadata/tracks/{id}", post(write_one))
        .route("/metadata/jobs/{job_id}/snapshot", get(snapshot))
        .route("/metadata/jobs/{job_id}/undo", post(undo))
        .with_state(ctx)
}

async fn status(State(ctx): State<Ctx>) -> ApiResult<Json<MetadataStatus>> {
    let running = ctx.jobs.list().iter().filter(|t| t.kind == runner::KIND && t.state == "running").count() as i64;
    ctx.read_async(move |c| {
        let one = |sql: &str| -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
        Ok(MetadataStatus {
            total_tracks: one("SELECT COUNT(*) FROM tracks"),
            analysed: one("SELECT COUNT(*) FROM analysis WHERE status != 'failed'"),
            writable_extensions: runner::writable_extensions(),
            default_groups: runner::group_names(&bc_media::write::DEFAULT_GROUPS),
            write_mode: "atomic".into(),
            running_jobs: running,
        })
    })
    .await
    .map(Json)
}

fn resolve_ids(c: &bc_db::rusqlite::Connection, scope: MetadataScope, ids: &[i64]) -> ApiResult<Vec<i64>> {
    match scope {
        MetadataScope::Ids => {
            if ids.is_empty() {
                return Err(ApiError::bad("scope 'ids' needs track_ids"));
            }
            Ok(ids.to_vec())
        }
        MetadataScope::All => runner::pending_track_ids(c, true),
        MetadataScope::Analysed => runner::pending_track_ids(c, false),
    }
}

async fn preview(State(ctx): State<Ctx>, Json(body): Json<PreviewRequest>) -> ApiResult<Json<PreviewOut>> {
    let c2 = ctx.clone();
    tokio::task::spawn_blocking(move || -> ApiResult<PreviewOut> {
        let groups = runner::parse_groups(&body.groups);
        let ids: Vec<i64> = c2.read(|c| resolve_ids(c, body.scope, &body.track_ids))?.into_iter().take(body.limit.clamp(1, 500) as usize).collect();
        let mut summary = PlanSummary { tracks: ids.len() as i64, ..Default::default() };
        let mut items = vec![];
        for id in ids {
            let (rep, _) = c2.read(|c| runner::plan_track(c, id, &groups))?;
            if !rep.conflicts.is_empty() {
                summary.conflicts += 1;
            }
            match rep.status.as_str() {
                "written" => {
                    summary.would_write += 1;
                    for f in &rep.written {
                        *summary.per_field.entry(f.clone()).or_default() += 1;
                    }
                }
                "not_analysed" => summary.not_analysed += 1,
                "unsupported" => summary.unsupported += 1,
                _ => summary.no_gaps += 1,
            }
            items.push(plan_out(&rep));
        }
        Ok(PreviewOut { summary, items })
    })
    .await
    .map_err(ApiError::internal)?
    .map(Json)
}

fn plan_out(r: &runner::TrackReport) -> TrackPlanOut {
    TrackPlanOut {
        track_id: r.track_id,
        status: r.status.clone(),
        message: r.message(),
        would_write: r.written.clone(),
        conflicts: r.conflicts.clone(),
        fields: r.fields.clone(),
    }
}

async fn write(State(ctx): State<Ctx>, Json(body): Json<WriteRequest>) -> ApiResult<Response> {
    if !body.dry_run && !body.confirm {
        return Err(ApiError::bad("a real tag write needs confirm=true; preview it first"));
    }
    let groups = runner::parse_groups(&body.groups);
    let (scope, track_ids, limit) = (body.scope, body.track_ids.clone(), body.limit);
    let mut ids = ctx.read_async(move |c| resolve_ids(c, scope, &track_ids)).await?;
    if let Some(l) = limit {
        ids.truncate(l.max(0) as usize);
    }
    if ids.is_empty() {
        let out = WriteQueued { queued: 0, job_id: None, dry_run: body.dry_run, detail: "nothing to write".into() };
        return Ok((StatusCode::OK, Json(out)).into_response());
    }
    let n = ids.len() as i64;
    let params = runner::JobParams { groups: runner::group_names(&groups), dry_run: body.dry_run, mode: body.mode.clone().unwrap_or_else(|| "atomic".into()) };
    let id = runner::spawn_job(&ctx, ids, params);
    let out = WriteQueued { queued: n, job_id: Some(id), dry_run: body.dry_run, detail: "queued".into() };
    Ok((StatusCode::ACCEPTED, Json(out)).into_response())
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OneQ {
    dry_run: Option<bool>,
    groups: Vec<String>,
}

async fn write_one(State(ctx): State<Ctx>, Path(id): Path<i64>, Q(q): Q<OneQ>) -> ApiResult<Json<TrackPlanOut>> {
    let exists = ctx.read_async(move |c| Ok(c.query_row("SELECT 1 FROM tracks WHERE id = ?1", [id], |_| Ok(())).is_ok())).await?;
    if !exists {
        return Err(ApiError::not_found(format!("track {id} not found")));
    }
    let groups = runner::parse_groups(&q.groups);
    let dry = q.dry_run.unwrap_or(true);
    let c2 = ctx.clone();
    // No undo journal: one track is trivially re-derivable; this endpoint is for trying a change out.
    let rep = tokio::task::spawn_blocking(move || runner::write_track(&c2, id, &groups, dry, None, None)).await.map_err(ApiError::internal)??;
    Ok(Json(plan_out(&rep)))
}

async fn snapshot(State(ctx): State<Ctx>, Path(job_id): Path<String>) -> ApiResult<Response> {
    let p = journal::path(&ctx.config.backups_dir(), &job_id);
    let bytes = tokio::fs::read(&p).await.map_err(|_| ApiError::not_found(format!("no undo journal for job {job_id}")))?;
    let mut resp = bytes.into_response();
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-ndjson"));
    Ok(resp)
}

async fn undo(State(ctx): State<Ctx>, Path(job_id): Path<String>) -> ApiResult<Json<UndoOut>> {
    let c2 = ctx.clone();
    tokio::task::spawn_blocking(move || runner::undo_job(&c2, &job_id)).await.map_err(ApiError::internal)?.map(Json)
}
