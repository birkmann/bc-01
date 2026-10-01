//! The download worker (port of `services/download/worker.py`) on top of [`bc_jobs::KindWorker`].
//!
//! * [`DownloadHandler`] is the per-item [`bc_jobs::ItemHandler`]: widen `/track/` to its album, expand
//!   an artist/label page into its releases, preflight against the library / blacklist, download into
//!   a per-item staging dir (`<base>/.staging/item-<id>`), merge into the target, ingest through the
//!   [`LibraryPort`], stamp the source / URL / label, record completeness, settle the inbox and
//!   loved streams, and create an `analyze` job for the new tracks.
//! * [`DownloadWorker`] owns the [`KindWorker`] (semaphore **before** claim, lease + heartbeat) and
//!   the disk guard ([`DiskGuard`], a [`bc_jobs::Gate`]).
//! * `init` / `start` are the module convention called by [`crate::service::BandcampService`]:
//!   `init` registers a [`DownloadServices`] in the `Ctx` and the worker as `JobHooks`; `start`
//!   runs crash recovery (jobs, then staging dirs and orphaned `bandcamp-dl` children) and spawns
//!   the loop.

#![allow(clippy::collapsible_if)]

mod difflib;
mod handler;
mod ingest;
pub mod staging;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_jobs::{Gate, Interrupt, JobHooks, JobStore, KindWorker, WorkerSpec};
use bc_types::jobs::{DiskOut, TOPIC_DOWNLOADS_DISK};
use parking_lot::{Mutex, RwLock};
use tokio::sync::Mutex as AsyncMutex;

use super::bcdl::{BandcampDl, BcdlError, RunOptions, snapshot_tree};
use super::diskguard::{self, DiskState};
use super::library_port::{BcLibrary, LibraryPort};
use super::native::NativeDownloader;
use super::verify::classify;
use super::{DownloadSpec, Downloader, Outcome, OutcomeKind, ProgressFn};
use crate::net::BandcampClient;
use crate::service::Ctx;
use crate::sources::BandPage;

pub use handler::DownloadHandler;
pub use ingest::url_plausibly_names;
pub use staging::{STAGING_DIRNAME, merge_staging, purge_stale_staging};

/// Setting key choosing the downloader, read per item: `"native"` (default) or `"bandcamp-dl"`.
pub const DOWNLOADER_KEY: &str = "downloads.downloader";

/// How often the disk guard looks while every slot is busy or the worker is held.
pub const DISK_POLL: Duration = Duration::from_secs(5);

/// Suffix of the detail the bandcamp-dl wrapper reports for a missing binary; the handler maps it
/// to `error_class = missing_binary` (not retryable).
pub const MISSING_BINARY_SUFFIX: &str = " not found on PATH";

/// Fetch a band page (test hook; the default is [`crate::sources::fetch_band_page`] on the client).
pub type BandFetchFuture = Pin<Box<dyn Future<Output = crate::Result<BandPage>> + Send>>;
pub type BandFetcher = Arc<dyn Fn(String) -> BandFetchFuture + Send + Sync>;
/// Resolve a `/track/` URL to its album (test hook; the default is [`crate::sources::resolve_album_url`]).
pub type AlbumResolveFuture = Pin<Box<dyn Future<Output = crate::Result<String>> + Send>>;
pub type AlbumResolver = Arc<dyn Fn(String) -> AlbumResolveFuture + Send + Sync>;
/// Free bytes on the volume of a path (test hook; the default is [`diskguard::free_bytes`]).
pub type FreeBytesFn = Arc<dyn Fn(&Path) -> Option<i64> + Send + Sync>;

/// Where new downloads go: the registered, enabled `downloads` root (so moving the folder to
/// another drive redirects new downloads), else `Config::download_dir`. Blocking.
pub fn resolve_downloads_base(db: &Db, cfg: &Config) -> PathBuf {
    let root: Option<String> = db
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
    root.map(PathBuf::from).unwrap_or_else(|| cfg.download_dir.clone())
}

// -- the bandcamp-dl wrapper ----------------------------------------------------------------

/// [`BandcampDl`] with the worker's own semantics (`_run_item_inner`): after the run, classify by
/// filesystem diff; if `-f` skipped the album ("Full album not available") retry once without it,
/// say so in the detail, and do not retry a release that streams nothing.
///
/// (`BandcampDl`'s own `Downloader::download` retries without `-f` too but cannot report either of
/// the two details, so the worker drives `BandcampDl::run` itself.)
pub struct BcdlDownloader {
    pub dl: BandcampDl,
}

