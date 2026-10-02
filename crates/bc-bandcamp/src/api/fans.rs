//! Wishlists: mine and other people's -- follow, walk, play through, download from (port of
//! `api/routes/fans.py`).
//!
//! The self fan is the old "saved wishlist" (Settings > Wishlist backfill); every other fan is
//! someone else's list. Items come back through `/harvest/items` with `fan_id=` (the inbox is
//! the one store of releases seen on Bandcamp), and `/fans/{id}/next` is what the player's
//! continuation asks for: the next records of a list in its own order, or in a seeded shuffle
//! order.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use bc_jobs::ApiError;
use bc_types::bandcamp::{
    AddFanRequest, FanNextOut, FanOut, FanPeekOut, FanPeekPageOut, PeekItem, TOPIC_LIBRARY_CHANGED, WalkRequest,
};
use serde::Deserialize;

use crate::download::dedup::{self, NameLookup};
use crate::error::HarvestError;
use crate::extract::HarvestedRelease;
use crate::harvest::fans::{self, FanOrder, FanTab, FanWalker, dbr};
use crate::service::Ctx;
use crate::sources;

type ApiResult<T> = Result<T, ApiError>;

fn walker(ctx: &Ctx) -> Arc<FanWalker> {
    ctx.expect::<FanWalker>()
}

fn parse_body<T: serde::de::DeserializeOwned + Default>(b: &Bytes) -> ApiResult<T> {
    if b.iter().all(|c| c.is_ascii_whitespace()) {
        return Ok(T::default());
    }
    serde_json::from_slice(b).map_err(|e| ApiError::bad_request(format!("invalid request body: {e}")))
}

/// Raw coercion errors are the user's: a bad link is a 400, not a 500.
fn coerce(url: &str) -> ApiResult<String> {
    fans::coerce_fan_url(url).map_err(|e| ApiError::bad_request(e.to_string()))
}

async fn fan_out_for(ctx: &Arc<Ctx>, fan_id: i64) -> ApiResult<FanOut> {
    let w = walker(ctx);
    let out = dbr(&ctx.db, move |c| {
        let Some(fan) = fans::get_fan(c, fan_id)? else { return Ok(None) };
        Ok(Some(fans::fan_out(c, &fan, &w, None, None)?))
    })
    .await?;
    out.ok_or_else(|| ApiError::not_found("no such wishlist"))
}

/// The signed-in account becomes the self fan ("me"), so Home and Fans can open its collection
/// and wishlist straight away. Called after every successful sign-in check; a fan page is only
/// fetched while the account is not linked yet. Nothing is walked or queued. Signing in as
/// someone else moves "me" to that account.
pub(crate) async fn link_self(ctx: &Arc<Ctx>, who: &sources::Whoami) -> Result<(), HarvestError> {
    let Some(url) = who.url.clone().or_else(|| who.username.as_ref().map(|u| format!("https://bandcamp.com/{u}"))) else {
        return Ok(());
    };
    let canonical = fans::coerce_fan_url(&url)?;
    let username = fans::username_from_url(&canonical);
    if dbr(&ctx.db, move |c| Ok(fans::self_fan(c)?.is_some_and(|f| f.username == username))).await? {
        return Ok(());
    }
    // The counts on the Home cards come from the fan page; without them the cards just say less.
    let probe = walker(ctx).source().probe_fan(&canonical).await.ok();
    fans::dbw(&ctx.db, move |tx| {
        let fan = fans::create_fan(tx, &canonical, true, probe.as_ref())?;
        tx.execute("UPDATE fans SET is_self = (id = ?1)", [fan.id])?;
        Ok(())
    })
    .await?;
    ctx.bus.invalidate("fan", vec![]);
    Ok(())
}

async fn require_fan(ctx: &Arc<Ctx>, fan_id: i64) -> ApiResult<fans::FanRow> {
    dbr(&ctx.db, move |c| fans::get_fan(c, fan_id))
        .await?
        .ok_or_else(|| ApiError::not_found("no such wishlist"))
}

