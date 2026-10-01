//! LAN pairing: tokens, host check, non-loopback peers.
mod common;
use std::net::SocketAddr;

use axum::extract::ConnectInfo;
use tower::ServiceExt;

fn lan_config(dir: &std::path::Path) -> bc_core::Config {
    let mut c = common::config(dir);
    c.lan = true;
    c
}

fn from_peer(mut req: axum::http::Request<axum::body::Body>, ip: &str) -> axum::http::Request<axum::body::Body> {
    let addr: SocketAddr = format!("{ip}:50000").parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

#[tokio::test]
async fn remote_peer_needs_a_token_but_health_is_open() {
    let d = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(lan_config(d.path()), common::opts_with_ui(None)).unwrap();
    let health = app.clone().oneshot(from_peer(common::get("/api/health"), "192.168.1.20")).await.unwrap();
    assert_eq!(health.status(), 200);
    let doctor = app.oneshot(from_peer(common::get("/api/doctor"), "192.168.1.20")).await.unwrap();
    assert_eq!(doctor.status(), 401);
}

#[tokio::test]
async fn pairing_flow_issues_a_device_token() {
    let d = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(lan_config(d.path()), common::opts_with_ui(None)).unwrap();
    // Only the host machine may mint a pairing code.
    let denied = axum::http::Request::builder().method("POST").uri("/api/auth/pairing").header("host", "x").body(axum::body::Body::empty()).unwrap();
    assert_eq!(app.clone().oneshot(from_peer(denied, "192.168.1.20")).await.unwrap().status(), 401);
    let mint = axum::http::Request::builder().method("POST").uri("/api/auth/pairing").header("host", "localhost").body(axum::body::Body::empty()).unwrap();
    let resp = app.clone().oneshot(from_peer(mint, "127.0.0.1")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&common::body_string(resp).await).unwrap();
    let code = v["code"].as_str().unwrap().to_string();

    let bad = axum::http::Request::builder().method("POST").uri("/api/auth/pair").header("host", "h").header("content-type", "application/json")
        .body(axum::body::Body::from(r#"{"code":"ZZZZ-ZZZZ","name":"x"}"#)).unwrap();
    assert_eq!(app.clone().oneshot(from_peer(bad, "192.168.1.20")).await.unwrap().status(), 401);

    let pair = axum::http::Request::builder().method("POST").uri("/api/auth/pair").header("host", "h").header("content-type", "application/json")
        .body(axum::body::Body::from(format!(r#"{{"code":"{code}","name":"phone"}}"#))).unwrap();
    let resp = app.clone().oneshot(from_peer(pair, "192.168.1.20")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["set-cookie"].to_str().unwrap().contains("bc_token="));
    let v: serde_json::Value = serde_json::from_str(&common::body_string(resp).await).unwrap();
    let token = v["token"].as_str().unwrap().to_string();

    // The code is single use.
    let again = axum::http::Request::builder().method("POST").uri("/api/auth/pair").header("host", "h").header("content-type", "application/json")
        .body(axum::body::Body::from(format!(r#"{{"code":"{code}","name":"phone"}}"#))).unwrap();
    assert_eq!(app.clone().oneshot(from_peer(again, "192.168.1.21")).await.unwrap().status(), 401);

    // The token opens the API.
    let authed = axum::http::Request::builder().uri("/api/doctor").header("host", "h").header("authorization", format!("Bearer {token}")).body(axum::body::Body::empty()).unwrap();
    assert_eq!(app.oneshot(from_peer(authed, "192.168.1.20")).await.unwrap().status(), 200);
}

#[test]
fn host_matching() {
    assert!(bc_server::auth::host_is_local("localhost:8420"));
    assert!(bc_server::auth::host_is_local("127.0.0.1"));
    assert!(bc_server::auth::host_is_local("[::1]:80"));
    assert!(!bc_server::auth::host_is_local("evil.com:8420"));
    assert!(!bc_server::auth::host_is_local("127.0.0.1.evil.com"));
}
