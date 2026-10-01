//! Port of test_health.py.
mod common;
use tower::ServiceExt;

#[tokio::test]
async fn health_returns_ok() {
    let dir = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(common::config(dir.path()), common::opts_with_ui(None)).unwrap();
    let resp = app.oneshot(common::get("/api/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&common::body_string(resp).await).unwrap();
    assert_eq!(v, serde_json::json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}));
}

#[tokio::test]
async fn doctor_reports_every_required_dependency() {
    let dir = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(common::config(dir.path()), common::opts_with_ui(None)).unwrap();
    let resp = app.oneshot(common::get("/api/doctor")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&common::body_string(resp).await).unwrap();
    let names: std::collections::HashSet<String> =
        v["checks"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect();
    for n in ["ffmpeg", "bandcamp-dl", "sqlite-fts5", "data-dir"] {
        assert!(names.contains(n), "missing check {n}");
    }
    assert!(["ok", "degraded", "unhealthy"].contains(&v["status"].as_str().unwrap()));
}

#[tokio::test]
async fn doctor_status_reflects_required_checks() {
    // A missing *optional* backend must not make the app look unhealthy.
    let dir = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(common::config(dir.path()), common::opts_with_ui(None)).unwrap();
    let resp = app.oneshot(common::get("/api/doctor")).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&common::body_string(resp).await).unwrap();
    let required_missing = v["checks"].as_array().unwrap().iter().any(|c| c["required"] == true && c["status"] != "ok");
    if required_missing {
        assert_eq!(v["status"], "unhealthy");
    } else {
        assert!(["ok", "degraded"].contains(&v["status"].as_str().unwrap()));
    }
}

#[tokio::test]
async fn cross_origin_isolation_headers_are_set() {
    let dir = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(common::config(dir.path()), common::opts_with_ui(None)).unwrap();
    let resp = app.oneshot(common::get("/api/health")).await.unwrap();
    assert_eq!(resp.headers()["cross-origin-opener-policy"], "same-origin");
    assert_eq!(resp.headers()["cross-origin-embedder-policy"], "credentialless");
}

#[tokio::test]
async fn foreign_host_header_is_rejected_on_a_loopback_server() {
    let dir = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(common::config(dir.path()), common::opts_with_ui(None)).unwrap();
    let req = axum::http::Request::builder().uri("/api/health").header("host", "evil.example.com").body(axum::body::Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn ui_state_roundtrips() {
    let dir = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(common::config(dir.path()), common::opts_with_ui(None)).unwrap();
    let put = axum::http::Request::builder()
        .method("PUT")
        .uri("/api/ui-state/themes")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"mode":"dark","themes":[]}"#))
        .unwrap();
    assert_eq!(app.clone().oneshot(put).await.unwrap().status(), 204);
    let resp = app.clone().oneshot(common::get("/api/ui-state/themes")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&common::body_string(resp).await).unwrap();
    assert_eq!(v["mode"], "dark");
    let missing = app.oneshot(common::get("/api/ui-state/nope")).await.unwrap();
    assert_eq!(missing.status(), 200);
    assert_eq!(common::body_string(missing).await, "null");
}
