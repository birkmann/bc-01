//! RFC 9457 problem+json responses shared by the WS2 routers.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bc_types::Problem;

#[derive(Debug)]
pub struct ApiError {
    pub problem: Problem,
    pub retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(status: u16, title: impl Into<String>) -> Self {
        Self { problem: Problem::new(status, title), retry_after: None }
    }
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.problem.detail = Some(d.into());
        self
    }
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(400, "Bad Request").detail(msg)
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(404, "Not Found").detail(msg)
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::new(409, "Conflict").detail(msg)
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(401, "Unauthorized").detail(msg)
    }
    pub fn busy(msg: impl Into<String>, retry_after: u64) -> Self {
        let mut e = Self::new(503, "Service Unavailable").detail(msg);
        e.retry_after = Some(retry_after);
        e
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(500, "Internal Server Error").detail(msg)
    }
}

impl From<bc_db::DbError> for ApiError {
    fn from(e: bc_db::DbError) -> Self {
        match e {
            bc_db::DbError::NotFound => Self::not_found("not found"),
            bc_db::DbError::Conflict(m) => Self::conflict(m),
            other => {
                tracing::error!("db error: {other}");
                Self::internal(other.to_string())
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let mut resp = (status, Json(&self.problem)).into_response();
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/problem+json"));
        if let Some(s) = self.retry_after {
            if let Ok(v) = HeaderValue::from_str(&s.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        resp
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
