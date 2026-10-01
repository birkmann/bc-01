//! HTTP routes of the read side (tracks, releases, artists, labels, tags, facets, favorites, stats,
//! history, home, tasks). Paths are WITHOUT the `/api` prefix; legacy paths and shapes.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use bc_libcore::{ApiError, ApiResult, Ctx, Q, Scope, hydrate};
use bc_types::library::*;
use serde::Deserialize;

use crate::{artists, favorites, history, home, labels, releases, stats, tags, tracks};

pub fn router(ctx: Ctx) -> Router {
    Router::new()
        // tracks
        .route("/tracks", get(list_tracks))
        .route("/tracks/ids", get(track_ids))
        .route("/tracks/export", get(export_tracks))
        .route("/tracks/love", post(set_loved))
        .route("/tracks/{id}", get(get_track))
        .route("/tracks/{id}/love", post(toggle_love))
        .route("/tracks/{id}/rating", put(set_rating))
        // releases
        .route("/releases", get(list_releases))
        .route("/releases/ids", get(release_ids))
        .route("/releases/{id}", get(get_release))
        .route("/releases/{id}/next", get(next_release))
        .route("/releases/{id}/related", get(related_releases))
        // artists
        .route("/artists", get(list_artists))
        .route("/artists/{id}", get(get_artist).patch(patch_artist))
        .route("/artists/{id}/related", get(artist_related))
        // labels
        .route("/labels", get(list_labels))
        .route("/labels/shuffle", get(shuffle_labels))
        .route("/labels/random", get(random_label))
        .route("/labels/{id}", get(get_label).patch(patch_label))
        .route("/labels/{id}/next", get(next_label))
        // tags, facets, favorites
        .route("/tags", get(list_tags))
        .route("/facets", get(get_facets))
        .route("/favorites", get(list_favorites))
        .route("/favorites/tag", put(pin_tag).delete(unpin_tag))
        .route("/favorites/{kind}/{id}", put(pin).delete(unpin))
        // library
        .route("/library/stats", get(library_stats))
        .route("/library/home", get(library_home))
        .route("/library/import", post(start_import))
        .route("/library/tasks", get(list_tasks))
        .route("/library/tasks/{id}", get(get_task))
        // history
        .route("/history/play", post(record_play))
        .route("/history/top", get(history_top))
        .route("/history/recent", get(history_recent))
        .route("/history", axum::routing::delete(reset_history))
        .with_state(ctx)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ScopeQ {
    scope: Option<bc_types::ScopeMode>,
    source_fan_id: Option<i64>,
}

fn resolve(c: &bc_db::rusqlite::Connection, s: &ScopeQ) -> ApiResult<Scope> {
    Scope::resolve(c, s.scope, s.source_fan_id)
}

// ---------------------------------------------------------------- tracks

async fn list_tracks(State(ctx): State<Ctx>, Q(q): Q<TrackQuery>) -> ApiResult<Json<TrackPage>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            tracks::list_tracks(c, &q, &scope)
        })
        .await?,
    ))
}

async fn track_ids(State(ctx): State<Ctx>, Q(q): Q<TrackQuery>) -> ApiResult<Json<Vec<i64>>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            tracks::all_ids(c, &q, &scope, None)
        })
        .await?,
    ))
}

async fn get_track(State(ctx): State<Ctx>, Path(id): Path<i64>) -> ApiResult<Json<TrackOut>> {
    ctx.read_async(move |c| hydrate::track_out(c, id)?.ok_or_else(|| ApiError::not_found(format!("track {id} not found"))))
        .await
        .map(Json)
}

/// Loving is the most personal thing one can do with a record: a track loved off someone else's shelf belongs
/// in my library from then on, or the Loved shelf would hide what it was just told to keep.
fn adopt_releases_of(t: &bc_db::rusqlite::Transaction<'_>, track_ids: &[i64]) -> ApiResult<()> {
    let json = serde_json::to_string(track_ids).unwrap_or_default();
    t.execute(
        "UPDATE releases SET source_fan_id = NULL WHERE source_fan_id IS NOT NULL
           AND id IN (SELECT release_id FROM tracks WHERE id IN (SELECT value FROM json_each(?1)) AND release_id IS NOT NULL)",
        [json],
    )?;
    Ok(())
}

