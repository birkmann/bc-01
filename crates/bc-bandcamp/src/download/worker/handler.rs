//! The per-item handler (`_run_item_inner` and friends).

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bc_core::paths::safe_subdir_name;
use bc_db::Db;
use bc_jobs::{Complete, HandlerOutcome, ItemCtx, JobItem, NewItem};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::staging::{merge_staging, staging_dir};
use super::{DownloadDeps, MISSING_BINARY_SUFFIX};
use crate::download::bcdl::is_downloadable;
use crate::download::dedup::{find_known, url_key};
use crate::download::slug::FLAT_TEMPLATE;
use crate::download::{DownloadSpec, OutcomeKind, Progress};
use crate::error::HarvestError;
use crate::harvest::inbox;
use crate::urls::{UrlKind, classify, display_name, normalise};

/// A job's params, as the worker reads them.
#[derive(Debug, Clone, Default)]
pub(super) struct Params {
    pub force: bool,
    pub flat: bool,
    pub tracks_only: bool,
    pub label_name: Option<String>,
    pub label_url: Option<String>,
    pub source_fan_id: Option<i64>,
}

fn int_or_none(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn non_empty_str(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

impl Params {
    pub(super) fn parse(raw: &str) -> Self {
        let v: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
        let truthy = |k: &str| match v.get(k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => !s.is_empty(),
            _ => false,
        };
        Self {
            force: truthy("force"),
            flat: v.get("layout").and_then(Value::as_str) == Some("flat"),
            tracks_only: truthy("tracks_only"),
            label_name: non_empty_str(v.get("label_name")),
            label_url: non_empty_str(v.get("label_url")),
            source_fan_id: int_or_none(v.get("source_fan_id")),
        }
    }
}

/// Run a phase that has no cancel token of its own: `None` when the item was interrupted meanwhile.
async fn cancellable<T>(cancel: &CancellationToken, fut: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        v = fut => Some(v),
    }
}

fn failed(error: impl Into<String>, class: &str, retryable: bool) -> HandlerOutcome {
    HandlerOutcome::Failed { error: error.into(), class: class.to_string(), retryable }
}

pub struct DownloadHandler {
    deps: Arc<DownloadDeps>,
}

impl DownloadHandler {
    pub fn new(deps: Arc<DownloadDeps>) -> Self {
        Self { deps }
    }

    pub fn deps(&self) -> &Arc<DownloadDeps> {
        &self.deps
    }
}

#[async_trait]
impl bc_jobs::ItemHandler for DownloadHandler {
    async fn run(&self, ctx: ItemCtx) -> HandlerOutcome {
        self.run_item(ctx).await
    }
}

impl DownloadHandler {
    async fn run_item(&self, ctx: ItemCtx) -> HandlerOutcome {
        let mut item = ctx.item.clone();
        let Some(submitted) = item.url.clone() else {
            return failed("The queued item has no URL.", "internal", false);
        };
        let params = Params::parse(&ctx.job.params);

        // What was queued, before any track URL is widened to its album: the inbox row for a
        // wishlist track knows the item by this URL, so it is what settles that row at the end.
        let Some(url) = cancellable(&ctx.cancel, self.widen_to_album(&mut item, &submitted, &params)).await else {
            return HandlerOutcome::Interrupted;
        };

        // An artist or label page names a catalogue rather than a record. Expanded before
        // anything else touches this item: what follows -- the staging dir, the preflight, the
        // downloader -- is all about one release, and this item no longer stands for one.
        match cancellable(&ctx.cancel, self.expand_artist(&ctx, &item, &url)).await {
            None => return HandlerOutcome::Interrupted,
            Some(Some(done)) => return done,
            Some(None) => {}
        }

        let base = {
            let deps = self.deps.clone();
            tokio::task::spawn_blocking(move || deps.downloads_base()).await.unwrap_or_else(|_| self.deps.cfg.download_dir.clone())
        };
        let target = match item.target_dir.as_deref().filter(|d| !d.is_empty()) {
            Some(d) => base.join(safe_subdir_name(d)),
            None => base.clone(),
        };
        // A flat job drops the <artist>/<album> nesting: the whole batch is meant to land as one
        // folder of files.
        let template = params.flat.then(|| FLAT_TEMPLATE.to_string());
        // The downloader runs against a private staging dir, not the shared downloads root.
        // Concurrent items therefore write to disjoint trees: the before/after diff can only ever
        // contain this item's own files, so a sibling's album can never be attributed -- and
        // ingested, and URL-stamped -- as this item's. (That misattribution is how thousands of
        // releases ended up carrying another record's bandcamp_url, and through it another
        // record's label.) Keyed by item id so a retry resumes into the same partial download.
        let staging = staging_dir(&base, item.id);

        match cancellable(&ctx.cancel, self.preflight_skip(&ctx, &item, &submitted, &params)).await {
            None => return HandlerOutcome::Interrupted,
            Some(Some(done)) => return done,
            Some(None) => {}
        }

        // bandcamp-dl's __main__ drops any URL without /album/ or /track/ in it -- silently,
        // before fetching anything, exiting 0 with no output. Named here instead, once, and not
        // retried: nothing about the URL changes on a second run.
        if !is_downloadable(&url) {
            let kind = classify(&normalise(&url)).as_str();
            return failed(
                format!("bandcamp-dl only downloads /album/ and /track/ pages, and {url} is a {kind} page."),
                "unsupported_url",
                false,
            );
        }

        let dl = self.deps.pick_downloader();
        let mut spec = DownloadSpec::new(url.clone(), &staging);
        spec.template = template;
        spec.timeout = Duration::from_secs(self.deps.cfg.download_timeout_s);
        spec.tracks_only = params.tracks_only;
        // Read per item, so a changed preference applies to the next download.
        spec.format = {
            let db = self.deps.db.clone();
            tokio::task::spawn_blocking(move || crate::download::owned::read_format(&db)).await.ok().flatten().map(str::to_string)
        };

        // Progress arrives on the downloader's task; persist/publish it (<= 4/s) off a forwarder.
        let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<Progress>();
        let mut reporter = ctx.reporter();
        let forwarder = tokio::spawn(async move {
            while let Some(p) = prx.recv().await {
                let message = format!("{}: {} ({}/{})", p.phase, p.track_name, p.track_index, p.track_total);
                reporter.report_async(p.fraction, &message).await;
            }
        });
        let mut on_progress = move |p: Progress| {
            let _ = ptx.send(p);
        };
        let outcome = dl.download(&spec, &mut on_progress, &ctx.cancel).await;
        drop(on_progress);
        forwarder.abort();

        if outcome.kind == OutcomeKind::Crash && outcome.detail.ends_with(MISSING_BINARY_SUFFIX) {
            return failed(outcome.detail, "missing_binary", false);
        }
        // The download is already dead (the downloader kills its process group on its way out);
        // what is left is to say what the item is now -- the runner settles it by *why*.
        if ctx.cancel.is_cancelled() && !outcome.ok() {
            return HandlerOutcome::Interrupted;
        }

        let url_kind = item.url_kind.clone();
        if outcome.ok() {
            // Merge the whole staging tree, not just this attempt's diff: after a resumed retry the
            // earlier attempts' tracks are in staging but were never ingested (a retryable partial
            // ingests nothing, so `.staging/` paths never reach the database).
            let merged = merge_blocking(self.deps.db.clone(), url.clone(), staging.clone(), target.clone()).await;
            // Read before completion replaces it with where the download actually landed.
            let queued_release_id = item.release_id;
            let ingested =
                if merged.is_empty() { None } else { self.ingest(&ctx, &base, &merged, &url, url_kind.as_deref(), &params).await };
            // Bandcamp's own count of what this record holds -- the later "this album is only
            // partly here" verdict compares against it.
            //
            // Recorded against the item's own release when nothing was ingested, which is the
            // normal outcome of a fill: re-fetching an album whose files are already on disk
            // produces no new files and no ingest. Dropping the count there is what let a release
            // be filled over and over -- each run read the page, learned the record holds 8, and
            // threw the answer away, so the album stayed "missing tracks" forever.
            let measured = ingested.or(item.release_id);
            if let (Some(rid), Some(expected)) = (measured, outcome.tracks_expected.filter(|e| *e > 0)) {
                if let Err(e) = self.deps.library().record_expected(rid, i64::from(expected)).await {
                    tracing::warn!("could not record the expected track count on release {rid}: {e}");
                }
            }
            // The tralbum this run already read says which tracks are out: a pre-order's early
            // release is complete as far as Bandcamp goes, so remember it (no extra request).
            if let (Some(rid), Some(av)) = (measured, outcome.availability.as_ref()) {
                if let Err(e) = self.deps.library().record_availability(rid, av).await {
                    tracing::warn!("could not record availability on release {rid}: {e}");
                }
            }
            // A fill whose download landed on a *different* record has proved its own URL wrong:
            // the page it names is that other release, not the one being repaired. Two albums
            // sharing a title is all it takes for a URL to be stamped on the wrong one, and the
            // mistake is self-perpetuating -- every fill re-downloads the stranger's record,
            // ingests it there, and leaves the short release exactly as short as it was.
            if let (Some("fill"), Some(queued), Some(landed)) = (item.source.as_deref(), queued_release_id, ingested) {
                if queued != landed {
                    if let Err(e) = self.deps.library().disown_url(queued, &url, landed).await {
                        tracing::warn!("could not disown {url} on release {queued}: {e}");
                    }
                }
            }
            // The inbox row that put this item in the queue is the only thing left once the job is
            // deleted, so it has to be told the download is over (`inbox::settle`). Both URLs when
            // a track was widened: the wishlist's inbox row still goes by the track URL.
            self.settle_inbox(&url, ingested).await;
            if submitted != url {
                self.settle_inbox(&submitted, ingested).await;
            }
            // And the hearts this download just satisfied: a stream loved while browsing becomes
            // the library track it was standing in for, so the Loved shelf stops listing both.
            if let Some(rid) = ingested {
                self.settle_loved(rid).await;
            }
            return HandlerOutcome::Done(Complete {
                message: Some(outcome.detail.clone()),
                result: Some(json!({
                    "files": merged.iter().filter_map(|p| p.file_name()).map(|n| n.to_string_lossy()).collect::<Vec<_>>(),
                    "tracks": merged.len(),
                })),
                release_id: ingested,
            });
        }

        // A failed attempt. Whether it will be retried is decided by the store with the same rule.
        let will_retry = outcome.retryable && item.attempts < item.max_attempts;
        if !will_retry {
            // Terminal failure: salvage whatever audio did land so a mostly-complete album is not
            // thrown away with the item.
            let merged = merge_blocking(self.deps.db.clone(), url.clone(), staging.clone(), target.clone()).await;
            if !merged.is_empty() {
                let salvaged = self.ingest(&ctx, &base, &merged, &url, url_kind.as_deref(), &params).await;
                // A salvaged album is incomplete by definition; record how long it should be, so
                // the grid can say "4/12" and offer the fill instead of forgetting the rest of the
                // record ever existed.
                if let (Some(rid), Some(expected)) = (salvaged, outcome.tracks_expected.filter(|e| *e > 0)) {
                    if let Err(e) = self.deps.library().record_expected(rid, i64::from(expected)).await {
                        tracing::warn!("could not record the expected track count on release {rid}: {e}");
                    }
                }
            }
        }
        write_error_log(&target, &url, &outcome.detail);
        failed(outcome.detail, outcome.kind.as_str(), outcome.retryable)
    }

    /// Trade a `/track/` URL for its parent album's before anything runs (`_widen_to_album`).
    ///
    /// Albums are the unit this library collects: a wishlist can heart one track off a record, but
    /// downloading just that track leaves an album with one file in it. Widened here, at the one
    /// point every download passes through, so wishlist queues, the player's download button and
    /// hand-pasted track links all widen the same way -- and the widened URL is written back onto
    /// the item, so the queue shows the record it will actually fetch and dedup keys on the URL that
    /// identifies it. A standalone single resolves to itself; a resolution failure falls back to the
    /// track, which still beats failing the item.
    ///
    /// `tracks_only` opts a whole job out: a crate assembled from a DJ tracklist asked for those
    /// tracks and nothing else.
    async fn widen_to_album(&self, item: &mut JobItem, url: &str, params: &Params) -> String {
        if params.tracks_only {
            return url.to_string();
        }
        if classify(&normalise(url)) != UrlKind::Track {
            return url.to_string();
        }
        let resolved = match (&self.deps.album_resolver, &self.deps.client) {
            (Some(f), _) => f(url.to_string()).await,
            (None, Some(client)) => crate::sources::resolve_album_url(client, url).await,
            (None, None) => return url.to_string(),
        };
        let resolved = match resolved {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("could not widen {url} to its album, downloading as is: {e}");
                return url.to_string();
            }
        };
        if resolved != url && classify(&resolved) == UrlKind::Album {
            tracing::info!("widened {url} to its album {resolved}");
            let (id, new_url) = (item.id, resolved.clone());
            let wrote = self
                .deps
                .db
                .write_async(move |tx| {
                    tx.execute("UPDATE job_items SET url = ?2, url_kind = 'album' WHERE id = ?1", bc_db::rusqlite::params![id, new_url])?;
                    Ok(())
                })
                .await;
            if let Err(e) = wrote {
                tracing::warn!("could not write the widened URL back onto item {}: {e}", item.id);
            }
            item.url = Some(resolved.clone());
            item.url_kind = Some("album".into());
            return resolved;
        }
        url.to_string()
    }

    /// Trade an artist or label page for the releases it lists (`_expand_artist`).
    ///
    /// `bandcamp-dl` cannot download a discography. Its `__main__` walks the positional URLs with
    /// `if "/album/" not in url and "/track/" not in url: continue`, so a `/music` page is dropped
    /// before a single request -- empty work list, exit 0, no output -- and `verify` can only read
    /// that as `no_output`. That is why one queued band page failed "No audio files were produced."
    /// three identical times. Expanded here, at the one point every download passes through, so a
    /// paste, an Explore submission and a wishlist queue all expand the same way -- and so an item
    /// already sitting failed in the queue heals when it is retried.
    ///
    /// `Some(outcome)` once the item has been settled (the caller must stop: it no longer stands
    /// for a single release), `None` for a release URL.
    pub(super) async fn expand_artist(&self, ctx: &ItemCtx, item: &JobItem, url: &str) -> Option<HandlerOutcome> {
        if !matches!(classify(&normalise(url)), UrlKind::Artist | UrlKind::Music) {
            return None;
        }
        let trunc = |s: String| -> String { s.chars().take(500).collect() };

        let fetched = match (&self.deps.band_fetcher, &self.deps.client) {
            (Some(f), _) => f(url.to_string()).await,
            (None, Some(client)) => crate::sources::fetch_band_page(client, url, false).await,
            (None, None) => {
                return Some(failed("An artist page needs a Bandcamp client to expand into its releases.", "no_client", false));
            }
        };
        let page = match fetched {
            Ok(p) => p,
            Err(HarvestError::IdentityExpired(m)) => {
                // Deterministic until the user acts, so retrying it three times only buries the
                // one line that says what to do.
                return Some(failed(
                    format!("Bandcamp identity expired -- re-paste the cookie in Settings, then retry. ({m})"),
                    "identity",
                    false,
                ));
            }
            Err(HarvestError::RateLimited(m)) => return Some(failed(trunc(m), "network", true)),
            Err(HarvestError::Cancelled) => return Some(HandlerOutcome::Interrupted),
            Err(e) => {
                // No status survives on the error, so the message is the only signal -- the same
                // sniff the explore routes translate 404s by.
                let text = e.to_string();
                let missing = text.to_lowercase().contains("not found") || text.contains("404");
                return Some(failed(
                    trunc(format!("Could not read {url}: {text}")),
                    if missing { "not_found" } else { "network" },
                    !missing,
                ));
            }
        };

        if page.tier == crate::extract::Tier::Css {
            // Only the rendered head of the grid; a large catalogue keeps its tail in
            // data-client-items, so its absence *may* mean a short read. Logged, not shown: a small
            // band is rendered whole and reports this legitimately.
            tracing::warn!("{url}: expanding from the rendered grid, may be truncated");
        }

        let job_id = item.job_id.clone();
        let existing: Vec<String> = self
            .deps
            .db
            .read_async(move |c| {
                let mut st = c.prepare("SELECT url FROM job_items WHERE job_id = ?1 AND url IS NOT NULL")?;
                Ok(st.query_map([&job_id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
            })
            .await
            .unwrap_or_default();
        let mut seen: std::collections::HashSet<String> = existing.iter().filter(|u| !u.is_empty()).map(|u| url_key(u)).collect();
        let mut releases: Vec<String> = Vec::new();
        for entry in &page.releases {
            let candidate = normalise(&entry.page_url);
            if seen.contains(&url_key(&candidate)) || !matches!(classify(&candidate), UrlKind::Album | UrlKind::Track) {
                continue;
            }
            seen.insert(url_key(&candidate));
            releases.push(candidate);
        }

        let band = if page.profile.name.is_empty() { display_name(url) } else { page.profile.name.clone() };

        if releases.is_empty() {
            // The page answered, and the answer was "nothing here". Skipped rather than failed:
            // nothing is broken, and a failed row would keep the Retry button lit on something
            // that cannot succeed.
            return Some(HandlerOutcome::Skipped(format!("{band} lists no downloadable releases")));
        }

        // A label page names the record label for everything on it, which is what files the
        // downloads under it. Only when this job *is* that one page: params are shared by every
        // item, so stamping a label onto a job that also holds hand-pasted albums would mislabel
        // those.
        let job_id = item.job_id.clone();
        let total = ctx.store.run({
            let id = job_id.clone();
            move |s| s.get_job(&id)
        }).await.ok().flatten().map(|j| j.total).unwrap_or(ctx.job.total);
        if page.profile.is_label && total == 1 {
            let label_url = if page.profile.url.is_empty() { url.to_string() } else { page.profile.url.clone() };
            self.stamp_label(&job_id, &band, &label_url).await;
        }

        // Library, blacklist and shelf checks are deliberately not repeated here -- each new item
        // runs its own preflight, which is the one place that knows about force, blacklists and
        // adoption. Nor is the inbox settled: it holds release URLs only, so no row can match a
        // band page.
        let plural = if releases.len() == 1 { "" } else { "s" };
        let message = format!("{band}: expanded into {} release{plural}", releases.len());
        let items: Vec<NewItem> = releases
            .iter()
            .map(|r| {
                let mut n = NewItem::url(r.clone(), classify(r).as_str());
                n.source = item.source.clone();
                n.target_dir = item.target_dir.clone();
                n
            })
            .collect();
        let id = item.id;
        match ctx.store.run(move |s| s.expand_item(id, items, &message)).await {
            Ok(n) => {
                tracing::info!("expanded {url} into {n} release(s)");
                Some(HandlerOutcome::Handled)
            }
            Err(e) => Some(failed(format!("Internal error: could not expand {url}: {e}"), "internal", true)),
        }
    }

    /// Record the label a whole-catalogue job downloads for. Read back per item at ingest time
    /// from the job's params, so it has to be there before the expanded items run.
    async fn stamp_label(&self, job_id: &str, name: &str, url: &str) {
        let (job_id, name, url) = (job_id.to_string(), name.to_string(), url.to_string());
        let log_id = job_id.clone();
        let res = self
            .deps
            .db
            .write_async(move |tx| {
                let raw: Option<String> =
                    tx.query_row("SELECT params FROM jobs WHERE id = ?1", [&job_id], |r| r.get(0)).unwrap_or(None);
                let mut params: Value = raw.as_deref().and_then(|r| serde_json::from_str(r).ok()).unwrap_or_else(|| json!({}));
                if !params.is_object() {
                    params = json!({});
                }
                if params.get("label_name").is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty())) {
                    return Ok(());
                }
                params["label_name"] = json!(name);
                params["label_url"] = json!(url);
                tx.execute("UPDATE jobs SET params = ?2 WHERE id = ?1", bc_db::rusqlite::params![job_id, params.to_string()])?;
                Ok(())
            })
            .await;
        if let Err(e) = res {
            tracing::warn!("could not stamp the label on job {log_id}: {e}");
        }
    }

    /// Skip a claimed item whose URL the library already has, or refuses (`_preflight_skip`).
    ///
    /// This is what lets an old multi-thousand-item queue drain at query speed instead of one
    /// subprocess per already-owned album. The claimed item is 'running', never 'done', so it
    /// cannot match itself.
    ///
    /// `force` overrides "you already have this" -- that is what it is for -- but never the
    /// blacklist. Force means "fetch it again anyway"; the blacklist means "I threw this away on
    /// purpose", and a re-queue from a stale wishlist must not quietly undo that.
    pub(super) async fn preflight_skip(&self, ctx: &ItemCtx, item: &JobItem, submitted: &str, params: &Params) -> Option<HandlerOutcome> {
        let url = item.url.clone()?;
        let urls = vec![url.clone()];
        let known = self.deps.db.read_async(move |c| find_known(c, &urls, None)).await.unwrap_or_default();
        let reason = known.get(&url_key(&url)).cloned();
        let blacklisted = reason.as_deref() == Some("blacklist");
        if !blacklisted && (params.force || reason.is_none()) {
            return None;
        }

        let mut adopted = 0;
        if !blacklisted && params.source_fan_id.is_none() {
            // A personal job skipping a record that sits on someone else's shelf: the user asked
            // for it for themselves, so it moves into the library instead of staying hidden
            // behind "already have".
            adopted = self.deps.library().adopt_for_urls(std::slice::from_ref(&url)).await.unwrap_or_else(|e| {
                tracing::warn!("could not adopt {url}: {e}");
                0
            });
        }
        let message = if blacklisted {
            "Blacklisted — skipped"
        } else if adopted > 0 {
            "Already downloaded for another wishlist — moved into your library"
        } else {
            "Already in library — skipped"
        };
        let id = item.id;
        let msg = message.to_string();
        let _ = ctx.store.run(move |s| s.skip_item(id, &msg)).await;
        // Skipped for ownership is still an answer to "is this downloading?", and no. The
        // blacklist is left alone: the blacklist service owns that transition and marks the row
        // 'ignored'. A track URL that was widened settles its own inbox row too -- the wishlist
        // queued it by that URL.
        if !blacklisted {
            self.settle_inbox(&url, None).await;
            if submitted != url {
                self.settle_inbox(submitted, None).await;
            }
        }
        Some(HandlerOutcome::Handled)
    }

    pub(super) async fn settle_inbox(&self, url: &str, release_id: Option<i64>) {
        let (db, u) = (self.deps.db.clone(), url.to_string());
        let res = tokio::task::spawn_blocking(move || inbox::settle(&db, &u, release_id)).await;
        match res {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => tracing::warn!("could not settle the inbox row for {url}: {e}"),
            Err(e) => tracing::warn!("inbox settle task failed: {e}"),
        }
    }

    /// Convert the loved streams this release supersedes, and say so. The event is what lets the
    /// Loved shelf move a row out of its "Bandcamp streams" group the moment the album lands.
    async fn settle_loved(&self, release_id: i64) {
        match self.deps.library().reconcile_loved(release_id).await {
            Ok(r) if r.streams_cleared > 0 => {
                self.deps.bus.publish(
                    bc_types::library::TOPIC_LOVED_RECONCILED,
                    &json!({ "release_id": release_id, "track_ids": r.track_ids, "stream_ids": r.stream_ids }),
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("loved reconcile failed for release {release_id}: {e}"),
        }
    }
}

async fn merge_blocking(db: Db, url: String, staging: PathBuf, target: PathBuf) -> Vec<PathBuf> {
    tokio::task::spawn_blocking(move || merge_staging(&staging, &target, Some((&db, &url)))).await.unwrap_or_default()
}

/// Mirror failures to `error.log` (`job_items.last_error` is authoritative; this file just makes
/// failures greppable outside the app, which the gen-2 scripts relied on).
fn write_error_log(target: &std::path::Path, url: &str, detail: &str) {
    use std::io::Write;
    if std::fs::create_dir_all(target).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(target.join("error.log")) {
        let _ = writeln!(f, "Failed: {url}\n  {detail}");
    }
}
