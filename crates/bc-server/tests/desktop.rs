//! `/api/desktop`: the Bandcamp sign-in window is offered only inside the desktop app, and asking
//! for it goes to the desktop app over the event bus.
mod common;
use tower::ServiceExt;

fn post(path: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder().method("POST").uri(path).header("host", "localhost").body(axum::body::Body::empty()).unwrap()
}

#[tokio::test]
async fn plain_server_has_no_sign_in_window() {
    let dir = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(common::config(dir.path()), common::opts_with_ui(None)).unwrap();
    let resp = app.clone().oneshot(common::get("/api/desktop")).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&common::body_string(resp).await).unwrap();
    assert_eq!(v, serde_json::json!({ "bandcamp_login": false }));
    assert_eq!(app.oneshot(post("/api/desktop/bandcamp-login")).await.unwrap().status(), 404);
}

#[tokio::test]
async fn desktop_app_gets_the_sign_in_request() {
    let dir = tempfile::tempdir().unwrap();
    let opts = bc_server::ServerOptions { bandcamp_login: true, ..common::opts_with_ui(None) };
    let state = bc_server::build_state(common::config(dir.path()), opts).unwrap();
    let mut rx = state.bus.subscribe();
    let app = bc_server::build_router(state);
    let resp = app.clone().oneshot(common::get("/api/desktop")).await.unwrap();
    let v: serde_json::Value = serde_json::from_str(&common::body_string(resp).await).unwrap();
    assert_eq!(v["bandcamp_login"], true);
    assert_eq!(app.oneshot(post("/api/desktop/bandcamp-login")).await.unwrap().status(), 202);
    assert_eq!(rx.recv().await.unwrap().topic, bc_types::bandcamp::TOPIC_BANDCAMP_LOGIN_REQUEST);
}
