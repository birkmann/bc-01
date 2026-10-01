use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct Health {
    pub status: &'static str,
    pub version: &'static str,
}

#[derive(Serialize)]
pub struct Check {
    pub name: String,
    pub status: &'static str, // ok | missing | error
    pub required: bool,
    pub detail: Option<String>,
}

#[derive(Serialize)]
pub struct Doctor {
    pub status: &'static str, // ok | degraded | unhealthy
    pub checks: Vec<Check>,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/health", get(health)).route("/doctor", get(doctor))
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok", version: env!("CARGO_PKG_VERSION") })
}

fn bin_check(name: &str, bin: &str, required: bool) -> Check {
    match which::which(bin) {
        Ok(p) => Check { name: name.into(), status: "ok", required, detail: Some(p.display().to_string()) },
        Err(_) => Check { name: name.into(), status: "missing", required, detail: Some(format!("`{bin}` not on PATH")) },
    }
}

async fn doctor(State(st): State<AppState>) -> Json<Doctor> {
    let mut checks = vec![
        bin_check("ffmpeg", &st.config.ffmpeg_bin, false),
        bin_check("bandcamp-dl", &st.config.bandcamp_dl_bin, false),
    ];
    let fts = st
        .db
        .read_async(|c| {
            Ok(c.query_row("SELECT sqlite_compileoption_used('ENABLE_FTS5')", [], |r| r.get::<_, i64>(0))? == 1)
        })
        .await;
    checks.push(match fts {
        Ok(true) => Check { name: "sqlite-fts5".into(), status: "ok", required: true, detail: None },
        Ok(false) => Check { name: "sqlite-fts5".into(), status: "missing", required: true, detail: None },
        Err(e) => Check { name: "sqlite-fts5".into(), status: "error", required: true, detail: Some(e.to_string()) },
    });
    let dir = &st.config.data_dir;
    let writable = dir.is_dir()
        && std::fs::metadata(dir).map(|m| !m.permissions().readonly()).unwrap_or(false);
    checks.push(Check {
        name: "data-dir".into(),
        status: if writable { "ok" } else { "missing" },
        required: true,
        detail: Some(dir.display().to_string()),
    });
    checks.push(Check { name: "runtime".into(), status: "ok", required: false, detail: Some(format!("bc-server {}", env!("CARGO_PKG_VERSION"))) });
    let status = if checks.iter().any(|c| c.required && c.status != "ok") {
        "unhealthy"
    } else if checks.iter().any(|c| c.status != "ok") {
        "degraded"
    } else {
        "ok"
    };
    Json(Doctor { status, checks })
}
