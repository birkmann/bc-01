//! Follows: saved Explore queries, followed pages, and the feed sweep (port of
//! `api/routes/follows.py`).
//!
//! A follow row is a `harvest_sources` record. `kind` says what it points at (a discover query,
//! a search, an artist or label page), `config` carries the query in both the form the Explore
//! page recalls (`explore_params`) and the form the sweeper feeds to the API (`api_params`), and
//! `enabled` is the follow switch -- a saved-but-not-followed query is simply disabled.
//!
//! The sweep itself lives in [`crate::harvest::feed`]; the routes here mirror the label-sweep
//! trio (status / start / stop).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, put};
use axum::{Json, Router};
use bc_db::rusqlite::{OptionalExtension, params};
use bc_jobs::ApiError;
use bc_types::bandcamp::{
    FeedSweepRequest, FeedSweepStatus, FollowCreate, FollowOut, FollowPatch, FollowSettingsIn, FollowsOut,
};

use crate::harvest::fans::{dbr, dbw};
use crate::harvest::feed::{self, FeedSweeper, StartError};
use crate::service::Ctx;
use crate::urls;

type ApiResult<T> = Result<T, ApiError>;

fn sweeper(ctx: &Ctx) -> Arc<FeedSweeper> {
    ctx.expect::<FeedSweeper>()
}

// -- the sweep (declared first in the legacy module; axum matches static paths first anyway) --

async fn get_sweep(State(ctx): State<Arc<Ctx>>) -> Json<FeedSweepStatus> {
    Json(sweeper(&ctx).state())
}

/// Returns as soon as the sweep starts. One page fetch per follow at the polite rate is minutes
/// of work for a real shelf, so it runs in the background and reports on the event bus as
/// `feed.sweep`. Nothing is queued: new finds land in the feed for review.
async fn start_sweep(State(ctx): State<Arc<Ctx>>, body: Bytes) -> ApiResult<(StatusCode, Json<FeedSweepStatus>)> {
    let req: FeedSweepRequest = if body.iter().all(|c| c.is_ascii_whitespace()) {
        FeedSweepRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))?
    };
    if dbr(&ctx.db, feed::label_sweep_running).await? {
        return Err(ApiError::bad_request("a label sweep is running; wait for it to finish"));
    }
    match sweeper(&ctx).start(Some(&req.source_ids)).await {
        Ok(s) => Ok((StatusCode::ACCEPTED, Json(s))),
        Err(StartError::AlreadyRunning(e)) => Err(ApiError::bad_request(e.to_string())),
        Err(StartError::Other(e)) => Err(e.into()),
    }
}

/// Stops the walk, keeping whatever it has already found in the feed.
async fn stop_sweep(State(ctx): State<Arc<Ctx>>) -> Json<FeedSweepStatus> {
    let s = sweeper(&ctx);
    s.stop().await;
    Json(s.state())
}

// -- CRUD ---------------------------------------------------------------------

async fn list_follows(State(ctx): State<Arc<Ctx>>) -> ApiResult<Json<FollowsOut>> {
    Ok(Json(dbr(&ctx.db, feed::follows_out).await?))
}

