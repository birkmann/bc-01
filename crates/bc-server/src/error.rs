//! problem+json (RFC 9457) errors.
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bc_types::Problem;

#[derive(Debug)]
pub struct ApiError {
    pub problem: Problem,
    pub retry_after: Option<u32>,
}

impl ApiError {
    pub fn new(status: u16, title: impl Into<String>) -> Self {
        Self { problem: Problem::new(status, title), retry_after: None }
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.problem.detail = Some(d.into());
        self
    }
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::new(404, "Not Found").detail(what)
    }
    pub fn bad_request(what: impl Into<String>) -> Self {
        Self::new(400, "Bad Request").detail(what)
    }
    pub fn unauthorized(what: impl Into<String>) -> Self {
        Self::new(401, "Unauthorized").detail(what)
    }
    pub fn forbidden(what: impl Into<String>) -> Self {
        Self::new(403, "Forbidden").detail(what)
    }
    pub fn busy(retry_after: u32) -> Self {
        Self { problem: Problem::new(503, "Service Unavailable"), retry_after: Some(retry_after) }
    }
    pub fn internal(what: impl std::fmt::Display) -> Self {
        tracing::error!("internal error: {what}");
        Self::new(500, "Internal Server Error").detail(what.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = serde_json::to_vec(&self.problem).unwrap_or_default();
        let mut resp = (status, body).into_response();
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/problem+json"));
        if let Some(s) = self.retry_after
            && let Ok(v) = HeaderValue::from_str(&s.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        resp
    }
}

impl From<bc_db::DbError> for ApiError {
    fn from(e: bc_db::DbError) -> Self {
        match e {
            bc_db::DbError::NotFound => Self::not_found("not found"),
            bc_db::DbError::Conflict(m) => Self::new(409, "Conflict").detail(m),
            other => Self::internal(other),
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
