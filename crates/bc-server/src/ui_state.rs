//! Server-side UI state (themes, column prefs, plan store, scratch pool, ...).
//! `ui_state(key, value JSON text, updated_at)`. The table is created here with
//! IF NOT EXISTS until WS1 adds it to the migrations.
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::Value;

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub const DDL: &str = "CREATE TABLE IF NOT EXISTS ui_state (
    key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)";

pub fn router() -> Router<AppState> {
    Router::new().route("/ui-state/{key}", get(get_one).put(put_one).delete(del_one))
}

fn valid_key(k: &str) -> bool {
    !k.is_empty() && k.len() <= 128 && k.chars().all(|c| c.is_ascii_alphanumeric() || "._-:".contains(c))
}

async fn get_one(State(st): State<AppState>, Path(key): Path<String>) -> ApiResult<Json<Value>> {
    if !valid_key(&key) {
        return Err(ApiError::bad_request("invalid key"));
    }
    let k = key.clone();
    let v = st
        .db
        .read_async(move |c| {
            use bc_db::rusqlite::OptionalExtension;
            Ok(c.query_row("SELECT value FROM ui_state WHERE key=?1", [&k], |r| r.get::<_, String>(0)).optional()?)
        })
        .await?;
    match v {
        Some(s) => Ok(Json(serde_json::from_str(&s).unwrap_or(Value::Null))),
        // Missing keys answer `null` (200) instead of 404: a first-run page load must not log a console error.
        None => Ok(Json(Value::Null)),
    }
}

async fn put_one(State(st): State<AppState>, Path(key): Path<String>, Json(body): Json<Value>) -> ApiResult<StatusCode> {
    if !valid_key(&key) {
        return Err(ApiError::bad_request("invalid key"));
    }
    let text = serde_json::to_string(&body).map_err(ApiError::internal)?;
    let k = key.clone();
    st.db
        .write_async(move |t| {
            t.execute(
                "INSERT INTO ui_state(key,value,updated_at) VALUES (?1,?2,CURRENT_TIMESTAMP)
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
                [&k, &text],
            )?;
            Ok(())
        })
        .await?;
    st.bus.publish("ui_state.changed", &serde_json::json!({ "key": key }));
    Ok(StatusCode::NO_CONTENT)
}

async fn del_one(State(st): State<AppState>, Path(key): Path<String>) -> ApiResult<StatusCode> {
    let k = key.clone();
    st.db.write_async(move |t| { t.execute("DELETE FROM ui_state WHERE key=?1", [&k])?; Ok(()) }).await?;
    st.bus.publish("ui_state.changed", &serde_json::json!({ "key": key }));
    Ok(StatusCode::NO_CONTENT)
}