async fn toggle_love(State(ctx): State<Ctx>, Path(id): Path<i64>) -> ApiResult<Json<TrackOut>> {
    ctx.write_async(move |t| {
        let n = t.execute("UPDATE tracks SET loved = 1 - loved WHERE id = ?1", [id])?;
        if n == 0 {
            return Err(ApiError::not_found(format!("track {id} not found")));
        }
        let loved: bool = t.query_row("SELECT loved FROM tracks WHERE id = ?1", [id], |r| r.get(0))?;
        if loved {
            adopt_releases_of(t, &[id])?;
        }
        Ok(())
    })
    .await?;
    ctx.bus.invalidate("track", vec![id]);
    ctx.read_async(move |c| hydrate::track_out(c, id)?.ok_or_else(|| ApiError::not_found(format!("track {id} not found")))).await.map(Json)
}

async fn set_loved(State(ctx): State<Ctx>, Json(body): Json<SetLoved>) -> ApiResult<Json<ChangedOut>> {
    if body.track_ids.is_empty() {
        return Ok(Json(ChangedOut { changed: 0 }));
    }
    let ids = body.track_ids.clone();
    let loved = body.loved;
    let changed = ctx
        .write_async(move |t| {
            let json = serde_json::to_string(&ids).unwrap_or_default();
            let n = t.execute(
                "UPDATE tracks SET loved = ?2 WHERE loved != ?2 AND id IN (SELECT value FROM json_each(?1))",
                bc_db::rusqlite::params![json, loved as i32],
            )?;
            if loved {
                adopt_releases_of(t, &ids)?;
            }
            Ok(n as i64)
        })
        .await?;
    if changed > 0 {
        ctx.bus.invalidate("track", body.track_ids);
    }
    Ok(Json(ChangedOut { changed }))
}

async fn set_rating(State(ctx): State<Ctx>, Path(id): Path<i64>, Json(body): Json<SetRating>) -> ApiResult<Json<TrackOut>> {
    if let Some(r) = body.rating
        && !(0..=5).contains(&r)
    {
        return Err(ApiError::bad("rating must be 0..=5"));
    }
    ctx.write_async(move |t| {
        let n = t.execute("UPDATE tracks SET rating = ?2 WHERE id = ?1", bc_db::rusqlite::params![id, body.rating])?;
        if n == 0 {
            return Err(ApiError::not_found(format!("track {id} not found")));
        }
        Ok(())
    })
    .await?;
    ctx.bus.invalidate("track", vec![id]);
    ctx.read_async(move |c| hydrate::track_out(c, id)?.ok_or_else(|| ApiError::not_found(format!("track {id} not found")))).await.map(Json)
}

// ---------------------------------------------------------------- releases

async fn list_releases(State(ctx): State<Ctx>, Q(q): Q<ReleaseQuery>) -> ApiResult<Json<bc_types::Page<ReleaseOut>>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            releases::list_releases(c, &q, &scope)
        })
        .await?,
    ))
}

async fn release_ids(State(ctx): State<Ctx>, Q(q): Q<ReleaseQuery>) -> ApiResult<Json<Vec<ReleaseStub>>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            releases::release_stubs(c, &q, &scope)
        })
        .await?,
    ))
}

async fn get_release(State(ctx): State<Ctx>, Path(id): Path<i64>) -> ApiResult<Json<ReleaseOut>> {
    ctx.read_async(move |c| releases::get_release(c, id)).await.map(Json)
}

async fn next_release(State(ctx): State<Ctx>, Path(id): Path<i64>, Q(q): Q<ReleaseQuery>) -> ApiResult<Json<Option<ReleaseOut>>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            releases::next_release(c, id, &q, &scope)
        })
        .await?,
    ))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LimitQ {
    limit: Option<i64>,
    scope: Option<bc_types::ScopeMode>,
    source_fan_id: Option<i64>,
}

async fn related_releases(State(ctx): State<Ctx>, Path(id): Path<i64>, Q(q): Q<LimitQ>) -> ApiResult<Json<Vec<RelatedGroup>>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            releases::related_releases(c, id, q.limit.unwrap_or(18).clamp(1, 60), &scope)
        })
        .await?,
    ))
}

// ---------------------------------------------------------------- artists

async fn list_artists(State(ctx): State<Ctx>, Q(q): Q<ArtistQuery>) -> ApiResult<Json<bc_types::Page<ArtistOut>>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            artists::list_artists(c, &q, &scope)
        })
        .await?,
    ))
}

