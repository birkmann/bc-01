use std::collections::HashMap;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use bc_libcore::queue::{self, NewItem};
use bc_libcore::{ApiError, ApiResult, Ctx, Q};
use bc_types::library::{LovedAutoOut, LovedDownloadOut, LovedStreamIn, LovedStreamOut, TOPIC_LOVED_RECONCILED};
use serde::Deserialize;
use serde_json::json;

use super::{MaintState, blocking};
use crate::dedup::find_known_ids;
use crate::loved::{self, LovedStream, ReconcileReport};
use crate::urls::url_key;
use crate::{completeness, lookup::BandcampLookup};

/// `GET /loved-streams`: tracks loved straight off Bandcamp, newest first. Separate from
/// `/tracks?loved=true` because these have no library row at all.
pub async fn list(State(s): State<MaintState>) -> ApiResult<Json<Vec<LovedStreamOut>>> {
    let ctx = s.ctx.clone();
    let rows = blocking(move || ctx.read(loved::list)).await?;
    Ok(Json(rows.iter().map(LovedStream::out).collect()))
}

/// `POST /loved-streams`: idempotent. With auto-download on, a newly loved stream also queues its
/// album (best-effort: loving a track must succeed even when Bandcamp is unreachable).
pub async fn love(State(s): State<MaintState>, Json(body): Json<LovedStreamIn>) -> ApiResult<Json<LovedStreamOut>> {
    let ctx = s.ctx.clone();
    let (row, is_new, auto) = blocking(move || {
        ctx.write(move |t| {
            let (row, is_new) = loved::love(t, &body)?;
            Ok((row, is_new, loved::auto_download(t)?))
        })
    })
    .await?;
    if is_new && auto {
        auto_download(&s, &row.page_url).await;
    }
    Ok(Json(row.out()))
}

async fn auto_download(s: &MaintState, page_url: &str) {
    let Some(lookup) = s.lookup.clone() else { return };
    let album = match lookup.resolve_album_url(page_url).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(page_url, error = %e, "auto-download: could not resolve the album");
            return;
        }
    };
    let ctx = s.ctx.clone();
    if let Err(e) = blocking(move || queue_albums(&ctx, &[album], "loved: 1 album").map(|_| ())).await {
        tracing::warn!(page_url, error = %e, "auto-download failed");
    }
}

#[derive(Debug, Deserialize)]
pub struct UnloveQuery {
    pub page_url: String,
    pub track_key: String,
}

/// `DELETE /loved-streams?page_url=&track_key=`: keyed by the pair rather than the row id, so the
/// player can un-love what it is playing without having looked the row up first.
pub async fn unlove(State(s): State<MaintState>, Q(q): Q<UnloveQuery>) -> ApiResult<StatusCode> {
    let ctx = s.ctx.clone();
    if blocking(move || ctx.write(move |t| loved::unlove(t, &q.page_url, &q.track_key))).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("that stream is not loved"))
    }
}

pub async fn get_auto(State(s): State<MaintState>) -> ApiResult<Json<LovedAutoOut>> {
    let ctx = s.ctx.clone();
    Ok(Json(LovedAutoOut { enabled: blocking(move || ctx.read(loved::auto_download)).await? }))
}

/// Persisted in the database rather than the browser: this drives a server-side action, so it has
/// to hold for a love that arrives from anywhere.
pub async fn set_auto(State(s): State<MaintState>, Json(body): Json<LovedAutoOut>) -> ApiResult<Json<LovedAutoOut>> {
    let ctx = s.ctx.clone();
    blocking(move || ctx.write(move |t| loved::set_auto_download(t, body.enabled))).await?;
    Ok(Json(body))
}

/// Create a download job for whatever the library does not already hold: the same `find_known`
/// check the downloads route uses, so an album already on the shelf -- or one deliberately
/// blacklisted -- is never fetched again. Returns `(job id and item count, skipped)`.
pub fn queue_albums(ctx: &Ctx, urls: &[String], label: &str) -> ApiResult<(Option<(String, i64)>, i64)> {
    let urls = urls.to_vec();
    let label = label.to_string();
    let res = ctx.write(move |t| {
        let known = find_known_ids(t, &urls, None)?;
        let pending: Vec<&String> = urls.iter().filter(|u| !known.contains_key(&url_key(u))).collect();
        let skipped = (urls.len() - pending.len()) as i64;
        if pending.is_empty() {
            return Ok((None, skipped));
        }
        let items: Vec<NewItem> = pending
            .iter()
            .map(|u| NewItem { url: Some((*u).clone()), url_kind: Some("album".into()), source: Some("loved".into()), ..Default::default() })
            .collect();
        // Ahead of a harvest backfill but behind a hand-pasted download: a batch the user started
        // and is watching drain, not a sweep.
        let id = queue::create_job(t, "download", &label, completeness::PRIORITY_SINGLE, &json!({"target_subdir": null, "force": false}), &items)?;
        Ok((Some((id, items.len() as i64)), skipped))
    })?;
    if res.0.is_some() {
        ctx.jobs.notify_download_queue();
    }
    Ok(res)
}

fn announce(ctx: &Ctx, r: &ReconcileReport) {
    if r.streams_cleared > 0 {
        ctx.bus.publish(TOPIC_LOVED_RECONCILED, &json!({"track_ids": r.track_ids, "stream_ids": r.stream_ids}));
        ctx.bus.invalidate("track", r.track_ids.clone());
    }
}

