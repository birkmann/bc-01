//! `BandcampService` (COORDINATION "Service shape") and the shared [`Ctx`].
//!
//! `Ctx` is what every module of this crate receives: DB, event bus, config, the
//! job store, the one shared rate-limited Bandcamp client, the cookie store, and a
//! small type-keyed extension map where each service registers itself in its
//! `init` so routers and other services can find it (`ctx.get::<FanWalker>()`).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_jobs::{ApiError, JobsService};
use parking_lot::RwLock;

use crate::error::HarvestError;
use crate::identity::CookieStore;
use crate::net::{self, BandcampClient};

pub struct Ctx {
    pub db: Db,
    pub bus: Arc<EventBus>,
    pub cfg: Config,
    pub jobs: JobsService,
    /// The one shared, rate-limited client: the token bucket, page cache and 429 back-off
    /// only mean anything if every caller shares them. The cookie is updated in place.
    pub client: BandcampClient,
    pub cookies: CookieStore,
    ext: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl Ctx {
    pub fn new(db: Db, bus: Arc<EventBus>, cfg: Config, jobs: JobsService) -> Arc<Self> {
        let cookies = CookieStore::new(&cfg.data_dir);
        // Move a legacy `settings` cookie into the keyring / 0600 file (best effort).
        let _ = cookies.migrate_from_settings(&db);
        let client = net::client_from_config(&cfg, cookies.load());
        Arc::new(Self { db, bus, cfg, jobs, client, cookies, ext: RwLock::new(HashMap::new()) })
    }

    /// Construct with an explicit client (tests: a client pointed at a local fake Bandcamp).
    pub fn with_client(db: Db, bus: Arc<EventBus>, cfg: Config, jobs: JobsService, client: BandcampClient) -> Arc<Self> {
        let cookies = CookieStore::file_only(&cfg.data_dir);
        Arc::new(Self { db, bus, cfg, jobs, client, cookies, ext: RwLock::new(HashMap::new()) })
    }

    /// Register a service so others can find it by type.
    pub fn put<T: Any + Send + Sync>(&self, v: Arc<T>) {
        self.ext.write().insert(TypeId::of::<T>(), v);
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.ext.read().get(&TypeId::of::<T>()).cloned().and_then(|a| a.downcast::<T>().ok())
    }

    /// Like [`Ctx::get`] but for services that must exist by the time they are used.
    pub fn expect<T: Any + Send + Sync>(&self) -> Arc<T> {
        self.get::<T>().unwrap_or_else(|| panic!("service {} was not initialised", std::any::type_name::<T>()))
    }

    /// Re-read the cookie from its store into the shared client (after PUT/DELETE identity).
    pub fn reload_cookie(&self) {
        self.client.set_cookie(self.cookies.load());
    }

    /// Wake the download worker (new download-queue rows were inserted).
    pub fn notify_downloads(&self) {
        self.jobs.store().notify();
    }

    /// Where new downloads go: the registered, enabled `downloads` root (so moving the folder
    /// to another drive redirects new downloads), else `Config::download_dir`. Blocking.
    pub fn downloads_base(&self) -> PathBuf {
        let root: Option<String> = self
            .db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT path FROM library_roots WHERE kind = 'downloads' AND enabled = 1 ORDER BY id LIMIT 1",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .ok())
            })
            .ok()
            .flatten();
        root.map(PathBuf::from).unwrap_or_else(|| self.cfg.download_dir.clone())
    }
}

/// `translate_harvest_error` (api/deps.py): the scraper's failures as responses a client can act on.
impl From<HarvestError> for ApiError {
    fn from(e: HarvestError) -> Self {
        match &e {
            HarvestError::IdentityExpired(m) => ApiError::unauthorized(m.clone()),
            HarvestError::RateLimited(m) => {
                ApiError::bad_request(format!("Bandcamp is rate-limiting us — try again shortly ({m})"))
            }
            HarvestError::Db(d) => ApiError::internal(d.to_string()),
            HarvestError::Cancelled => ApiError::conflict("cancelled"),
            other if other.to_string().to_lowercase().contains("not found") => ApiError::not_found(other.to_string()),
            other => ApiError::bad_request(other.to_string()),
        }
    }
}

/// Everything WS2 serves besides the generic job routes (those are `JobsService::router`).
#[derive(Clone)]
pub struct BandcampService {
    ctx: Arc<Ctx>,
}

impl BandcampService {
    pub fn new(db: Db, bus: Arc<EventBus>, cfg: Config, jobs: JobsService) -> Self {
        let ctx = Ctx::new(db, bus, cfg, jobs);
        crate::download::worker::init(&ctx);
        crate::harvest::init(&ctx);
        crate::api::init(&ctx);
        Self { ctx }
    }

    pub fn ctx(&self) -> &Arc<Ctx> {
        &self.ctx
    }

    /// Spawn the background workers: download worker + disk guard, harvest/walk/sweep/enrich
    /// workers, the feed scheduler. Crash recovery runs first (idempotent).
    pub async fn start(&self) {
        self.ctx.jobs.recover().await;
        crate::download::worker::start(&self.ctx).await;
        crate::harvest::start(&self.ctx).await;
        crate::api::explore::start(&self.ctx).await;
    }

    /// Routes WITHOUT the `/api` prefix, state applied.
    pub fn router(&self) -> Router {
        crate::api::router(&self.ctx)
    }

    /// The Bandcamp lookup WS1 needs for the stray merge and locate flows:
    /// `LibraryService::with_bandcamp(svc.lookup())`.
    pub fn lookup(&self) -> Arc<crate::lookup::BcLookup> {
        Arc::new(crate::lookup::BcLookup::new(self.ctx.client.clone()))
    }

    /// Signed CDN URL for `(release_url, track_key)` (`track_key` = `bc_track_id` or `i<index>`):
    /// cached tralbum, pre-resolved on the reserved token lane, refreshed at 80 % of its TTL. For
    /// WS4's engine. `refresh = true` re-resolves (after a 403).
    pub async fn resolve_stream(
        &self,
        release_url: &str,
        track_key: &str,
        refresh: bool,
    ) -> Result<crate::stream::ResolvedStream, HarvestError> {
        self.ctx.expect::<crate::stream::StreamService>().resolve_stream(release_url, track_key, refresh).await
    }

    /// An Explore release with its tracks (ids + proxy stream URLs): `QueueSource::Explore`.
    pub async fn release_tracks(&self, release_url: &str) -> Result<bc_types::bandcamp::ExploreReleaseOut, HarvestError> {
        self.ctx.expect::<crate::stream::StreamService>().release_tracks(release_url).await
    }

    /// The next records of a followed fan's list: `QueueSource::Fan`.
    #[allow(clippy::too_many_arguments)]
    pub async fn fan_next(
        &self,
        fan_id: i64,
        after: Option<i64>,
        order: crate::harvest::fans::FanOrder,
        seed: u32,
        states: &[String],
        tab: crate::harvest::fans::FanTab,
        limit: usize,
    ) -> Result<bc_types::bandcamp::FanNextOut, HarvestError> {
        crate::harvest::fans::fan_next(&self.ctx, fan_id, after, order, seed, states, tab, limit).await
    }

    /// Wake the download worker (WS1's `JobHost::notify_download_queue` should call this).
    pub fn notify_downloads(&self) {
        self.ctx.notify_downloads();
    }
}
