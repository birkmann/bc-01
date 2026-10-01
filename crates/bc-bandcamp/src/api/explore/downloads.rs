//! `POST /explore/download` and `POST /explore/download/catalog`.
//!
//! They create download jobs through the same logic as `POST /downloads` (legacy
//! `submit_downloads`) but directly on the job store, so this module does not depend on the
//! downloads router: known URLs are created-and-skipped up front in the same transaction as the
//! job (the worker cannot claim an item before its skip lands), blacklisted ones are skipped
//! even when forced, and a personal request for a record on someone else's shelf adopts it.

use std::collections::HashSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use bc_db::rusqlite::params;
use bc_jobs::{ApiError, NewItem, NewJob};
use bc_types::bandcamp::{CatalogResult, DownloadCatalogRequest, DownloadReleasesRequest};
use bc_types::jobs::{JobOut, KIND_DOWNLOAD, TOPIC_JOB_PROGRESS};

use super::common::{bandcamp_url, known_releases, unprocessable};
use crate::download::dedup::{file_known_releases_under_label_async, find_known, url_key};
use crate::service::Ctx;
use crate::{sources, urls};

/// What `submit_downloads` takes (the subset the explore routes use).
#[derive(Debug, Clone, Default)]
pub struct QueueRequest {
    pub urls: Vec<String>,
    pub target_subdir: Option<String>,
    pub label: Option<String>,
    pub label_name: Option<String>,
    pub label_url: Option<String>,
    pub source_fan_id: Option<i64>,
    pub force: bool,
}