impl BcdlDownloader {
    pub fn new(dl: BandcampDl) -> Self {
        Self { dl }
    }
}

fn spawn_failure(e: &BcdlError, binary: &str) -> Outcome {
    let (detail, kind) = match e {
        BcdlError::Spawn { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            (format!("{binary}{MISSING_BINARY_SUFFIX}"), OutcomeKind::Crash)
        }
        BcdlError::NotDownloadable(_) => (e.to_string(), OutcomeKind::NoOutput),
        other => (other.to_string(), OutcomeKind::Crash),
    };
    Outcome { kind, new_files: Vec::new(), tracks_expected: None, availability: None, tracks_finished: 0, retryable: false, detail }
}

async fn snapshot_async(dir: PathBuf) -> super::bcdl::SnapshotMap {
    tokio::task::spawn_blocking(move || snapshot_tree(&dir)).await.unwrap_or_default()
}

#[async_trait]
impl Downloader for BcdlDownloader {
    fn name(&self) -> &'static str {
        "bandcamp-dl"
    }

    async fn download(&self, spec: &DownloadSpec, progress: ProgressFn<'_>, cancel: &tokio_util::sync::CancellationToken) -> Outcome {
        let base = spec.base_dir.clone();
        let mut opts = RunOptions { timeout: spec.timeout, full_album: spec.full_album, template: spec.template.clone() };
        // Non-empty only when a retry resumes a partial download.
        let before = snapshot_async(base.clone()).await;
        let mut run = match self.dl.run(&spec.url, &base, &opts, Some(&mut *progress), cancel).await {
            Ok(r) => r,
            Err(e) => return spawn_failure(&e, &self.dl.binary),
        };
        let mut after = snapshot_async(base.clone()).await;
        let mut outcome = classify(&before, &after, &run, &base);

        // `-f` refuses the album outright unless every track has a public stream, and says so
        // instead of downloading anything -- the shape of a release behind a loved stream. The
        // command is deterministic, so retrying it unchanged fails identically. Drop the flag
        // once and take the tracks that do stream.
        if run.full_album_skipped && !outcome.ok() && !run.cancelled && opts.full_album {
            tracing::info!("only part of {} streams; retrying without -f", spec.url);
            opts.full_album = false;
            run = match self.dl.run(&spec.url, &base, &opts, Some(&mut *progress), cancel).await {
                Ok(r) => r,
                Err(e) => return spawn_failure(&e, &self.dl.binary),
            };
            after = snapshot_async(base.clone()).await;
            outcome = classify(&before, &after, &run, &base);
            if outcome.ok() {
                outcome.detail.push_str(" Only part of this release is publicly streamable; the rest is download-only on Bandcamp.");
            } else if outcome.kind == OutcomeKind::NoOutput {
                // Which tracks Bandcamp streams is a property of the release, not of this
                // attempt, so another identical run fetches the same nothing. A timeout or a
                // network error keeps its own verdict and its retries.
                outcome.detail = "None of this release is publicly streamable.".into();
                outcome.retryable = false;
            }
        }
        if run.cancelled && !outcome.ok() {
            outcome.detail = "Cancelled.".into();
            outcome.retryable = false;
        }
        outcome
    }
}

// -- shared dependencies ---------------------------------------------------------------------

/// Everything the handler and the disk guard need (no `Ctx`, so tests and the kill -9 harness can
/// build a worker on their own).
pub struct DownloadDeps {
    pub db: Db,
    pub bus: Arc<EventBus>,
    pub cfg: Config,
    /// The shared Bandcamp client (widening a `/track/` URL, expanding a band page). `None`:
    /// track URLs download as submitted and an artist page fails with `no_client`.
    pub client: Option<BandcampClient>,
    library: RwLock<Option<Arc<dyn LibraryPort>>>,
    downloader_override: Option<Arc<dyn Downloader>>,
    native: OnceLock<Arc<dyn Downloader>>,
    bcdl: OnceLock<Arc<dyn Downloader>>,
    pub(crate) band_fetcher: Option<BandFetcher>,
    pub(crate) album_resolver: Option<AlbumResolver>,
    free_bytes: FreeBytesFn,
}

