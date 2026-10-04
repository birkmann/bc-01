use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use bc_core::{Config, EventBus};
use bc_db::Db;

/// Sink for `ClientMsg::Player` commands (adapter over `PlayerService::handle_command`).
/// The reply (`{"ok":true}`, a `DevicesInfo`, or a problem) is sent back to the client.
pub trait CommandSink: Send + Sync {
    fn handle_command(&self, command: serde_json::Value) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, bc_types::Problem>> + Send>>;
}

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub bus: Arc<EventBus>,
    pub config: Arc<Config>,
    pub opts: Arc<ServerOptions>,
    pub player: Option<Arc<dyn CommandSink>>,
    pub pairing: Arc<crate::auth::PairingStore>,
    /// Live LAN mode; `config.lan` is only the value it started with.
    pub net: Arc<crate::net::Net>,
    pub services: Arc<crate::services::Services>,
}

#[derive(Debug, Clone, Default)]
pub struct ServerOptions {
    /// Serve the UI from this directory instead of the embedded bundle (dev, tests).
    pub ui_dir: Option<PathBuf>,
    /// Disable embedded UI entirely (API only).
    pub no_ui: bool,
    /// Do not construct the domain services (tests of the plain server).
    pub no_services: bool,
    /// `Cross-Origin-Embedder-Policy` value (`credentialless` default, or `require-corp`).
    pub coep: Option<String>,
    /// Set by the desktop app: it answers `desktop.bandcamp_login` requests with a sign-in window.
    pub bandcamp_login: bool,
}

impl ServerOptions {
    /// `BC_UI_DIR` serves the UI from disk (dev), `BC_COEP=require-corp` switches the COEP value.
    pub fn from_env() -> Self {
        Self {
            ui_dir: std::env::var_os("BC_UI_DIR").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty()),
            coep: std::env::var("BC_COEP").ok().filter(|v| v == "require-corp" || v == "credentialless"),
            ..Default::default()
        }
    }
}
