//! `/playlists...`: manual playlists (a frozen, reorderable list), smart playlists (a saved
//! `/tracks` filter evaluated live) and "save this view as a playlist".
//!
//! Item rows are `playlist_items(id, playlist_id, track_id, position)`; `TrackOut.item_id`
//! carries the row id in listings, because removing "this track" from a list means removing one
//! *place* in it and a playlist may hold the same track twice.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use bc_db::rusqlite::{Connection, OptionalExtension, params};
use bc_db::util::{iso, now_db};
use bc_libcore::hydrate::{art_url, tracks_out};
use bc_libcore::{ApiError, ApiResult, Q, Scope};
use bc_types::library::{
    Page, PlaylistAddTracks, PlaylistAdded, PlaylistCreate, PlaylistExportQuery, PlaylistFromTracks,
    PlaylistMoved, PlaylistOut, PlaylistPatch, ScopeMode, TrackOut, TrackQuery, TrackSort, SortDir,
};
use bc_types::sets::MoveItem;
use serde::Deserialize;

use crate::lane::{self, Lane};
use crate::{J, PlistState, TrackResolver, export};

pub fn routes() -> Router<PlistState> {
    Router::new()
        .route("/playlists", get(list).post(create))
        .route("/playlists/from-tracks", post(from_tracks))
        .route("/playlists/{id}", get(get_one).patch(patch).delete(delete))
        .route("/playlists/{id}/tracks", get(tracks).post(add_tracks))
        .route("/playlists/{id}/tracks/{item_id}", axum::routing::delete(remove_track))
        .route("/playlists/{id}/tracks/{item_id}/move", post(move_track))
        .route("/playlists/{id}/export", get(export_playlist))
}

/// Library-scope query parameters (smart playlists evaluate a filter under a scope).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ScopeQuery {
    pub scope: Option<ScopeMode>,
    pub source_fan_id: Option<i64>,
}

// ---------------------------------------------------------------------------------------
// Storage-level API (sync; callers choose the connection/transaction)
// ---------------------------------------------------------------------------------------

/// `base`, or `base 2`, `base 3`... whichever is free.
///
/// Saving the same view twice is a normal thing to do; two rows called "Loved" that differ in
/// ways nothing on screen explains are worse than a number. The LIKE may match more names than
/// it means to (a name carrying `_` or `%`), which only ever widens the set of names avoided.
pub fn unique_name(c: &Connection, base: &str) -> ApiResult<String> {
    let mut st = c.prepare_cached("SELECT name FROM playlists WHERE name LIKE ?1")?;
    let taken: std::collections::HashSet<String> =
        st.query_map([format!("{base}%")], |r| r.get(0))?.collect::<Result<_, _>>()?;
    if !taken.contains(base) {
        return Ok(base.to_string());
    }
    let mut n = 2;
    while taken.contains(&format!("{base} {n}")) {
        n += 1;
    }
    Ok(format!("{base} {n}"))
}

/// A manual playlist and its items in one go, in the order given.
pub fn build_playlist(c: &Connection, name: &str, description: Option<&str>, track_ids: &[i64]) -> ApiResult<i64> {
    let now = now_db();
    c.execute(
        "INSERT INTO playlists(name, description, kind, rules, created_at, updated_at) VALUES (?1, ?2, 'manual', NULL, ?3, ?3)",
        params![name, description, now],
    )?;
    let id = c.last_insert_rowid();
    let mut st = c.prepare_cached(
        "INSERT INTO playlist_items(playlist_id, track_id, position, added_at) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (i, tid) in track_ids.iter().enumerate() {
        st.execute(params![id, tid, bc_music::ordering::STEP * (i as f64 + 1.0), now])?;
    }
    Ok(id)
}

struct Meta {
    kind: String,
    rules: Option<String>,
}

fn meta(c: &Connection, id: i64) -> ApiResult<Meta> {
    c.query_row("SELECT kind, rules FROM playlists WHERE id = ?1", [id], |r| {
        Ok(Meta { kind: r.get(0)?, rules: r.get(1)? })
    })
    .optional()?
    .ok_or_else(|| ApiError::not_found(format!("playlist {id} not found")))
}

fn parse_rules(raw: Option<&str>) -> Option<TrackQuery> {
    raw.and_then(|r| serde_json::from_str::<TrackQuery>(r).ok())
}

