use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bc_libcore::{ApiError, ApiResult, Ctx};
use bc_types::Accepted;
use bc_types::library::{
    LibraryChanged, MoveOutcome, MovePlanOut, MoveProgress, MoveRequest, MoveResult, TOPIC_LIBRARY_CHANGED, TOPIC_LIBRARY_MOVE_PROGRESS,
};
use parking_lot::Mutex;

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

/// Moves running in this process, by root: the latest progress of each (what a client that opens
/// the dialog mid-move, or reloads, picks up from `GET /library/moves`).
static ACTIVE: Mutex<BTreeMap<i64, MoveProgress>> = Mutex::new(BTreeMap::new());

/// `POST /library/roots/{id}/move`: relocate a library or downloads folder, including to another
/// drive or into another root, as a tracked task (`202 { job_id }`, progress as
/// `library.move.progress`, then `library.changed`). Each file is moved and its row updated before
/// the next one is touched, so an interruption leaves the database agreeing with the filesystem.
/// A blocked plan is refused up front with 400 and changes nothing; a root that is already being
/// moved (or scanned) answers 409; `dry_run` answers with the plan instead.
pub async fn start(State(s): State<MaintState>, Path(id): Path<i64>, Json(body): Json<MoveRequest>) -> ApiResult<Response> {
    if body.dry_run {
        let ctx = s.ctx.clone();
        let checked = blocking(move || relocate::plan(&ctx, id, &body.target_path).map_err(ApiError::from)).await?;
        return Ok(Json(checked.out()).into_response());
    }
    let job_id = spawn(&s.ctx, id, &body.target_path).await?;
    Ok((StatusCode::ACCEPTED, Json(Accepted { job_id })).into_response())
}

/// `POST /library/roots/{id}/move/cancel`: stop the root's move after the file in hand. Everything
/// moved so far stays consistent; starting the move again resumes it.
pub async fn cancel(State(s): State<MaintState>, Path(id): Path<i64>) -> ApiResult<(StatusCode, Json<Accepted>)> {
    let job_id = ACTIVE.lock().get(&id).map(|p| p.job_id.clone()).ok_or_else(|| ApiError::not_found(format!("no move of root {id} is running")))?;
    s.ctx.jobs.request_cancel(&job_id);
    Ok((StatusCode::ACCEPTED, Json(Accepted { job_id })))
}

/// `GET /library/moves`: the moves running now, with their latest progress.
pub async fn list(State(_): State<MaintState>) -> Json<Vec<MoveProgress>> {
    Json(ACTIVE.lock().values().cloned().collect())
}

