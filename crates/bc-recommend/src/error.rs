//! Errors of the recommender crate and their problem+json mapping.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bc_types::Problem;

#[derive(Debug, thiserror::Error)]
pub enum RecommendError {
    /// 404, like the legacy `NotFound`.
    #[error("{0}")]
    NotFound(String),
    /// 400, like the legacy `BadRequest`.
    #[error("{0}")]
    BadRequest(String),
    #[error(transparent)]
    Db(#[from] bc_db::DbError),
    /// An error of WS1's shared layer (scope, hydration, set detail).
    #[error(transparent)]
    Api(#[from] bc_libcore::error::ApiError),
}

impl From<bc_db::rusqlite::Error> for RecommendError {
    fn from(e: bc_db::rusqlite::Error) -> Self {
        RecommendError::Db(e.into())
    }
}

pub type Result<T, E = RecommendError> = std::result::Result<T, E>;

impl RecommendError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }

    pub fn problem(&self) -> Problem {
        match self {
            Self::NotFound(m) => Problem::new(404, "Not Found").detail(m.clone()),
            Self::BadRequest(m) => Problem::new(400, "Bad Request").detail(m.clone()),
            Self::Api(e) => e.problem(),
            Self::Db(bc_db::DbError::NotFound) => Problem::new(404, "Not Found").detail("not found"),
            Self::Db(bc_db::DbError::Conflict(m)) => Problem::new(409, "Conflict").detail(m.clone()),
            Self::Db(e) => {
                tracing::error!("recommend db error: {e}");
                Problem::new(500, "Internal Server Error").detail(e.to_string())
            }
        }
    }
}

impl IntoResponse for RecommendError {
    fn into_response(self) -> Response {
        if let Self::Api(e) = self {
            return e.into_response();
        }
        let problem = self.problem();
        let status = StatusCode::from_u16(problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut resp = (status, Json(&problem)).into_response();
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/problem+json"));
        resp
    }
}

/// Run a blocking read whose body can fail with a [`RecommendError`] (404/400 included).
pub fn read<T>(db: &bc_db::Db, f: impl FnOnce(&bc_db::rusqlite::Connection) -> Result<T>) -> Result<T> {
    db.read(|c| Ok(f(c)))?
}

/// Async flavour of [`read`], off the tokio workers.
pub async fn read_async<T: Send + 'static>(
    db: &bc_db::Db,
    f: impl FnOnce(&bc_db::rusqlite::Connection) -> Result<T> + Send + 'static,
) -> Result<T> {
    db.read_async(|c| Ok(f(c))).await?
}

/// Write counterpart of [`read`].
pub fn write<T: Send + 'static>(
    db: &bc_db::Db,
    f: impl FnOnce(&bc_db::rusqlite::Transaction<'_>) -> Result<T> + Send + 'static,
) -> Result<T> {
    // An `Err` from the body must roll back, so it is smuggled out as a DbError and recovered.
    let slot: std::sync::Arc<std::sync::Mutex<Option<RecommendError>>> = Default::default();
    let slot2 = slot.clone();
    let res = db.write(move |t| {
        f(t).map_err(|e| match e {
            RecommendError::Db(d) => d,
            other => {
                if let Ok(mut s) = slot2.lock() {
                    *s = Some(other);
                }
                bc_db::DbError::Other("aborted".into())
            }
        })
    });
    match res {
        Ok(v) => Ok(v),
        Err(e) => {
            if let Some(own) = slot.lock().ok().and_then(|mut s| s.take()) {
                Err(own)
            } else {
                Err(e.into())
            }
        }
    }
}

/// Async flavour of [`write`].
pub async fn write_async<T: Send + 'static>(
    db: &bc_db::Db,
    f: impl FnOnce(&bc_db::rusqlite::Transaction<'_>) -> Result<T> + Send + 'static,
) -> Result<T> {
    let db = db.clone();
    tokio::task::spawn_blocking(move || write(&db, f))
        .await
        .map_err(|e| RecommendError::Db(bc_db::DbError::Other(e.to_string())))?
}
