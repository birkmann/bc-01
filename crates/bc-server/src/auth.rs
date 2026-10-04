//! LAN pairing (PLAN §3.2). Localhost peers are always trusted. In LAN mode any
//! other peer needs a per-device token, obtained by redeeming a short-lived
//! pairing code (shown as text and QR in Settings on the desktop).
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS paired_devices (
    id INTEGER PRIMARY KEY, name TEXT NOT NULL, token_hash TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, last_seen TEXT)";
const CODE_TTL: Duration = Duration::from_secs(300);
pub const COOKIE: &str = "bc_token";

#[derive(Default)]
pub struct PairingStore {
    codes: Mutex<HashMap<String, Instant>>,
}

impl PairingStore {
    pub fn issue(&self) -> String {
        let mut codes = self.codes.lock();
        codes.retain(|_, t| t.elapsed() < CODE_TTL);
        // 8 chars from an unambiguous alphabet, grouped for readability.
        const A: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let raw: [u8; 8] = rand::random();
        let code: String = raw.iter().map(|b| A[*b as usize % A.len()] as char).collect();
        let code = format!("{}-{}", &code[..4], &code[4..]);
        codes.insert(code.clone(), Instant::now());
        code
    }
    /// One-time redeem.
    pub fn redeem(&self, code: &str) -> bool {
        let mut codes = self.codes.lock();
        codes.retain(|_, t| t.elapsed() < CODE_TTL);
        codes.remove(&code.trim().to_ascii_uppercase()).is_some()
    }
}

fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn new_token() -> String {
    let raw: [u8; 32] = rand::random();
    hex::encode(raw)
}

pub fn host_is_local(host: &str) -> bool {
    let h = host.trim();
    let name = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        h.rsplit_once(':').map(|(a, _)| a).unwrap_or(h)
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1") || name.ends_with(".localhost")
}

fn token_from(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    if let Some(a) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
        && let Some(t) = a.strip_prefix("Bearer ") {
            return Some(t.trim().to_string());
        }
    if let Some(c) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for part in c.split(';') {
            if let Some(v) = part.trim().strip_prefix(&format!("{COOKIE}=")) {
                return Some(v.to_string());
            }
        }
    }
    query.and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token=").map(str::to_string)))
}

pub(crate) fn is_loopback(peer: Option<SocketAddr>) -> bool {
    match peer {
        None => true, // in-process (tests, tower oneshot)
        Some(a) => match a.ip() {
            IpAddr::V4(v) => v.is_loopback(),
            IpAddr::V6(v) => v.is_loopback() || v.to_ipv4_mapped().is_some_and(|m| m.is_loopback()),
        },
    }
}

fn is_public_path(path: &str) -> bool {
    !path.starts_with("/api")
        || matches!(path, "/api/health" | "/api/auth/pair" | "/api/auth/status")
}

/// Host check (DNS-rebinding defence) plus LAN token auth.
pub async fn guard(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let local_host = host.is_empty() || host_is_local(&host);
    if !st.net.lan() {
        // Loopback-only server: reject foreign Host headers outright, and remote peers on
        // connections still open from before LAN mode went off.
        if !local_host {
            return ApiError::forbidden(format!("host '{host}' not allowed")).into_response();
        }
        if !is_loopback(peer) {
            return ApiError::forbidden("LAN mode is off").into_response();
        }
        return next.run(req).await;
    }
    // A loopback peer is the host machine only under a loopback Host name: a page that
    // DNS-rebinds its own name to 127.0.0.1 gets the token check like any other device.
    if (is_loopback(peer) && local_host) || is_public_path(req.uri().path()) {
        return next.run(req).await;
    }
    let tok = token_from(req.headers(), req.uri().query());
    if let Some(t) = tok {
        let h = hash(&t);
        let ok = st
            .db
            .read_async(move |c| Ok(c.query_row("SELECT count(*) FROM paired_devices WHERE token_hash=?1", [&h], |r| r.get::<_, i64>(0))? > 0))
            .await
            .unwrap_or(false);
        if ok {
            return next.run(req).await;
        }
    }
    ApiError::unauthorized("pair this device in Settings > LAN").into_response()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/status", get(status))
        .route("/auth/lan", post(set_lan))
        .route("/auth/pairing", post(create_pairing))
        .route("/auth/pairing/qr.svg", get(qr))
        .route("/auth/pair", post(pair))
        .route("/auth/devices", get(devices))
        .route("/auth/devices/{id}", delete(revoke))
}

#[derive(Serialize)]
struct Status {
    lan: bool,
    /// Started with `--lan` / `BC_LAN`: LAN mode is on again at the next start.
    forced: bool,
    local: bool,
    authenticated: bool,
    bind: String,
}

async fn status(State(st): State<AppState>, req: Request) -> Json<Status> {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let local = is_loopback(peer) && req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).is_none_or(host_is_local);
    let mut authenticated = local || !st.net.lan();
    if !authenticated
        && let Some(t) = token_from(req.headers(), req.uri().query()) {
            let h = hash(&t);
            authenticated = st
                .db
                .read_async(move |c| Ok(c.query_row("SELECT count(*) FROM paired_devices WHERE token_hash=?1", [&h], |r| r.get::<_, i64>(0))? > 0))
                .await
                .unwrap_or(false);
        }
    Json(status_of(&st, local, authenticated))
}

