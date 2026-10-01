//! `RecommendService`: the axum router for the recommender routes (docs/api/ws3.md).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_types::sets::{AutomixRequest, DjSetDetail, PoolPage, PoolQuery, SetSuggestRequest, SetSuggestResponse};
use bc_types::suggest::{
    LovedQuery, LovedSuggestResponse, SimilarPlaylistOut, SimilarPlaylistRequest, SimilarRequest, SimilarResponse, SuggestRequest,
    SuggestResponse };

use crate::cue::{CueSource, HeuristicCues};
use bc_types::library::TrackOut;
use crate::error::{RecommendError, read_async, write_async};
use crate::nextup::{self, RunOpts};
use crate::scope::ScopeParams;
use crate::{playlists, sets, similar, taste};

type ApiResult<T> = Result<Json<T>, RecommendError>;

#[derive(Clone)]
struct AppState {
    db: Db,
    bus: Arc<EventBus>,
    cues: Arc<dyn CueSource>,
}

/// Recommenders and DJ-set planning. Construct once, nest `router()` under `/api`.
pub struct RecommendService {
    db: Db,
    bus: Arc<EventBus>,
    #[allow(dead_code)]
    config: Arc<Config>,
    cues: Arc<dyn CueSource>,
}

impl RecommendService {
    pub fn new(db: Db, bus: Arc<EventBus>, config: Arc<Config>) -> Self {
        Self { db, bus, config, cues: Arc::new(HeuristicCues) }
    }

    /// Plug in a waveform-aware cue planner (default: the duration/tempo heuristic).
    pub fn with_cue_source(mut self, cues: Arc<dyn CueSource>) -> Self {
        self.cues = cues;
        self
    }

    /// No background workers; present for the shared service shape.
    pub async fn start(&self) {}

    pub fn router(&self) -> Router {
        let state = AppState { db: self.db.clone(), bus: self.bus.clone(), cues: self.cues.clone() };
        Router::new()
            .route("/suggest/next", post(suggest_next))
            .route("/suggest/loved", get(suggest_loved))
            .route("/suggest/similar", post(suggest_similar))
            .route("/sets/{id}/pool", get(set_pool))
            .route("/sets/{id}/suggest", post(set_suggest))
            .route("/sets/{id}/automix", post(set_automix))
            .route("/playlists/{id}/similar", post(playlist_similar))
            .with_state(state)
    }
}

async fn suggest_next(State(s): State<AppState>, Query(sp): Query<ScopeParams>, Json(req): Json<SuggestRequest>) -> ApiResult<SuggestResponse<TrackOut>> {
    read_async(&s.db, move |c| {
        let scope = crate::scope::resolve(c, &sp)?;
        nextup::run(c, &scope, &req, RunOpts::default())
    })
    .await
    .map(Json)
}

async fn suggest_loved(State(s): State<AppState>, Query(sp): Query<ScopeParams>, Query(q): Query<LovedQuery>) -> ApiResult<LovedSuggestResponse<TrackOut>> {
    read_async(&s.db, move |c| {
        let scope = crate::scope::resolve(c, &sp)?;
        taste::run(c, &scope, &q)
    })
    .await
    .map(Json)
}

async fn suggest_similar(State(s): State<AppState>, Query(sp): Query<ScopeParams>, Json(req): Json<SimilarRequest>) -> ApiResult<SimilarResponse<TrackOut>> {
    read_async(&s.db, move |c| {
        let scope = crate::scope::resolve(c, &sp)?;
        similar::run(c, &scope, &req)
    })
    .await
    .map(Json)
}

async fn set_pool(State(s): State<AppState>, Path(id): Path<i64>, Query(sp): Query<ScopeParams>, Query(q): Query<PoolQuery>) -> ApiResult<PoolPage<TrackOut>> {
    read_async(&s.db, move |c| {
        let scope = crate::scope::resolve(c, &sp)?;
        sets::pool_page(c, &scope, id, &q)
    })
    .await
    .map(Json)
}

async fn set_suggest(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Query(sp): Query<ScopeParams>,
    Json(req): Json<SetSuggestRequest>,
) -> ApiResult<SetSuggestResponse<TrackOut>> {
    read_async(&s.db, move |c| {
        let scope = crate::scope::resolve(c, &sp)?;
        sets::suggest(c, &scope, id, &req)
    })
    .await
    .map(Json)
}

async fn set_automix(State(s): State<AppState>, Path(id): Path<i64>, Query(sp): Query<ScopeParams>, Json(req): Json<AutomixRequest>) -> ApiResult<DjSetDetail> {
    let cues = s.cues.clone();
    let detail = write_async(&s.db, move |t| {
        let scope = crate::scope::resolve(t, &sp)?;
        sets::automix_in(t, &scope, cues.as_ref(), id, &req)
    })
    .await?;
    s.bus.invalidate("set", vec![id]);
    Ok(Json(detail))
}

async fn playlist_similar(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Query(sp): Query<ScopeParams>,
    Json(req): Json<SimilarPlaylistRequest>,
) -> ApiResult<SimilarPlaylistOut> {
    let db = s.db.clone();
    let out = tokio::task::spawn_blocking(move || {
        let scope = crate::error::read(&db, |c| crate::scope::resolve(c, &sp))?;
        playlists::similar_playlist(&db, &scope, id, &req)
    })
    .await
    .map_err(|e| RecommendError::Db(bc_db::DbError::Other(e.to_string())))??;
    s.bus.invalidate("playlist", vec![]);
    Ok(Json(out))
}
