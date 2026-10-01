//! Harvest routes (port of `api/routes/harvest.py`): resolve a source, run it into the inbox as
//! a job, list/queue/ignore inbox items, tag enrichment, label resolution, the label and
//! favourites sweeps, identity, health, and locate for artist/label pages.
//!
//! Everything long returns `202 Accepted {job_id}` (or, for the sweeps/enrich/resolver, `202`
//! plus their legacy status struct) and runs as a job; see `docs/api/ws2.md`.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use bc_db::rusqlite::{params_from_iter, types::Value as SqlValue};
use bc_db::util::name_key;
use bc_jobs::ApiError;
use bc_types::bandcamp::{
    CacheHealth, CookieRequest, EnrichRequest, EnrichState, HarvestHealth, HarvestItemOut, HarvestItemsQuery, IdentityStatus,
    LabelResolveStatus, LabelSweepRequest, LocateOut, QueueRequest, QueueResult, RateLimitHealth, ResolveRequest, ResolveResult, RunRequest,
    RunResult, SweepStatus, TagCount, TOPIC_LIBRARY_CHANGED,
};
use bc_types::{Accepted, Page};
use serde_json::Value;

use crate::error::HarvestError;
use crate::harvest::artists::locate_artist_page;
use crate::harvest::enrich::TagEnricher;
use crate::harvest::inbox::{self, HarvestRow, QueueOpts, ROW_COLS};
use crate::harvest::labels::{LabelResolver, LocateResult, locate_label_page};
use crate::harvest::runs::{RunLookup, RunService};
use crate::harvest::sweep::{SweepKind, SweepService, Sweeper};
use crate::identity;
use crate::net::Lane;
use crate::service::Ctx;
use crate::sources;
use crate::urls::{self, UrlKind};

type ApiResult<T> = Result<T, ApiError>;

pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/harvest/resolve", post(resolve))
        .route("/harvest/run", post(run))
        .route("/harvest/runs/{job_id}", get(run_result))
        .route("/harvest/items", get(list_items))
        .route("/harvest/stats", get(stats))
        .route("/harvest/tags", get(tag_counts))
        .route("/harvest/items/queue", post(queue_items))
        .route("/harvest/items/{id}/ignore", post(ignore_item))
        .route("/harvest/items/ignore", post(ignore_items))
        .route("/harvest/enrich", get(enrich_status).post(start_enrich).delete(stop_enrich))
        .route("/harvest/labels", get(get_label_resolution))
        .route("/harvest/labels/resolve", post(start_label_resolution))
        .route("/harvest/labels/sweep", get(get_label_sweep).post(start_label_sweep).delete(stop_label_sweep))
        .route("/harvest/favorites/sweep", get(get_favorites_sweep).post(start_favorites_sweep).delete(stop_favorites_sweep))
        .route("/harvest/identity", get(get_identity).put(put_identity).delete(delete_identity))
        .route("/harvest/health", get(health))
        .route("/artists/{id}/locate", post(locate_artist))
        .route("/labels/{id}/locate", post(locate_label))
        .with_state(ctx)
}

fn unprocessable(msg: impl Into<String>) -> ApiError {
    ApiError::new(422, "Unprocessable Entity").detail(msg)
}

fn de(e: HarvestError) -> bc_db::DbError {
    bc_db::DbError::Other(e.to_string())
}

// ---------------------------------------------------------------------------------------
// resolve / run
// ---------------------------------------------------------------------------------------

