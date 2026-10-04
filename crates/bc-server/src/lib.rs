//! bc-server: axum app, WS hub, embedded UI, LAN pairing (PLAN §3.2, §9.1).
//!
//! `build_app(config)` constructs the Db + EventBus and nests every domain
//! service router under `/api`. Domain crates are mounted in [`services`] as
//! they publish their `XService`.

pub mod auth;
pub mod desktop;
pub mod error;
pub mod health;
pub mod net;
pub mod services;
pub mod state;
pub mod static_ui;
pub mod ui_state;
pub mod ws;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use bc_core::{Config, EventBus};
use bc_db::Db;
use tower_http::compression::CompressionLayer;
use tower_http::set_header::SetResponseHeaderLayer;

pub use error::{ApiError, ApiResult};
pub use state::{AppState, CommandSink, ServerOptions};

/// Build the whole application router (opens the DB, creates services).
pub fn build_app(config: Config) -> anyhow::Result<Router> {
    build_app_with(config, ServerOptions::from_env())
}

pub fn build_app_with(config: Config, opts: ServerOptions) -> anyhow::Result<Router> {
    Ok(build_router(build_state(config, opts)?))
}

pub fn build_state(config: Config, opts: ServerOptions) -> anyhow::Result<AppState> {
    config.ensure_dirs()?;
    let db = Db::open(config.db_path())?;
    db.write(|t| {
        t.execute_batch(ui_state::DDL)?;
        t.execute_batch(auth::DDL)?;
        Ok(())
    })?;
    let bus = Arc::new(EventBus::new());
    let config = Arc::new(config);
    let services = if opts.no_services { services::Services::default() } else { services::Services::build(&db, &bus, &config) };
    #[allow(unused_mut)]
    let mut player: Option<Arc<dyn CommandSink>> = None;
    #[cfg(feature = "player")]
    if let Some(p) = &services.player {
        player = Some(Arc::new(services::PlayerSink(p.clone())));
    }
    let net = Arc::new(net::Net::new(config.lan));
    Ok(AppState {
        db,
        bus,
        config,
        opts: Arc::new(opts),
        player,
        pairing: Arc::new(auth::PairingStore::default()),
        net,
        services: Arc::new(services),
    })
}

pub fn build_router(state: AppState) -> Router {
    // Own routes carry AppState; service routers already have theirs applied. Merge as `Router<()>`.
    let own: Router = Router::new()
        .merge(health::router())
        .merge(ui_state::router())
        .merge(auth::router())
        .merge(desktop::router())
        .route("/ws", get(ws::upgrade))
        .with_state(state.clone());
    let api: Router = own
        .merge(services::routers(&state))
        .fallback(|| async { ApiError::not_found("no such API route").into_response() });

    let mut app = Router::new().nest("/api", api);
    if static_ui::has_ui(&state.opts) {
        let opts = state.opts.clone();
        app = app.fallback(move |req: Request<Body>| {
            let opts = opts.clone();
            async move { static_ui::serve(&opts, &req) }
        });
    } else {
        app = app.fallback(|| async { ApiError::not_found("no route").into_response() });
    }
    let coep = match state.opts.coep.as_deref() {
        Some("require-corp") => HeaderValue::from_static("require-corp"),
        _ => HeaderValue::from_static("credentialless"),
    };
    app.layer(middleware::from_fn_with_state(state, auth::guard))
        // SharedArrayBuffer for the worklet host. `credentialless` (not
        // `require-corp`) so cross-origin Bandcamp art keeps loading.
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-embedder-policy"),
            coep,
        ))
        .layer(CompressionLayer::new())
}

pub struct RunningServer {
    pub addr: SocketAddr,
    pub state: AppState,
    handle: tokio::task::JoinHandle<()>,
    shutdown: tokio::sync::oneshot::Sender<()>,
}

impl RunningServer {
    pub async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.handle.await;
    }
    /// Always the loopback address: stable across LAN switches, and browsers refuse `0.0.0.0`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }
}

/// Bind and serve in the background; `port: 0` picks a free port (desktop shell).
pub async fn start(mut config: Config, opts: ServerOptions) -> anyhow::Result<RunningServer> {
    // `bc_core::Config` only parses BC_LAN=true; the documented `BC_LAN=1` must work too.
    if matches!(std::env::var("BC_LAN"), Ok(v) if ["1", "true", "yes", "on"].contains(&v.trim().to_lowercase().as_str())) {
        config.lan = true;
    }
    let forced = config.lan;
    // LAN mode (switched in Settings, or forced) is the only way off loopback.
    config.lan = forced || net::saved(&config.db_path());
    let listener = net::bind(config.lan, config.port).await?;
    let addr = listener.local_addr()?;
    config.port = addr.port();
    config.host = net::host(config.lan).to_string();
    let state = build_state(config, opts)?;
    state.services.start().await;
    let app = build_router(state.clone());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let handle = net::spawn(state.net.clone(), listener, app, forced, rx);
    tracing::info!("bc-server listening on {addr}");
    Ok(RunningServer { addr, state, handle, shutdown: tx })
}

/// Serve until ctrl-c (the `bc serve` entry point).
pub async fn run(config: Config) -> anyhow::Result<()> {
    let srv = start(config, ServerOptions::from_env()).await?;
    println!("bc listening on {}", srv.url());
    tokio::signal::ctrl_c().await?;
    srv.stop().await;
    Ok(())
}