// -- routes ----------------------------------------------------------------

async fn list_fans(State(ctx): State<Arc<Ctx>>) -> ApiResult<Json<Vec<FanOut>>> {
    let w = walker(&ctx);
    let out = dbr(&ctx.db, move |c| {
        let rows = fans::list_fans(c)?;
        let totals = fans::fan_item_totals(c)?;
        let mut st = c.prepare(
            "SELECT source_fan_id, COUNT(id) FROM releases WHERE source_fan_id IS NOT NULL GROUP BY source_fan_id",
        )?;
        let shelves: HashMap<i64, i64> = st
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<bc_db::rusqlite::Result<_>>()?;
        rows.iter().map(|f| fans::fan_out(c, f, &w, Some(&totals), Some(&shelves))).collect::<Result<Vec<_>, _>>()
    })
    .await?;
    Ok(Json(out))
}

/// Probe the fan page first so an unreachable or private profile is reported now rather than by
/// a walk that quietly finds nothing; then start the walk (202: it runs in the background,
/// progress on `fans.walk`).
async fn add_fan(State(ctx): State<Arc<Ctx>>, Json(body): Json<AddFanRequest>) -> ApiResult<(StatusCode, Json<FanOut>)> {
    let canonical = coerce(&body.url)?;
    let w = walker(&ctx);
    let probe = w.source().probe_fan(&canonical).await.map_err(|e| match e {
        HarvestError::IdentityExpired(m) => ApiError::unauthorized(m),
        other => ApiError::bad_request(format!("could not open that fan page: {other}")),
    })?;
    let url = canonical.clone();
    let fan = fans::dbw(&ctx.db, move |tx| fans::create_fan(tx, &url, false, Some(&probe))).await?;
    if body.walk {
        w.request(fan.id, None, None).await?;
    }
    Ok((StatusCode::ACCEPTED, Json(fan_out_for(&ctx, fan.id).await?)))
}

fn peek_items(c: &bc_db::rusqlite::Connection, rows: &[HarvestedRelease]) -> Result<Vec<PeekItem>, HarvestError> {
    // Harvested rows as peek cards, with the ones the library already holds marked -- the one
    // thing a stranger's list is read for.
    let urls: Vec<String> = rows.iter().map(|r| r.url.clone()).collect();
    let names: NameLookup = rows.iter().map(|r| (r.url.clone(), (r.artist_name.clone(), r.title.clone()))).collect();
    let known = dedup::find_known(c, &urls, Some(&names)).map_err(|e| HarvestError::other(e.to_string()))?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        if !seen.insert(r.url.clone()) {
            continue;
        }
        let reason = known.get(&dedup::url_key(&r.url));
        out.push(PeekItem {
            url: r.url.clone(),
            title: r.title.clone(),
            artist_name: r.artist_name.clone(),
            art_url: r.art_url.clone(),
            item_type: r.item_type.clone(),
            in_library: reason.is_some_and(|x| x != "blacklist"),
        });
    }
    Ok(out)
}

#[derive(Deserialize)]
struct PeekQuery {
    url: String,
}