/// Work out what a pasted URL or handle actually addresses.
async fn resolve(State(ctx): State<Arc<Ctx>>, Json(body): Json<ResolveRequest>) -> ApiResult<Json<ResolveResult>> {
    let raw = urls::coerce(&body.input);
    if raw.is_empty() {
        return Err(ApiError::bad_request("nothing to resolve"));
    }

    // A multi-line paste is a URL list, not a single source.
    if raw.lines().count() > 1 {
        let parsed = raw
            .lines()
            .filter(|l| matches!(urls::classify(&urls::coerce(l)), UrlKind::Album | UrlKind::Track))
            .count() as i64;
        return Ok(Json(ResolveResult {
            kind: "url_list".into(),
            label: "pasted list".into(),
            total_hint: Some(parsed),
            auth_ok: true,
            detail: format!("{parsed} album/track URLs"),
            ..Default::default()
        }));
    }

    let kind = urls::classify(&raw);
    if matches!(kind, UrlKind::Album | UrlKind::Track) {
        return Ok(Json(ResolveResult {
            kind: "url_list".into(),
            label: urls::display_name(&raw),
            url: Some(urls::normalise(&raw)),
            total_hint: Some(1),
            auth_ok: true,
            detail: format!("single {}", kind.as_str()),
            ..Default::default()
        }));
    }

    let probe = match kind {
        UrlKind::Artist | UrlKind::Music | UrlKind::Artists => sources::probe_artist(&ctx.client, &raw).await,
        UrlKind::Fan => sources::probe_fan(&ctx.client, &urls::normalise(&raw)).await.map(|mut p| {
            // Paste the wishlist link and you get the wishlist.
            p.kind = urls::fan_tab(&raw).to_string();
            p
        }),
        UrlKind::Discover => {
            let tags = urls::discover_tags(&raw);
            let mut params = BTreeMap::new();
            params.insert("tags".to_string(), serde_json::json!(tags));
            return Ok(Json(ResolveResult {
                kind: "discover".into(),
                label: urls::display_name(&raw),
                url: Some(urls::normalise(&raw)),
                auth_ok: true,
                detail: if tags.is_empty() { "discover feed \u{2014} all genres".into() } else { format!("discover feed \u{2014} {}", tags.join(", ")) },
                params,
                ..Default::default()
            }));
        }
        _ => return Err(ApiError::bad_request(format!("could not classify: {raw}"))),
    };
    let probe = probe.map_err(|e| match e {
        HarvestError::IdentityExpired(_) => ApiError::from(e),
        e => ApiError::bad_request(format!("could not reach that source: {e}")),
    })?;

    Ok(Json(ResolveResult {
        kind: probe.kind,
        label: probe.label,
        url: Some(if kind != UrlKind::Fan { urls::artist_root(&raw) } else { urls::normalise(&raw) }),
        total_hint: probe.total_hint,
        requires_auth: probe.requires_auth,
        auth_ok: probe.auth_ok,
        detail: probe.detail,
        params: probe.params,
    }))
}

/// `POST /harvest/run`: the run is a `harvest` job; `202 {job_id}`.
async fn run(State(ctx): State<Arc<Ctx>>, Json(body): Json<RunRequest>) -> ApiResult<(StatusCode, Json<Accepted>)> {
    let job_id = ctx.expect::<RunService>().submit(body).await.map_err(|e| match e {
        HarvestError::Other(m) if m.starts_with("limit ") || m.starts_with("depth ") => unprocessable(m),
        e => e.into(),
    })?;
    Ok((StatusCode::ACCEPTED, Json(Accepted { job_id })))
}

/// `GET /harvest/runs/{job_id}`: the run's [`RunResult`] once finished (404 before).
async fn run_result(State(ctx): State<Arc<Ctx>>, Path(job_id): Path<String>) -> ApiResult<Json<RunResult>> {
    match ctx.expect::<RunService>().result(&job_id).await {
        RunLookup::Done(r) => Ok(Json(r)),
        RunLookup::Pending => Err(ApiError::not_found(format!("harvest run {job_id} has not finished"))),
        RunLookup::Missing => Err(ApiError::not_found(format!("harvest run {job_id} not found"))),
        RunLookup::Cancelled => Err(ApiError::conflict("harvest run was cancelled")),
        RunLookup::Failed { message, identity_expired: true } => Err(ApiError::unauthorized(message)),
        RunLookup::Failed { message, .. } => Err(ApiError::bad_request(message)),
    }
}

// ---------------------------------------------------------------------------------------
// the inbox
// ---------------------------------------------------------------------------------------

/// Raw query pairs: repeated keys (`tags=a&tags=b`) need more than `serde_urlencoded` offers.
fn pairs(raw: Option<String>) -> Vec<(String, String)> {
    url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
}

fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn parse_query(raw: Option<String>) -> ApiResult<HarvestItemsQuery> {
    let mut q = HarvestItemsQuery { state: Some("new".into()), ..Default::default() };
    for (k, v) in pairs(raw) {
        match k.as_str() {
            "state" => q.state = Some(v),
            "source_kind" => q.source_kind = Some(v),
            "source_label" => q.source_label = Some(v),
            "in_wishlist" => q.in_wishlist = Some(parse_bool(&v).ok_or_else(|| unprocessable("in_wishlist must be a boolean"))?),
            "fan_id" => q.fan_id = Some(v.parse().map_err(|_| unprocessable("fan_id must be an integer"))?),
            "tab" => q.tab = Some(v),
            "order" => q.order = Some(v),
            "q" => q.q = Some(v),
            "tags" => q.tags.push(v),
            "offset" => q.offset = v.parse().map_err(|_| unprocessable("offset must be an integer"))?,
            "limit" => q.limit = Some(v.parse().map_err(|_| unprocessable("limit must be an integer"))?),
            _ => {}
        }
    }
    Ok(q)
}

fn tab_filter(tab: Option<&str>) -> Option<&str> {
    tab.filter(|t| matches!(*t, "wishlist" | "collection"))
}

/// One fan's list(s) as a subquery of `(item_id, position)`, one row per item. A record on
/// both lists is still one row, at the earlier of its two positions, which is what keeps the
/// unified view and its counts honest.
fn members_sql(tab: Option<&str>, binds: &mut Vec<SqlValue>, fan_id: i64) -> String {
    binds.push(fan_id.into());
    let mut sql = "(SELECT item_id, MIN(position) AS position FROM fan_items WHERE fan_id = ?".to_string();
    if let Some(t) = tab {
        sql.push_str(" AND tab = ?");
        binds.push(t.to_string().into());
    }
    sql.push_str(" GROUP BY item_id)");
    sql
}