/// The saved filter of a smart playlist, evaluated now.
fn eval_smart(c: &Connection, resolver: &dyn TrackResolver, m: &Meta, scope: &Scope) -> ApiResult<Vec<i64>> {
    // A smart playlist whose rules cannot be read is an empty one, never "every track".
    let Some(mut q) = parse_rules(m.rules.as_deref()) else { return Ok(vec![]) };
    q.offset = None;
    q.limit = None;
    resolver.resolve_ids(c, &q, scope, None)
}

/// `(item id, track id)` of a playlist in order. Smart playlists have no item rows: their
/// tracks come from the live filter and the item id is `None`.
pub fn playlist_items(
    c: &Connection,
    resolver: &dyn TrackResolver,
    id: i64,
    scope: &Scope,
) -> ApiResult<Vec<(Option<i64>, i64)>> {
    let m = meta(c, id)?;
    if m.kind == "smart" {
        return Ok(eval_smart(c, resolver, &m, scope)?.into_iter().map(|t| (None, t)).collect());
    }
    let mut st = c.prepare_cached("SELECT id, track_id FROM playlist_items WHERE playlist_id = ?1 ORDER BY position, id")?;
    let rows = st.query_map([id], |r| Ok((Some(r.get::<_, i64>(0)?), r.get::<_, i64>(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Track ids of a playlist in order (smart playlists evaluated under the default scope).
pub fn playlist_track_ids(c: &Connection, resolver: &dyn TrackResolver, id: i64) -> ApiResult<Vec<i64>> {
    let scope = Scope::resolve(c, None, None)?;
    Ok(playlist_items(c, resolver, id, &scope)?.into_iter().map(|(_, t)| t).collect())
}

/// Playlist cards: `only` = one playlist, else all, by name. Counts, durations and covers come
/// from one aggregate query however many playlists exist (never a per-playlist item load); only
/// smart playlists evaluate their filter.
pub fn playlists_out(c: &Connection, resolver: &dyn TrackResolver, only: Option<i64>) -> ApiResult<Vec<PlaylistOut>> {
    struct Row {
        out: PlaylistOut,
        rules: Option<String>,
    }
    let mut rows: Vec<Row> = Vec::new();
    {
        let mut st = c.prepare_cached(
            "SELECT q.id, q.name, q.description, q.kind, q.rules, q.created_at, q.n, q.d, q.cover, aw.version
               FROM (SELECT p.id, p.name, p.description, p.kind, p.rules, p.created_at,
                            COALESCE(a.n, 0) AS n, COALESCE(a.d, 0) AS d,
                            (SELECT r.id FROM playlist_items pi
                               JOIN tracks t ON t.id = pi.track_id
                               JOIN releases r ON r.id = t.release_id
                              WHERE pi.playlist_id = p.id AND r.cover_path IS NOT NULL
                              ORDER BY pi.position, pi.id LIMIT 1) AS cover
                       FROM playlists p
                       LEFT JOIN (SELECT pi.playlist_id AS pid, COUNT(*) AS n, COALESCE(SUM(t.duration_ms), 0) AS d
                                    FROM playlist_items pi LEFT JOIN tracks t ON t.id = pi.track_id
                                   GROUP BY pi.playlist_id) a ON a.pid = p.id
                      WHERE (?1 IS NULL OR p.id = ?1)) q
               LEFT JOIN artwork aw ON aw.release_id = q.cover
              ORDER BY q.name, q.id",
        )?;
        let mapped = st.query_map([only], |r| {
            let rules: Option<String> = r.get(4)?;
            let cover: Option<i64> = r.get(8)?;
            let version: Option<String> = r.get(9)?;
            Ok(Row {
                out: PlaylistOut {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    description: r.get(2)?,
                    kind: r.get(3)?,
                    track_count: r.get(6)?,
                    duration_ms: r.get(7)?,
                    art_url: cover.map(|rid| art_url(rid, version.as_deref(), true)),
                    created_at: Some(iso(&r.get::<_, String>(5)?)),
                    rules: parse_rules(rules.as_deref()),
                },
                rules,
            })
        })?;
        for r in mapped {
            rows.push(r?);
        }
    }
    if rows.iter().any(|r| r.out.kind == "smart") {
        let scope = Scope::resolve(c, None, None)?;
        for r in rows.iter_mut().filter(|r| r.out.kind == "smart") {
            let m = Meta { kind: "smart".into(), rules: r.rules.clone() };
            let ids = eval_smart(c, resolver, &m, &scope)?;
            let json = serde_json::to_string(&ids).map_err(ApiError::internal)?;
            let (n, d): (i64, i64) = c.query_row(
                "SELECT COUNT(*), COALESCE(SUM(duration_ms), 0) FROM tracks WHERE id IN (SELECT value FROM json_each(?1))",
                [&json],
                |x| Ok((x.get(0)?, x.get(1)?)),
            )?;
            r.out.track_count = n;
            r.out.duration_ms = d;
            let mut st = c.prepare_cached(
                "SELECT r.id, aw.version FROM tracks t JOIN releases r ON r.id = t.release_id
                   LEFT JOIN artwork aw ON aw.release_id = r.id
                  WHERE t.id = ?1 AND r.cover_path IS NOT NULL",
            )?;
            for tid in ids.iter().take(30) {
                if let Some((rid, v)) =
                    st.query_row([tid], |x| Ok((x.get::<_, i64>(0)?, x.get::<_, Option<String>>(1)?))).optional()?
                {
                    r.out.art_url = Some(art_url(rid, v.as_deref(), true));
                    break;
                }
            }
        }
    }
    Ok(rows.into_iter().map(|r| r.out).collect())
}

fn one_out(c: &Connection, resolver: &dyn TrackResolver, id: i64) -> ApiResult<PlaylistOut> {
    playlists_out(c, resolver, Some(id))?.pop().ok_or_else(|| ApiError::not_found(format!("playlist {id} not found")))
}

fn require(c: &Connection, id: i64) -> ApiResult<()> {
    meta(c, id).map(|_| ())
}

/// Error when any of `ids` is not a track.
pub(crate) fn require_tracks(c: &Connection, ids: &[i64]) -> ApiResult<()> {
    let json = serde_json::to_string(ids).map_err(ApiError::internal)?;
    let known: i64 = c.query_row(
        "SELECT COUNT(*) FROM tracks WHERE id IN (SELECT value FROM json_each(?1))",
        [&json],
        |r| r.get(0),
    )?;
    let distinct: std::collections::HashSet<&i64> = ids.iter().collect();
    if known as usize != distinct.len() {
        return Err(ApiError::bad("some of those tracks do not exist"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------

async fn list(State(st): State<PlistState>) -> ApiResult<axum::Json<Vec<PlaylistOut>>> {
    let resolver = st.resolver.clone();
    Ok(axum::Json(st.ctx.read_async(move |c| playlists_out(c, resolver.as_ref(), None)).await?))
}

async fn get_one(State(st): State<PlistState>, Path(id): Path<i64>) -> ApiResult<axum::Json<PlaylistOut>> {
    let resolver = st.resolver.clone();
    Ok(axum::Json(st.ctx.read_async(move |c| one_out(c, resolver.as_ref(), id)).await?))
}

async fn create(State(st): State<PlistState>, J(body): J<PlaylistCreate>) -> ApiResult<axum::Json<PlaylistOut>> {
    let kind = body.kind.as_deref().unwrap_or("manual").to_string();
    if kind != "manual" && kind != "smart" {
        return Err(ApiError::bad("kind must be 'manual' or 'smart'"));
    }
    if kind == "smart" && body.rules.is_none() {
        return Err(ApiError::bad("a smart playlist needs its rules (the saved filter)"));
    }
    let name = body.name.trim();
    let name = if name.is_empty() { "Untitled".to_string() } else { name.to_string() };
    let rules = match (&kind[..], &body.rules) {
        ("smart", Some(q)) => Some(serde_json::to_string(q).map_err(ApiError::internal)?),
        _ => None,
    };
    let resolver = st.resolver.clone();
    let out = st
        .ctx
        .write_async(move |t| {
            let now = now_db();
            t.execute(
                "INSERT INTO playlists(name, description, kind, rules, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![name, body.description, kind, rules, now],
            )?;
            one_out(t, resolver.as_ref(), t.last_insert_rowid())
        })
        .await?;
    st.ctx.bus.invalidate("playlist", vec![out.id]);
    Ok(axum::Json(out))
}

async fn patch(
    State(st): State<PlistState>,
    Path(id): Path<i64>,
    J(body): J<PlaylistPatch>,
) -> ApiResult<axum::Json<PlaylistOut>> {
    let resolver = st.resolver.clone();
    let out = st
        .ctx
        .write_async(move |t| {
            let m = meta(t, id)?;
            let now = now_db();
            if let Some(name) = &body.name {
                let name = name.trim();
                if name.is_empty() {
                    return Err(ApiError::bad("a playlist needs a name"));
                }
                t.execute("UPDATE playlists SET name = ?1 WHERE id = ?2", params![name, id])?;
            }
            if let Some(d) = &body.description {
                let d = d.trim();
                t.execute(
                    "UPDATE playlists SET description = ?1 WHERE id = ?2",
                    params![if d.is_empty() { None } else { Some(d) }, id],
                )?;
            }
            if let Some(rules) = &body.rules {
                if m.kind != "smart" {
                    return Err(ApiError::bad("only a smart playlist has rules"));
                }
                let json = serde_json::to_string(rules).map_err(ApiError::internal)?;
                t.execute("UPDATE playlists SET rules = ?1 WHERE id = ?2", params![json, id])?;
            }
            t.execute("UPDATE playlists SET updated_at = ?1 WHERE id = ?2", params![now, id])?;
            one_out(t, resolver.as_ref(), id)
        })
        .await?;
    st.ctx.bus.invalidate("playlist", vec![id]);
    Ok(axum::Json(out))
}

async fn delete(State(st): State<PlistState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    st.ctx
        .write_async(move |t| {
            require(t, id)?;
            // items go with it through the foreign key
            t.execute("DELETE FROM playlists WHERE id = ?1", [id])?;
            Ok(())
        })
        .await?;
    st.ctx.bus.invalidate("playlist", vec![id]);
    Ok(StatusCode::NO_CONTENT)
}

/// Hydrated tracks of a list of `(item id, track id)`, in that order. A track held twice is
/// hydrated once and cloned.
pub fn hydrate_items(c: &Connection, items: &[(Option<i64>, i64)]) -> ApiResult<Vec<TrackOut>> {
    let mut unique: Vec<i64> = Vec::with_capacity(items.len());
    let mut seen = std::collections::HashSet::new();
    for (_, t) in items {
        if seen.insert(*t) {
            unique.push(*t);
        }
    }
    let by_id: HashMap<i64, TrackOut> = tracks_out(c, &unique)?.into_iter().map(|t| (t.id, t)).collect();
    Ok(items
        .iter()
        .filter_map(|(item, tid)| {
            by_id.get(tid).map(|t| {
                let mut t = t.clone();
                t.item_id = *item;
                t
            })
        })
        .collect())
}

fn all_page<T>(items: Vec<T>) -> Page<T> {
    let n = items.len() as i64;
    Page { items, total: n, offset: 0, limit: n.max(1) }
}

async fn tracks(
    State(st): State<PlistState>,
    Path(id): Path<i64>,
    Q(sq): Q<ScopeQuery>,
) -> ApiResult<axum::Json<Page<TrackOut>>> {
    let resolver = st.resolver.clone();
    let page = st
        .ctx
        .read_async(move |c| {
            let scope = Scope::resolve(c, sq.scope, sq.source_fan_id)?;
            let items = playlist_items(c, resolver.as_ref(), id, &scope)?;
            Ok(all_page(hydrate_items(c, &items)?))
        })
        .await?;
    Ok(axum::Json(page))
}

async fn add_tracks(
    State(st): State<PlistState>,
    Path(id): Path<i64>,
    J(body): J<PlaylistAddTracks>,
) -> ApiResult<axum::Json<PlaylistAdded>> {
    if body.track_ids.is_empty() {
        // legacy order: the playlist is looked up first
        st.ctx.read_async(move |c| require(c, id)).await?;
        return Err(ApiError::bad("no tracks supplied"));
    }
    let out = st
        .ctx
        .write_async(move |t| {
            let m = meta(t, id)?;
            if m.kind == "smart" {
                return Err(ApiError::bad("a smart playlist is computed from its rules; it has no items to add to"));
            }
            require_tracks(t, &body.track_ids)?;
            let existing: Vec<f64> = lane::PLAYLIST.items(t, id)?.into_iter().map(|(_, p)| p).collect();
            let positions = Lane::plan_insert(&existing, body.track_ids.len(), body.at_index);
            let now = now_db();
            {
                let mut ins = t.prepare_cached(
                    "INSERT INTO playlist_items(playlist_id, track_id, position, added_at) VALUES (?1, ?2, ?3, ?4)",
                )?;
                for (tid, p) in body.track_ids.iter().zip(&positions) {
                    ins.execute(params![id, tid, p, now])?;
                }
            }
            t.execute("UPDATE playlists SET updated_at = ?1 WHERE id = ?2", params![now, id])?;
            lane::PLAYLIST.renormalise_if_needed(t, id)?;
            Ok(PlaylistAdded {
                added: body.track_ids.len() as i64,
                total: (existing.len() + body.track_ids.len()) as i64,
            })
        })
        .await?;
    st.ctx.bus.invalidate("playlist", vec![id]);
    Ok(axum::Json(out))
}

async fn remove_track(State(st): State<PlistState>, Path((id, item_id)): Path<(i64, i64)>) -> ApiResult<StatusCode> {
    st.ctx
        .write_async(move |t| {
            if !lane::PLAYLIST.contains(t, id, item_id)? {
                return Err(ApiError::not_found(format!("item {item_id} not in playlist {id}")));
            }
            t.execute("DELETE FROM playlist_items WHERE id = ?1", [item_id])?;
            Ok(())
        })
        .await?;
    st.ctx.bus.invalidate("playlist", vec![id]);
    Ok(StatusCode::NO_CONTENT)
}

async fn move_track(
    State(st): State<PlistState>,
    Path((id, item_id)): Path<(i64, i64)>,
    J(body): J<MoveItem>,
) -> ApiResult<axum::Json<PlaylistMoved>> {
    let position = st
        .ctx
        .write_async(move |t| lane::PLAYLIST.move_item(t, id, item_id, body.to_index))
        .await?;
    st.ctx.bus.invalidate("playlist", vec![id]);
    Ok(axum::Json(PlaylistMoved { position }))
}

async fn export_playlist(
    State(st): State<PlistState>,
    Path(id): Path<i64>,
    Q(eq): Q<PlaylistExportQuery>,
    Q(sq): Q<ScopeQuery>,
) -> ApiResult<Response> {
    let resolver = st.resolver.clone();
    let (name, tracks) = st
        .ctx
        .read_async(move |c| {
            let name: String = c
                .query_row("SELECT name FROM playlists WHERE id = ?1", [id], |r| r.get(0))
                .optional()?
                .ok_or_else(|| ApiError::not_found(format!("playlist {id} not found")))?;
            let scope = Scope::resolve(c, sq.scope, sq.source_fan_id)?;
            let ids: Vec<i64> =
                playlist_items(c, resolver.as_ref(), id, &scope)?.into_iter().map(|(_, t)| t).collect();
            Ok((name, export::load_export_tracks(c, &ids)?))
        })
        .await?;
    // zip streaming reads files: keep it off the async workers' hands (the writer is its own thread)
    export::export_response(eq.format, &tracks, &name)
}

/// `POST /playlists/from-tracks`: keep what a track view is showing (the loved shelf, the
/// favourites pool, a tag, a search) as a playlist.
///
/// A playlist is a frozen list and not a saved search: what the view holds when the button is
/// pressed is what the playlist holds from then on, which is the point. (A saved search is a
/// smart playlist.) Files that are gone are left out, the same call the m3u8 export makes.
async fn from_tracks(
    State(st): State<PlistState>,
    J(body): J<PlaylistFromTracks>,
) -> ApiResult<axum::Json<PlaylistOut>> {
    let resolver = st.resolver.clone();
    let reader_resolver = st.resolver.clone();
    let PlaylistFromTracks { name, filter } = body;
    let limit = filter.limit;
    let given = name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(str::to_string);
    let (ids, default_name) = st
        .ctx
        .read_async(move |c| {
            let scope = Scope::resolve(c, filter.scope, filter.source_fan_id)?;
            let mut q = filter.clone();
            q.missing = Some(false);
            q.sort = Some(TrackSort::Added);
            q.order = Some(SortDir::Desc);
            q.offset = None;
            q.limit = None;
            let ids = reader_resolver.resolve_ids(c, &q, &scope, limit)?;
            let default_name = if given.is_none() && !ids.is_empty() { Some(export::export_name(c, &filter)?) } else { None };
            Ok((ids, default_name.or(given)))
        })
        .await?;
    if ids.is_empty() {
        return Err(ApiError::bad("that view has no tracks to save"));
    }
    let base = default_name.unwrap_or_else(|| "Tracks".into());
    let out = st
        .ctx
        .write_async(move |t| {
            let name = unique_name(t, &base)?;
            let id = build_playlist(t, &name, None, &ids)?;
            one_out(t, resolver.as_ref(), id)
        })
        .await?;
    st.ctx.bus.invalidate("playlist", vec![out.id]);
    Ok(axum::Json(out))
}