fn status_of(st: &AppState, local: bool, authenticated: bool) -> Status {
    let lan = st.net.lan();
    Status { lan, forced: st.net.forced(), local, authenticated, bind: format!("{}:{}", crate::net::host(lan), st.config.port) }
}

#[derive(Deserialize)]
struct LanIn {
    on: bool,
}

/// Switch LAN mode from Settings. Host machine only; the JSON body (a cross-site form cannot
/// send one without a CORS preflight, which this server never grants) and the Origin check
/// keep other web pages from flipping it.
async fn set_lan(State(st): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, Json(body): Json<LanIn>) -> ApiResult<Json<Status>> {
    require_local(Some(peer))?;
    if let Some(o) = headers.get(header::ORIGIN) {
        let origin = o.to_str().unwrap_or("");
        let host = origin.split_once("://").map(|(_, h)| h).unwrap_or("");
        if host.is_empty() || !host_is_local(host) {
            return Err(ApiError::forbidden(format!("origin '{origin}' not allowed")));
        }
    }
    st.net.set_lan(body.on).await.map_err(|e| ApiError::new(500, "Internal Server Error").detail(format!("cannot switch LAN mode: {e}")))?;
    let v = if body.on { "true" } else { "false" };
    st.db.write_async(move |t| Ok(bc_db::settings::set(t, crate::net::SETTING_KEY, v)?)).await?;
    Ok(Json(status_of(&st, true, true)))
}

fn require_local(peer: Option<SocketAddr>) -> ApiResult<()> {
    if is_loopback(peer) { Ok(()) } else { Err(ApiError::forbidden("only the host machine may manage pairing")) }
}

#[derive(Serialize)]
struct PairingOut {
    code: String,
    expires_in_s: u64,
    lan_ip: Option<String>,
    url: Option<String>,
}

/// Outbound-interface address, found without sending a packet.
fn local_ip() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

async fn create_pairing(State(st): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> ApiResult<Json<PairingOut>> {
    require_local(Some(peer))?;
    if !st.net.lan() {
        return Err(ApiError::new(409, "Conflict").detail("LAN mode is off (turn it on in Settings > LAN)"));
    }
    let code = st.pairing.issue();
    let ip = local_ip();
    let url = ip.map(|ip| format!("http://{}:{}/?pair={}", ip, st.config.port, code));
    Ok(Json(PairingOut { code, expires_in_s: CODE_TTL.as_secs(), lan_ip: ip.map(|i| i.to_string()), url }))
}

#[derive(Deserialize)]
struct QrQuery {
    data: String,
}

async fn qr(ConnectInfo(peer): ConnectInfo<SocketAddr>, Query(q): Query<QrQuery>) -> ApiResult<Response> {
    require_local(Some(peer))?;
    let code = qrcode::QrCode::new(q.data.as_bytes()).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let svg = code.render::<qrcode::render::svg::Color>().min_dimensions(200, 200).build();
    let mut r = svg.into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"));
    Ok(r)
}

#[derive(Deserialize)]
struct PairIn {
    code: String,
    #[serde(default)]
    name: String,
}

#[derive(Serialize)]
struct PairOut {
    token: String,
    device_id: i64,
}

async fn pair(State(st): State<AppState>, Json(body): Json<PairIn>) -> ApiResult<Response> {
    if !st.net.lan() {
        return Err(ApiError::new(409, "Conflict").detail("LAN mode is off"));
    }
    if !st.pairing.redeem(&body.code) {
        return Err(ApiError::unauthorized("invalid or expired pairing code"));
    }
    let token = new_token();
    let h = hash(&token);
    let name = if body.name.trim().is_empty() { "device".to_string() } else { body.name.trim().chars().take(60).collect() };
    let id = st
        .db
        .write_async(move |t| {
            t.execute("INSERT INTO paired_devices(name, token_hash) VALUES (?1, ?2)", [&name, &h])?;
            Ok(t.last_insert_rowid())
        })
        .await?;
    let mut resp = Json(PairOut { token: token.clone(), device_id: id }).into_response();
    let cookie = format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000");
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    Ok(resp)
}

#[derive(Serialize)]
struct Device {
    id: i64,
    name: String,
    created_at: String,
}

async fn devices(State(st): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> ApiResult<Json<Vec<Device>>> {
    require_local(Some(peer))?;
    let v = st
        .db
        .read_async(|c| {
            let mut s = c.prepare("SELECT id, name, created_at FROM paired_devices ORDER BY id")?;
            let rows = s.query_map([], |r| Ok(Device { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? }))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    Ok(Json(v))
}

async fn revoke(State(st): State<AppState>, ConnectInfo(peer): ConnectInfo<SocketAddr>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    require_local(Some(peer))?;
    st.db.write_async(move |t| { t.execute("DELETE FROM paired_devices WHERE id=?1", [id])?; Ok(()) }).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[allow(dead_code)]
fn _unused(_: Body) {}