impl DownloadDeps {
    pub fn new(db: Db, bus: Arc<EventBus>, cfg: Config, client: Option<BandcampClient>) -> Self {
        Self {
            db,
            bus,
            cfg,
            client,
            library: RwLock::new(None),
            downloader_override: None,
            native: OnceLock::new(),
            bcdl: OnceLock::new(),
            band_fetcher: None,
            album_resolver: None,
            free_bytes: Arc::new(diskguard::free_bytes),
        }
    }

    /// Use this library port (default: [`BcLibrary`]).
    pub fn with_library(self, lib: Arc<dyn LibraryPort>) -> Self {
        *self.library.write() = Some(lib);
        self
    }

    /// Always use this downloader instead of reading the `downloads.downloader` setting.
    pub fn with_downloader(mut self, dl: Arc<dyn Downloader>) -> Self {
        self.downloader_override = Some(dl);
        self
    }

    pub fn with_band_fetcher(mut self, f: BandFetcher) -> Self {
        self.band_fetcher = Some(f);
        self
    }

    pub fn with_album_resolver(mut self, f: AlbumResolver) -> Self {
        self.album_resolver = Some(f);
        self
    }

    pub fn with_free_bytes(mut self, f: FreeBytesFn) -> Self {
        self.free_bytes = f;
        self
    }

    /// The library port in force (swappable at runtime with [`DownloadDeps::set_library`]).
    pub fn library(&self) -> Arc<dyn LibraryPort> {
        if let Some(l) = self.library.read().clone() {
            return l;
        }
        let mut slot = self.library.write();
        slot.get_or_insert_with(|| Arc::new(BcLibrary::new(self.db.clone(), self.bus.clone(), self.cfg.clone()))).clone()
    }

    pub fn set_library(&self, lib: Arc<dyn LibraryPort>) {
        *self.library.write() = Some(lib);
    }

    pub fn downloads_base(&self) -> PathBuf {
        resolve_downloads_base(&self.db, &self.cfg)
    }

    /// The downloader for the next item: the override, else the `downloads.downloader` setting
    /// (`native` default, `bandcamp-dl`). Read per item, so a settings change applies to the next one.
    pub fn pick_downloader(&self) -> Arc<dyn Downloader> {
        if let Some(d) = &self.downloader_override {
            return d.clone();
        }
        let choice = self
            .db
            .read(|c| bc_db::settings::get(c, DOWNLOADER_KEY))
            .ok()
            .flatten()
            .map(|v| v.trim().trim_matches('"').to_lowercase())
            .unwrap_or_default();
        if matches!(choice.as_str(), "bandcamp-dl" | "bcdl" | "bandcamp_dl") {
            return self
                .bcdl
                .get_or_init(|| {
                    Arc::new(BcdlDownloader::new(
                        BandcampDl::new(self.cfg.bandcamp_dl_bin.clone()).with_template(self.cfg.download_template.clone()),
                    ))
                })
                .clone();
        }
        self.native
            .get_or_init(|| {
                // The native downloader needs a client; without one (tests) build a default one.
                let client = self.client.clone().unwrap_or_else(|| BandcampClient::new(Default::default()));
                Arc::new(NativeDownloader::new(client, self.cfg.download_template.clone()))
            })
            .clone()
    }

    fn read_disk_state(&self, held: bool) -> DiskState {
        let base = self.downloads_base();
        let min_free = diskguard::read_min_free_db(&self.db).unwrap_or(diskguard::DEFAULT_MIN_FREE_BYTES);
        DiskState { path: base.to_string_lossy().into_owned(), free_bytes: (self.free_bytes)(&base), min_free_bytes: min_free, held }
    }
}

// -- disk guard --------------------------------------------------------------------------------

struct GuardInner {
    held: bool,
    state: Option<DiskState>,
}

/// Stop downloading before the disk is full: reads the free space of the downloads drive before
/// every claim and holds -- nothing is claimed and what is in flight is handed back (a pause, not a
/// cancel: staging dirs resume) -- until space is freed with a margin ([`diskguard::should_hold`];
/// an unreadable drive holds).
pub struct DiskGuard {
    deps: Arc<DownloadDeps>,
    inner: Mutex<GuardInner>,
    serial: AsyncMutex<()>,
}

