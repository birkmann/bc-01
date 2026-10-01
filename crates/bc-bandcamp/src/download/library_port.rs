//! What the download worker needs from the library (WS1): index freshly downloaded files and a
//! handful of bookkeeping calls. A trait so the worker is testable with a fake and so this crate
//! does not hard-wire the library; [`BcLibrary`] is the real implementation over
//! `bc_scan::ingest` and `bc_maint`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bc_core::{Config, EventBus};
use bc_db::Db;

use crate::error::{HarvestError, Result};

/// What an ingest touched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IngestResult {
    pub track_ids: Vec<i64>,
    /// Every release written or confirmed (`releases_touched`).
    pub release_ids: Vec<i64>,
    /// The subset that did not exist before this ingest.
    pub releases_created: Vec<i64>,
    pub tracks_added: i64,
    pub errors: Vec<String>,
}

/// Which loved streams a download superseded (`loved.reconciled`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LovedOutcome {
    pub track_ids: Vec<i64>,
    pub stream_ids: Vec<i64>,
    pub streams_cleared: i64,
}

#[async_trait]
pub trait LibraryPort: Send + Sync {
    /// Index `files` (all under `downloads_base`) into the library: tags, art, FTS, snippet flags.
    /// Idempotent (re-ingesting a known path updates it). `source_fan_id`: NEW releases are filed on
    /// that fan's shelf. Creates the `downloads` library root for `downloads_base` if missing.
    async fn ingest(&self, downloads_base: &Path, files: &[PathBuf], source_fan_id: Option<i64>) -> Result<IngestResult>;
    /// Stamp what a download run said the release holds (`completeness.record_expected`).
    async fn record_expected(&self, release_id: i64, expected: i64) -> Result<()>;
    /// Remember which tracks of the record Bandcamp had out when the download read its page
    /// (`release_availability`). Default: ignore (no library wired).
    async fn record_availability(&self, _release_id: i64, _a: &bc_types::library::ReleaseAvailability) -> Result<()> {
        Ok(())
    }
    /// A fill whose download landed on a different record proved its URL wrong (`completeness.disown_url`).
    async fn disown_url(&self, release_id: i64, url: &str, landed_on: i64) -> Result<()>;
    /// A personal download of something a fan's shelf holds adopts it (`scope.adopt_for_urls`).
    async fn adopt_for_urls(&self, urls: &[String]) -> Result<i64>;
    /// Move releases into my library (`scope.adopt_releases`).
    async fn adopt_releases(&self, ids: &[i64]) -> Result<i64>;
    /// Convert the loved streams this release supersedes (`loved.reconcile_release`).
    async fn reconcile_loved(&self, release_id: i64) -> Result<LovedOutcome>;
}

/// Does nothing (no library wired): used by tests of other layers and as a placeholder.
#[derive(Debug, Default, Clone)]
pub struct NoLibrary;

#[async_trait]
impl LibraryPort for NoLibrary {
    async fn ingest(&self, _b: &Path, _f: &[PathBuf], _s: Option<i64>) -> Result<IngestResult> {
        Ok(IngestResult::default())
    }
    async fn record_expected(&self, _r: i64, _e: i64) -> Result<()> {
        Ok(())
    }
    async fn disown_url(&self, _r: i64, _u: &str, _l: i64) -> Result<()> {
        Ok(())
    }
    async fn adopt_for_urls(&self, _u: &[String]) -> Result<i64> {
        Ok(0)
    }
    async fn adopt_releases(&self, _i: &[i64]) -> Result<i64> {
        Ok(0)
    }
    async fn reconcile_loved(&self, _r: i64) -> Result<LovedOutcome> {
        Ok(LovedOutcome::default())
    }
}

fn ae(e: bc_libcore::ApiError) -> HarvestError {
    HarvestError::Other(e.to_string())
}

/// The real thing: `bc_scan::ingest` + `bc_maint`.
pub struct BcLibrary {
    db: Db,
    lib: bc_libcore::Ctx,
}