/// `submit_downloads`: create a `download` job for the album/track URLs in `req`, with the ones
/// the library already holds (or that are blacklisted) skipped up front.
pub async fn queue_downloads(ctx: &Arc<Ctx>, req: QueueRequest) -> Result<JobOut, ApiError> {
    let mut valid: Vec<String> = Vec::new();
    let mut invalid: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for raw in &req.urls {
        let candidate = urls::normalise(&urls::coerce(raw));
        if matches!(urls::classify(&candidate), urls::UrlKind::Album | urls::UrlKind::Track) {
            if seen.insert(candidate.clone()) {
                valid.push(candidate);
            }
        } else {
            invalid.push(raw.clone());
        }
    }
    if valid.is_empty() {
        let shown: Vec<_> = invalid.iter().take(20).cloned().collect();
        return Err(ApiError::bad_request(format!("No valid Bandcamp album or track URLs found. Invalid: {shown:?}")));
    }

    let subdir = req.target_subdir.as_deref().filter(|s| !s.is_empty()).map(bc_core::paths::safe_subdir_name);
    let label = req.label.clone().filter(|l| !l.is_empty()).or_else(|| subdir.clone()).unwrap_or_else(|| format!("{} downloads", valid.len()));

    let mut params_json = serde_json::json!({"target_subdir": subdir, "force": req.force});
    if let Some(name) = req.label_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        params_json["label_name"] = name.into();
        if let Some(u) = req.label_url.as_deref().filter(|u| !u.is_empty()) {
            params_json["label_url"] = u.into();
        }
    }
    if let Some(f) = req.source_fan_id {
        params_json["source_fan_id"] = f.into();
    }

    let nj = NewJob::new(
        KIND_DOWNLOAD,
        valid
            .iter()
            .map(|u| NewItem { url: Some(u.clone()), url_kind: Some(urls::classify(u).as_str().to_string()), target_dir: subdir.clone(), ..Default::default() })
            .collect(),
    )
    .label(label)
    .params(params_json);
    let (force, source_fan_id) = (req.force, req.source_fan_id);
    let urls_in = valid.clone();

    // Looked up and skipped inside the job's own transaction: the worker wakes on every new job,
    // and the skip has to land before it can claim the very items it skips.
    let (job_id, skipped) = ctx
        .db
        .write_async(move |tx| {
            let to_db = |e: bc_libcore::ApiError| bc_db::DbError::Other(e.to_string());
            let known = find_known(tx, &urls_in, None).map_err(|e| bc_db::DbError::Other(e.to_string()))?;
            let blocked: HashSet<&String> = known.iter().filter(|(_, r)| r.as_str() == "blacklist").map(|(k, _)| k).collect();
            if !known.is_empty() && source_fan_id.is_none() {
                // A personal request for a record that sits on someone else's shelf moves it
                // into my library: "already have it" is only an answer when it is where I can
                // see it.
                let owned: Vec<&String> = urls_in.iter().filter(|u| known.contains_key(&url_key(u)) && !blocked.contains(&url_key(u))).collect();
                if !owned.is_empty() {
                    bc_maint::adopt::adopt_for_urls(tx, &owned).map_err(to_db)?;
                }
            }
            let job = bc_jobs::create_job_in(tx, &nj).map_err(|e| bc_db::DbError::Other(e.to_string()))?;
            let mut skipped = 0i64;
            if !known.is_empty() {
                let now = bc_jobs::time::now();
                for (seq, u) in urls_in.iter().enumerate() {
                    let key = url_key(u);
                    let message = if blocked.contains(&key) {
                        "Blacklisted — skipped"
                    } else if known.contains_key(&key) && !force {
                        "Already in library — skipped"
                    } else {
                        continue;
                    };
                    skipped += tx.execute(
                        "UPDATE job_items SET status = 'skipped', progress = 1.0, message = ?3, finished_at = ?4 WHERE job_id = ?1 AND seq = ?2",
                        params![job.id, seq as i64, message, now],
                    )? as i64;
                }
                if skipped > 0 {
                    tx.execute("UPDATE jobs SET skipped = ?2 WHERE id = ?1", params![job.id, skipped])?;
                    if skipped as usize == urls_in.len() {
                        tx.execute("UPDATE jobs SET status = 'completed', finished_at = ?2 WHERE id = ?1", params![job.id, now])?;
                    }
                }
            }
            Ok((job.id, skipped))
        })
        .await?;

    let store = ctx.jobs.store();
    let job = store.get_job(&job_id)?.ok_or_else(|| ApiError::internal("job vanished after creation"))?;
    store.announce_created(&job);
    if skipped > 0 {
        ctx.bus.publish(TOPIC_JOB_PROGRESS, &job.progress_event());
    }
    Ok(job.to_out())
}

/// `POST /explore/download`: queue one or more releases straight from a browse view.
pub async fn download_releases(State(ctx): State<Arc<Ctx>>, Json(body): Json<DownloadReleasesRequest>) -> Result<Json<JobOut>, ApiError> {
    if body.urls.len() > 500 {
        return Err(unprocessable("urls: at most 500 items"));
    }
    if body.urls.is_empty() {
        return Err(ApiError::bad_request("no releases to queue"));
    }
    let validated = body.urls.iter().map(|u| bandcamp_url(u, "url")).collect::<Result<Vec<_>, _>>()?;
    let label = body.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| format!("{} from Explore", validated.len()));
    let job = queue_downloads(
        &ctx,
        QueueRequest {
            urls: validated,
            target_subdir: body.target_subdir,
            label: Some(label),
            label_name: body.label_name,
            label_url: body.label_url,
            source_fan_id: body.source_fan_id,
            force: false,
        },
    )
    .await?;
    Ok(Json(job))
}

