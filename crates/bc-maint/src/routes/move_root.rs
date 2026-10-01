use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bc_libcore::{ApiError, ApiResult};
use bc_types::Accepted;
use bc_types::library::{LibraryChanged, MovePlanOut, MoveProgress, MoveRequest, MoveResult, TOPIC_LIBRARY_CHANGED, TOPIC_LIBRARY_MOVE_PROGRESS};

use super::{MaintState, blocking};
use crate::relocate;

/// `POST /library/roots/{id}/move/plan`: validate free space, permissions and destination before
/// moving anything.
pub async fn plan(State(s): State<MaintState>, Path(id): Path<i64>, Json(body): Json<MoveRequest>) -> ApiResult<Json<MovePlanOut>> {
    let ctx = s.ctx.clone();
    let p = blocking(move || relocate::plan(&ctx, id, &body.target_path).map_err(ApiError::from)).await?;
    Ok(Json(p.out()))
}

/// Progress events are coalesced to at most this many per second.
pub const PROGRESS_PER_SEC: u64 = 4;

/// `POST /library/roots/{id}/move`: relocate a library or downloads folder, including to another
/// drive, as a tracked task (`202 { job_id }`, progress as `library.move.progress`, then
/// `library.changed`). Each file is moved and its row updated before the next one is touched, so an
/// interruption leaves the database agreeing with the filesystem. A blocked plan is refused up
/// front with 400 and changes nothing; `dry_run` answers with the plan instead.
pub async fn start(State(s): State<MaintState>, Path(id): Path<i64>, Json(body): Json<MoveRequest>) -> ApiResult<Response> {
    let ctx = s.ctx.clone();
    let target = body.target_path.clone();
    let checked = {
        let (ctx, target) = (ctx.clone(), target.clone());
        blocking(move || relocate::plan(&ctx, id, &target).map_err(ApiError::from)).await?
    };
    if body.dry_run {
        return Ok(Json(checked.out()).into_response());
    }
    if !checked.ok() {
        let msg = checked.warnings.iter().filter(|w| w.starts_with("BLOCK:")).map(|w| w.trim_start_matches("BLOCK: ").to_string()).collect::<Vec<_>>().join("; ");
        return Err(ApiError::bad(msg));
    }
    let handle = ctx.jobs.begin("move", &format!("move {}", checked.source));
    let job_id = handle.id.clone();
    let task_ctx = ctx.clone();
    tokio::task::spawn_blocking(move || {
        let mut last_publish: Option<Instant> = None;
        let interval = Duration::from_millis(1000 / PROGRESS_PER_SEC);
        let result = relocate::execute(&task_ctx, id, &target, |p| {
            handle.progress(p.moved, Some(p.total), Some(&p.current));
            // Coalesce: a 190k-file move would otherwise flood the stream.
            if p.done || last_publish.is_none_or(|t| t.elapsed() >= interval) {
                last_publish = Some(Instant::now());
                task_ctx.bus.publish(
                    TOPIC_LIBRARY_MOVE_PROGRESS,
                    &MoveProgress { root_id: id, moved: p.moved, total: p.total, bytes_moved: p.bytes_moved, current: Some(p.current.clone()), done: p.done },
                );
            }
            !handle.cancelled()
        });
        match result {
            Ok(p) => {
                if !p.done {
                    // Cancelled: say so once, with the final counts.
                    task_ctx.bus.publish(
                        TOPIC_LIBRARY_MOVE_PROGRESS,
                        &MoveProgress { root_id: id, moved: p.moved, total: p.total, bytes_moved: p.bytes_moved, current: Some(p.current.clone()), done: true },
                    );
                }
                let new_path = task_ctx
                    .read(|c| Ok(c.query_row("SELECT path FROM library_roots WHERE id = ?1", [id], |r| r.get::<_, String>(0))?))
                    .unwrap_or_default();
                let res = MoveResult { moved: p.moved, total: p.total, bytes_moved: p.bytes_moved, errors: p.errors.iter().take(50).cloned().collect(), new_path };
                task_ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &LibraryChanged { moved_root: Some(id), ..Default::default() });
                handle.finish_ok(serde_json::to_value(res).unwrap_or_default());
            }
            Err(e) => handle.finish_err(e),
        }
    });
    Ok((StatusCode::ACCEPTED, Json(Accepted { job_id })).into_response())
}
