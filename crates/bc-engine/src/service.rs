//! `PlayerService`: the player as a service of the server (`new`, `start`,
//! `router`) plus `handle_command(Value)` for the WebSocket hub's remote control.

use crate::host::OutputKind;
use crate::ports::{DbPorts, Ports};
use crate::session::{PlayerError, PlayerReply, Publisher, Session, SessionConfig, SessionMsg};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get, patch, post, put};
use axum::{Json, Router};
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_types::Problem;
use bc_types::player::*;
use crossbeam_channel::Sender;
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use std::sync::Arc;

/// Latest published state, readable without asking the session thread.
struct Shared {
    state: RwLock<PlayerState>,
    clock: RwLock<Clock>,
}

struct BusPublisher {
    bus: Arc<EventBus>,
    shared: Arc<Shared>,
    mpris: Option<Box<dyn Publisher>>,
}

impl Publisher for BusPublisher {
    fn state(&self, s: &PlayerState) {
        {
            let mut cur = self.shared.state.write();
            let keep_queue = if s.queue_included { None } else { Some(std::mem::take(&mut cur.queue)) };
            *cur = s.clone();
            if let Some(q) = keep_queue {
                cur.queue = q;
            }
            cur.queue_included = true;
        }
        self.bus.publish(TOPIC_PLAYER_STATE, s);
        if let Some(m) = &self.mpris {
            m.state(s);
        }
    }
    fn clock(&self, c: &Clock) {
        *self.shared.clock.write() = c.clone();
        self.bus.publish(TOPIC_PLAYER_CLOCK, c);
        if let Some(m) = &self.mpris {
            m.clock(c);
        }
    }
    fn transition(&self, t: Option<&TransitionState>) {
        self.bus.publish(TOPIC_PLAYER_TRANSITION, &t);
    }
}

/// Cheap clonable handle for routes and the MPRIS thread.
#[derive(Clone)]
pub struct PlayerHandle {
    tx: Sender<SessionMsg>,
    shared: Arc<Shared>,
}

impl PlayerHandle {
    pub async fn run(&self, cmd: PlayerCommand) -> Result<PlayerReply, PlayerError> {
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        self.tx.send(SessionMsg::Cmd(cmd, Some(rtx))).map_err(|_| PlayerError::Unavailable("player stopped".into()))?;
        rrx.await.map_err(|_| PlayerError::Unavailable("player stopped".into()))?
    }

    /// Fire and forget (media keys).
    pub fn send(&self, cmd: PlayerCommand) {
        let _ = self.tx.send(SessionMsg::Cmd(cmd, None));
    }

    pub fn state(&self) -> PlayerState {
        self.shared.state.read().clone()
    }

    pub fn clock(&self) -> Clock {
        self.shared.clock.read().clone()
    }
}

pub struct PlayerService {
    handle: PlayerHandle,
    db: Option<Db>,
    ffmpeg: String,
    session: Mutex<Option<(Session, crossbeam_channel::Receiver<SessionMsg>)>>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
    mpris: bool,
}

/// The output the service opens, from `BC_AUDIO` (`null` for headless runs and tests).
pub fn output_from_env() -> OutputKind {
    match std::env::var("BC_AUDIO").ok().as_deref() {
        Some("null") => OutputKind::Null { sample_rate: 48_000, block: 512, speed: 1.0, capture: None, cue: None },
        _ => OutputKind::Cpal(OutputTarget::default()),
    }
}

impl PlayerService {
    /// Build the service over the library DB (the shim ports until WS1-3 publish theirs).
    pub fn new(db: Db, bus: Arc<EventBus>, config: &Config) -> Self {
        let base = format!("http://{}:{}", if config.host == "0.0.0.0" { "127.0.0.1" } else { &config.host }, config.port);
        Self::new_with(db, bus, config, SessionConfig { output: output_from_env(), base_url: base, ..Default::default() })
    }

    /// Like [`new`](Self::new) with an explicit session config (output, MPRIS, stretch quality).
    pub fn new_with(db: Db, bus: Arc<EventBus>, config: &Config, cfg: SessionConfig) -> Self {
        let ports = DbPorts::new(db.clone()).into_ports(&cfg.base_url);
        let mut svc = Self::with_ports(ports, bus, cfg);
        svc.db = Some(db);
        svc.ffmpeg = config.ffmpeg_bin.clone();
        svc
    }

