//! `ApiError` -> RFC 9457 `application/problem+json` (`bc_types::Problem`).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bc_db::DbError;
use bc_types::Problem;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unauthorized(String),
    /// 400 `urn:bcapp:unsafe-path`: a path escapes its permitted root.
    #[error("{0}")]
    Unsafe(String),
    /// 422: the request is well-formed JSON/query but a value is not acceptable.
    #[error("{0}")]
    Unprocessable(String),
    /// Any of the above plus extra problem-document members (`offending_url`, ...).
    #[error("{0}")]
    WithExtra(Box<ApiError>, serde_json::Map<String, serde_json::Value>),
    /// 503 with `Retry-After` (seconds).
    #[error("{0}")]
    Busy(String, u64),
    #[error("{0}")]
    Internal(String),
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::NotFound(m.into())
    }
    pub fn bad(m: impl Into<String>) -> Self {
        Self::BadRequest(m.into())
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::Conflict(m.into())
    }
    pub fn internal(m: impl ToString) -> Self {
        Self::Internal(m.to_string())
    }
    pub fn unprocessable(m: impl Into<String>) -> Self {
        Self::Unprocessable(m.into())
    }
    /// Attach an extra member to the problem document.
    pub fn with(self, key: &str, value: impl serde::Serialize) -> Self {
        let v = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
        match self {
            Self::WithExtra(inner, mut m) => {
                m.insert(key.into(), v);
                Self::WithExtra(inner, m)
            }
            other => {
                let mut m = serde_json::Map::new();
                m.insert(key.into(), v);
                Self::WithExtra(Box::new(other), m)
            }
        }
    }
    /// `urn:bcapp:*` problem type.
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "urn:bcapp:not-found",
            Self::BadRequest(_) => "urn:bcapp:bad-request",
            Self::Conflict(_) => "urn:bcapp:conflict",
            Self::Unauthorized(_) => "urn:bcapp:unauthorized",
            Self::Unsafe(_) => "urn:bcapp:unsafe-path",
            Self::Unprocessable(_) => "urn:bcapp:validation",
            Self::Busy(..) => "urn:bcapp:library-busy",
            Self::Internal(_) => "urn:bcapp:internal",
            Self::WithExtra(inner, _) => inner.error_type(),
        }
    }
    pub fn status(&self) -> StatusCode {
        match self {
            Self::WithExtra(inner, _) => inner.status(),
            Self::Unsafe(_) => StatusCode::BAD_REQUEST,
            Self::Unprocessable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Busy(..) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
    pub fn problem(&self) -> Problem {
        let status = self.status();
        let title = match self {
            Self::Unsafe(_) => "Unsafe Path",
            Self::Busy(..) => "Library Busy",
            _ => status.canonical_reason().unwrap_or("Error"),
        };
        let mut p = Problem::new(status.as_u16(), title).detail(self.to_string());
        p.kind = self.error_type().to_string();
        p
    }

    /// The JSON body: the problem document plus any extra members.
    pub fn body(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self.problem()).unwrap_or(serde_json::Value::Null);
        if let (Self::WithExtra(_, extra), Some(obj)) = (self, v.as_object_mut()) {
            for (k, val) in extra {
                obj.insert(k.clone(), val.clone());
            }
        }
        v
    }

    fn retry_after(&self) -> Option<u64> {
        match self {
            Self::Busy(_, s) => Some(*s),
            Self::WithExtra(inner, _) => inner.retry_after(),
            _ => None,
        }
    }
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::NotFound => Self::NotFound("not found".into()),
            DbError::Conflict(m) => Self::Conflict(m),
            DbError::Sqlite(rusqlite_err) => {
                use bc_db::rusqlite::{Error, ErrorCode};
                if let Error::SqliteFailure(f, _) = &rusqlite_err {
                    if matches!(f.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) {
                        return Self::Busy("the library database is busy, try again shortly".into(), 5);
                    }
                    if f.code == ErrorCode::ConstraintViolation {
                        return Self::Conflict(rusqlite_err.to_string());
                    }
                }
                tracing::error!(error = %rusqlite_err, "database error");
                Self::Internal(rusqlite_err.to_string())
            }
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<bc_db::rusqlite::Error> for ApiError {
    fn from(e: bc_db::rusqlite::Error) -> Self {
        DbError::Sqlite(e).into()
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound {
            Self::NotFound(e.to_string())
        } else {
            Self::Internal(e.to_string())
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = serde_json::to_vec(&self.body()).unwrap_or_default();
        let mut resp = (status, body).into_response();
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/problem+json"));
        if let Some(secs) = self.retry_after()
            && let Ok(v) = HeaderValue::from_str(&secs.to_string())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        resp
    }
}

/// `Router::fallback` handler: unmatched routes answer problem+json too, not a bare 404.
pub async fn fallback_404() -> ApiError {
    ApiError::NotFound("no such route".into())
}

/// Method-not-allowed as problem+json (405).
pub fn method_not_allowed() -> Response {
    let p = Problem::new(405, "Method Not Allowed").detail("method not allowed for this route");
    let mut resp = (StatusCode::METHOD_NOT_ALLOWED, serde_json::to_vec(&p).unwrap_or_default()).into_response();
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/problem+json"));
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::routing::{delete, get};
    use axum::Router;
    use tower::ServiceExt;

    async fn json_of(resp: Response) -> serde_json::Value {
        serde_json::from_slice(&to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap()
    }

    #[test]
    fn statuses_and_retry_after() {
        assert_eq!(ApiError::not_found("x").status(), 404);
        assert_eq!(ApiError::bad("x").status(), 400);
        assert_eq!(ApiError::conflict("x").status(), 409);
        assert_eq!(ApiError::unprocessable("x").status(), 422);
        let r = ApiError::Busy("busy".into(), 5).into_response();
        assert_eq!(r.status(), 503);
        assert_eq!(r.headers()[header::RETRY_AFTER], "5");
        assert_eq!(r.headers()[header::CONTENT_TYPE], "application/problem+json");
    }

    #[test]
    fn db_not_found_maps() {
        let e: ApiError = DbError::NotFound.into();
        assert_eq!(e.status(), 404);
    }

    // ---- ports of tests/unit/test_errors.py ------------------------------------------------

    #[tokio::test]
    async fn unmatched_route_returns_problem_json() {
        let app = Router::new().route("/api/health", get(|| async { "ok" })).fallback(fallback_404);
        let resp = app.oneshot(axum::http::Request::get("/api/definitely-not-a-route").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), 404);
        assert!(resp.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("application/problem+json"));
        let body = json_of(resp).await;
        for k in ["type", "title", "status", "detail"] {
            assert!(body.get(k).is_some(), "missing {k}");
        }
        assert_eq!(body["status"], 404);
    }

    #[test]
    fn method_not_allowed_returns_problem_json() {
        let resp = method_not_allowed();
        assert_eq!(resp.status(), 405);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "application/problem+json");
    }

    #[tokio::test]
    async fn app_error_maps_to_its_status_and_type() {
        let app = Router::new().route("/api/_boom", get(|| async { Err::<(), _>(ApiError::Unsafe("path escapes its permitted root: ../etc".into())) }));
        let resp = app.oneshot(axum::http::Request::get("/api/_boom").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), 400);
        assert!(resp.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("application/problem+json"));
        let body = json_of(resp).await;
        assert_eq!(body["type"], "urn:bcapp:unsafe-path");
        assert_eq!(body["title"], "Unsafe Path");
        assert!(body["detail"].as_str().unwrap().contains("escapes"));
    }

    #[tokio::test]
    async fn validation_error_returns_422_problem_json() {
        #[derive(serde::Deserialize)]
        struct P {
            #[allow(dead_code)]
            n: i64,
        }
        async fn needs_int(crate::extract::Q(_p): crate::extract::Q<P>) -> &'static str {
            "ok"
        }
        let app = Router::new().route("/api/_needs_int", get(needs_int));
        let resp = app.oneshot(axum::http::Request::get("/api/_needs_int?n=not-a-number").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), 422);
        assert!(resp.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("application/problem+json"));
        let body = json_of(resp).await;
        for k in ["type", "title", "status", "detail"] {
            assert!(body.get(k).is_some());
        }
    }

    #[tokio::test]
    async fn extra_fields_survive_into_the_problem_document() {
        let e = ApiError::bad("bad url").with("offending_url", "not://a/url");
        assert_eq!(e.status(), 400);
        let body = json_of(e.into_response()).await;
        assert_eq!(body["offending_url"], "not://a/url");
        assert_eq!(body["status"], 400);
    }

    #[test]
    fn error_subclasses_declare_distinct_types() {
        let types: std::collections::HashSet<_> = [ApiError::not_found(""), ApiError::bad(""), ApiError::Unsafe(String::new())].iter().map(|e| e.error_type()).collect();
        assert_eq!(types.len(), 3);
        assert!(types.iter().all(|t| t.starts_with("urn:bcapp:")));
    }

    #[test]
    fn base_error_defaults_to_500() {
        assert_eq!(ApiError::internal("boom").status(), 500);
        assert_eq!(ApiError::internal("boom").to_string(), "boom");
    }

    /// `BEGIN IMMEDIATE` giving up on the write lock is contention, not a bug: a retryable 503.
    #[tokio::test]
    async fn write_lock_timeout_is_a_retryable_503() {
        use bc_db::rusqlite::{Error, ffi};
        fn busy() -> ApiError {
            ApiError::from(DbError::Sqlite(Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_BUSY), Some("database is locked".into()))))
        }
        fn broken() -> ApiError {
            ApiError::from(DbError::Sqlite(Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_ERROR), Some("no such table: nope".into()))))
        }
        let app = Router::new().route("/api/_locked", delete(|| async { Err::<(), _>(busy()) })).route("/api/_broken", get(|| async { Err::<(), _>(broken()) }));
        let r = app.clone().oneshot(axum::http::Request::delete("/api/_locked").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), 503);
        assert!(r.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("application/problem+json"));
        assert_eq!(r.headers()[header::RETRY_AFTER], "5");
        let body = json_of(r).await;
        assert_eq!(body["type"], "urn:bcapp:library-busy");
        assert!(body["detail"].as_str().unwrap().to_lowercase().contains("busy"));
        // every other database fault is still a real 500, not quietly softened
        let r = app.oneshot(axum::http::Request::get("/api/_broken").body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(r.status(), 500);
    }
}

