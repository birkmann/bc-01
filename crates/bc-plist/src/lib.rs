//! Workstream 1: playlists, DJ-set storage CRUD and track exports.
//!
//! * [`playlists`]: `/playlists...` (manual and smart playlists, save-a-view, export).
//! * [`sets`]: `/sets...` storage CRUD (items with frozen snapshots, move, detail with
//!   `bc_music::setmath` summary and transitions, export). The pool/suggest/automix routes of a
//!   set and `/playlists/{id}/similar` belong to the recommender crate.
//! * [`export`]: m3u8 / csv / streaming zip, used by those routes and by `GET /tracks/export`.
//! * [`lane`]: fractional-position bookkeeping shared by playlist items and set items
//!   (a thin SQL layer over `bc_music::ordering`).
//!
//! The library's track filter engine lives in `bc-library`, which depends on this crate, so
//! the routes that need it (`/playlists/from-tracks`, smart playlists) take a [`TrackResolver`].

pub mod export;
pub mod lane;
pub mod playlists;
pub mod sets;

use std::sync::Arc;

use axum::Router;
use bc_db::rusqlite::Connection;
use bc_libcore::{ApiResult, Ctx, Scope};
use bc_types::library::TrackQuery;

/// Evaluates a track filter (the `/tracks` filters) to track ids. Implemented by `bc-library`.
///
/// Contract: honour every filter of `q` EXCEPT `offset`/`limit` (ignored: `limit` below is
/// authoritative, `None` = unlimited), apply `scope`, and return ids in the order the query's
/// `sort`/`order` ask for (default: newest added first, then id). `missing` follows
/// `TrackQuery::missing` (default: hide tracks whose files are all gone).
pub trait TrackResolver: Send + Sync {
    fn resolve_ids(&self, c: &Connection, q: &TrackQuery, scope: &Scope, limit: Option<i64>) -> ApiResult<Vec<i64>>;
}

/// Shared router state.
#[derive(Clone)]
pub struct PlistState {
    pub ctx: Ctx,
    pub resolver: Arc<dyn TrackResolver>,
}

/// `/playlists...` and `/sets...` (paths WITHOUT the `/api` prefix, state applied).
pub fn router(ctx: Ctx, resolver: Arc<dyn TrackResolver>) -> Router {
    let st = PlistState { ctx, resolver };
    Router::new().merge(playlists::routes()).merge(sets::routes()).with_state(st)
}

/// JSON body extractor whose rejections are problem+json 400s like every other error.
pub struct J<T>(pub T);

impl<S: Send + Sync, T: serde::de::DeserializeOwned> axum::extract::FromRequest<S> for J<T> {
    type Rejection = bc_libcore::ApiError;
    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        axum::Json::<T>::from_request(req, state)
            .await
            .map(|j| J(j.0))
            .map_err(|e| bc_libcore::ApiError::bad(e.body_text()))
    }
}

#[cfg(test)]
mod tests;
