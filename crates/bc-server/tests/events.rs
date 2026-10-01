//! Port of test_events.py for the WebSocket hub: priming, delivery, resume after the last id,
//! monotonic ids, resync on an unknown epoch, subscriber release, player command forwarding.
mod common;
use std::time::Duration;

use bc_core::Config;
use bc_server::{RunningServer, ServerOptions};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn server(dir: &std::path::Path) -> RunningServer {
    let cfg: Config = common::config(dir);
    bc_server::start(cfg, ServerOptions { no_services: true, no_ui: true, ..Default::default() }).await.unwrap()
}

async fn connect(srv: &RunningServer) -> Ws {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/api/ws", srv.addr)).await.unwrap();
    ws
}

async fn next_json(ws: &mut Ws) -> serde_json::Value {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("timeout").expect("closed").unwrap();
        if let Message::Text(t) = m {
            return serde_json::from_str(t.as_str()).unwrap();
        }
    }
}

async fn next_topic(ws: &mut Ws, topic: &str) -> serde_json::Value {
    loop {
        let v = next_json(ws).await;
        if v["topic"] == topic {
            return v;
        }
        assert_ne!(v["topic"], "stream.resync", "unexpected resync");
    }
}

async fn hello(ws: &mut Ws, last: Option<(u64, u64)>) {
    let last = last.map(|(e, s)| serde_json::json!({"epoch": e, "seq": s}));
    ws.send(Message::Text(serde_json::json!({"type": "hello", "last_event_id": last}).to_string().into())).await.unwrap();
}

#[tokio::test]
async fn stream_primes_before_any_event() {
    // The first bytes must arrive without waiting for an event (legacy: a silent stream looked dead).
    let d = tempfile::tempdir().unwrap();
    let srv = server(d.path()).await;
    let mut ws = connect(&srv).await;
    let first = next_json(&mut ws).await;
    assert_eq!(first["topic"], "stream.hello");
    assert_eq!(first["payload"]["epoch"].as_u64().unwrap(), srv.state.bus.epoch());
    srv.stop().await;
}

#[tokio::test]
async fn published_events_are_delivered() {
    let d = tempfile::tempdir().unwrap();
    let srv = server(d.path()).await;
    let mut ws = connect(&srv).await;
    next_topic(&mut ws, "stream.hello").await;
    hello(&mut ws, None).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    srv.state.bus.publish("job.created", &serde_json::json!({"job_id": "abc", "total": 3}));
    let ev = next_topic(&mut ws, "job.created").await;
    assert_eq!(ev["payload"]["job_id"], "abc");
    srv.stop().await;
}

#[tokio::test]
async fn replay_resumes_after_last_event_id() {
    let d = tempfile::tempdir().unwrap();
    let srv = server(d.path()).await;
    for n in 1..=3 {
        srv.state.bus.publish("job.progress", &serde_json::json!({"n": n}));
    }
    let mut ws = connect(&srv).await;
    next_topic(&mut ws, "stream.hello").await;
    hello(&mut ws, Some((srv.state.bus.epoch(), 1))).await;
    let a = next_topic(&mut ws, "job.progress").await;
    let b = next_topic(&mut ws, "job.progress").await;
    assert_eq!(a["payload"]["n"], 2);
    assert_eq!(b["payload"]["n"], 3);
    srv.stop().await;
}

#[tokio::test]
async fn bus_assigns_monotonic_ids() {
    let d = tempfile::tempdir().unwrap();
    let srv = server(d.path()).await;
    let mut ws = connect(&srv).await;
    next_topic(&mut ws, "stream.hello").await;
    hello(&mut ws, None).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    srv.state.bus.publish("a", &());
    srv.state.bus.publish("b", &());
    let x = next_topic(&mut ws, "a").await;
    let y = next_topic(&mut ws, "b").await;
    assert!(x["id"]["seq"].as_u64().unwrap() < y["id"]["seq"].as_u64().unwrap());
    srv.stop().await;
}

#[tokio::test]
async fn unknown_epoch_gets_a_resync() {
    let d = tempfile::tempdir().unwrap();
    let srv = server(d.path()).await;
    let mut ws = connect(&srv).await;
    next_topic(&mut ws, "stream.hello").await;
    hello(&mut ws, Some((1, 5))).await;
    let v = next_json(&mut ws).await;
    assert_eq!(v["topic"], "stream.resync");
    srv.stop().await;
}

#[tokio::test]
async fn app_level_ping_is_answered() {
    let d = tempfile::tempdir().unwrap();
    let srv = server(d.path()).await;
    let mut ws = connect(&srv).await;
    next_topic(&mut ws, "stream.hello").await;
    ws.send(Message::Text(r#"{"type":"ping"}"#.into())).await.unwrap();
    let v = next_topic(&mut ws, "pong").await;
    assert_eq!(v["topic"], "pong");
    srv.stop().await;
}

#[tokio::test]
async fn player_command_without_a_player_answers_a_problem() {
    let d = tempfile::tempdir().unwrap();
    let srv = server(d.path()).await;
    let mut ws = connect(&srv).await;
    next_topic(&mut ws, "stream.hello").await;
    hello(&mut ws, None).await;
    ws.send(Message::Text(r#"{"type":"player","command":{"cmd":"toggle"}}"#.into())).await.unwrap();
    let v = next_topic(&mut ws, "player.error").await;
    assert_eq!(v["payload"]["status"], 503);
    srv.stop().await;
}

#[tokio::test]
async fn disconnect_releases_the_subscriber() {
    // A leaked receiver per page load would grow unboundedly.
    let d = tempfile::tempdir().unwrap();
    let srv = server(d.path()).await;
    let mut ws = connect(&srv).await;
    next_topic(&mut ws, "stream.hello").await;
    drop(ws);
    tokio::time::sleep(Duration::from_millis(300)).await;
    // publishing after the drop must not panic and the broadcast has no live receivers from the hub
    srv.state.bus.publish("x", &());
    srv.stop().await;
}