/// Upserts: saving the same query or page twice updates the one row. Without this, the
/// `(kind, identifier)` unique constraint would turn the second press of "Save" into a 500 --
/// and pressing Save on something already saved plainly means "keep it saved", not "fail".
async fn create_follow(State(ctx): State<Arc<Ctx>>, Json(body): Json<FollowCreate>) -> ApiResult<Json<FollowOut>> {
    if !matches!(body.kind.as_str(), "discover" | "search" | "artist" | "label") {
        return Err(ApiError::bad_request("kind must be one of discover, search, artist, label"));
    }
    if let Some(l) = body.limit {
        if !(1..=2000).contains(&l) {
            return Err(ApiError::bad_request("limit must be between 1 and 2000"));
        }
    }
    let page_kind = matches!(body.kind.as_str(), "artist" | "label");
    let url = if page_kind { body.url.as_deref().filter(|u| !u.is_empty()).map(urls::artist_root) } else { None };
    if page_kind && url.is_none() {
        return Err(ApiError::bad_request("url is required to follow an artist or label"));
    }
    if body.kind == "search" {
        let q = body.explore_params.get("q").and_then(|v| v.as_str()).unwrap_or("");
        if q.trim().is_empty() {
            return Err(ApiError::bad_request("a saved search needs its q"));
        }
    }
    let identifier = feed::canonical_identifier(&body.kind, url.as_deref(), &body.api_params);
    let mut config = serde_json::Map::new();
    config.insert("explore_params".into(), serde_json::to_value(&body.explore_params).unwrap_or_default());
    config.insert("api_params".into(), serde_json::to_value(&body.api_params).unwrap_or_default());
    if let Some(l) = body.limit.filter(|l| *l != 0) {
        config.insert("limit".into(), l.into());
    }
    let config = serde_json::Value::Object(config).to_string();
    // Search results are not a release feed, so a saved search is never swept.
    let enabled = body.enabled && body.kind != "search";
    let label = body.label.trim().to_string();
    let kind = body.kind.clone();

    let row = dbw(&ctx.db, move |tx| {
        let existing: Option<i64> = tx
            .query_row("SELECT id FROM harvest_sources WHERE kind = ?1 AND identifier = ?2", params![kind, identifier], |r| r.get(0))
            .optional()?;
        let id = match existing {
            None => {
                tx.execute(
                    "INSERT INTO harvest_sources (kind, identifier, label, url, enabled, config, items_seen, items_new, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, ?7)",
                    params![kind, identifier, (!label.is_empty()).then_some(&label), url, enabled, config, bc_jobs::time::now()],
                )?;
                tx.last_insert_rowid()
            }
            Some(id) => {
                tx.execute(
                    "UPDATE harvest_sources SET label = CASE WHEN ?2 = '' THEN label ELSE ?2 END, \
                            url = COALESCE(?3, url), enabled = ?4, config = ?5 WHERE id = ?1",
                    params![id, label, url, enabled, config],
                )?;
                id
            }
        };
        feed::get_source(tx, id)?.ok_or_else(|| crate::error::HarvestError::other("follow vanished after save"))
    })
    .await?;
    Ok(Json(row.to_out()))
}

async fn patch_follow(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>, Json(body): Json<FollowPatch>) -> ApiResult<Json<FollowOut>> {
    let row = dbw(&ctx.db, move |tx| {
        let Some(row) = feed::get_source(tx, id)? else { return Ok(None) };
        if let Some(l) = body.label.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
            tx.execute("UPDATE harvest_sources SET label = ?2 WHERE id = ?1", params![id, l])?;
        }
        if let Some(e) = body.enabled {
            // A saved search stays recall-only; see create_follow.
            tx.execute("UPDATE harvest_sources SET enabled = ?2 WHERE id = ?1", params![id, e && row.kind != "search"])?;
        }
        feed::get_source(tx, id)
    })
    .await?;
    row.map(|r| Json(r.to_out())).ok_or_else(|| ApiError::not_found(format!("follow {id} not found")))
}

async fn delete_follow(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    let n = dbw(&ctx.db, move |tx| Ok(tx.execute("DELETE FROM harvest_sources WHERE id = ?1", [id])?)).await?;
    if n == 0 {
        return Err(ApiError::not_found(format!("follow {id} not found")));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn put_settings(State(ctx): State<Arc<Ctx>>, Json(body): Json<FollowSettingsIn>) -> ApiResult<Json<FollowsOut>> {
    if let Some(h) = body.poll_hours {
        if !h.is_finite() || !(0.0..=24.0 * 14.0).contains(&h) {
            return Err(ApiError::bad_request("poll_hours must be between 0 and 336"));
        }
    }
    let out = dbw(&ctx.db, move |tx| {
        if let Some(v) = body.include_library_artists {
            feed::store_setting(tx, feed::LIBRARY_ARTISTS_KEY, if v { "1" } else { "0" })?;
        }
        if let Some(v) = body.include_library_labels {
            feed::store_setting(tx, feed::LIBRARY_LABELS_KEY, if v { "1" } else { "0" })?;
        }
        if let Some(h) = body.poll_hours {
            feed::store_setting(tx, feed::POLL_HOURS_KEY, &format!("{h:?}"))?;
        }
        feed::follows_out(tx)
    })
    .await?;
    Ok(Json(out))
}

pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/follows/sweep", get(get_sweep).post(start_sweep).delete(stop_sweep))
        .route("/follows/settings", put(put_settings))
        .route("/follows", get(list_follows).post(create_follow))
        .route("/follows/{id}", patch(patch_follow).delete(delete_follow))
        .with_state(ctx)
}
