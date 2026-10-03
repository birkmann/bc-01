//! The HTTP routes of the maintenance services (legacy paths, no `/api` prefix, state applied):
//!
//! | method + path | what |
//! | --- | --- |
//! | `GET /cleanup/candidates` | junk albums (short tracks / sample-pack titles) |
//! | `GET\|POST /blacklist`, `DELETE /blacklist/{id}` | the never-download-again list |
//! | `POST /releases/adopt` | move releases / a fan's shelf into my library |
//! | `GET /releases/{id}/availability` | pre-order state: which tracks Bandcamp has out (cached 12 h) |
//! | `POST /releases/{id}/fill`, `POST /releases/fill` | re-download to fill missing tracks |
//! | `GET /releases/strays`, `POST\|GET\|DELETE /releases/strays/merge` | stray tracks and the merger |
//! | `POST /releases/delete`, `DELETE /tracks/{id}`, `DELETE /releases/{id}`, `DELETE /labels/{id}` | deletes (files confined to registered roots) |
//! | `POST /tracks/remove` | out of the library, files kept (and excluded from scans) |
//! | `POST /library/roots/{id}/move/plan`, `POST /library/roots/{id}/move` | relocate a root (the move is a tracked task, `202 Accepted`) |
//! | `GET\|PUT /library/scope`, `GET\|PUT /library/snippets` | library scope and snippet settings |
//!
//! The `/loved-streams*` routes (list, love, unlove, auto, download) belong to WS2 (orchestrator
//! ruling) and are **not** part of [`router`]; the handlers live on in [`loved_router`], which
//! WS2 may mount, and the logic is plain Rust API in [`crate::loved`].

use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, post};
use bc_libcore::{ApiError, ApiResult, Ctx};

use crate::lookup::BandcampLookup;
use crate::strays::{LoftyRetagger, Retagger, StrayMerger};

mod cleanup;
mod delete_routes;
mod fill;
mod loved;
mod move_root;
mod settings;
mod strays_routes;

/// Shared router state.
#[derive(Clone)]
pub struct MaintState {
    pub ctx: Ctx,
    pub lookup: Option<Arc<dyn BandcampLookup>>,
    pub merger: Option<Arc<StrayMerger>>,
}

impl MaintState {
    pub(crate) fn lookup(&self) -> ApiResult<&Arc<dyn BandcampLookup>> {
        self.lookup.as_ref().ok_or_else(|| ApiError::conflict("the Bandcamp client is not available"))
    }
}

/// Run blocking work (SQLite, file I/O) off the async workers.
pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> ApiResult<T> + Send + 'static) -> ApiResult<T> {
    tokio::task::spawn_blocking(f).await.map_err(ApiError::internal)?
}

/// Router with the default file retagger ([`LoftyRetagger`]). `lookup` is WS2's Bandcamp client; with
/// `None` the stray merge and the loved-streams download answer 409.
pub fn router(ctx: Ctx, lookup: Option<Arc<dyn BandcampLookup>>) -> Router {
    router_with(ctx, lookup, Arc::new(LoftyRetagger))
}

/// [`router`] with a caller-supplied retagger (the metadata crate's writer, or a test double).
pub fn router_with(ctx: Ctx, lookup: Option<Arc<dyn BandcampLookup>>, retag: Arc<dyn Retagger>) -> Router {
    let merger = lookup.as_ref().map(|l| Arc::new(StrayMerger::new(ctx.clone(), l.clone(), retag)));
    let state = MaintState { ctx, lookup, merger };
    Router::new()
        .route("/cleanup/candidates", get(cleanup::candidates))
        .route("/blacklist", get(cleanup::list_blacklist).post(cleanup::add_blacklist))
        .route("/blacklist/{id}", delete(cleanup::remove_blacklist))
        .route("/releases/adopt", post(fill::adopt))
        .route("/releases/fill", post(fill::fill_all))
        .route("/releases/{id}/fill", post(fill::fill_one))
        .route("/releases/{id}/availability", get(fill::availability))
        .route("/releases/strays", get(strays_routes::list))
        .route("/releases/strays/merge", post(strays_routes::merge).get(strays_routes::status).delete(strays_routes::stop))
        .route("/releases/delete", post(delete_routes::delete_releases))
        .route("/tracks/{id}", delete(delete_routes::delete_track))
        .route("/tracks/remove", post(delete_routes::remove_tracks))
        .route("/releases/{id}", delete(delete_routes::delete_release))
        .route("/labels/{id}", delete(delete_routes::delete_label))
        .route("/library/roots/{id}/move/plan", post(move_root::plan))
        .route("/library/roots/{id}/move", post(move_root::start))
        .route("/library/scope", get(settings::get_scope).put(settings::set_scope))
        .route("/library/snippets", get(settings::get_snippets).put(settings::set_snippets))
        .with_state(state)
}

/// The `/loved-streams*` handlers (not mounted by [`router`]; see the module docs).
pub fn loved_router(ctx: Ctx, lookup: Option<Arc<dyn BandcampLookup>>) -> Router {
    let state = MaintState { ctx, lookup, merger: None };
    Router::new()
        .route("/loved-streams", get(loved::list).post(loved::love).delete(loved::unlove))
        .route("/loved-streams/auto", get(loved::get_auto).put(loved::set_auto))
        .route("/loved-streams/download", post(loved::download))
        .with_state(state)
}

#[cfg(test)]
mod tests;