/// `GET /harvest/items`. `fan_id` narrows to one followed fan's items -- their wishlist, their
/// collection, or (`tab=all`, the default) both as one list -- and, unless told otherwise, lists
/// them in the list's own order (newest first).
async fn list_items(State(ctx): State<Arc<Ctx>>, RawQuery(raw): RawQuery) -> ApiResult<Json<Page<HarvestItemOut>>> {
    let q = parse_query(raw)?;
    let limit = q.limit.unwrap_or(100);
    if !(1..=500).contains(&limit) {
        return Err(unprocessable("limit must be between 1 and 500"));
    }
    if q.offset < 0 {
        return Err(unprocessable("offset must be >= 0"));
    }
    let offset = q.offset;
    let page = ctx
        .db
        .read_async(move |c| {
            let mut binds: Vec<SqlValue> = Vec::new();
            let mut from = "harvest_items hi".to_string();
            let members = q.fan_id.is_some();
            if let Some(fan_id) = q.fan_id {
                from.push_str(&format!(" JOIN {} m ON m.item_id = hi.id", members_sql(tab_filter(q.tab.as_deref()), &mut binds, fan_id)));
            }
            let mut wh: Vec<String> = Vec::new();
            if let Some(s) = q.state.as_deref().filter(|s| !s.is_empty() && *s != "all") {
                wh.push("hi.state = ?".into());
                binds.push(s.to_string().into());
            }
            if let Some(s) = q.source_kind.as_deref().filter(|s| !s.is_empty()) {
                wh.push("hi.source_kind = ?".into());
                binds.push(s.to_string().into());
            }
            if let Some(s) = q.source_label.as_deref().filter(|s| !s.is_empty()) {
                wh.push("hi.source_label = ?".into());
                binds.push(s.to_string().into());
            }
            if let Some(w) = q.in_wishlist {
                // The sticky flag, not source_kind: a later harvest of another source refreshes
                // source_kind, but the row is still on the wishlist.
                wh.push("hi.in_wishlist = ?".into());
                binds.push(i64::from(w).into());
            }
            // Every tag must be present -- picking a second tag narrows the feed, which is what
            // combining filters means everywhere else in the app.
            for tag in &q.tags {
                wh.push("EXISTS (SELECT 1 FROM harvest_item_tags t WHERE t.item_id = hi.id AND t.tag_key = ?)".into());
                binds.push(name_key(tag).into());
            }
            if let Some(text) = q.q.as_deref().filter(|s| !s.is_empty()) {
                let like = format!("%{}%", text.to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
                wh.push("(lower(hi.title) LIKE ? ESCAPE '\\' OR lower(hi.artist_name) LIKE ? ESCAPE '\\')".into());
                binds.push(like.clone().into());
                binds.push(like.into());
            }
            let where_sql = if wh.is_empty() { String::new() } else { format!(" WHERE {}", wh.join(" AND ")) };

            let total: i64 = c.query_row(&format!("SELECT COUNT(*) FROM {from}{where_sql}"), params_from_iter(binds.iter()), |r| r.get(0))?;
            let by_position = q.order.as_deref() == Some("position") || (q.order.is_none() && q.fan_id.is_some());
            let order = if by_position && members { "coalesce(m.position, 1000000000), hi.id" } else { "hi.discovered_at DESC, hi.id" };
            let cols = ROW_COLS.split(", ").map(|c| format!("hi.{}", c.trim())).collect::<Vec<_>>().join(", ");
            let pos = if members { "m.position" } else { "NULL" };
            let sql = format!("SELECT {cols}, {pos} FROM {from}{where_sql} ORDER BY {order} LIMIT {limit} OFFSET {offset}");
            let mut st = c.prepare(&sql)?;
            let rows: Vec<(HarvestRow, Option<i64>)> = st
                .query_map(params_from_iter(binds.iter()), |r| Ok((HarvestRow::from_row(r)?, r.get::<_, Option<i64>>(24)?)))?
                .collect::<Result<_, _>>()?;

            let mut on_lists: std::collections::HashMap<i64, Vec<String>> = Default::default();
            if let Some(fan_id) = q.fan_id {
                let mut ids: Vec<i64> = rows.iter().map(|(r, _)| r.id).collect();
                ids.sort_unstable();
                ids.dedup();
                for chunk in ids.chunks(400) {
                    let ph = vec!["?"; chunk.len()].join(",");
                    let mut st = c.prepare(&format!(
                        "SELECT item_id, tab FROM fan_items WHERE fan_id = ?1 AND item_id IN ({ph}) ORDER BY item_id, tab DESC"
                    ))?;
                    let mut b: Vec<SqlValue> = vec![fan_id.into()];
                    b.extend(chunk.iter().map(|i| SqlValue::from(*i)));
                    let it = st.query_map(params_from_iter(b.iter()), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
                    for r in it {
                        let (id, tab) = r?;
                        on_lists.entry(id).or_default().push(tab);
                    }
                }
            }
            let items = rows
                .into_iter()
                .map(|(row, position)| {
                    let tabs = on_lists.remove(&row.id).unwrap_or_default();
                    row.to_out(false, position, tabs)
                })
                .collect();
            Ok(Page { items, total, offset, limit })
        })
        .await?;
    Ok(Json(page))
}

/// Inbox counts by state.
async fn stats(State(ctx): State<Arc<Ctx>>) -> ApiResult<Json<BTreeMap<String, i64>>> {
    let m = ctx
        .db
        .read_async(|c| {
            let mut st = c.prepare("SELECT state, COUNT(id) FROM harvest_items GROUP BY state")?;
            Ok(st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?.collect::<Result<BTreeMap<_, _>, _>>()?)
        })
        .await?;
    Ok(Json(m))
}

/// What a tag filter can offer over one inbox slice, hottest first. Aggregated inside SQLite
/// from the normalised `harvest_item_tags` table (the follow slice alone runs to six figures);
/// the displayed spelling is `min(tag)` per folded key.
async fn tag_counts(State(ctx): State<Arc<Ctx>>, RawQuery(raw): RawQuery) -> ApiResult<Json<Vec<TagCount>>> {
    let (mut state, mut source_kind, mut source_label, mut fan_id, mut tab, mut limit) = (Some("new".to_string()), None, None, None, None, 40i64);
    for (k, v) in pairs(raw) {
        match k.as_str() {
            "state" => state = Some(v),
            "source_kind" => source_kind = Some(v),
            "source_label" => source_label = Some(v),
            "fan_id" => fan_id = Some(v.parse::<i64>().map_err(|_| unprocessable("fan_id must be an integer"))?),
            "tab" => tab = Some(v),
            "limit" => limit = v.parse().map_err(|_| unprocessable("limit must be an integer"))?,
            _ => {}
        }
    }
    if !(1..=200).contains(&limit) {
        return Err(unprocessable("limit must be between 1 and 200"));
    }
    let rows = ctx
        .db
        .read_async(move |c| {
            let mut wh = vec!["1 = 1".to_string()];
            let mut binds: Vec<SqlValue> = Vec::new();
            if let Some(s) = state.as_deref().filter(|s| !s.is_empty() && *s != "all") {
                wh.push("hi.state = ?".into());
                binds.push(s.to_string().into());
            }
            if let Some(s) = source_kind.filter(|s| !s.is_empty()) {
                wh.push("hi.source_kind = ?".into());
                binds.push(s.into());
            }
            if let Some(s) = source_label.filter(|s| !s.is_empty()) {
                wh.push("hi.source_label = ?".into());
                binds.push(s.into());
            }
            if let Some(f) = fan_id {
                let mut clause = "EXISTS (SELECT 1 FROM fan_items fi WHERE fi.item_id = hi.id AND fi.fan_id = ?".to_string();
                binds.push(f.into());
                if let Some(t) = tab_filter(tab.as_deref()) {
                    clause.push_str(" AND fi.tab = ?");
                    binds.push(t.to_string().into());
                }
                clause.push(')');
                wh.push(clause);
            }
            let sql = format!(
                "SELECT min(t.tag) AS tag, COUNT(*) AS n FROM harvest_item_tags t JOIN harvest_items hi ON hi.id = t.item_id \
                 WHERE {} GROUP BY t.tag_key ORDER BY n DESC, tag LIMIT {limit}",
                wh.join(" AND ")
            );
            let mut st = c.prepare(&sql)?;
            Ok(st
                .query_map(params_from_iter(binds.iter()), |r| Ok(TagCount { tag: r.get(0)?, count: r.get(1)? }))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await?;
    Ok(Json(rows))
}

/// Queue inbox items for download.
async fn queue_items(State(ctx): State<Arc<Ctx>>, Json(body): Json<QueueRequest>) -> ApiResult<Json<QueueResult>> {
    let b = body.clone();
    let candidates: Vec<HarvestRow> = ctx
        .db
        .read_async(move |c| {
            if b.all_matching {
                // With include_in_library, in-library rows are in scope by definition, so a state
                // filter of "new" alone would silently exclude the very rows the flag exists to
                // bring along.
                let mut states = vec![b.state.clone()];
                if b.include_in_library && b.state != "in_library" {
                    states.push("in_library".into());
                }
                let mut binds: Vec<SqlValue> = Vec::new();
                let mut from = "harvest_items hi".to_string();
                if let Some(fan_id) = b.fan_id {
                    from.push_str(&format!(" JOIN {} m ON m.item_id = hi.id", members_sql(tab_filter(b.tab.as_deref()), &mut binds, fan_id)));
                }
                let mut wh = vec![format!("hi.state IN ({})", vec!["?"; states.len()].join(","))];
                binds.extend(states.into_iter().map(SqlValue::from));
                if b.in_wishlist {
                    wh.push("hi.in_wishlist = 1".into());
                }
                if let Some(s) = b.source_kind.as_deref().filter(|s| !s.is_empty()) {
                    wh.push("hi.source_kind = ?".into());
                    binds.push(s.to_string().into());
                }
                if let Some(s) = b.source_label.as_deref().filter(|s| !s.is_empty()) {
                    wh.push("hi.source_label = ?".into());
                    binds.push(s.to_string().into());
                }
                let cols = ROW_COLS.split(", ").map(|c| format!("hi.{}", c.trim())).collect::<Vec<_>>().join(", ");
                let mut st = c.prepare(&format!("SELECT {cols} FROM {from} WHERE {} ORDER BY hi.id", wh.join(" AND ")))?;
                Ok(st.query_map(params_from_iter(binds.iter()), HarvestRow::from_row)?.collect::<Result<Vec<_>, _>>()?)
            } else {
                inbox::load_rows(c, &b.item_ids).map_err(de)
            }
        })
        .await?;
    if candidates.is_empty() {
        return Err(ApiError::bad_request("no matching inbox items"));
    }

    let opts = QueueOpts {
        allow_unowned: body.allow_unowned,
        target_subdir: body.target_subdir.clone(),
        include_in_library: body.include_in_library,
        single_folder: body.single_folder,
        source_fan_id: body.source_fan_id,
        label: body.label.clone(),
    };
    let (db, store) = (ctx.db.clone(), ctx.jobs.store().clone());
    let outcome = tokio::task::spawn_blocking(move || inbox::queue(&db, &store, &candidates, &opts))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))??;

    if outcome.queued > 0 {
        ctx.notify_downloads();
    }
    if outcome.adopted > 0 {
        ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &serde_json::json!({"adopted": outcome.adopted}));
        ctx.bus.invalidate("release", vec![]);
    }
    Ok(Json(QueueResult {
        queued: outcome.queued,
        skipped_in_library: outcome.skipped_in_library,
        needs_confirmation: outcome.needs_confirmation.into_iter().take(50).map(Value::String).collect(),
        job_id: outcome.job_id,
        adopted: outcome.adopted,
    }))
}

#[derive(serde::Deserialize)]
struct IgnoreBatch {
    ids: Vec<i64>,
}

/// Toggle `ignored` <-> `new` for many rows in ONE request and ONE transaction. Each row flips
/// individually (like the single route); returns the refreshed rows.
async fn ignore_items(State(ctx): State<Arc<Ctx>>, Json(body): Json<IgnoreBatch>) -> ApiResult<Json<Vec<HarvestItemOut>>> {
    if body.ids.is_empty() {
        return Ok(Json(vec![]));
    }
    let ids = body.ids.clone();
    let rows = ctx
        .db
        .write_async(move |tx| {
            let mut out = Vec::new();
            for id in ids {
                let Some(row) = inbox::load_row(tx, id).map_err(de)? else { continue };
                let next = if row.state != "ignored" { "ignored" } else { "new" };
                tx.execute("UPDATE harvest_items SET state = ?2 WHERE id = ?1", bc_db::rusqlite::params![id, next])?;
                if let Some(r) = inbox::load_row(tx, id).map_err(de)? {
                    out.push(r);
                }
            }
            Ok(out)
        })
        .await?;
    Ok(Json(rows.iter().map(|r| r.to_out(false, None, vec![])).collect()))
}

/// Toggle `ignored` <-> `new`.
async fn ignore_item(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>) -> ApiResult<Json<HarvestItemOut>> {
    let row = ctx
        .db
        .write_async(move |tx| {
            let Some(row) = inbox::load_row(tx, id).map_err(de)? else { return Ok(None) };
            let next = if row.state != "ignored" { "ignored" } else { "new" };
            tx.execute("UPDATE harvest_items SET state = ?2 WHERE id = ?1", bc_db::rusqlite::params![id, next])?;
            inbox::load_row(tx, id).map_err(de)
        })
        .await?;
    match row {
        Some(r) => Ok(Json(r.to_out(false, None, vec![]))),
        None => Err(ApiError::not_found(format!("inbox item {id} not found"))),
    }
}

// ---------------------------------------------------------------------------------------
// tag enrichment
// ---------------------------------------------------------------------------------------

/// Fetch each item's release page and stamp its tags onto the row (`202`, job kind `enrich`).
async fn start_enrich(State(ctx): State<Arc<Ctx>>, Json(body): Json<EnrichRequest>) -> ApiResult<(StatusCode, Json<EnrichState>)> {
    if body.item_ids.is_empty() {
        return Err(ApiError::bad_request("no items to enrich"));
    }
    let state = ctx.expect::<TagEnricher>().start(&body.item_ids).await?;
    Ok((StatusCode::ACCEPTED, Json(state)))
}

async fn enrich_status(State(ctx): State<Arc<Ctx>>) -> Json<EnrichState> {
    Json(ctx.expect::<TagEnricher>().state())
}

async fn stop_enrich(State(ctx): State<Arc<Ctx>>) -> Json<EnrichState> {
    Json(ctx.expect::<TagEnricher>().stop().await)
}

// ---------------------------------------------------------------------------------------
// label resolution and sweeps
// ---------------------------------------------------------------------------------------

async fn get_label_resolution(State(ctx): State<Arc<Ctx>>) -> Json<LabelResolveStatus> {
    Json(ctx.expect::<LabelResolver>().status())
}

/// Returns as soon as the sweep starts (`202`, job kind `sweep`): opens one album page per
/// candidate label host and files the releases under what those pages say.
async fn start_label_resolution(State(ctx): State<Arc<Ctx>>) -> ApiResult<(StatusCode, Json<LabelResolveStatus>)> {
    let s = ctx.expect::<LabelResolver>().start_run().await?;
    Ok((StatusCode::ACCEPTED, Json(s)))
}

fn sweeper(ctx: &Ctx, kind: SweepKind) -> Arc<Sweeper> {
    ctx.expect::<SweepService>().get(kind).clone()
}

async fn get_label_sweep(State(ctx): State<Arc<Ctx>>) -> Json<SweepStatus> {
    Json(sweeper(&ctx, SweepKind::Labels).status())
}

/// `label_ids` checks just those folders; an empty list (or no body at all) sweeps the lot.
async fn start_label_sweep(
    State(ctx): State<Arc<Ctx>>,
    body: Option<Json<LabelSweepRequest>>,
) -> ApiResult<(StatusCode, Json<SweepStatus>)> {
    let ids = body.map(|Json(b)| b.label_ids).unwrap_or_default();
    let s = sweeper(&ctx, SweepKind::Labels).start(Some(&ids)).await?;
    Ok((StatusCode::ACCEPTED, Json(s)))
}

/// Stops the walk and queues whatever it has already found. Idle is not an error.
async fn stop_label_sweep(State(ctx): State<Arc<Ctx>>) -> Json<SweepStatus> {
    Json(sweeper(&ctx, SweepKind::Labels).stop().await)
}

async fn get_favorites_sweep(State(ctx): State<Arc<Ctx>>) -> Json<SweepStatus> {
    Json(sweeper(&ctx, SweepKind::Favorites).status())
}

/// The Home strip's one button: check every pinned artist and label, queue the lot as one
/// download job. Takes no arguments on purpose.
async fn start_favorites_sweep(State(ctx): State<Arc<Ctx>>) -> ApiResult<(StatusCode, Json<SweepStatus>)> {
    let s = sweeper(&ctx, SweepKind::Favorites).start(None).await?;
    Ok((StatusCode::ACCEPTED, Json(s)))
}

async fn stop_favorites_sweep(State(ctx): State<Arc<Ctx>>) -> Json<SweepStatus> {
    Json(sweeper(&ctx, SweepKind::Favorites).stop().await)
}

// ---------------------------------------------------------------------------------------
// identity
// ---------------------------------------------------------------------------------------

async fn get_identity(State(ctx): State<Arc<Ctx>>) -> ApiResult<Json<IdentityStatus>> {
    let cookies = ctx.cookies.clone();
    let cookie = tokio::task::spawn_blocking(move || cookies.load()).await.map_err(|e| ApiError::internal(e.to_string()))?;
    let Some(cookie) = cookie else {
        return Ok(Json(IdentityStatus { configured: false, detail: "No Bandcamp cookie stored.".into(), ..Default::default() }));
    };
    let fp = identity::fingerprint(&cookie);
    Ok(Json(match sources::whoami(&ctx.client).await {
        Ok(who) => IdentityStatus {
            configured: true,
            valid: Some(true),
            fingerprint: Some(fp),
            username: who.username,
            fan_id: who.fan_id,
            detail: "Cookie is valid.".into(),
        },
        Err(HarvestError::IdentityExpired(_)) => IdentityStatus {
            configured: true,
            valid: Some(false),
            fingerprint: Some(fp),
            detail: "Cookie is no longer valid -- paste a fresh one.".into(),
            ..Default::default()
        },
        Err(e) => IdentityStatus {
            configured: true,
            valid: None,
            fingerprint: Some(fp),
            detail: format!("Could not verify: {}", identity::redact(&e.to_string())),
            ..Default::default()
        },
    }))
}

async fn put_identity(State(ctx): State<Arc<Ctx>>, Json(body): Json<CookieRequest>) -> ApiResult<Json<IdentityStatus>> {
    let cookie = identity::normalise_cookie(&body.cookie);
    if !identity::has_identity(&cookie) {
        return Err(ApiError::bad_request(
            "That does not contain an `identity` cookie. Copy the whole Cookie header from a logged-in bandcamp.com request in your browser's network tab.",
        ));
    }
    let cookies = ctx.cookies.clone();
    let fp = tokio::task::spawn_blocking(move || cookies.store(&cookie)).await.map_err(|e| ApiError::internal(e.to_string()))?;
    ctx.reload_cookie();

    Ok(Json(match sources::whoami(&ctx.client).await {
        Ok(who) => IdentityStatus {
            configured: true,
            valid: Some(true),
            fingerprint: Some(fp),
            username: who.username,
            fan_id: who.fan_id,
            detail: "Cookie stored and verified.".into(),
        },
        Err(HarvestError::IdentityExpired(_)) => IdentityStatus {
            configured: true,
            valid: Some(false),
            fingerprint: Some(fp),
            detail: "Stored, but Bandcamp rejected it. It may already have expired.".into(),
            ..Default::default()
        },
        Err(e) => IdentityStatus {
            configured: true,
            valid: None,
            fingerprint: Some(fp),
            detail: format!("Stored, but unverified: {}", identity::redact(&e.to_string())),
            ..Default::default()
        },
    }))
}

async fn delete_identity(State(ctx): State<Arc<Ctx>>) -> ApiResult<StatusCode> {
    let cookies = ctx.cookies.clone();
    tokio::task::spawn_blocking(move || cookies.clear()).await.map_err(|e| ApiError::internal(e.to_string()))?;
    ctx.reload_cookie();
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------
// health
// ---------------------------------------------------------------------------------------

/// Extraction-tier telemetry is the early warning for markup drift. A spike in `css` tier
/// alongside HTTP 200s is the signature of Bandcamp changing its blobs -- or of a soft block
/// serving a challenge page.
async fn health(State(ctx): State<Arc<Ctx>>) -> ApiResult<Json<HarvestHealth>> {
    let tiers: BTreeMap<String, i64> = ctx
        .db
        .read_async(|c| {
            let mut st = c.prepare("SELECT coalesce(extract_tier, 'unknown'), COUNT(id) FROM harvest_items GROUP BY extract_tier")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            let mut m = BTreeMap::new();
            for r in rows {
                let (k, n) = r?;
                *m.entry(k).or_insert(0) += n;
            }
            Ok(m)
        })
        .await?;
    let total = tiers.values().sum::<i64>().max(1);
    let round = |v: f64, places: i32| (v * 10f64.powi(places)).round() / 10f64.powi(places);
    let client = &ctx.client;
    let limiter = client.limiter(Lane::Normal);
    let stats = client.stats();
    let cache = client.cache().and_then(|c| c.stats().ok()).unwrap_or_default();
    let last = stats.last_errors.len().saturating_sub(5);
    Ok(Json(HarvestHealth {
        rate_limit: RateLimitHealth { current_rate: round(limiter.rate(), 3), penalised_for_s: round(limiter.penalised_for(), 1) },
        cache: CacheHealth { entries: cache.entries as i64, bytes: cache.bytes as i64 },
        requests: stats.requests as i64,
        cache_hits: stats.cache_hits as i64,
        errors: stats.errors as i64,
        last_errors: stats.last_errors[last..].to_vec(),
        blob_ratio: round(*tiers.get("blob").unwrap_or(&0) as f64 / total as f64, 3),
        extract_tiers: tiers,
    }))
}

// ---------------------------------------------------------------------------------------
// locate
// ---------------------------------------------------------------------------------------

fn locate_out(r: LocateResult) -> Json<LocateOut> {
    Json(LocateOut { url: r.url, matched: r.matched, detail: r.detail })
}

/// Search Bandcamp for the artist's page and pin it, proven against the library.
async fn locate_artist(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>) -> ApiResult<Json<LocateOut>> {
    let src = ctx.expect::<LabelResolver>().source();
    match locate_artist_page(&ctx.db, &*src, id).await {
        Some(r) => Ok(locate_out(r)),
        None => Err(ApiError::not_found(format!("artist {id} not found"))),
    }
}

/// Search Bandcamp for the label's page and pin it, proven against the shelf.
async fn locate_label(State(ctx): State<Arc<Ctx>>, Path(id): Path<i64>) -> ApiResult<Json<LocateOut>> {
    let src = ctx.expect::<LabelResolver>().source();
    match locate_label_page(&ctx.db, &*src, id).await {
        Some(r) => Ok(locate_out(r)),
        None => Err(ApiError::not_found(format!("label {id} not found"))),
    }
}
