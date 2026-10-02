//! The `/player/*` HTTP routes and the WebSocket command entry point, over a
//! software-paced output and an ad-hoc file library.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bc_core::EventBus;
use bc_engine::host::OutputKind;
use bc_engine::ports::FilePorts;
use bc_engine::session::{PlayerError, SessionConfig};
use bc_engine::PlayerService;
use bc_types::player::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn wav(dir: &std::path::Path, name: &str, secs: f32) -> std::path::PathBuf {
    let p = dir.join(name);
    let spec = hound::WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(&p, spec).unwrap();
    for i in 0..(48_000.0 * secs) as usize {
        let v = ((i as f32 * 0.03).sin() * 6000.0) as i16;
        w.write_sample(v).unwrap();
        w.write_sample(v).unwrap();
    }
    w.finalize().unwrap();
    p
}

async fn service(n: usize) -> (PlayerService, Arc<EventBus>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let files = (0..n).map(|i| wav(dir.path(), &format!("t{i}.wav"), 3.0)).collect();
    let bus = Arc::new(EventBus::new());
    let cfg = SessionConfig { output: OutputKind::Null { sample_rate: 48_000, block: 256, speed: 4.0, capture: None, cue: None }, mpris: false, ..Default::default() };
    let svc = PlayerService::with_ports(FilePorts::new(files).into_ports(), bus.clone(), cfg);
    svc.start().await;
    (svc, bus, dir)
}

async fn call(app: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_drive_the_session_and_publish_events() {
    let (svc, bus, _d) = service(3).await;
    let mut rx = bus.subscribe();
    let app = svc.router();

    let items: Vec<Value> = (1..=3).map(|i| json!({"track_id": i, "title": format!("t{i}")})).collect();
    let (st, state) = call(&app, "POST", "/player/queue/play", json!({"items": items, "start_index": 1})).await;
    assert_eq!(st, StatusCode::OK, "{state}");
    assert_eq!(state["queue_index"], 1);
    assert_eq!(state["current"]["title"], "t2");
    assert_eq!(state["queue"].as_array().unwrap().len(), 3);

    // a volume change and a repeat change come back in the state
    let (_, state) = call(&app, "POST", "/player/volume", json!({"volume": 0.4})).await;
    assert!((state["volume"].as_f64().unwrap() - 0.4).abs() < 1e-9);
    let (_, state) = call(&app, "POST", "/player/repeat", json!({"mode": "all"})).await;
    assert_eq!(state["repeat"], "all");

    // queue edits through the routes
    let (_, state) = call(&app, "POST", "/player/queue/remove", json!({"index": 2})).await;
    assert_eq!(state["queue"].as_array().unwrap().len(), 2);
    let (_, state) = call(&app, "POST", "/player/queue/play-next", json!({"items": [{"track_id": 3, "title": "again"}]})).await;
    assert_eq!(state["queue"][2]["title"], "again");

    // planner
    let (st, plan) = call(&app, "POST", "/player/plan/op", json!({"op": "set_auto_fill", "on": false})).await;
    assert_eq!((st, &plan["auto_fill"]), (StatusCode::OK, &json!(false)));
    let (_, plan) = call(&app, "GET", "/player/plan", json!({})).await;
    assert_eq!(plan["auto_fill"], false);

    // errors are problem+json
    let (st, prob) = call(&app, "POST", "/player/jump", json!({"index": 99})).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(prob["status"], 404);
    let (st, _) = call(&app, "POST", "/player/command", json!({"cmd": "no_such_command"})).await;
    assert!(st.is_client_error());

    // the bus carried the state and clock topics
    let mut topics = std::collections::HashSet::new();
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_secs(3) && !(topics.contains(TOPIC_PLAYER_STATE) && topics.contains(TOPIC_PLAYER_CLOCK)) {
        if let Ok(Ok(ev)) = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await {
            topics.insert(ev.topic);
        }
    }
    assert!(topics.contains(TOPIC_PLAYER_STATE), "topics: {topics:?}");
    assert!(topics.contains(TOPIC_PLAYER_CLOCK), "topics: {topics:?}");
    svc.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_command_entry_point() {
    let (svc, _bus, _d) = service(2).await;
    let r = svc
        .handle_command(json!({"cmd": "play_queue", "items": [{"track_id": 1, "title": "t1"}, {"track_id": 2, "title": "t2"}], "start_index": 0}))
        .await
        .unwrap();
    assert_eq!(r, json!({"ok": true}));
    svc.handle_command(json!({"cmd": "toggle_shuffle"})).await.unwrap();
    assert!(svc.handle().state().shuffle);
    let err = svc.handle_command(json!({"cmd": "seek"})).await.unwrap_err();
    assert!(matches!(err, PlayerError::BadCommand(_)));
    let devices = svc.handle_command(json!({"cmd": "list_devices"})).await.unwrap();
    assert!(devices.get("outputs").is_some());
    svc.shutdown();
}
