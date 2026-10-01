use axum::Json;
use axum::extract::{Path, State};
use bc_libcore::ApiResult;
use bc_types::library::{AdoptRequest, AdoptResult, FillAllResult, FillResult, LibraryChanged, ReleaseAvailability, TOPIC_LIBRARY_CHANGED, TrackAvailability};

use super::{MaintState, blocking};
use crate::{adopt, completeness};

/// `POST /releases/adopt`: move releases (or a whole fan shelf) into my library.
pub async fn adopt(State(s): State<MaintState>, Json(body): Json<AdoptRequest>) -> ApiResult<Json<AdoptResult>> {
    let ctx = s.ctx.clone();
    let moved = blocking(move || {
        ctx.write(move |t| {
            let mut moved = 0;
            if let Some(f) = body.fan_id {
                moved += adopt::adopt_fan(t, f)?;
            }
            if !body.ids.is_empty() {
                moved += adopt::adopt_releases(t, &body.ids)?;
            }
            Ok(moved)
        })
    })
    .await?;
    if moved > 0 {
        s.ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &LibraryChanged { adopted: Some(moved as i64), ..Default::default() });
    }
    Ok(Json(AdoptResult { adopted: moved as i64 }))
}

/// `POST /releases/{id}/fill`: queue the whole record again with `force` -- the one-click answer
/// to "4/12 tracks". 409 when nothing ever linked the release to Bandcamp.
pub async fn fill_one(State(s): State<MaintState>, Path(id): Path<i64>) -> ApiResult<Json<FillResult>> {
    let ctx = s.ctx.clone();
    Ok(Json(blocking(move || completeness::fill_release(&ctx, id)).await?))
}

/// `POST /releases/fill`: every release short of tracks, one job per shelf.
pub async fn fill_all(State(s): State<MaintState>) -> ApiResult<Json<FillAllResult>> {
    let ctx = s.ctx.clone();
    Ok(Json(blocking(move || completeness::fill_all(&ctx)).await?))
}

/// `GET /releases/{id}/availability`: what Bandcamp has out for the record right now. The cached row
/// answers while it is fresh (under 12 h and, for a pre-order, before its release date); otherwise
/// the album page is read through the rate-limited client, stored and returned (`fetched: true`).
/// `null` for a release with no album page to read. A failed read falls back to the stale row.
pub async fn availability(State(s): State<MaintState>, Path(id): Path<i64>) -> ApiResult<Json<Option<ReleaseAvailability>>> {
    let ctx = s.ctx.clone();
    let probe = blocking(move || {
        ctx.read(move |c| {
            if !bc_libcore::hydrate::exists(c, "releases", id)? {
                return Err(bc_libcore::ApiError::not_found(format!("release {id} not found")));
            }
            let url = completeness::fill_source_url(c, id)?.filter(|u| !crate::urls::is_track(u));
            let fresh = if url.is_some() { bc_libcore::availability::is_fresh(c, id)? } else { false };
            Ok((url, fresh, bc_libcore::availability::load(c, id)?))
        })
    })
    .await?;
    let (url, fresh, cached) = probe;
    Ok(Json(resolve_availability(&s, id, url, fresh, cached).await?))
}

async fn resolve_availability(
    s: &MaintState,
    id: i64,
    url: Option<String>,
    fresh: bool,
    cached: Option<ReleaseAvailability>,
) -> ApiResult<Option<ReleaseAvailability>> {
    let Some(url) = url else { return Ok(None) };
    if fresh {
        return Ok(cached);
    }
    let lookup = match s.lookup() {
        Ok(l) => l.clone(),
        Err(e) => return if cached.is_some() { Ok(cached) } else { Err(e) },
    };
    let album = match lookup.fetch_album(&url).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(release_id = id, %url, "availability lookup failed: {e}");
            return if cached.is_some() { Ok(cached) } else { Err(bc_libcore::ApiError::conflict(format!("Bandcamp could not be read: {e}"))) };
        }
    };
    let fresh_row = ReleaseAvailability {
        release_id: id,
        checked_at: String::new(),
        is_preorder: album.is_preorder,
        release_date: album.release_date.clone(),
        tracks: album
            .tracks
            .iter()
            .map(|t| TrackAvailability { track_num: t.track_num, title: t.title.clone(), duration_sec: t.duration_sec, available: t.available })
            .collect(),
        fetched: true,
    };
    let ctx = s.ctx.clone();
    let mut stored = blocking(move || ctx.write(move |t| bc_libcore::availability::store(t, &fresh_row))).await?;
    stored.fetched = true;
    Ok(Some(stored))
}
