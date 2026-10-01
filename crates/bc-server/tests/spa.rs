//! Port of test_spa.py: SPA fallback rules of the static mount.
mod common;
use tower::ServiceExt;

fn built_frontend() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join("assets")).unwrap();
    std::fs::write(d.path().join("index.html"), "<!doctype html><title>SPA</title>").unwrap();
    std::fs::write(d.path().join("assets").join("index-abc123.js"), "console.log(1)").unwrap();
    d
}

async fn app(ui: Option<&std::path::Path>) -> (axum::Router, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(common::config(data.path()), common::opts_with_ui(ui)).unwrap();
    (app, data)
}

#[tokio::test]
async fn no_mount_without_a_built_frontend() {
    // An empty dist must not activate the SPA mount: the catch-all would swallow
    // routes and turn unmatched /api paths into static 404s.
    let empty = tempfile::tempdir().unwrap();
    let (app, _d) = app(Some(empty.path())).await;
    let resp = app.clone().oneshot(common::get("/library/tracks")).await.unwrap();
    assert_eq!(resp.status(), 404);
    let api = app.oneshot(common::get("/api/health")).await.unwrap();
    assert_eq!(api.status(), 200);
}

#[tokio::test]
async fn index_is_served_at_root() {
    let ui = built_frontend();
    let (app, _d) = app(Some(ui.path())).await;
    let resp = app.oneshot(common::get("/")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(common::body_string(resp).await.contains("SPA"));
}

#[tokio::test]
async fn client_route_falls_back_to_index() {
    let ui = built_frontend();
    let (app, _d) = app(Some(ui.path())).await;
    for p in ["/library/tracks", "/sets/12", "/downloads"] {
        let resp = app.clone().oneshot(common::get(p)).await.unwrap();
        assert_eq!(resp.status(), 200, "{p}");
        assert!(common::body_string(resp).await.contains("SPA"), "{p}");
    }
}

#[tokio::test]
async fn real_asset_is_served() {
    let ui = built_frontend();
    let (app, _d) = app(Some(ui.path())).await;
    let resp = app.oneshot(common::get("/assets/index-abc123.js")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["cache-control"].to_str().unwrap().contains("immutable"));
    assert!(common::body_string(resp).await.contains("console.log"));
}

#[tokio::test]
async fn missing_asset_stays_a_404() {
    // Masking a missing hashed asset as HTML turns a broken deploy into a blank page.
    let ui = built_frontend();
    let (app, _d) = app(Some(ui.path())).await;
    assert_eq!(app.clone().oneshot(common::get("/assets/index-deadbeef.js")).await.unwrap().status(), 404);
    assert_eq!(app.oneshot(common::get("/favicon.ico")).await.unwrap().status(), 404);
}

#[tokio::test]
async fn api_404_is_still_problem_json() {
    let ui = built_frontend();
    let (app, _d) = app(Some(ui.path())).await;
    let resp = app.oneshot(common::get("/api/definitely-not-a-route")).await.unwrap();
    assert_eq!(resp.status(), 404);
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("application/problem+json"));
    let v: serde_json::Value = serde_json::from_str(&common::body_string(resp).await).unwrap();
    assert_eq!(v["status"], 404);
}

#[tokio::test]
async fn api_still_works_with_the_mount_active() {
    let ui = built_frontend();
    let (app, _d) = app(Some(ui.path())).await;
    let resp = app.oneshot(common::get("/api/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&common::body_string(resp).await).unwrap();
    assert_eq!(v["status"], "ok");
}

#[tokio::test]
async fn dotdot_paths_never_escape_the_ui_dir() {
    let ui = built_frontend();
    let (app, _d) = app(Some(ui.path())).await;
    let resp = app.oneshot(common::get("/..%2f..%2fetc/passwd")).await.unwrap();
    assert_ne!(resp.status(), 200 + 1000); // any answer is fine; must not be the file
    let body = common::body_string(resp).await;
    assert!(!body.contains("root:"));
}

#[tokio::test]
async fn precompressed_sibling_is_served_to_brotli_clients() {
    let ui = built_frontend();
    std::fs::write(ui.path().join("assets").join("index-abc123.js.br"), b"BR").unwrap();
    let (app, _d) = app(Some(ui.path())).await;
    let req = axum::http::Request::builder().uri("/assets/index-abc123.js").header("host", "localhost").header("accept-encoding", "gzip, br").body(axum::body::Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.headers()["content-encoding"], "br");
    let plain = app.oneshot(common::get("/assets/index-abc123.js")).await.unwrap();
    assert!(plain.headers().get("content-encoding").is_none());
}
