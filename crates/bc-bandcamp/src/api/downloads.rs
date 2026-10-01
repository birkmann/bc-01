//! Download submission routes (port of `api/routes/downloads.py`; the generic `/jobs*` routes are
//! served by `bc_jobs::JobsService`):
//!
//! | Method | Path | |
//! | --- | --- | --- |
//! | POST | `/downloads/parse` | validate a URL list |
//! | POST | `/downloads` | queue downloads (idempotent on `job_id`) |
//! | GET / PUT | `/downloads/disk` | disk guard state / free-space limit |

#![allow(clippy::collapsible_if)]

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use bc_core::paths::safe_subdir_name;
use bc_jobs::{ApiError, NewItem, NewJob, create_job_in};
use bc_types::bandcamp::{DownloadRequest, ParseUrlsRequest, ParsedUrls};
use bc_types::jobs::{DiskIn, DiskOut, JobOut, TOPIC_JOB_PROGRESS};
use serde_json::json;

use crate::download::dedup::{find_known, url_key};
use crate::download::diskguard;
use crate::download::library_port::{BcLibrary, LibraryPort};
use crate::download::worker::{DownloadServices, resolve_downloads_base};
use crate::service::Ctx;

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// URL handling (legacy `classify_url`: only album / track / artist-root)
// ---------------------------------------------------------------------------

/// `(scheme, netloc, path)` of a URL, the way `urllib.parse.urlparse` splits it (host case kept).
fn urlparse(raw: &str) -> (String, String, String) {
    let raw = raw.trim();
    let Some((scheme, rest)) = raw.split_once("://") else { return (String::new(), String::new(), raw.to_string()) };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let netloc = rest[..end].to_string();
    let tail = &rest[end..];
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    (scheme.to_lowercase(), netloc, tail[..path_end].to_string())
}

fn is_bandcamp_host(host: &str) -> bool {
    let h = host.to_lowercase();
    h == "bandcamp.com" || h.ends_with(".bandcamp.com")
}

/// `'album' | 'track' | 'artist'` for a Bandcamp URL, else `None`.
pub fn classify_url(raw: &str) -> Option<&'static str> {
    let (scheme, netloc, path) = urlparse(raw);
    if !matches!(scheme.as_str(), "http" | "https") || netloc.is_empty() {
        return None;
    }
    let host = netloc.split(':').next().unwrap_or("");
    // Custom domains are legitimate, so a non-bandcamp host is not disqualifying by itself --
    // the path shape is what tells us what we are looking at.
    if path.contains("/album/") {
        return Some("album");
    }
    if path.contains("/track/") {
        return Some("track");
    }
    if is_bandcamp_host(host) && matches!(path.as_str(), "" | "/" | "/music") {
        return Some("artist");
    }
    None
}

/// Strip the query noise Bandcamp links accumulate. The gen-1 bookmarklet scraped `?action=buy`
/// links; discover results carry `?from=`; label pages add `?label=`/`?tab=`. All of them address
/// the same release, so leaving them on would defeat deduplication. (Host case is kept, like the
/// legacy writer; `url_key` lowercases for comparisons.)
pub fn normalise_url(raw: &str) -> String {
    let (scheme, netloc, path) = urlparse(raw);
    format!("{scheme}://{netloc}{path}").trim_end_matches('/').to_string()
}