/// Start a move task. The roots it touches stay locked (no scan, watcher ingest or second move)
/// until it ends, and a `root_moves` row records it until it completes or is stopped, so a move cut
/// short by a quit or crash resumes at the next start ([`resume_pending`]).
pub async fn spawn(ctx: &Ctx, id: i64, target: &str) -> ApiResult<String> {
    let checked = {
        let (ctx, target) = (ctx.clone(), target.to_string());
        blocking(move || relocate::plan(&ctx, id, &target).map_err(ApiError::from)).await?
    };
    if !checked.ok() {
        let msg = checked.warnings.iter().filter(|w| w.starts_with("BLOCK:")).map(|w| w.trim_start_matches("BLOCK: ").to_string()).collect::<Vec<_>>().join("; ");
        return Err(ApiError::bad(msg));
    }
    let guard = bc_libcore::root_lock::try_lock(&ctx.db, &checked.roots(), "move").map_err(|(_, holder)| {
        ApiError::conflict(match holder {
            "move" => "this folder is already being moved",
            "scan" => "a scan of this folder is running; stop it or wait for it to finish",
            _ => "the library is taking in file changes; try again in a moment",
        })
    })?;
    {
        let target = target.to_string();
        ctx.write_async(move |t| {
            t.execute("INSERT OR REPLACE INTO root_moves(root_id, target) VALUES (?1, ?2)", (id, target))?;
            Ok(())
        })
        .await?;
    }
    let handle = ctx.jobs.begin("move", &format!("move {}", checked.source));
    let job_id = handle.id.clone();
    let first = MoveProgress { job_id: job_id.clone(), root_id: id, target: checked.target.clone(), total: checked.file_count, ..Default::default() };
    ACTIVE.lock().insert(id, first.clone());
    handle.progress(0, Some(checked.file_count), None);

    let (task_ctx, target) = (ctx.clone(), target.to_string());
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let mut last_publish: Option<Instant> = None;
        let interval = Duration::from_millis(1000 / PROGRESS_PER_SEC);
        let snapshot = |p: &relocate::RelocateProgress| MoveProgress {
            moved: p.moved,
            total: p.total,
            bytes_moved: p.bytes_moved,
            current: Some(p.current.clone()),
            done: p.done,
            duplicates: p.duplicates,
            left: (p.conflicts.len() + p.errors.len()) as i64,
            outcome: p.outcome,
            ..first.clone()
        };
        let result = relocate::execute(&task_ctx, id, &target, |p| {
            let ev = snapshot(p);
            ACTIVE.lock().insert(id, ev.clone());
            // Coalesce: a 190k-file move would otherwise flood the stream.
            if !p.done && last_publish.is_none_or(|t| t.elapsed() >= interval) {
                last_publish = Some(Instant::now());
                handle.progress(p.moved, Some(p.total), Some(&p.current));
                task_ctx.bus.publish(TOPIC_LIBRARY_MOVE_PROGRESS, &ev);
            }
            !handle.cancelled()
        });
        let (last, outcome, error) = match &result {
            Ok(p) if p.done => (snapshot(p), p.outcome.unwrap_or(MoveOutcome::Complete), None),
            Ok(p) => (snapshot(p), MoveOutcome::Cancelled, None),
            Err(e) => (ACTIVE.lock().get(&id).cloned().unwrap_or_else(|| first.clone()), MoveOutcome::Failed, Some(e.to_string())),
        };
        // A move that failed part-way is still owed: keep its row so the next start resumes it.
        // One that finished, or that the user stopped, is not.
        if outcome != MoveOutcome::Failed {
            let _ = task_ctx.write(move |t| {
                t.execute("DELETE FROM root_moves WHERE root_id = ?1", [id])?;
                Ok(())
            });
        }
        let new_path = task_ctx
            .read(|c| Ok(c.query_row("SELECT path FROM library_roots WHERE id = ?1", [id], |r| r.get::<_, String>(0))?))
            .unwrap_or_default();
        ACTIVE.lock().remove(&id);
        task_ctx.bus.publish(TOPIC_LIBRARY_MOVE_PROGRESS, &MoveProgress { done: true, outcome: Some(outcome), error: error.clone(), ..last.clone() });
        task_ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &LibraryChanged { moved_root: Some(id), ..Default::default() });
        handle.progress(last.moved, Some(last.total), last.current.as_deref());
        match result {
            Ok(p) => {
                let res = MoveResult {
                    moved: p.moved,
                    total: p.total,
                    bytes_moved: p.bytes_moved,
                    errors: p.errors.iter().take(50).cloned().collect(),
                    new_path,
                    conflicts: p.conflicts.iter().take(50).cloned().collect(),
                    conflict_count: p.conflicts.len() as i64,
                    error_count: p.errors.len() as i64,
                    duplicates: p.duplicates,
                    outcome,
                };
                handle.finish_ok(serde_json::to_value(res).unwrap_or_default());
            }
            Err(e) => handle.finish_err(e),
        }
    });
    Ok(job_id)
}

/// Restart the moves a quit or crash cut short (their `root_moves` rows). Call once at startup,
/// after the roots are registered. A row whose root already sits at the target (the move had
/// finished, the row had not yet gone) is dropped; one that cannot start now (the drive is not
/// mounted) is kept for the next start.
pub async fn resume_pending(ctx: &Ctx) {
    let rows: Vec<(i64, String, String)> = ctx
        .read_async(|c| {
            let mut st = c.prepare("SELECT m.root_id, m.target, r.path FROM root_moves m JOIN library_roots r ON r.id = m.root_id")?;
            Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?)
        })
        .await
        .unwrap_or_default();
    for (id, target, current) in rows {
        if crate::util::canon(std::path::Path::new(&target)) == crate::util::canon(std::path::Path::new(&current)) {
            let _ = ctx
                .write_async(move |t| {
                    t.execute("DELETE FROM root_moves WHERE root_id = ?1", [id])?;
                    Ok(())
                })
                .await;
            continue;
        }
        match spawn(ctx, id, &target).await {
            Ok(job) => tracing::info!(root_id = id, %target, job, "resuming an interrupted move"),
            Err(e) => tracing::warn!(root_id = id, %target, error = %e, "cannot resume an interrupted move yet"),
        }
    }
}