    /// Like [`new_with`](Self::new_with) but with the Bandcamp port replaced (the server's
    /// `BcPlayerPort`, so `QueueSource::Fan` / `Explore` and Bandcamp streams resolve). The
    /// `/sets/{id}/render` route stays.
    pub fn new_with_bandcamp(db: Db, bus: Arc<EventBus>, config: &Config, cfg: SessionConfig, bandcamp: Arc<dyn crate::ports::BandcampPort>) -> Self {
        let mut ports = DbPorts::new(db.clone()).into_ports(&cfg.base_url);
        ports.bandcamp = bandcamp;
        Self::with_ports(ports, bus, cfg).with_render(db, &config.ffmpeg_bin)
    }

    /// Serve `GET /sets/{id}/render` from this DB (what `new` / `new_with` do already); for a
    /// service built with [`with_ports`](Self::with_ports).
    pub fn with_render(mut self, db: Db, ffmpeg_bin: &str) -> Self {
        self.db = Some(db);
        self.ffmpeg = ffmpeg_bin.to_string();
        self
    }

    pub fn with_ports(ports: Ports, bus: Arc<EventBus>, cfg: SessionConfig) -> Self {
        let shared = Arc::new(Shared {
            state: RwLock::new(PlayerState { queue_included: true, queue_index: -1, history_pos: -1, ..Default::default() }),
            clock: RwLock::new(Clock { rate: 1.0, ..Default::default() }),
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = PlayerHandle { tx, shared: shared.clone() };
        let mpris_enabled = cfg.mpris;
        let mpris: Option<Box<dyn Publisher>> = if mpris_enabled { crate::mpris::start(handle.clone()) } else { None };
        let publisher = BusPublisher { bus, shared, mpris };
        let session = Session::new(ports, Box::new(publisher), cfg);
        Self { handle, db: None, ffmpeg: "ffmpeg".into(), session: Mutex::new(Some((session, rx))), join: Mutex::new(None), mpris: mpris_enabled }
    }

    /// Spawn the session thread (restores the persisted queue; idempotent).
    pub async fn start(&self) {
        let Some((session, rx)) = self.session.lock().take() else { return };
        let j = std::thread::Builder::new().name("bc-player-session".into()).spawn(move || session.run(rx)).ok();
        *self.join.lock() = j;
        tracing::info!("player session started (mpris: {})", self.mpris);
    }

    pub fn handle(&self) -> PlayerHandle {
        self.handle.clone()
    }

    /// Remote-control entry point for the WebSocket hub (`ClientMsg::Player`).
    pub async fn handle_command(&self, command: Value) -> Result<Value, PlayerError> {
        let cmd: PlayerCommand = serde_json::from_value(command).map_err(|e| PlayerError::BadCommand(e.to_string()))?;
        match self.handle.run(cmd).await? {
            PlayerReply::Ok => Ok(json!({ "ok": true })),
            PlayerReply::Devices(d) => Ok(serde_json::to_value(d).unwrap_or(Value::Null)),
        }
    }

    pub fn shutdown(&self) {
        let _ = self.handle.tx.send(SessionMsg::Shutdown);
        if let Some(j) = self.join.lock().take() {
            let _ = j.join();
        }
    }

    /// Routes under `/player/*` (no `/api` prefix; state already applied).
    pub fn router(&self) -> Router {
        let r = router(self.handle.clone());
        match &self.db {
            Some(db) => r.merge(render_router(RenderState { db: db.clone(), ffmpeg: self.ffmpeg.clone() })),
            None => r,
        }
    }
}

impl Drop for PlayerService {
    fn drop(&mut self) {
        let _ = self.handle.tx.send(SessionMsg::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

pub struct ApiError(PlayerError);

impl From<PlayerError> for ApiError {
    fn from(e: PlayerError) -> Self {
        ApiError(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, title) = match &self.0 {
            PlayerError::BadCommand(_) => (400, "Bad player command"),
            PlayerError::NotFound(_) => (404, "Not found"),
            PlayerError::Conflict(_) => (409, "Conflict"),
            PlayerError::Unavailable(_) => (503, "Audio unavailable"),
        };
        let body = Problem::new(status, title).detail(self.0.to_string());
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (code, [(header::CONTENT_TYPE, "application/problem+json")], Json(body)).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

async fn run_value(h: &PlayerHandle, v: Value) -> ApiResult<PlayerState> {
    let cmd: PlayerCommand = serde_json::from_value(v).map_err(|e| ApiError(PlayerError::BadCommand(e.to_string())))?;
    h.run(cmd).await?;
    Ok(Json(h.state()))
}

/// `POST` route that sets `"cmd": tag` on the (optional) JSON object body.
fn tagged(tag: &'static str) -> MethodRouter<PlayerHandle> {
    post(move |State(h): State<PlayerHandle>, body: Option<Json<Value>>| async move {
        let mut v = body.map(|b| b.0).unwrap_or_else(|| json!({}));
        if let Some(o) = v.as_object_mut() {
            o.insert("cmd".into(), tag.into());
        }
        run_value(&h, v).await
    })
}

pub fn router(h: PlayerHandle) -> Router {
    let wrap = |tag: &'static str, field: &'static str| {
        move |State(h): State<PlayerHandle>, Json(body): Json<Value>| async move { run_value(&h, json!({ "cmd": tag, field: body })).await }
    };
    Router::new()
        .route("/player/state", get(|State(h): State<PlayerHandle>| async move { Json(h.state()) }))
        .route("/player/clock", get(|State(h): State<PlayerHandle>| async move { Json(h.clock()) }))
        .route(
            "/player/queue",
            get(|State(h): State<PlayerHandle>| async move {
                let s = h.state();
                Json(json!({ "queue": s.queue, "queue_index": s.queue_index, "queue_rev": s.queue_rev, "history": s.history,
                             "history_pos": s.history_pos, "source": s.source, "shuffle": s.shuffle, "repeat": s.repeat }))
            }),
        )
        .route("/player/command", post(|State(h): State<PlayerHandle>, Json(cmd): Json<PlayerCommand>| async move {
            h.run(cmd).await?;
            Ok::<_, ApiError>(Json(h.state()))
        }))
        .route("/player/play", tagged("play"))
        .route("/player/pause", tagged("pause"))
        .route("/player/toggle", tagged("toggle"))
        .route("/player/stop", tagged("stop"))
        .route("/player/next", tagged("next"))
        .route("/player/previous", tagged("previous"))
        .route("/player/seek", tagged("seek"))
        .route("/player/jump", tagged("jump_to"))
        .route("/player/queue/play", tagged("play_queue"))
        .route("/player/queue/start-source", tagged("start_source"))
        .route("/player/queue/add", tagged("add_to_queue"))
        .route("/player/queue/insert", tagged("insert_at"))
        .route("/player/queue/play-next", tagged("play_next"))
        .route("/player/queue/move", tagged("move_in_queue"))
        .route("/player/queue/remove", tagged("remove_at"))
        .route("/player/queue/remove-range", tagged("remove_range"))
        .route("/player/queue/replace", tagged("replace_at"))
        .route("/player/volume", post(|State(h): State<PlayerHandle>, Json(b): Json<Value>| async move {
            if let Some(m) = b.get("muted").and_then(|m| m.as_bool()) {
                h.run(PlayerCommand::SetMuted { muted: m }).await?;
            }
            if let Some(v) = b.get("volume").and_then(|v| v.as_f64()) {
                h.run(PlayerCommand::SetVolume { volume: v }).await?;
            }
            Ok::<_, ApiError>(Json(h.state()))
        }))
        .route("/player/repeat", tagged("set_repeat"))
        .route("/player/shuffle", tagged("set_shuffle"))
        .route("/player/mix", put(|State(h): State<PlayerHandle>, Json(mut b): Json<Value>| async move {
            if let Some(o) = b.as_object_mut() {
                o.insert("cmd".into(), "set_mix".into());
            }
            run_value(&h, b).await
        }))
        .route("/player/mix/settings", patch(wrap("set_mix_settings", "patch")))
        .route("/player/mix/now", tagged("mix_now"))
        .route("/player/transition/cut", tagged("cut_now"))
        .route("/player/transition/retime", tagged("retime"))
        .route("/player/transition/echo", tagged("set_transition_echo"))
        .route("/player/transition/sync", tagged("set_transition_sync"))
        .route("/player/transition/nudge", tagged("nudge"))
        .route("/player/mix-out", put(|State(h): State<PlayerHandle>, Json(mut b): Json<Value>| async move {
            if let Some(o) = b.as_object_mut() {
                o.insert("cmd".into(), "set_mix_out_override".into());
            }
            run_value(&h, b).await
        }))
        .route("/player/strip", patch(wrap("set_strip", "patch")))
        .route("/player/plan", get(|State(h): State<PlayerHandle>| async move { Json(h.state().plan) })
            .put(|State(h): State<PlayerHandle>, Json(p): Json<PlanState>| async move {
                h.run(PlayerCommand::SetPlan { plan: p }).await?;
                Ok::<_, ApiError>(Json(h.state().plan))
            }))
        .route("/player/plan/op", post(|State(h): State<PlayerHandle>, Json(op): Json<PlanOp>| async move {
            h.run(PlayerCommand::Plan { op }).await?;
            Ok::<_, ApiError>(Json(h.state().plan))
        }))
        .route("/player/devices", get(|State(h): State<PlayerHandle>| async move {
            match h.run(PlayerCommand::ListDevices).await? {
                PlayerReply::Devices(d) => Ok::<_, ApiError>(Json(d)),
                _ => Ok(Json(h.state().devices)),
            }
        }))
        .route("/player/output", put(|State(h): State<PlayerHandle>, Json(t): Json<OutputTarget>| async move {
            h.run(PlayerCommand::SetOutput { target: t }).await?;
            Ok::<_, ApiError>(Json(h.state().devices))
        }))
        .route("/player/preview", post(tagged_fn("preview_start")).delete(|State(h): State<PlayerHandle>| async move {
            h.run(PlayerCommand::PreviewStop).await?;
            Ok::<_, ApiError>(Json(h.state()))
        }))
        .with_state(h)
}

#[allow(clippy::type_complexity)]
fn tagged_fn(tag: &'static str) -> impl Fn(State<PlayerHandle>, Option<Json<Value>>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Json<PlayerState>, ApiError>> + Send>> + Clone + Send + Sync + 'static {
    move |State(h), body| {
        Box::pin(async move {
            let mut v = body.map(|b| b.0).unwrap_or_else(|| json!({}));
            if let Some(o) = v.as_object_mut() {
                o.insert("cmd".into(), tag.into());
            }
            run_value(&h, v).await
        })
    }
}

// ---------------------------------------------------------------------------
// offline set render: GET /sets/{id}/render?format=mp3|wav
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct RenderState {
    db: Db,
    ffmpeg: String,
}

#[derive(serde::Deserialize)]
struct RenderQuery {
    format: Option<String>,
}

fn render_router(st: RenderState) -> Router {
    Router::new().route("/sets/{id}/render", get(render_set)).with_state(st)
}

fn problem(status: u16, title: &str, detail: String) -> Response {
    let body = Problem::new(status, title).detail(detail);
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (code, [(header::CONTENT_TYPE, "application/problem+json")], Json(body)).into_response()
}

/// The plan performed offline through the live graph, streamed while it renders.
async fn render_set(
    State(st): State<RenderState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    axum::extract::Query(q): axum::extract::Query<RenderQuery>,
) -> Response {
    use crate::render::{Format, RenderOptions, render_stream, slots_from_set};
    let format = match q.format.as_deref().map(Format::parse) {
        None => Format::Mp3,
        Some(Some(f)) => f,
        Some(None) => return problem(400, "Bad format", "format must be mp3 or wav".into()),
    };
    let db = st.db.clone();
    let loaded = tokio::task::spawn_blocking(move || slots_from_set(&db, id)).await;
    let (name, slots) = match loaded {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            let msg = e.to_string();
            return if msg.contains("not found") { problem(404, "Not found", msg) } else { problem(500, "Render failed", msg) };
        }
        Err(e) => return problem(500, "Render failed", e.to_string()),
    };
    if slots.is_empty() {
        return problem(400, "Nothing to render", "the set has no playable tracks".into());
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
    let opts = RenderOptions { ffmpeg: st.ffmpeg.clone(), ..Default::default() };
    std::thread::spawn(move || {
        if let Err(e) = render_stream(&slots, &opts, format, &tx) {
            tracing::warn!("set render failed: {e}");
            let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
        }
    });
    let safe: String = name.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' { c } else { '_' }).collect();
    (
        [
            (header::CONTENT_TYPE, format.content_type().to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}.{}\"", safe.trim(), format.ext())),
        ],
        axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx)),
    )
        .into_response()
}