impl DiskGuard {
    pub fn new(deps: Arc<DownloadDeps>) -> Arc<Self> {
        Arc::new(Self { deps, inner: Mutex::new(GuardInner { held: false, state: None }), serial: AsyncMutex::new(()) })
    }

    /// The last reading, or a fresh one when the loop has not looked yet. Blocking.
    pub fn state(&self) -> DiskState {
        let (cached, held) = {
            let g = self.inner.lock();
            (g.state.clone(), g.held)
        };
        if let Some(s) = cached {
            return s;
        }
        let st = self.deps.read_disk_state(held);
        self.inner.lock().state = Some(st.clone());
        st
    }

    pub fn payload(&self) -> DiskOut {
        let s = self.state();
        DiskOut { path: s.path, free_bytes: s.free_bytes, min_free_bytes: s.min_free_bytes, held: s.held }
    }

    fn publish(&self) {
        self.deps.bus.publish(TOPIC_DOWNLOADS_DISK, &self.payload());
    }

    /// A fresh reading applying the hold/release rule (from a request thread). Only the release is
    /// applied here: tripping also hands back in-flight items, which is the loop's job.
    pub fn reread(&self) -> DiskState {
        let held_now = self.inner.lock().held;
        let st = self.deps.read_disk_state(held_now);
        let held = diskguard::should_hold(st.free_bytes, st.min_free_bytes, held_now);
        if !held && held_now {
            {
                let mut g = self.inner.lock();
                g.held = false;
                g.state = Some(DiskState { held: false, ..st });
            }
            self.publish();
            return self.state();
        }
        let out = DiskState { held: held || held_now, ..st };
        self.inner.lock().state = Some(out.clone());
        out
    }

    /// Whether downloads must hold for lack of disk space -- and act on it.
    pub async fn check(&self, worker: &KindWorker) -> bool {
        let _serial = self.serial.lock().await;
        let held_now = self.inner.lock().held;
        let deps = self.deps.clone();
        let Ok(st) = tokio::task::spawn_blocking(move || deps.read_disk_state(held_now)).await else {
            return held_now;
        };
        let held = diskguard::should_hold(st.free_bytes, st.min_free_bytes, held_now);
        self.inner.lock().state = Some(DiskState { held, ..st.clone() });
        if held && !held_now {
            self.inner.lock().held = true;
            tracing::warn!("downloads held: {:?} free on {}, limit {}", st.free_bytes, st.path, st.min_free_bytes);
            worker.interrupt_all(Interrupt::Pause).await;
            self.publish();
        } else if !held && held_now {
            self.inner.lock().held = false;
            tracing::info!("downloads resumed: {:?} free on {}", st.free_bytes, st.path);
            self.publish();
            // The loop is idling on the hold poll: let it claim now.
            worker.notify();
        }
        held
    }
}

#[async_trait]
impl Gate for DiskGuard {
    async fn hold(&self, worker: &KindWorker) -> bool {
        self.check(worker).await
    }
}

// -- the worker --------------------------------------------------------------------------------