async fn get_artist(State(ctx): State<Ctx>, Path(id): Path<i64>, Q(s): Q<ScopeQ>) -> ApiResult<Json<ArtistDetailOut>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = resolve(c, &s)?;
            artists::get_artist(c, id, &scope)
        })
        .await?,
    ))
}

async fn artist_related(State(ctx): State<Ctx>, Path(id): Path<i64>, Q(q): Q<LimitQ>) -> ApiResult<Json<ArtistRelatedOut>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            artists::artist_related(c, id, q.limit.unwrap_or(18).clamp(1, 60), &scope)
        })
        .await?,
    ))
}

async fn patch_artist(State(ctx): State<Ctx>, Path(id): Path<i64>, Q(s): Q<ScopeQ>, Json(body): Json<ArtistPatch>) -> ApiResult<Json<ArtistDetailOut>> {
    let c2 = ctx.clone();
    tokio::task::spawn_blocking(move || artists::patch_artist(&c2, id, body)).await.map_err(ApiError::internal)??;
    ctx.bus.invalidate("artist", vec![id]);
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = resolve(c, &s)?;
            artists::get_artist(c, id, &scope)
        })
        .await?,
    ))
}

// ---------------------------------------------------------------- labels

async fn list_labels(State(ctx): State<Ctx>, Q(q): Q<LabelQuery>) -> ApiResult<Json<bc_types::Page<LabelOut>>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            labels::list_labels(c, &q, &scope)
        })
        .await?,
    ))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ShuffleQ {
    q: Option<String>,
    limit: Option<i64>,
    scope: Option<bc_types::ScopeMode>,
    source_fan_id: Option<i64>,
}

async fn shuffle_labels(State(ctx): State<Ctx>, Q(q): Q<ShuffleQ>) -> ApiResult<Json<bc_types::Page<TrackOut>>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            labels::shuffle_labels(c, q.q.as_deref().unwrap_or(""), q.limit.unwrap_or(500), &scope)
        })
        .await?,
    ))
}

async fn random_label(State(ctx): State<Ctx>, Q(q): Q<LabelQuery>) -> ApiResult<Json<Option<LabelOut>>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            labels::random_label(c, &q, &scope)
        })
        .await?,
    ))
}

async fn get_label(State(ctx): State<Ctx>, Path(id): Path<i64>, Q(s): Q<ScopeQ>) -> ApiResult<Json<LabelOut>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = resolve(c, &s)?;
            labels::get_label(c, id, &scope)
        })
        .await?,
    ))
}

async fn next_label(State(ctx): State<Ctx>, Path(id): Path<i64>, Q(q): Q<LabelQuery>) -> ApiResult<Json<Option<LabelOut>>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            labels::next_label(c, id, &q, &scope)
        })
        .await?,
    ))
}

async fn patch_label(State(ctx): State<Ctx>, Path(id): Path<i64>, Q(s): Q<ScopeQ>, Json(body): Json<LabelPatch>) -> ApiResult<Json<LabelOut>> {
    let name = body.name.clone();
    let c2 = ctx.clone();
    tokio::task::spawn_blocking(move || labels::patch_label(&c2, id, body)).await.map_err(ApiError::internal)??;
    ctx.bus.invalidate("label", vec![id]);
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = resolve(c, &s)?;
            let id = labels::label_id_after_patch(c, id, name.as_deref())?;
            labels::get_label(c, id, &scope)
        })
        .await?,
    ))
}

// ---------------------------------------------------------------- tags, facets, favorites

async fn list_tags(State(ctx): State<Ctx>, Q(q): Q<TagsQuery>) -> ApiResult<Json<Vec<TagOut>>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            tags::list_tags(c, &q, &scope)
        })
        .await?,
    ))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct FacetQ {
    limit: Option<i64>,
    scope: Option<bc_types::ScopeMode>,
    source_fan_id: Option<i64>,
}

async fn get_facets(State(ctx): State<Ctx>, Q(q): Q<FacetQ>) -> ApiResult<Json<Facets>> {
    Ok(Json(
        {
            let ctx2 = ctx.clone();
            ctx.read_async(move |c| {
                let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
                let limit = q.limit.unwrap_or(30);
                ctx2.memo(&format!("facets:{scope:?}:{limit}"), || tags::facets(c, limit, &scope))
            })
            .await?
        },
    ))
}