/// `POST /loved-streams/download`: complete the collection by fetching the record behind each
/// loved stream.
///
/// Whole albums, not single tracks. A `/track/` URL is resolved to its parent album first; a
/// standalone single resolves to itself. The reconcile sweep runs *before* queueing, so streams
/// whose album is already on disk convert into loved library tracks and are never queued. A stream
/// loved from a `/track/` page adopts through its resolved album URL (its own URL and title name
/// the track, not the album). When the album row is here but the loved track is missing from it
/// (a partial rip), the release is queued as a fill rather than counted as owned. Blacklisted
/// albums are skipped; pressing this twice still costs nothing. A page that cannot be resolved
/// counts as skipped instead of failing the whole request.
pub async fn download(State(s): State<MaintState>) -> ApiResult<Json<LovedDownloadOut>> {
    let lookup = s.lookup()?.clone();
    let ctx = s.ctx.clone();

    // Anything already downloaded becomes a loved track here, not a download.
    let c2 = ctx.clone();
    let adopted = blocking(move || c2.write(loved::reconcile_all)).await?;
    announce(&ctx, &adopted);

    let c2 = ctx.clone();
    let rows = blocking(move || c2.read(loved::list)).await?;
    if rows.is_empty() {
        return Ok(Json(LovedDownloadOut {
            queued: 0,
            already_owned: adopted.streams_cleared,
            skipped: 0,
            resolved: 0,
            job_id: None,
            detail: "every loved stream is in the library".into(),
        }));
    }

    // One resolve per stream, deduplicated: several loved tracks off the same record are one album
    // to fetch. Each album keeps the streams behind it for the adoption below.
    let (mut resolved, mut skipped) = (0i64, 0i64);
    let mut albums: Vec<(String, Vec<LovedStream>)> = Vec::new();
    for row in rows {
        let target = match resolve(&*lookup, &row.page_url).await {
            Some(t) => t,
            None => {
                skipped += 1;
                continue;
            }
        };
        if target != row.page_url {
            resolved += 1;
        }
        match albums.iter_mut().find(|(u, _)| *u == target) {
            Some((_, v)) => v.push(row),
            None => albums.push((target, vec![row])),
        }
    }

    let c2 = ctx.clone();
    let urls: Vec<String> = albums.iter().map(|(u, _)| u.clone()).collect();
    let known = blocking(move || c2.read(|c| find_known_ids(c, &urls, None))).await?;
    let mut to_queue: Vec<String> = Vec::new();
    let mut pairs: Vec<(LovedStream, i64)> = Vec::new();
    let mut fallback: HashMap<i64, String> = HashMap::new();
    for (url, streams) in &albums {
        match known.get(&url_key(url)) {
            Some((reason, _)) if reason == "blacklist" => skipped += 1,
            None => to_queue.push(url.clone()),
            // Known from job history alone, with no release row to adopt into.
            Some((_, None)) => skipped += 1,
            Some((_, Some(rid))) => {
                fallback.insert(*rid, url.clone());
                pairs.extend(streams.iter().map(|st| (st.clone(), *rid)));
            }
        }
    }

    let c2 = ctx.clone();
    let (adopted_now, unadopted) = blocking(move || c2.write(move |t| loved::adopt_streams(t, &pairs))).await?;
    announce(&ctx, &adopted_now);

    let mut fills = Vec::new();
    let mut filling = std::collections::HashSet::new();
    for (_, rid) in unadopted {
        if !filling.insert(rid) {
            continue;
        }
        let (c2, fb) = (ctx.clone(), fallback.get(&rid).cloned());
        match blocking(move || completeness::queue_fill(&c2, rid, fb.as_deref())).await {
            Ok(Some(f)) => fills.push(f),
            // No link to Bandcamp, or a pre-order whose missing tracks are not out yet.
            Ok(None) | Err(bc_libcore::ApiError::Conflict(_)) => skipped += 1,
            Err(e) => return Err(e),
        }
    }

    let (c2, label) = (ctx.clone(), format!("loved: {} album(s)", to_queue.len()));
    let (job, queue_skipped) = blocking(move || queue_albums(&c2, &to_queue, &label)).await?;
    skipped += queue_skipped;

    let queued = job.as_ref().map(|(_, n)| *n).unwrap_or(0) + fills.len() as i64;
    let already_owned = adopted.streams_cleared + adopted_now.streams_cleared;
    if queued == 0 {
        return Ok(Json(LovedDownloadOut { queued: 0, already_owned, skipped, resolved, job_id: None, detail: "nothing left to download".into() }));
    }
    Ok(Json(LovedDownloadOut {
        queued,
        already_owned,
        skipped,
        resolved,
        job_id: job.map(|(id, _)| id).or_else(|| fills.first().map(|f| f.job_id.clone())),
        detail: "queued".into(),
    }))
}

async fn resolve(lookup: &dyn BandcampLookup, url: &str) -> Option<String> {
    match lookup.resolve_album_url(url).await {
        Ok(u) => Some(u),
        Err(e) => {
            tracing::info!(url, error = %e, "loved download: page could not be resolved");
            None
        }
    }
}
