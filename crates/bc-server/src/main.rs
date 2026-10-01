//! `bc-server`: standalone server (the `bc serve` equivalent without the CLI crate).
//! Env: BC_DATA_DIR, BC_PORT (default 8420), BC_LAN=1 for LAN mode, RUST_LOG.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    bc_server::run(bc_core::Config::from_env()).await
}