async fn list_favorites(State(ctx): State<Ctx>, Q(s): Q<ScopeQ>) -> ApiResult<Json<FavoritesOut>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = resolve(c, &s)?;
            favorites::list(c, &scope)
        })
        .await?,
    ))
}

#[derive(Deserialize)]
struct NameQ {
    name: String,
}

async fn pin_tag(State(ctx): State<Ctx>, Q(q): Q<NameQ>) -> ApiResult<StatusCode> {
    if q.name.trim().is_empty() {
        return Err(ApiError::bad("name is required"));
    }
    let c2 = ctx.clone();
    tokio::task::spawn_blocking(move || favorites::pin_tag(&c2, q.name)).await.map_err(ApiError::internal)??;
    Ok(StatusCode::NO_CONTENT)
}

async fn unpin_tag(State(ctx): State<Ctx>, Q(q): Q<NameQ>) -> ApiResult<StatusCode> {
    let c2 = ctx.clone();
    tokio::task::spawn_blocking(move || favorites::unpin_tag(&c2, q.name)).await.map_err(ApiError::internal)??;
    Ok(StatusCode::NO_CONTENT)
}

async fn pin(State(ctx): State<Ctx>, Path((kind, id)): Path<(String, i64)>) -> ApiResult<StatusCode> {
    let kind = favorites::Kind::parse(&kind).ok_or_else(|| ApiError::unprocessable(format!("unknown favorite kind {kind}")))?;
    let c2 = ctx.clone();
    tokio::task::spawn_blocking(move || favorites::pin(&c2, kind, id)).await.map_err(ApiError::internal)??;
    Ok(StatusCode::NO_CONTENT)
}

async fn unpin(State(ctx): State<Ctx>, Path((kind, id)): Path<(String, i64)>) -> ApiResult<StatusCode> {
    let kind = favorites::Kind::parse(&kind).ok_or_else(|| ApiError::unprocessable(format!("unknown favorite kind {kind}")))?;
    let c2 = ctx.clone();
    tokio::task::spawn_blocking(move || favorites::unpin(&c2, kind, id)).await.map_err(ApiError::internal)??;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- library, tasks, history

async fn library_stats(State(ctx): State<Ctx>, Q(s): Q<ScopeQ>) -> ApiResult<Json<LibraryStats>> {
    Ok(Json(
        {
            let ctx2 = ctx.clone();
            ctx.read_async(move |c| {
                let scope = resolve(c, &s)?;
                let key = format!("stats:{scope:?}");
                ctx2.memo(&key, || stats::library_stats(c, &scope))
            })
            .await?
        },
    ))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HomeQ {
    seed: Option<i64>,
    scope: Option<bc_types::ScopeMode>,
    source_fan_id: Option<i64>,
}

async fn library_home(State(ctx): State<Ctx>, Q(q): Q<HomeQ>) -> ApiResult<Json<HomeShelves>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            home::home(c, q.seed, &scope)
        })
        .await?,
    ))
}

async fn list_tasks(State(ctx): State<Ctx>) -> Json<Vec<bc_libcore::TaskInfo>> {
    Json(ctx.jobs.list())
}

async fn get_task(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<bc_libcore::TaskInfo>> {
    ctx.jobs.get(&id).map(Json).ok_or_else(|| ApiError::not_found(format!("task {id} not found")))
}

async fn record_play(State(ctx): State<Ctx>, Json(ev): Json<PlayEvent>) -> ApiResult<StatusCode> {
    let c2 = ctx.clone();
    tokio::task::spawn_blocking(move || history::record_play(&c2, ev)).await.map_err(ApiError::internal)??;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TopQ {
    days: Option<i64>,
    limit: Option<i64>,
    scope: Option<bc_types::ScopeMode>,
    source_fan_id: Option<i64>,
}

async fn history_top(State(ctx): State<Ctx>, Q(q): Q<TopQ>) -> ApiResult<Json<HistoryTop>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            history::top(c, q.days.unwrap_or(30), q.limit.unwrap_or(10), &scope)
        })
        .await?,
    ))
}

async fn history_recent(State(ctx): State<Ctx>, Q(q): Q<TopQ>) -> ApiResult<Json<Vec<HistoryEntry>>> {
    Ok(Json(
        ctx.read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            history::recent(c, q.limit.unwrap_or(20), &scope)
        })
        .await?,
    ))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct DaysQ {
    days: Option<i64>,
}