/// `parse_url_list`: validate, normalise and de-duplicate a pasted list.
pub fn parse_url_list(text: &str) -> ParsedUrls {
    let mut out = ParsedUrls::default();
    let mut seen: HashSet<String> = HashSet::new();
    for line in text.replace(',', "\n").lines() {
        let candidate = line.trim();
        if candidate.is_empty() || candidate.starts_with('#') || candidate.starts_with("//") {
            continue;
        }
        let Some(kind) = classify_url(candidate) else {
            out.invalid.push(candidate.to_string());
            continue;
        };
        let url = normalise_url(candidate);
        if !seen.insert(url.clone()) {
            out.duplicates += 1;
            continue;
        }
        out.valid.push(url);
        match kind {
            "album" => out.albums += 1,
            "track" => out.tracks += 1,
            _ => out.artists += 1,
        }
    }
    out
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

fn library_for(ctx: &Ctx) -> Arc<dyn LibraryPort> {
    match ctx.get::<DownloadServices>() {
        Some(s) => s.deps.library(),
        None => Arc::new(BcLibrary::new(ctx.db.clone(), ctx.bus.clone(), ctx.cfg.clone())),
    }
}

fn notify(ctx: &Ctx) {
    match ctx.get::<DownloadServices>() {
        Some(s) => s.worker.notify(),
        None => ctx.notify_downloads(),
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/downloads/parse", post(parse_urls))
        .route("/downloads", post(submit_downloads))
        .route("/downloads/disk", get(get_disk).put(put_disk))
        .with_state(ctx)
}

async fn parse_urls(State(ctx): State<Arc<Ctx>>, Json(body): Json<ParseUrlsRequest>) -> ApiResult<Json<ParsedUrls>> {
    let mut parsed = parse_url_list(&body.text);
    if !parsed.valid.is_empty() {
        let urls = parsed.valid.clone();
        let known = ctx.db.read_async(move |c| find_known(c, &urls, None)).await?;
        parsed.already_have = known.len() as i64;
    }
    Ok(Json(parsed))
}

/// Params stored on the job (what the worker reads back per item).
fn job_params(body: &DownloadRequest, subdir: Option<&str>) -> serde_json::Value {
    let mut params = json!({ "target_subdir": subdir, "force": body.force });
    if body.single_folder {
        params["layout"] = json!("flat");
    }
    if body.tracks_only {
        params["tracks_only"] = json!(true);
    }
    if let Some(name) = body.label_name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        params["label_name"] = json!(name);
        if let Some(u) = body.label_url.as_deref().filter(|u| !u.is_empty()) {
            params["label_url"] = json!(u);
        }
    }
    if let Some(fan) = body.source_fan_id {
        params["source_fan_id"] = json!(fan);
    }
    params
}

async fn submit_downloads(State(ctx): State<Arc<Ctx>>, Json(body): Json<DownloadRequest>) -> ApiResult<Json<JobOut>> {
    let parsed = parse_url_list(&body.urls.join("\n"));
    if parsed.valid.is_empty() {
        let invalid: Vec<&str> = parsed.invalid.iter().take(20).map(String::as_str).collect();
        let mut detail = "No valid Bandcamp album or track URLs found.".to_string();
        if !invalid.is_empty() {
            detail.push_str(&format!(" Invalid: {}", invalid.join(", ")));
        }
        return Err(ApiError::bad_request(detail));
    }

    if let Some(id) = body.job_id.clone() {
        if let Some(existing) = ctx.jobs.store().run(move |s| s.get_job(&id)).await? {
            return Ok(Json(existing.to_out()));
        }
    }

    let subdir = body.target_subdir.as_deref().filter(|s| !s.is_empty()).map(safe_subdir_name);
    let label = body
        .label
        .clone()
        .filter(|l| !l.is_empty())
        .or_else(|| subdir.clone())
        .unwrap_or_else(|| format!("{} downloads", parsed.valid.len()));
    let params = job_params(&body, subdir.as_deref());

    // Items already in the library are created (so the job stays a faithful record of what was
    // pasted) but skipped up front -- the downloader never runs.
    //
    // Checked even under `force`, because the two reasons are not the same thing: force means
    // "fetch what I own again", and a blacklisted release is one the user deliberately threw
    // away. The worker's preflight draws the same distinction; doing it here as well keeps the
    // job an honest record and saves the work.
    //
    // Looked up before the job exists, and the skip lands in the same transaction that creates
    // it: the worker wakes on every new job, and it must not claim the very items we skip.
    let urls = parsed.valid.clone();
    let known = ctx.db.read_async(move |c| find_known(c, &urls, None)).await?;
    let blocked: HashSet<String> = known.iter().filter(|(_, r)| r.as_str() == "blacklist").map(|(k, _)| k.clone()).collect();
    if !known.is_empty() && body.source_fan_id.is_none() {
        // A personal request for a record that sits on someone else's shelf moves it into my
        // library: "already have it" is only an answer when it is where I can see it.
        let adoptable: Vec<String> =
            parsed.valid.iter().filter(|u| known.contains_key(&url_key(u)) && !blocked.contains(&url_key(u))).cloned().collect();
        if !adoptable.is_empty() {
            if let Err(e) = library_for(&ctx).adopt_for_urls(&adoptable).await {
                tracing::warn!("could not adopt already-known releases: {e}");
            }
        }
    }

    let items: Vec<NewItem> = parsed
        .valid
        .iter()
        .map(|u| {
            let mut it = NewItem::url(u.clone(), classify_url(u).unwrap_or("album"));
            it.target_dir = subdir.clone();
            it
        })
        .collect();
    let mut new_job = NewJob::new("download", items).label(label).priority(body.priority).params(params);
    if let Some(id) = body.job_id.clone() {
        new_job = new_job.id(id);
    }

    let owned: HashSet<String> = if body.force { HashSet::new() } else { known.keys().filter(|k| !blocked.contains(*k)).cloned().collect() };
    let job = ctx
        .db
        .write_async(move |tx| {
            let job = create_job_in(tx, &new_job)?;
            let now = bc_jobs::time::now();
            let rows: Vec<(i64, Option<String>)> = {
                let mut st = tx.prepare("SELECT id, url FROM job_items WHERE job_id = ?1")?;
                st.query_map([&job.id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
            };
            let mut skipped = 0i64;
            for (keys, message) in [(&blocked, "Blacklisted — skipped"), (&owned, "Already in library — skipped")] {
                for (id, url) in &rows {
                    if url.as_deref().is_some_and(|u| keys.contains(&url_key(u))) {
                        let n = tx.execute(
                            "UPDATE job_items SET status = 'skipped', progress = 1.0, message = ?2, finished_at = ?3 \
                             WHERE id = ?1 AND status = 'pending'",
                            bc_db::rusqlite::params![id, message, now],
                        )?;
                        skipped += n as i64;
                    }
                }
            }
            if skipped > 0 {
                tx.execute("UPDATE jobs SET skipped = skipped + ?2 WHERE id = ?1", bc_db::rusqlite::params![job.id, skipped])?;
                let open: i64 = tx.query_row(
                    "SELECT count(*) FROM job_items WHERE job_id = ?1 AND status IN ('pending','running')",
                    [&job.id],
                    |r| r.get(0),
                )?;
                if open == 0 {
                    tx.execute("UPDATE jobs SET status = 'completed', finished_at = ?2 WHERE id = ?1", bc_db::rusqlite::params![job.id, now])?;
                }
            }
            Ok(job.id)
        })
        .await?;
    let job_id = job;
    let job = ctx
        .jobs
        .store()
        .run({
            let id = job_id.clone();
            move |s| s.get_job(&id)
        })
        .await?
        .ok_or_else(|| ApiError::internal("the job vanished right after it was created"))?;

    notify(&ctx);
    // `job.created`, then (when anything was skipped) a progress frame so the UI sees the skips.
    ctx.jobs.store().announce_created(&job);
    if job.skipped > 0 {
        ctx.bus.publish(TOPIC_JOB_PROGRESS, &job.progress_event());
    }
    Ok(Json(job.to_out()))
}

fn disk_out(ctx: &Ctx) -> DiskOut {
    if let Some(s) = ctx.get::<DownloadServices>() {
        let st = s.worker.disk_state();
        return DiskOut { path: st.path, free_bytes: st.free_bytes, min_free_bytes: st.min_free_bytes, held: st.held };
    }
    let base = resolve_downloads_base(&ctx.db, &ctx.cfg);
    DiskOut {
        path: base.to_string_lossy().into_owned(),
        free_bytes: diskguard::free_bytes(&base),
        min_free_bytes: diskguard::read_min_free_db(&ctx.db).unwrap_or(diskguard::DEFAULT_MIN_FREE_BYTES),
        held: false,
    }
}

async fn get_disk(State(ctx): State<Arc<Ctx>>) -> ApiResult<Json<DiskOut>> {
    let out = tokio::task::spawn_blocking(move || disk_out(&ctx)).await.map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(out))
}

async fn put_disk(State(ctx): State<Arc<Ctx>>, Json(body): Json<DiskIn>) -> ApiResult<Json<DiskOut>> {
    if body.min_free_bytes < 0 {
        return Err(ApiError::new(422, "Unprocessable Entity").detail("min_free_bytes must be >= 0"));
    }
    diskguard::write_min_free_async(&ctx.db, body.min_free_bytes).await?;
    let c2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || {
        if let Some(s) = c2.get::<DownloadServices>() {
            // Re-read now, so the answer reflects the new limit and a hold lifts (or lands)
            // without waiting for the loop's next look.
            s.worker.reread_disk();
            s.worker.notify();
        }
        disk_out(&c2)
    })
    .await
    .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(out))
}
