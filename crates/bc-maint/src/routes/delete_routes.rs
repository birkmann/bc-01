use axum::Json;
use axum::extract::{Path, State};
use bc_libcore::ApiResult;
use bc_types::library::maint::{RemoveTracksRequest, RemovedOut};
use bc_types::library::{DeleteLabelResult, DeleteReleasesRequest, DeleteReleasesResult, DeletedOut};

use super::{MaintState, blocking};
use crate::delete;

/// `POST /releases/delete`: many releases, their files, optionally blacklisted. One transaction
/// per release; failures are named in the result rather than raised.
pub async fn delete_releases(State(s): State<MaintState>, Json(body): Json<DeleteReleasesRequest>) -> ApiResult<Json<DeleteReleasesResult>> {
    let ctx = s.ctx.clone();
    let r = blocking(move || delete::delete_releases(&ctx, &body.ids, body.blacklist, body.reason.as_deref())).await?;
    Ok(Json(r))
}

pub async fn delete_track(State(s): State<MaintState>, Path(id): Path<i64>) -> ApiResult<Json<DeletedOut>> {
    let ctx = s.ctx.clone();
    Ok(Json(blocking(move || delete::delete_track(&ctx, id)).await?))
}

/// `POST /tracks/remove`: out of the library, files kept and excluded from scans.
pub async fn remove_tracks(State(s): State<MaintState>, Json(body): Json<RemoveTracksRequest>) -> ApiResult<Json<RemovedOut>> {
    let ctx = s.ctx.clone();
    Ok(Json(blocking(move || delete::remove_tracks(&ctx, &body.track_ids)).await?))
}

pub async fn delete_release(State(s): State<MaintState>, Path(id): Path<i64>) -> ApiResult<Json<DeletedOut>> {
    let ctx = s.ctx.clone();
    Ok(Json(blocking(move || delete::delete_release(&ctx, id)).await?))
}

pub async fn delete_label(State(s): State<MaintState>, Path(id): Path<i64>) -> ApiResult<Json<DeleteLabelResult>> {
    let ctx = s.ctx.clone();
    Ok(Json(blocking(move || delete::delete_label(&ctx, id)).await?))
}