impl BcLibrary {
    pub fn new(db: Db, bus: Arc<EventBus>, cfg: Config) -> Self {
        Self { lib: bc_libcore::Ctx::new(db.clone(), bus, cfg), db }
    }
}

#[async_trait]
impl LibraryPort for BcLibrary {
    async fn ingest(&self, base: &Path, files: &[PathBuf], source_fan_id: Option<i64>) -> Result<IngestResult> {
        if files.is_empty() {
            return Ok(IngestResult::default());
        }
        let base_s = base.canonicalize().unwrap_or_else(|_| base.to_path_buf()).to_string_lossy().into_owned();
        let root_id: i64 = self
            .db
            .write_async(move |tx| {
                use bc_db::rusqlite::OptionalExtension;
                if let Some(id) =
                    tx.query_row("SELECT id FROM library_roots WHERE path = ?1", [&base_s], |r| r.get::<_, i64>(0)).optional()?
                {
                    return Ok(id);
                }
                tx.execute("INSERT INTO library_roots (path, kind, watch, enabled) VALUES (?1, 'downloads', 0, 1)", [&base_s])?;
                Ok(tx.last_insert_rowid())
            })
            .await?;
        let mut opts = bc_scan::ingest::IngestOptions::new();
        opts.source_fan_id = source_fan_id;
        let rep = bc_scan::ingest::ingest_paths_async(&self.lib, root_id, files.to_vec(), opts).await.map_err(ae)?;
        Ok(IngestResult {
            track_ids: rep.track_ids,
            release_ids: rep.release_ids,
            releases_created: rep.releases_created,
            tracks_added: rep.tracks_added,
            errors: rep.errors,
        })
    }

    async fn record_expected(&self, release_id: i64, expected: i64) -> Result<()> {
        Ok(self
            .db
            .write_async(move |tx| {
                bc_maint::completeness::record_expected(tx, release_id, Some(expected)).map_err(|e| bc_db::DbError::Other(e.to_string()))
            })
            .await?)
    }

    async fn record_availability(&self, release_id: i64, a: &bc_types::library::ReleaseAvailability) -> Result<()> {
        let mut a = a.clone();
        a.release_id = release_id;
        Ok(self
            .db
            .write_async(move |tx| {
                bc_libcore::availability::store(tx, &a).map(|_| ()).map_err(|e| bc_db::DbError::Other(e.to_string()))
            })
            .await?)
    }

    async fn disown_url(&self, release_id: i64, url: &str, landed_on: i64) -> Result<()> {
        let url = url.to_string();
        Ok(self
            .db
            .write_async(move |tx| {
                bc_maint::completeness::disown_url(tx, release_id, Some(&url), landed_on).map_err(|e| bc_db::DbError::Other(e.to_string()))
            })
            .await?)
    }

    async fn adopt_for_urls(&self, urls: &[String]) -> Result<i64> {
        let urls = urls.to_vec();
        Ok(self
            .db
            .write_async(move |tx| {
                bc_maint::adopt::adopt_for_urls(tx, &urls).map(|n| n as i64).map_err(|e| bc_db::DbError::Other(e.to_string()))
            })
            .await?)
    }

    async fn adopt_releases(&self, ids: &[i64]) -> Result<i64> {
        let ids = ids.to_vec();
        Ok(self
            .db
            .write_async(move |tx| {
                bc_maint::adopt::adopt_releases(tx, &ids).map(|n| n as i64).map_err(|e| bc_db::DbError::Other(e.to_string()))
            })
            .await?)
    }

    async fn reconcile_loved(&self, release_id: i64) -> Result<LovedOutcome> {
        let r = self
            .db
            .write_async(move |tx| bc_maint::loved::reconcile_release(tx, release_id).map_err(|e| bc_db::DbError::Other(e.to_string())))
            .await?;
        Ok(LovedOutcome { track_ids: r.track_ids, stream_ids: r.stream_ids, streams_cleared: r.streams_cleared })
    }
}
