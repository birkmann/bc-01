//! Route tests over an in-process router (tower `oneshot`) and the watcher.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::testutil::*;

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn roots_and_scan_flow() {
    let e = env();
    let m = e.music();
    for n in 1..=3 {
        write_junk_mp3(&m.join("Art/Alb").join(format!("{n:02}.mp3")), n * 100);
    }
    crate::roots::ensure_roots(&e.ctx).unwrap();
    let app = crate::router(e.ctx.clone());

    let (st, roots) = call(&app, "GET", "/library/roots", None).await;
    assert_eq!(st, 200);
    let lib = roots.as_array().unwrap().iter().find(|r| r["kind"] == "library").unwrap().clone();
    let id = lib["id"].as_i64().unwrap();

    let (st, j) = call(&app, "POST", &format!("/library/scan?root_id={id}"), None).await;
    assert_eq!(st, 202);
    let job = j["job_id"].as_str().unwrap().to_string();
    let mut status = Value::Null;
    for _ in 0..200 {
        let (st, s) = call(&app, "GET", &format!("/library/scan/{job}"), None).await;
        assert_eq!(st, 200);
        if s["state"] != "running" {
            status = s;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(status["state"], "done", "{status}");
    let r = &status["results"][0];
    assert_eq!((r["files_seen"].as_i64(), r["files_added"].as_i64(), r["tracks_added"].as_i64()), (Some(3), Some(3), Some(3)));
    assert_eq!(r["root_path"], lib["path"]);

    // rescan is incremental
    let (_, j) = call(&app, "POST", "/library/scan", None).await;
    let job = j["job_id"].as_str().unwrap().to_string();
    let mut status = Value::Null;
    for _ in 0..200 {
        let (_, s) = call(&app, "GET", &format!("/library/scan/{job}"), None).await;
        if s["state"] != "running" {
            status = s;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let results = status["results"].as_array().unwrap();
    let lib_res = results.iter().find(|r| r["root_id"].as_i64() == Some(id)).unwrap();
    assert_eq!(lib_res["files_unchanged"], 3);

    let (_, roots) = call(&app, "GET", "/library/roots", None).await;
    let lib = roots.as_array().unwrap().iter().find(|r| r["id"].as_i64() == Some(id)).unwrap();
    assert_eq!(lib["track_count"], 3);
    assert!(lib["last_scan_at"].is_string());

    assert_eq!(call(&app, "GET", "/library/scan/nope", None).await.0, 404);
    assert_eq!(call(&app, "POST", "/library/scan?root_id=9999", None).await.0, 404);

    // patch + add + delete
    let (st, p) = call(&app, "PATCH", &format!("/library/roots/{id}"), Some(json!({"watch": true}))).await;
    assert_eq!(st, 200);
    assert_eq!(p["watch"], true);
    let (st, _) = call(&app, "POST", "/library/roots", Some(json!({"path": "/no/such/dir"}))).await;
    assert_eq!(st, 400);
    let extra = e.dir.path().join("extra");
    std::fs::create_dir_all(&extra).unwrap();
    let (st, a) = call(&app, "POST", "/library/roots", Some(json!({"path": extra.to_string_lossy(), "kind": "downloads"}))).await;
    assert_eq!(st, 200);
    let (_, a2) = call(&app, "POST", "/library/roots", Some(json!({"path": extra.to_string_lossy(), "kind": "downloads"}))).await;
    assert_eq!(a["id"], a2["id"], "idempotent");
    assert_eq!(call(&app, "DELETE", &format!("/library/roots/{}", a["id"]), None).await.0, 204);
    assert_eq!(call(&app, "DELETE", &format!("/library/roots/{}", a["id"]), None).await.0, 404);
}

#[tokio::test]
async fn scan_publishes_events() {
    let e = env();
    let m = e.music();
    write_junk_mp3(&m.join("A/B/01.mp3"), 10);
    let id = add_root(&e.ctx, &m, "library");
    let mut rx = e.ctx.bus.subscribe();
    let app = crate::router(e.ctx.clone());
    let (st, _) = call(&app, "POST", &format!("/library/scan?root_id={id}"), None).await;
    assert_eq!(st, 202);
    let mut topics = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Ok(Ok(ev)) = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
            topics.push(ev.topic);
            if topics.iter().any(|t| t == "library.scan.done") && topics.iter().any(|t| t == "library.changed") {
                break;
            }
        }
    }
    assert!(topics.iter().any(|t| t == "library.scan.progress"), "{topics:?}");
    assert!(topics.iter().any(|t| t == "library.scan.done"), "{topics:?}");
    assert!(topics.iter().any(|t| t == "library.changed"), "{topics:?}");
}

#[test]
fn watcher_ingests_new_files() {
    use crate::watcher::{WatchConfig, Watcher};
    use std::time::Duration;
    let e = env();
    let m = e.music();
    let id = add_root(&e.ctx, &m, "library");
    crate::roots::patch_root(&e.ctx, id, bc_types::library::RootPatch { enabled: None, watch: Some(true) }).unwrap();
    let w = Watcher::start_with(&e.ctx, WatchConfig { debounce: Duration::from_millis(300), max_wait: Duration::from_secs(5) });
    std::thread::sleep(Duration::from_millis(600));
    write_junk_mp3(&m.join("Live/Rec/01.mp3"), 50);
    let mut ok = false;
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(100));
        if scalar(&e.ctx, "SELECT COUNT(*) FROM files") == 1 {
            ok = true;
            break;
        }
    }
    assert!(ok, "watcher should have ingested the new file");
    // removal marks missing
    std::fs::remove_file(m.join("Live/Rec/01.mp3")).unwrap();
    let mut gone = false;
    for _ in 0..80 {
        std::thread::sleep(Duration::from_millis(100));
        if scalar(&e.ctx, "SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL") == 1 {
            gone = true;
            break;
        }
    }
    assert!(gone, "watcher should mark the removed file missing");
    w.stop();
}
