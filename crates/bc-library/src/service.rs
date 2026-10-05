//! `LibraryService`: assembles every WS1 router and background worker behind the COORDINATION contract
//! (`new`, `start`, `router`).

use std::sync::Arc;

use axum::Router;
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_libcore::{ApiResult, Ctx, JobHost, Scope};
use bc_types::library::TrackQuery;

/// Implements the filter engine for `bc-plist` (smart playlists, "save this view as a playlist").
pub struct Resolver;

impl bc_plist::TrackResolver for Resolver {
    fn resolve_ids(&self, c: &bc_db::rusqlite::Connection, q: &TrackQuery, scope: &Scope, limit: Option<i64>) -> ApiResult<Vec<i64>> {
        crate::tracks::all_ids(c, q, scope, limit)
    }
}

#[derive(Clone)]
pub struct LibraryService {
    ctx: Ctx,
    watcher: Arc<parking_lot::Mutex<Option<bc_scan::watcher::Watcher>>>,
    lookup: Option<Arc<dyn bc_maint::BandcampLookup>>,
}

impl LibraryService {
    pub fn new(db: Db, bus: Arc<EventBus>, config: Config) -> Self {
        Self { ctx: Ctx::new(db, bus, config), watcher: Default::default(), lookup: None }
    }

    /// Inject a durable job host (e.g. an adapter over `bc-jobs`); default is the in-memory `LocalJobs`.
    pub fn with_jobs(mut self, jobs: Arc<dyn JobHost>) -> Self {
        self.ctx = self.ctx.with_jobs(jobs);
        self
    }

    /// Inject the Bandcamp client used by the stray merger.
    pub fn with_bandcamp(mut self, lookup: Arc<dyn bc_maint::BandcampLookup>) -> Self {
        self.lookup = Some(lookup);
        self
    }

    pub fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    /// Register configured roots, start the watcher, the Bandcamp-links file sync and the lazy WebP
    /// conversion of imported covers.
    /// Nothing here blocks startup (the server is accepting connections immediately).
    pub async fn start(&self) {
        let ctx = self.ctx.clone();
        let _ = tokio::task::spawn_blocking(move || bc_scan::roots::ensure_roots(&ctx)).await;
        *self.watcher.lock() = Some(bc_scan::watcher::Watcher::start(&self.ctx));
        crate::ledger::spawn(self.ctx.clone());
        let ctx = self.ctx.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = bc_scan::art::repair_shared_folder_art(&ctx) {
                tracing::warn!(error = %e, "repairing shared-folder cover art failed");
            }
        });
        let pending: i64 = self
            .ctx
            .read_async(|c| Ok(c.query_row("SELECT COUNT(*) FROM artwork WHERE source = 'legacy' AND sizes = 0", [], |r| r.get(0))?))
            .await
            .unwrap_or(0);
        if pending > 0 {
            let handle = self.ctx.jobs.begin("art", &format!("convert {pending} cover(s) to WebP"));
            let ctx = self.ctx.clone();
            let legacy = ctx.config.art_dir();
            tokio::spawn(async move { bc_scan::art::convert_legacy_art(&ctx, &legacy, handle).await });
        }
    }

    /// Paths WITHOUT the `/api` prefix, state applied.
    pub fn router(&self) -> Router {
        let ctx = self.ctx.clone();
        let art_ctx = ctx.clone();
        let hook: crate::media::LegacyArtHook = Arc::new(move |release_id| {
            let c = art_ctx.clone();
            std::thread::spawn(move || {
                let _ = bc_scan::art::convert_one(&c, release_id);
            });
        });
        Router::new()
            .merge(crate::routes::router(ctx.clone()))
            .merge(crate::media::router(ctx.clone(), Some(hook)))
            .merge(bc_scan::routes::router(ctx.clone()))
            .merge(bc_maint::routes::router(ctx.clone(), self.lookup.clone()))
            .merge(bc_meta::router(ctx.clone()))
            .merge(bc_plist::router(ctx.clone(), Arc::new(Resolver)))
            .fallback(bc_libcore::error::fallback_404)
    }
}
