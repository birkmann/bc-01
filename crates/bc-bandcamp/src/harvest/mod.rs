//! Harvest services (inbox, fan walker, feed sweeper, label/favourites sweeps, tag enricher,
//! label resolver, relink, artist/label locate). Each submodule exposes
//! `pub fn init(ctx: &Arc<Ctx>)` (construct + `ctx.put`) and `pub async fn start(ctx: &Arc<Ctx>)`
//! (spawn background workers); this file fans out to them.

use std::sync::Arc;

use crate::service::Ctx;

pub mod artists;
pub mod enrich;
pub mod fans;
pub mod feed;
pub mod inbox;
pub mod labels;
pub mod relink;
pub mod runs;
pub mod sweep;

pub fn init(ctx: &Arc<Ctx>) {
    inbox::init(ctx);
    labels::init(ctx);
    relink::init(ctx);
    sweep::init(ctx);
    runs::init(ctx);
    enrich::init(ctx);
    artists::init(ctx);
    fans::init(ctx);
    feed::init(ctx);
}

pub async fn start(ctx: &Arc<Ctx>) {
    inbox::start(ctx).await;
    labels::start(ctx).await;
    relink::start(ctx).await;
    sweep::start(ctx).await;
    runs::start(ctx).await;
    enrich::start(ctx).await;
    artists::start(ctx).await;
    fans::start(ctx).await;
    feed::start(ctx).await;
    // The feed sweep pins free artist/label page URLs before it walks (cheap, no network).
    ctx.expect::<feed::FeedSweeper>().set_backfill(Arc::new(|c: &bc_db::rusqlite::Connection| {
        let _ = labels::backfill_label_urls(c);
        let _ = artists::backfill_artist_urls(c);
    }));
}