async fn reset_history(State(ctx): State<Ctx>, Q(q): Q<DaysQ>) -> ApiResult<Json<HistoryReset>> {
    let c2 = ctx.clone();
    Ok(Json(tokio::task::spawn_blocking(move || history::reset(&c2, q.days)).await.map_err(ApiError::internal)??))
}

/// `GET /tracks/export?format=m3u8|csv|zip`: the `/tracks` filters, unpaged. A record, artist or label comes
/// out in listening order (disc/track); anything broader newest first. Missing files are left out of m3u8 and
/// zip (a playlist entry that points nowhere just errors) but kept in csv, which is an inventory.
async fn export_tracks(State(ctx): State<Ctx>, Q(mut q): Q<TrackQuery>, Q(f): Q<ExportFormatQuery>) -> ApiResult<axum::response::Response> {
    let format = f.format;
    let (tracks_v, name) = ctx
        .read_async(move |c| {
            let scope = Scope::resolve(c, q.scope, q.source_fan_id)?;
            q.missing = if format == ExportFormat::Csv { None } else { Some(false) };
            let by_record = q.release_id.is_some() || !q.release_ids.is_empty() || q.artist_id.is_some() || q.label_id.is_some();
            if by_record {
                q.sort = Some(TrackSort::Album);
                q.order = Some(SortDir::Asc);
            } else if q.sort.is_none() && q.q.is_none() {
                q.sort = Some(TrackSort::Added);
            }
            let ids = tracks::all_ids(c, &q, &scope, None)?;
            let name = bc_plist::export::export_name(c, &q)?;
            Ok((bc_plist::export::load_export_tracks(c, &ids)?, name))
        })
        .await?;
    if tracks_v.is_empty() && format == ExportFormat::Zip {
        return Err(ApiError::bad("none of these tracks have a file on disk"));
    }
    bc_plist::export::export_response(format, &tracks_v, &name)
}

/// `POST /library/import`: run the legacy importer into the live database as a tracked task (202 + job id).
async fn start_import(State(ctx): State<Ctx>, Json(body): Json<ImportRequest>) -> ApiResult<(StatusCode, Json<bc_types::Accepted>)> {
    let empty = ctx.read_async(|c| Ok(crate::import::is_empty(c))).await?;
    if !empty && !body.force {
        return Err(ApiError::conflict("the library is not empty; pass force=true to replace it"));
    }
    let running = ctx.jobs.list().iter().any(|t| t.kind == "import" && t.state == "running");
    if running {
        return Err(ApiError::conflict("an import is already running"));
    }
    let opts = crate::import::ImportOptions { from: body.from.clone().map(Into::into), force: body.force, skip_repairs: body.skip_repairs };
    // fail fast on a bad source instead of inside the task
    if let Some(f) = opts.from.clone().or_else(|| ctx.config.legacy_db.clone()) {
        crate::import::resolve_source(&f).map_err(|e| ApiError::bad(e.to_string()))?;
    } else {
        return Err(ApiError::bad("no source: pass `from` or set BC_LEGACY_DB"));
    }
    let handle = ctx.jobs.begin("import", "import the legacy library");
    let job_id = handle.id.clone();
    let c2 = ctx.clone();
    std::thread::spawn(move || {
        let (bus, id) = (c2.bus.clone(), handle.id.clone());
        let log = |m: &str| {
            bus.publish(TOPIC_IMPORT_PROGRESS, &ImportProgress { job_id: id.clone(), message: m.to_string(), done: false, ok: None });
            handle.progress(0, None, Some(m));
        };
        match crate::import::run_import_live(&c2, &opts, &log) {
            Ok(rep) => {
                bus.publish(TOPIC_IMPORT_PROGRESS, &ImportProgress { job_id: id.clone(), message: "finished".into(), done: true, ok: Some(rep.ok) });
                handle.finish_ok(serde_json::to_value(&rep).unwrap_or_default());
            }
            Err(e) => {
                bus.publish(TOPIC_IMPORT_PROGRESS, &ImportProgress { job_id: id.clone(), message: e.to_string(), done: true, ok: Some(false) });
                handle.finish_err(e);
            }
        }
    });
    Ok((StatusCode::ACCEPTED, Json(bc_types::Accepted { job_id })))
}