/// One fetch of the fan page, nothing written: who they are, how much they own and wish for,
/// and the first screenful of each -- enough to decide whether their taste is worth following.
async fn peek(State(ctx): State<Arc<Ctx>>, Query(q): Query<PeekQuery>) -> ApiResult<Json<FanPeekOut>> {
    let canonical = coerce(&q.url)?;
    let (page, collection, wishlist) = sources::peek_fan(&ctx.client, &canonical).await.map_err(|e| match e {
        HarvestError::IdentityExpired(m) => ApiError::unauthorized(m),
        other => ApiError::bad_request(format!("could not open that fan page: {other}")),
    })?;
    let username = if page.username.is_empty() { fans::username_from_url(&canonical) } else { page.username.clone() };
    let (c2, w2, name, bc) = (collection.clone(), wishlist.clone(), username.clone(), page.fan_id);
    let (items_c, items_w, followed) = dbr(&ctx.db, move |c| {
        let mut followed = fans::fan_by_username(c, &name)?;
        if followed.is_none() && bc != 0 {
            followed = fans::fan_by_bc_id(c, bc)?;
        }
        Ok((peek_items(c, &c2)?, peek_items(c, &w2)?, followed.map(|f| f.id)))
    })
    .await?;
    // A record on both lists is one card by URL; each list keeps its own order.
    let pick = |rows: &[HarvestedRelease], items: &[PeekItem]| -> Vec<PeekItem> {
        let by: HashMap<&str, &PeekItem> = items.iter().map(|i| (i.url.as_str(), i)).collect();
        rows.iter().filter_map(|r| by.get(r.url.as_str()).map(|i| (*i).clone())).collect()
    };
    Ok(Json(FanPeekOut {
        url: canonical,
        display_name: if page.display_name.is_empty() { username.clone() } else { page.display_name.clone() },
        username,
        bc_fan_id: (page.fan_id != 0).then_some(page.fan_id),
        wishlist_count: page.wishlist_count,
        collection_count: page.collection_count,
        followed_id: followed,
        collection: pick(&collection, &items_c),
        wishlist: pick(&wishlist, &items_w),
    }))
}

#[derive(Deserialize)]
struct PeekItemsQuery {
    url: String,
    which: Option<String>,
    cursor: Option<String>,
    fan_id: Option<i64>,
    count: Option<usize>,
}

/// Read further down a fan's list without following them. The first page comes free off the fan
/// page itself; each one after it is a single request to Bandcamp. Nothing is written down.
async fn peek_items_route(State(ctx): State<Arc<Ctx>>, Query(q): Query<PeekItemsQuery>) -> ApiResult<Json<FanPeekPageOut>> {
    let canonical = coerce(&q.url)?;
    let which = q.which.unwrap_or_else(|| "collection".into());
    if which != "collection" && which != "wishlist" {
        return Err(ApiError::bad_request("which must be collection or wishlist"));
    }
    let count = q.count.unwrap_or(sources::PEEK_PAGE);
    if !(1..=100).contains(&count) {
        return Err(ApiError::bad_request("count must be 1..100"));
    }
    let page = match sources::peek_fan_items(&ctx.client, &canonical, &which, q.fan_id, q.cursor.as_deref(), count).await {
        Ok(p) => p,
        Err(HarvestError::IdentityExpired(m)) => return Err(ApiError::unauthorized(m)),
        // A list Bandcamp will not show (a private wishlist) is empty as far as any reader here
        // is concerned -- not a bad request.
        Err(HarvestError::ListUnavailable(_)) => {
            return Ok(Json(FanPeekPageOut { items: vec![], cursor: None, more: false, total: Some(0) }));
        }
        Err(other) => return Err(ApiError::bad_request(format!("could not read that list: {other}"))),
    };
    let rows = page.rows.clone();
    let items = dbr(&ctx.db, move |c| peek_items(c, &rows)).await?;
    Ok(Json(FanPeekPageOut {
        items,
        cursor: if page.more { page.cursor } else { None },
        more: page.more,
        total: page.total,
    }))
}

async fn get_fan(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>) -> ApiResult<Json<FanOut>> {
    Ok(Json(fan_out_for(&ctx, id).await?))
}

#[derive(Deserialize)]
struct DeleteQuery {
    /// What to do with releases downloaded onto this fan's shelf: `adopt` moves them into my
    /// library first. Without it the request is refused while any remain, so forgetting a
    /// wishlist can never silently merge its records into mine.
    releases: Option<String>,
}

