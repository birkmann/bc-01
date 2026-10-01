//! Index freshly merged downloads (`_ingest` and its helpers) through the [`LibraryPort`].

use std::path::{Path, PathBuf};

use bc_db::rusqlite::{OptionalExtension, params};
use bc_jobs::{ItemCtx, NewItem, NewJob};

use super::difflib::ratio;
use super::handler::{DownloadHandler, Params};
use crate::download::dedup::get_or_create_label;
use crate::urls::normalise;

/// Whether an album URL's slug resembles the release it is being written on (`_url_plausibly_names`).
///
/// Last line of defence: with per-item staging the ingest can only ever see its own item's files,
/// so this should never fire. If a future regression reintroduces cross-item attribution, failing
/// here loses a URL -- which the backfills can restore -- instead of corrupting a release row.
///
/// Uses CPython's `SequenceMatcher.ratio() >= 0.5` (a private port of the tracklist module's).
pub fn url_plausibly_names(url: &str, title: &str, folder_path: Option<&str>) -> bool {
    let slug = url.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_lowercase();
    if slug.is_empty() {
        return false;
    }
    let mut candidates: Vec<String> = Vec::new();
    if let Some(folder) = folder_path.filter(|f| !f.is_empty()) {
        if let Some(name) = Path::new(folder).file_name() {
            candidates.push(name.to_string_lossy().to_lowercase());
        }
    }
    if !title.is_empty() {
        let lowered = title.to_lowercase();
        let mut out = String::new();
        let mut last_dash = false;
        for ch in lowered.chars() {
            if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
                out.push(ch);
                last_dash = false;
            } else if !last_dash {
                out.push('-');
                last_dash = true;
            }
        }
        candidates.push(out.trim_matches('-').to_string());
    }
    if candidates.is_empty() {
        return true;
    }
    candidates.iter().any(|c| ratio(&slug, c) >= 0.5)
}

impl DownloadHandler {
    /// Index freshly downloaded files into the library, once per item, and return the release id.
    ///
    /// `source_fan_id` is the shelf this job downloads for: releases it *creates* are filed under
    /// that fan (the port does it); releases that already existed keep their owner. A personal job
    /// (`None`) does the opposite -- every release it touches that was on some fan's shelf is
    /// adopted into my library, because asking for it for myself is what moves it.
    pub(super) async fn ingest(
        &self,
        ctx: &ItemCtx,
        base: &Path,
        files: &[PathBuf],
        url: &str,
        url_kind: Option<&str>,
        params: &Params,
    ) -> Option<i64> {
        if files.is_empty() {
            return None;
        }
        let lib = self.deps().library();
        let result = match lib.ingest(base, files, params.source_fan_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("post-download ingest failed: {e}");
                return None;
            }
        };
        for err in &result.errors {
            tracing::warn!("post-download ingest: {err}");
        }

        if params.source_fan_id.is_none() && !result.release_ids.is_empty() {
            if let Err(e) = lib.adopt_releases(&result.release_ids).await {
                tracing::warn!("could not file releases under their source: {e}");
            }
        }

        let release_id = result.release_ids.first().copied();

        // Remember where the release came from, so future pastes of the same URL are skipped
        // without running the downloader. Only an album URL identifies a release, and only when
        // the download touched exactly one release is the mapping unambiguous.
        if let (Some(rid), "album", 1) = (release_id, url_kind.unwrap_or(""), result.release_ids.len()) {
            self.remember_release_url(rid, url).await;
        }

        // Unlike the URL above, the label applies to *every* touched release: the job was queued
        // off a label page, and everything a label-page item produced came out on that label.
        if let Some(label) = params.label_name.as_deref().filter(|l| !l.is_empty()) {
            if !result.release_ids.is_empty() {
                self.file_under_label(&result.release_ids, label, params.label_url.as_deref()).await;
            }
        }

        // The new audio is waiting for analysis: one job per ingest (kind `analyze`, run by WS3).
        if !result.track_ids.is_empty() {
            let ids = result.track_ids.clone();
            let created = ctx
                .store
                .run(move |s| {
                    let items = ids.into_iter().map(NewItem::track).collect();
                    s.create_job(NewJob::new("analyze", items).label("Analyze new downloads").priority(50))
                })
                .await;
            if let Err(e) = created {
                tracing::warn!("could not queue the analysis of the new downloads: {e}");
            }
        }
        release_id
    }

    /// Stamp `releases.bandcamp_url` when it is empty, plausible, and not taken by another row.
    pub(super) async fn remember_release_url(&self, release_id: i64, url: &str) {
        let canonical = normalise(url);
        let res = self
            .deps()
            .db
            .write_async(move |tx| {
                let row: Option<(Option<String>, String, Option<String>)> = tx
                    .query_row("SELECT bandcamp_url, title, folder_path FROM releases WHERE id = ?1", [release_id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                    })
                    .optional()?;
                let Some((existing, title, folder)) = row else { return Ok(()) };
                if existing.is_some() {
                    return Ok(());
                }
                if !url_plausibly_names(&canonical, &title, folder.as_deref()) {
                    tracing::warn!("not recording {canonical} on release {release_id} ({title:?}): URL does not resemble it");
                    return Ok(());
                }
                let taken: Option<i64> =
                    tx.query_row("SELECT id FROM releases WHERE bandcamp_url = ?1", [&canonical], |r| r.get(0)).optional()?;
                if taken.is_none() {
                    tx.execute("UPDATE releases SET bandcamp_url = ?2 WHERE id = ?1", params![release_id, canonical])?;
                }
                Ok(())
            })
            .await;
        if let Err(e) = res {
            tracing::warn!("could not record bandcamp_url for release {release_id}: {e}");
        }
    }

    /// File freshly ingested releases under the label page they came from (`_file_under_label`).
    ///
    /// Only fills an empty `label_id` -- a publisher named by the files themselves always wins.
    /// Failures are logged, never fatal: the download already succeeded and the label can be
    /// re-filed by a catalogue re-run.
    pub(super) async fn file_under_label(&self, release_ids: &[i64], label_name: &str, label_url: Option<&str>) {
        let (ids, name, url) = (release_ids.to_vec(), label_name.to_string(), label_url.map(str::to_string));
        let res = self
            .deps()
            .db
            .write_async(move |tx| {
                let Some(label_id) = get_or_create_label(tx, &name)? else { return Ok(()) };
                if let Some(u) = url.filter(|u| !u.is_empty()) {
                    let current: Option<String> =
                        tx.query_row("SELECT bandcamp_url FROM labels WHERE id = ?1", [label_id], |r| r.get(0)).optional()?.flatten();
                    if current.is_none() {
                        let canonical = normalise(&u);
                        let taken: Option<i64> = tx
                            .query_row("SELECT id FROM labels WHERE bandcamp_url = ?1 AND id != ?2", params![canonical, label_id], |r| r.get(0))
                            .optional()?;
                        if taken.is_none() {
                            tx.execute("UPDATE labels SET bandcamp_url = ?2 WHERE id = ?1", params![label_id, canonical])?;
                        }
                    }
                }
                for rid in ids {
                    tx.execute("UPDATE releases SET label_id = ?2 WHERE id = ?1 AND label_id IS NULL", params![rid, label_id])?;
                }
                Ok(())
            })
            .await;
        if let Err(e) = res {
            tracing::warn!("could not file downloads under label {label_name:?}: {e}");
        }
    }
}
