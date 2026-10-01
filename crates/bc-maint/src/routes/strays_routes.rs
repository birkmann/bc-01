use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use bc_libcore::{ApiError, ApiResult, Q};
use bc_types::library::{StrayMergeRequest, StraySweepStatus, StraysOut};
use serde::Deserialize;

use super::{MaintState, blocking};
use crate::strays;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct StraysQuery {
    pub label_id: Option<i64>,
    pub limit: Option<i64>,
}

/// `GET /releases/strays`: what a Bandcamp single-track download leaves behind. Listing is free;
/// only the merge needs Bandcamp.
pub async fn list(State(s): State<MaintState>, Q(q): Q<StraysQuery>) -> ApiResult<Json<StraysOut>> {
    let limit = q.limit.unwrap_or(50);
    if !(1..=500).contains(&limit) {
        return Err(ApiError::bad("limit must be between 1 and 500"));
    }
    let ctx = s.ctx.clone();
    Ok(Json(blocking(move || ctx.read(|c| strays::strays_out(c, q.label_id, limit as usize))).await?))
}

fn merger(s: &MaintState) -> ApiResult<&std::sync::Arc<strays::StrayMerger>> {
    s.merger.as_ref().ok_or_else(|| ApiError::conflict("the Bandcamp client is not available"))
}

/// `POST /releases/strays/merge`: returns as soon as the sweep starts (each stray costs a
/// rate-limited page fetch, so a library-wide run is half an hour of work); progress is the
/// `library.strays` event and `GET` of this path; stoppable with `DELETE`.
pub async fn merge(State(s): State<MaintState>, Json(body): Json<StrayMergeRequest>) -> ApiResult<(StatusCode, Json<StraySweepStatus>)> {
    let m = merger(&s)?;
    if body.limit.is_some_and(|l| l < 1) {
        return Err(ApiError::bad("limit must be >= 1"));
    }
    let ids = if body.ids.is_empty() { None } else { Some(body.ids) };
    let state = m.start(body.label_id, ids, body.limit.map(|l| l as usize)).map_err(|e| ApiError::conflict(e.to_string()))?;
    Ok((StatusCode::ACCEPTED, Json(state)))
}

pub async fn status(State(s): State<MaintState>) -> ApiResult<Json<StraySweepStatus>> {
    Ok(Json(merger(&s)?.state()))
}

pub async fn stop(State(s): State<MaintState>) -> ApiResult<Json<StraySweepStatus>> {
    let m = merger(&s)?;
    m.stop().await;
    Ok(Json(m.state()))
}
