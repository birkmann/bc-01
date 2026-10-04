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

fn switch(on: bool, ip: &str, host: &str, origin: Option<&str>) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder().method("POST").uri("/api/auth/lan").header("host", host).header("content-type", "application/json");
    if let Some(o) = origin {
        b = b.header("origin", o);
    }
    from_peer(b.body(axum::body::Body::from(format!(r#"{{"on":{on}}}"#))).unwrap(), ip)
}

#[tokio::test]
async fn host_machine_switches_lan_mode_and_it_is_saved() {
    let d = tempfile::tempdir().unwrap();
    let config = common::config(d.path());
    let db_path = config.db_path();
    let app = bc_server::build_app_with(config, common::opts_with_ui(None)).unwrap();
    // LAN off: a remote peer is refused outright, and cannot switch it on.
    assert_eq!(app.clone().oneshot(from_peer(common::get("/api/doctor"), "192.168.1.20")).await.unwrap().status(), 403);
    assert_eq!(app.clone().oneshot(switch(true, "192.168.1.20", "localhost", None)).await.unwrap().status(), 403);
    // Neither can another web page in the host's browser.
    assert_eq!(app.clone().oneshot(switch(true, "127.0.0.1", "127.0.0.1:8420", Some("http://evil.com"))).await.unwrap().status(), 403);

    let resp = app.clone().oneshot(switch(true, "127.0.0.1", "127.0.0.1:8420", Some("http://127.0.0.1:8420"))).await.unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&common::body_string(resp).await).unwrap();
    assert_eq!(v["lan"], true);
    assert!(bc_server::net::saved(&db_path), "the choice survives a restart");
    // Now remote peers get the token check instead.
    assert_eq!(app.clone().oneshot(from_peer(common::get("/api/doctor"), "192.168.1.20")).await.unwrap().status(), 401);

    assert_eq!(app.clone().oneshot(switch(false, "127.0.0.1", "localhost", None)).await.unwrap().status(), 200);
    assert!(!bc_server::net::saved(&db_path));
    assert_eq!(app.oneshot(from_peer(common::get("/api/doctor"), "192.168.1.20")).await.unwrap().status(), 403);
}

#[tokio::test]
async fn rebinding_page_on_loopback_is_not_the_host_machine() {
    let d = tempfile::tempdir().unwrap();
    let app = bc_server::build_app_with(lan_config(d.path()), common::opts_with_ui(None)).unwrap();
    let req = axum::http::Request::builder().uri("/api/doctor").header("host", "evil.com:8420").body(axum::body::Body::empty()).unwrap();
    assert_eq!(app.clone().oneshot(from_peer(req, "127.0.0.1")).await.unwrap().status(), 401);
    let req = axum::http::Request::builder().method("POST").uri("/api/auth/pairing").header("host", "evil.com:8420").body(axum::body::Body::empty()).unwrap();
    assert_eq!(app.oneshot(from_peer(req, "127.0.0.1")).await.unwrap().status(), 401);
}

async fn health(port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(mut s) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await else { return false };
    let _ = s.write_all(b"GET /api/health HTTP/1.0\r\nHost: localhost\r\n\r\n").await;
    let mut out = String::new();
    let _ = s.read_to_string(&mut out).await;
    out.starts_with("HTTP/1.0 200") || out.starts_with("HTTP/1.1 200")
}

#[tokio::test]
async fn switching_rebinds_on_the_same_port() {
    let d = tempfile::tempdir().unwrap();
    let srv = bc_server::start(common::config(d.path()), common::opts_with_ui(None)).await.unwrap();
    let port = srv.addr.port();
    assert!(srv.addr.ip().is_loopback());
    assert!(health(port).await);

    srv.state.net.set_lan(true).await.unwrap();
    assert!(srv.state.net.lan());
    assert!(health(port).await, "still reachable on loopback after going LAN");
    assert_eq!(srv.url(), format!("http://127.0.0.1:{port}"));

    srv.state.net.set_lan(false).await.unwrap();
    assert!(!srv.state.net.lan());
    assert!(health(port).await);
    srv.stop().await;
}
