//! axum routers. One file per legacy route module; each exposes
//! `pub fn router(ctx: Arc<Ctx>) -> Router` (state applied, paths WITHOUT `/api`).

use std::sync::Arc;

use axum::Router;

use crate::service::Ctx;

pub mod downloads;
pub mod explore;
pub mod fans;
pub mod follows;
pub mod harvest;
pub mod tracklists;

/// Entity names used in `invalidate` events (stable; WS5's cache keys on them):
/// `job`, `inbox_item`, `fan`, `follow`, `release`. An empty id list = every entity of that kind.
pub const ENTITY_INBOX_ITEM: &str = "inbox_item";
pub const ENTITY_FAN: &str = "fan";
pub const ENTITY_FOLLOW: &str = "follow";

/// After any successful mutating request on `router`'s routes whose path starts with `prefix`,
/// publish `invalidate(entity, [])`.
fn invalidating(router: Router, ctx: &Arc<Ctx>, entity: &'static str, prefix: &'static str) -> Router {
    let bus = ctx.bus.clone();
    router.layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
        let bus = bus.clone();
        async move {
            let mutating = req.method() != axum::http::Method::GET && req.uri().path().starts_with(prefix);
            let resp = next.run(req).await;
            if mutating && resp.status().is_success() {
                bus.invalidate(entity, vec![]);
            }
            resp
        }
    }))
}

/// Build every WS2 router and merge them.
pub fn router(ctx: &Arc<Ctx>) -> Router {
    Router::new()
        .merge(downloads::router(ctx.clone()))
        .merge(invalidating(harvest::router(ctx.clone()), ctx, ENTITY_INBOX_ITEM, "/harvest/items"))
        .merge(invalidating(fans::router(ctx.clone()), ctx, ENTITY_FAN, "/fans"))
        .merge(invalidating(follows::router(ctx.clone()), ctx, ENTITY_FOLLOW, "/follows"))
        .merge(explore::router(ctx.clone()))
        .merge(tracklists::router(ctx.clone()))
}

/// Per-router service construction (called from `BandcampService::new`).
pub fn init(ctx: &Arc<Ctx>) {
    explore::init(ctx);
}
