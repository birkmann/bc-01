use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bc_types::Problem;

/// problem+json error (RFC 9457).
#[derive(Debug)]
pub struct ApiError(pub Problem);

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: u16, title: &str, detail: impl Into<String>) -> Self {
        Self(Problem::new(status, title).detail(detail))
    }
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(404, "Not Found", detail)
    }
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(400, "Bad Request", detail)
    }
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(500, "Internal Server Error", detail)
    }
}

impl From<bc_db::DbError> for ApiError {
    fn from(e: bc_db::DbError) -> Self {
        match e {
            bc_db::DbError::NotFound => ApiError::not_found("not found"),
            bc_db::DbError::Conflict(m) => ApiError::new(409, "Conflict", m),
            other => ApiError::internal(other.to_string()),
        }
    }
}

impl From<bc_db::rusqlite::Error> for ApiError {
    fn from(e: bc_db::rusqlite::Error) -> Self {
        ApiError::internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_vec(&self.0).unwrap_or_default();
        let mut resp = (status, body).into_response();
        resp.headers_mut().insert(header::CONTENT_TYPE, "application/problem+json".parse().expect("static header"));
        let _ = Json(()); // keep axum::Json in scope for downstream modules
        resp
    }
}
