//! `/api/ws`: Hello(last_event_id) resume, broadcast fan-out, heartbeat and
//! player commands.
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use bc_core::events::Replay;
use bc_types::events::{ClientMsg, Event, EventId, TOPIC_RESYNC};
use tokio::sync::broadcast::error::RecvError;

use crate::state::AppState;

pub async fn upgrade(State(st): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |sock| session(st, sock))
}

fn synthetic(epoch: u64, topic: &str, payload: serde_json::Value) -> String {
    serde_json::json!({ "id": { "epoch": epoch, "seq": 0 }, "topic": topic, "payload": payload }).to_string()
}

async fn send(sock: &mut WebSocket, text: String) -> bool {
    sock.send(Message::Text(text.into())).await.is_ok()
}

async fn session(st: AppState, mut sock: WebSocket) {
    let epoch = st.bus.epoch();
    // Subscribe first so nothing published during replay is lost; dedupe by seq.
    let mut rx = st.bus.subscribe();
    if !send(&mut sock, synthetic(epoch, "stream.hello", serde_json::json!({ "epoch": epoch, "version": env!("CARGO_PKG_VERSION") }))).await {
        return;
    }
    let mut max_sent: u64 = 0;
    let mut ready = false;
    let hello_deadline = tokio::time::sleep(Duration::from_millis(1500));
    tokio::pin!(hello_deadline);
    let mut hb = tokio::time::interval(Duration::from_secs(15));
    hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = &mut hello_deadline, if !ready => { ready = true; }
            _ = hb.tick() => {
                let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
                if !send(&mut sock, synthetic(epoch, "hb", serde_json::json!({ "t": t }))).await { break; }
            }
            ev = rx.recv(), if ready => match ev {
                Ok(e) => {
                    if e.id.seq <= max_sent { continue; }
                    max_sent = e.id.seq;
                    if !send_event(&mut sock, &e).await { break; }
                }
                Err(RecvError::Lagged(_)) => {
                    if !send(&mut sock, synthetic(epoch, TOPIC_RESYNC, serde_json::json!({ "reason": "lagged" }))).await { break; }
                    // skip to the live head
                    rx = st.bus.subscribe();
                    max_sent = 0;
                }
                Err(RecvError::Closed) => break,
            },
            msg = sock.recv() => {
                let Some(Ok(msg)) = msg else { break };
                match msg {
                    Message::Text(t) => {
                        let text = t.as_str();
                        match serde_json::from_str::<ClientMsg>(text) {
                            Ok(ClientMsg::Hello { last_event_id }) => {
                                if !handle_hello(&st, &mut sock, last_event_id, &mut max_sent).await { break; }
                                ready = true;
                            }
                            Ok(ClientMsg::Player { command }) => {
                                let reply = match &st.player {
                                    Some(p) => match p.handle_command(command).await {
                                        Ok(v) => synthetic(epoch, "player.reply", v),
                                        Err(prob) => synthetic(epoch, "player.error", serde_json::to_value(&prob).unwrap_or_default()),
                                    },
                                    None => synthetic(epoch, "player.error", serde_json::json!({"status": 503, "title": "player unavailable"})),
                                };
                                if !send(&mut sock, reply).await { break; }
                            }
                            Err(_) => {
                                // app-level ping
                                if text.contains("\"ping\"")
                                    && !send(&mut sock, synthetic(epoch, "pong", serde_json::json!({}))).await { break; }
                            }
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }
}

async fn send_event(sock: &mut WebSocket, e: &Event) -> bool {
    match serde_json::to_string(e) {
        Ok(s) => send(sock, s).await,
        Err(_) => true,
    }
}

async fn handle_hello(st: &AppState, sock: &mut WebSocket, last: Option<EventId>, max_sent: &mut u64) -> bool {
    match st.bus.replay_since(last) {
        Replay::Events(evs) => {
            for e in evs {
                if e.id.seq <= *max_sent { continue; }
                *max_sent = e.id.seq;
                if !send_event(sock, &e).await { return false; }
            }
            true
        }
        Replay::Resync => {
            // Everything currently buffered is covered by the refetch the client will do.
            send(sock, synthetic(st.bus.epoch(), TOPIC_RESYNC, serde_json::json!({ "reason": "gap" }))).await
        }
    }
}