/// The download worker: a [`KindWorker`] of kind `download` with the disk guard as its gate.
pub struct DownloadWorker {
    deps: Arc<DownloadDeps>,
    worker: Arc<KindWorker>,
    guard: Arc<DiskGuard>,
    watcher: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl DownloadWorker {
    pub fn new(deps: Arc<DownloadDeps>, store: JobStore) -> Arc<Self> {
        Self::with_spec(deps, store, None)
    }

    /// As [`DownloadWorker::new`] with an explicit [`WorkerSpec`] (tests tune poll / lease).
    pub fn with_spec(deps: Arc<DownloadDeps>, store: JobStore, spec: Option<WorkerSpec>) -> Arc<Self> {
        let spec = spec.unwrap_or_else(|| WorkerSpec::new("download", deps.cfg.download_concurrency));
        let handler = Arc::new(DownloadHandler::new(deps.clone()));
        let worker = KindWorker::new(store, spec, handler);
        let guard = DiskGuard::new(deps.clone());
        worker.set_gate(guard.clone());
        Arc::new(Self { deps, worker, guard, watcher: Mutex::new(None) })
    }

    pub fn deps(&self) -> &Arc<DownloadDeps> {
        &self.deps
    }

    /// The underlying loop.
    pub fn kind_worker(&self) -> &Arc<KindWorker> {
        &self.worker
    }

    /// Register with `ctx.jobs.add_hooks(..)` so cancel/pause reach in-flight items.
    pub fn hooks(&self) -> Arc<dyn JobHooks> {
        self.worker.clone()
    }

    pub fn disk_guard(&self) -> &Arc<DiskGuard> {
        &self.guard
    }

    /// Crash cleanup of the staging dirs (after the job store's recovery, before the loop): kill
    /// orphaned `bandcamp-dl` children, purge stale staging dirs and the partials of the survivors.
    pub async fn recover_staging(&self) {
        let deps = self.deps.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let base = deps.downloads_base();
            let (killed, partials) = staging::recover_staging(&base, &deps.cfg.bandcamp_dl_bin);
            let removed = staging::purge_stale_staging(&deps.db, &base);
            if killed + partials + removed > 0 {
                tracing::info!("download recovery: {killed} orphan(s) killed, {partials} partial(s) and {removed} stale staging dir(s) removed");
            }
        })
        .await;
    }

    /// Clean up after a crash, then spawn the claim loop and the periodic disk watcher. Job crash
    /// recovery (`JobsService::recover`) must already have run.
    pub async fn start(self: &Arc<Self>) {
        self.recover_staging().await;
        self.worker.start();
        let mut slot = self.watcher.lock();
        if slot.is_none() {
            let me = self.clone();
            *slot = Some(tokio::spawn(async move {
                // The gate only runs when a slot is free; this catches a disk filling up under a
                // long download while every slot is busy.
                loop {
                    tokio::time::sleep(DISK_POLL).await;
                    me.guard.check(&me.worker).await;
                }
            }));
        }
    }

    pub async fn stop(&self) {
        let w = self.watcher.lock().take();
        if let Some(w) = w {
            w.abort();
        }
        self.worker.stop().await;
    }

    /// New work was enqueued: claim now instead of waiting for the poll.
    pub fn notify(&self) {
        self.worker.notify();
    }

    /// The last disk reading (a fresh one when nothing has looked yet). Blocking.
    pub fn disk_state(&self) -> DiskState {
        self.guard.state()
    }

    /// A fresh reading applying the hold/release rule (request thread). Blocking.
    pub fn reread_disk(&self) -> DiskState {
        let st = self.guard.reread();
        if !st.held {
            self.worker.notify();
        }
        st
    }

    /// Run the disk guard now (what the gate does before each claim).
    pub async fn check_disk(&self) -> bool {
        self.guard.check(&self.worker).await
    }

    /// Ids of the items of `job_id` a download is running for right now.
    pub fn inflight_items(&self, job_id: &str) -> Vec<i64> {
        self.worker.inflight_items(job_id)
    }

    /// Stop the downloads in flight for a job, now, and settle their items: `Cancel` ends them
    /// (`cancelled`), `Pause` hands them back to `pending` with the attempt refunded (staging
    /// stays). Returns how many were interrupted; their rows are settled when this returns.
    pub async fn interrupt_job(&self, job_id: &str, reason: Interrupt) -> usize {
        let n = self.worker.inflight_items(job_id).len();
        self.worker.interrupt_job(job_id, reason).await;
        n
    }
}

// -- module convention ---------------------------------------------------------------------------

/// What `init` registers in the [`Ctx`]: the worker and its dependencies.
pub struct DownloadServices {
    pub worker: Arc<DownloadWorker>,
    pub deps: Arc<DownloadDeps>,
}

/// Replace the library port (default: [`BcLibrary`]); call before `start`.
pub fn set_library(ctx: &Arc<Ctx>, lib: Arc<dyn LibraryPort>) {
    ctx.expect::<DownloadServices>().deps.set_library(lib);
}

pub fn init(ctx: &Arc<Ctx>) {
    let deps = Arc::new(DownloadDeps::new(ctx.db.clone(), ctx.bus.clone(), ctx.cfg.clone(), Some(ctx.client.clone())));
    let worker = DownloadWorker::new(deps.clone(), ctx.jobs.store().clone());
    ctx.jobs.add_hooks(worker.hooks());
    ctx.put(Arc::new(DownloadServices { worker, deps }));
}

pub async fn start(ctx: &Arc<Ctx>) {
    ctx.jobs.recover().await;
    if let Some(s) = ctx.get::<DownloadServices>() {
        s.worker.start().await;
    }
}