async fn delete_fan(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>, Query(q): Query<DeleteQuery>) -> ApiResult<StatusCode> {
    let adopt = match q.releases.as_deref() {
        None | Some("") => false,
        Some("adopt") => true,
        Some(_) => return Err(ApiError::bad_request("releases must be `adopt`")),
    };
    require_fan(&ctx, id).await?;
    walker(&ctx).stop(Some(id)).await;
    let verdict = ctx
        .db
        .write_async(move |tx| {
            let other = |e: bc_libcore::ApiError| bc_db::DbError::Other(e.to_string());
            let held = bc_maint::adopt::count_for_fan(tx, id).map_err(other)?;
            if held > 0 && !adopt {
                return Ok(Err(held));
            }
            if held > 0 {
                bc_maint::adopt::adopt_fan(tx, id).map_err(other)?;
            }
            tx.execute("DELETE FROM fans WHERE id = ?1", [id])?;
            Ok(Ok(()))
        })
        .await?;
    if let Err(held) = verdict {
        return Err(ApiError::conflict(format!(
            "{held} release(s) downloaded from this wishlist are still on its shelf. Move them into your library or delete them first."
        )));
    }
    ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &serde_json::json!({"fan_deleted": id}));
    Ok(StatusCode::NO_CONTENT)
}

/// Returns as soon as the walk is queued. Walks run one at a time, in the order asked; progress
/// arrives on the event bus as `fans.walk`.
async fn walk_fan(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>, body: Bytes) -> ApiResult<(StatusCode, Json<FanOut>)> {
    let req: WalkRequest = parse_body(&body)?;
    if let Some(tabs) = &req.tabs {
        if tabs.iter().any(|t| t != "wishlist" && t != "collection") {
            return Err(ApiError::bad_request("tabs must be wishlist and/or collection"));
        }
    }
    require_fan(&ctx, id).await?;
    walker(&ctx).request(id, req.queue_new, req.tabs.as_deref()).await?;
    Ok((StatusCode::ACCEPTED, Json(fan_out_for(&ctx, id).await?)))
}

async fn stop_walk(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>) -> ApiResult<Json<FanOut>> {
    require_fan(&ctx, id).await?;
    walker(&ctx).stop(Some(id)).await;
    Ok(Json(fan_out_for(&ctx, id).await?))
}

/// `?after=&order=seq|shuffle&seed=&state=&tab=&limit=` (`state` repeats).
async fn next_items(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>, RawQuery(raw): RawQuery) -> ApiResult<Json<FanNextOut>> {
    let mut after = None;
    let mut order = FanOrder::Seq;
    let mut seed = 0u32;
    let mut states: Vec<String> = Vec::new();
    let mut tab = FanTab::All;
    let mut limit = 1usize;
    for (k, v) in url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        let bad = |what: &str| ApiError::bad_request(format!("invalid {what}"));
        match k.as_ref() {
            "after" if !v.is_empty() => after = Some(v.parse::<i64>().map_err(|_| bad("after"))?),
            "order" => {
                order = match v.as_ref() {
                    "seq" => FanOrder::Seq,
                    "shuffle" => FanOrder::Shuffle,
                    _ => return Err(bad("order")),
                }
            }
            "seed" => {
                let s: i64 = v.parse().map_err(|_| bad("seed"))?;
                if !(0..=i64::from(i32::MAX)).contains(&s) {
                    return Err(bad("seed"));
                }
                seed = s as u32;
            }
            "state" => states.push(v.into_owned()),
            "tab" => {
                tab = match v.as_ref() {
                    "wishlist" => FanTab::Wishlist,
                    "collection" => FanTab::Collection,
                    "all" => FanTab::All,
                    _ => return Err(bad("tab")),
                }
            }
            "limit" => {
                let l: usize = v.parse().map_err(|_| bad("limit"))?;
                if !(1..=50).contains(&l) {
                    return Err(bad("limit"));
                }
                limit = l;
            }
            _ => {}
        }
    }
    require_fan(&ctx, id).await?;
    Ok(Json(fans::fan_next(&ctx, id, after, order, seed, &states, tab, limit).await?))
}

pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/fans", get(list_fans).post(add_fan))
        .route("/fans/peek", get(peek))
        .route("/fans/peek/items", get(peek_items_route))
        .route("/fans/{id}", get(get_fan).delete(delete_fan))
        .route("/fans/{id}/walk", axum::routing::post(walk_fan).merge(delete(stop_walk)))
        .route("/fans/{id}/next", get(next_items))
        .with_state(ctx)
}
