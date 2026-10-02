//! What the desktop app (`bc-desktop`) adds around the server. The sign-in window for Bandcamp
//! lives in the desktop process: this route only asks for it over the event bus, and the desktop
//! app reports progress on [`TOPIC_BANDCAMP_LOGIN`](bc_types::bandcamp::TOPIC_BANDCAMP_LOGIN).
//! Only the host machine gets it: a paired phone would open a window nobody is looking at.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use bc_types::bandcamp::{DesktopInfo, TOPIC_BANDCAMP_LOGIN_REQUEST};

use crate::auth::is_loopback;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/desktop", get(info)).route("/desktop/bandcamp-login", post(bandcamp_login))
}

fn peer(req: &Request) -> Option<SocketAddr> {
    req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0)
}

async fn info(State(st): State<AppState>, req: Request) -> Json<DesktopInfo> {
    Json(DesktopInfo { bandcamp_login: st.opts.bandcamp_login && is_loopback(peer(&req)) })
}

async fn bandcamp_login(State(st): State<AppState>, req: Request) -> ApiResult<StatusCode> {
    if !st.opts.bandcamp_login || !is_loopback(peer(&req)) {
        return Err(ApiError::not_found("signing in through a window needs the bc desktop app"));
    }
    st.bus.publish(TOPIC_BANDCAMP_LOGIN_REQUEST, &serde_json::json!({}));
    Ok(StatusCode::ACCEPTED)
}
