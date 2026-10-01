use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use bc_libcore::{ApiError, ApiResult, Q, hydrate};
use bc_types::library::{BlacklistAdd, BlacklistOut, BlacklistQuery, CleanupCandidate, CleanupOut, CleanupQuery, Page};

use super::{MaintState, blocking};
use crate::{blacklist, cleanup};

/// `GET /cleanup/candidates`: releases flagged as sample packs or preview stubs, shortest track
/// first. Two independent signals reported per release; nothing is deleted and nothing is
/// pre-selected on the strength of a title alone.
pub async fn candidates(State(s): State<MaintState>, Q(q): Q<CleanupQuery>) -> ApiResult<Json<CleanupOut>> {
    let max_track_s = q.max_track_s.unwrap_or(60);
    if !(1..=600).contains(&max_track_s) {
        return Err(ApiError::bad("max_track_s must be between 1 and 600"));
    }
    let include_titles = q.include_titles.unwrap_or(true);
    let ctx = s.ctx.clone();
    let out = blocking(move || {
        ctx.read(|c| {
            let found = cleanup::find_candidates(c, max_track_s * 1000, include_titles)?;
            let ids: Vec<i64> = found.iter().map(|f| f.release_id).collect();
            let releases = hydrate::releases_out(c, &ids)?;
            let items = found
                .into_iter()
                .zip(releases)
                .map(|(f, release)| CleanupCandidate {
                    release,
                    longest_ms: f.longest_ms,
                    track_count: f.track_count,
                    reasons: f.reasons,
                    matched_phrases: f.matched_phrases,
                })
                .collect::<Vec<_>>();
            Ok(CleanupOut { total: items.len() as i64, items, max_track_s })
        })
    })
    .await?;
    Ok(Json(out))
}

pub async fn list_blacklist(State(s): State<MaintState>, Q(q): Q<BlacklistQuery>) -> ApiResult<Json<Page<BlacklistOut>>> {
    let offset = q.offset.unwrap_or(0);
    let limit = q.limit.unwrap_or(200);
    if offset < 0 || !(1..=500).contains(&limit) {
        return Err(ApiError::bad("offset must be >= 0 and limit between 1 and 500"));
    }
    let ctx = s.ctx.clone();
    let (rows, total) = blocking(move || ctx.read(|c| blacklist::listing(c, q.q.as_deref(), offset, limit))).await?;
    Ok(Json(Page { items: rows.iter().map(|e| e.out()).collect(), total, offset, limit }))
}

/// `POST /blacklist`: block a release by URL, by name, or both. At least one key must be usable,
/// otherwise the entry could never match anything and would sit in the list as a lie.
pub async fn add_blacklist(State(s): State<MaintState>, Json(body): Json<BlacklistAdd>) -> ApiResult<Json<BlacklistOut>> {
    let url = body.url.as_deref().map(str::trim).filter(|u| !u.is_empty()).map(str::to_string);
    let (artist, title) = (body.artist_name.trim().to_string(), body.title.trim().to_string());
    if url.is_none() && (artist.is_empty() || title.is_empty()) {
        return Err(ApiError::bad("give a Bandcamp URL, or both an artist and a title"));
    }
    let reason = body.reason.filter(|r| !r.is_empty()).unwrap_or_else(|| "manual".into());
    let ctx = s.ctx.clone();
    let entry = blocking(move || {
        ctx.write(move |t| {
            let e = blacklist::add(t, url.as_deref(), &artist, &title, Some(&reason))?;
            blacklist::ignore_matching_inbox(t, std::slice::from_ref(&e))?;
            Ok(e)
        })
    })
    .await?;
    Ok(Json(entry.out()))
}

/// `DELETE /blacklist/{id}`: stop blocking this release. Inbox rows retired when it was
/// blacklisted stay `ignored` (un-blocking says "may be downloaded again", not "queue it now").
pub async fn remove_blacklist(State(s): State<MaintState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    let ctx = s.ctx.clone();
    if blocking(move || ctx.write(move |t| blacklist::remove(t, id))).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("blacklist entry {id} not found")))
    }
}