/// `POST /explore/download/catalog`: every release an artist or label has published, as one job.
///
/// Only *offered* audio actually lands: free and name-your-price releases, and purchased ones
/// with a cookie. Paid releases you do not own fail their items rather than silently succeed,
/// which is why the result reports what was queued rather than promising a catalogue.
pub async fn download_catalog(State(ctx): State<Arc<Ctx>>, Json(body): Json<DownloadCatalogRequest>) -> Result<Json<CatalogResult>, ApiError> {
    if !(1..=2000).contains(&body.limit) {
        return Err(unprocessable("limit: must be between 1 and 2000"));
    }
    let limit = body.limit as usize;
    let target = bandcamp_url(&body.url, "url")?;
    let page = sources::fetch_band_page(&ctx.client, &target, false).await?;

    let mut found = page.releases.clone();
    let truncated = page.tier == crate::extract::Tier::Css || found.len() > limit;
    found.truncate(limit);

    let mut candidates: Vec<String> = found.iter().map(|i| i.page_url.clone()).collect();
    let mut skipped_not_free = 0i64;
    if body.free_only {
        let mut kept = Vec::new();
        for url in &candidates {
            match sources::fetch_release(&ctx.client, url).await {
                Ok(d) if d.is_free_download => kept.push(url.clone()),
                Ok(_) => skipped_not_free += 1,
                Err(e) => tracing::debug!("free_only probe failed for {url}: {e}"),
            }
        }
        candidates = kept;
    }

    // Same question as the badges answer, so the same matching: a catalogue run must not
    // re-queue the half of a label that is already on the shelf under no URL at all.
    let name_of = |url: &str| -> (String, String) {
        found
            .iter()
            .find(|i| i.page_url == url)
            .map(|i| (if i.artist.is_empty() { page.profile.name.clone() } else { i.artist.clone() }, i.title.clone()))
            .unwrap_or_default()
    };
    let items: Vec<(String, String, String)> = candidates
        .iter()
        .map(|u| {
            let (a, t) = name_of(u);
            (u.clone(), a, t)
        })
        .collect();
    let known = known_releases(&ctx.db, &items).await?;
    let queueable: Vec<String> = candidates.iter().filter(|u| !known.contains_key(&url_key(u))).cloned().collect();
    let skipped_in_library = (candidates.len() - queueable.len()) as i64;

    let name = if page.profile.name.is_empty() { urls::display_name(&target) } else { page.profile.name.clone() };
    let is_label = page.profile.is_label;
    let label_url = if page.profile.url.is_empty() { target.clone() } else { page.profile.url.clone() };

    // A label's catalogue names the label for every release on it -- including the ones already
    // on the shelf, which downloading again would only skip. Filing them here is what lets a
    // re-run of this button label a library that was downloaded before labels were recorded.
    let mut filed = 0usize;
    if is_label {
        let entries = found
            .iter()
            .map(|i| (i.page_url.clone(), if i.artist.is_empty() { name.clone() } else { i.artist.clone() }, i.title.clone()))
            .collect();
        filed = file_known_releases_under_label_async(&ctx.db, name.clone(), Some(label_url.clone()), entries).await?;
    }

    if queueable.is_empty() {
        let mut detail = if skipped_in_library > 0 { "everything here is already in the library" } else { "nothing to queue" }.to_string();
        if filed > 0 {
            detail.push_str(&format!(" — filed {filed} under {name}"));
        }
        return Ok(Json(CatalogResult {
            job: None,
            band: name,
            found: found.len() as i64,
            queued: 0,
            skipped_in_library,
            skipped_not_free,
            truncated,
            detail,
        }));
    }

    let subdir = body.target_subdir.clone().unwrap_or_else(|| bc_core::paths::safe_subdir_name(&name));
    let job = queue_downloads(
        &ctx,
        QueueRequest {
            urls: queueable.clone(),
            target_subdir: Some(subdir),
            label: Some(format!("{name} — full catalogue")),
            label_name: is_label.then(|| name.clone()),
            label_url: is_label.then(|| label_url.clone()),
            ..Default::default()
        },
    )
    .await?;

    Ok(Json(CatalogResult {
        job: Some(job),
        band: name,
        found: found.len() as i64,
        queued: queueable.len() as i64,
        skipped_in_library,
        skipped_not_free,
        truncated,
        detail: if truncated { "the discography grid was truncated — some releases may be missing".into() } else { String::new() },
    }))
}
