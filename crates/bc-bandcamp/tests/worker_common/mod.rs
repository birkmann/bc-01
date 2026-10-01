//! Shared fixtures of the worker tests: a temp DB, a fake `LibraryPort` that inserts minimal
//! artist/release/track/file rows with raw SQL, scripted downloaders, and the helpers the Python
//! tests got from `_queue_item` / `worker._run_item`.
#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bc_bandcamp::download::bcdl::{BandcampDl, SweepConfig};
use bc_bandcamp::download::dedup::normalise;
use bc_bandcamp::download::library_port::{IngestResult, LibraryPort, LovedOutcome};
use bc_bandcamp::download::worker::{BcdlDownloader, DownloadDeps, DownloadHandler};
use bc_bandcamp::download::{DownloadSpec, Downloader, Outcome, OutcomeKind, ProgressFn};
use bc_bandcamp::error::Result as HResult;
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_db::rusqlite::{OptionalExtension, params};
use bc_jobs::{HandlerOutcome, ItemCtx, ItemHandler, JobItem, JobStore, JobsService, NewItem, NewJob};
use tokio_util::sync::CancellationToken;

pub const FAKE: &str = env!("CARGO_BIN_EXE_fake_bandcamp_dl");
pub const GRID: &str = "https://somatic.bandcamp.com/album/grid-failure";

/// A temp DB, a bus, a config rooted in the temp dir and the jobs service.
pub struct Env {
    pub dir: tempfile::TempDir,
    pub db: Db,
    pub bus: Arc<EventBus>,
    pub cfg: Config,
    pub jobs: JobsService,
}

impl Env {
    pub fn new() -> Self {
        Self::with(|_| {})
    }

    pub fn with(tweak: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::open(dir.path().join("worker.db")).expect("open db");
        let mut cfg = Config::from_env();
        cfg.data_dir = dir.path().join("data");
        cfg.download_dir = cfg.data_dir.join("downloads");
        cfg.download_concurrency = 1;
        tweak(&mut cfg);
        let bus = Arc::new(EventBus::new());
        let jobs = JobsService::new(db.clone(), bus.clone());
        Self { dir, db, bus, cfg, jobs }
    }

    pub fn store(&self) -> &JobStore {
        self.jobs.store()
    }

    pub fn downloads(&self) -> PathBuf {
        self.cfg.download_dir.clone()
    }

    /// Deps with the fake library and no client.
    pub fn deps(&self) -> DownloadDeps {
        DownloadDeps::new(self.db.clone(), self.bus.clone(), self.cfg.clone(), None)
            .with_library(Arc::new(FakeLibrary::new(self.db.clone())))
    }

    pub fn handler(&self, dl: Arc<dyn Downloader>) -> DownloadHandler {
        DownloadHandler::new(Arc::new(self.deps().with_downloader(dl)))
    }

    pub fn count(&self, sql: &str) -> i64 {
        self.db.read(|c| Ok(c.query_row(sql, [], |r| r.get::<_, i64>(0))?)).expect("count")
    }

    pub fn item(&self, id: i64) -> JobItem {
        self.store().get_item(id).expect("get_item").expect("item exists")
    }

    pub fn job(&self, id: &str) -> bc_jobs::Job {
        self.store().get_job(id).expect("get_job").expect("job exists")
    }

    pub fn statuses(&self, job_id: &str) -> Vec<String> {
        let id = job_id.to_string();
        self.db
            .read(move |c| {
                let mut st = c.prepare("SELECT status FROM job_items WHERE job_id = ?1 ORDER BY seq")?;
                Ok(st.query_map([&id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
            })
            .expect("statuses")
    }

    pub fn create_download(&self, urls: &[&str]) -> bc_jobs::Job {
        let items = urls.iter().map(|u| NewItem::url(*u, "album")).collect();
        self.store().create_job(NewJob::new("download", items).label("test")).expect("create_job")
    }

    /// `_queue_item`: one job with one album item, claimed (so it is `running`).
    pub fn queue_claimed(&self, url: &str) -> i64 {
        self.queue_claimed_with(url, serde_json::json!({}), None)
    }

    pub fn queue_claimed_with(&self, url: &str, params: serde_json::Value, target_dir: Option<&str>) -> i64 {
        let mut item = NewItem::url(url, "album");
        item.target_dir = target_dir.map(str::to_string);
        self.store().create_job(NewJob::new("download", vec![item]).params(params)).expect("create_job");
        self.claim().expect("claim")
    }

    pub fn claim(&self) -> Option<i64> {
        self.store().claim_item("download", 1, 300.0).expect("claim").map(|c| c.item.id)
    }

    /// Stand in for the runner: run the handler on a claimed item and settle the outcome the way
    /// `KindWorker` does for a non-interrupted run. Returns the handler's own outcome.
    pub async fn run_item(&self, handler: &DownloadHandler, item_id: i64) -> HandlerOutcome {
        let item = self.item(item_id);
        let job = self.job(&item.job_id);
        let ctx = ItemCtx { store: self.store().clone(), job, item, cancel: CancellationToken::new() };
        let outcome = handler.run(ctx).await;
        let store = self.store().clone();
        match &outcome {
            HandlerOutcome::Done(c) => {
                store.complete_item(item_id, c.clone()).expect("complete");
            }
            HandlerOutcome::Skipped(m) => {
                store.skip_item(item_id, m).expect("skip");
            }
            HandlerOutcome::Failed { error, class, retryable } => {
                store.fail_item(item_id, error, class, *retryable).expect("fail");
            }
            HandlerOutcome::Interrupted => {
                store.fail_item(item_id, "Cancelled", "cancelled", true).expect("fail");
            }
            HandlerOutcome::Handled => {}
        }
        outcome
    }

    /// Make a pending item claimable again immediately (clear the retry backoff).
    pub fn clear_backoff(&self, item_id: i64) {
        self.db
            .write(move |t| {
                t.execute("UPDATE job_items SET next_attempt_at = NULL WHERE id = ?1", [item_id])?;
                Ok(())
            })
            .expect("clear_backoff");
    }

    pub fn set_max_attempts(&self, item_id: i64, n: i64) {
        self.db
            .write(move |t| {
                t.execute("UPDATE job_items SET max_attempts = ?2 WHERE id = ?1", params![item_id, n])?;
                Ok(())
            })
            .expect("max_attempts");
    }

    pub fn fan(&self, name: &str) -> i64 {
        let name = name.to_string();
        self.db
            .write(move |t| {
                t.execute(
                    "INSERT INTO fans(username, url, display_name, is_self, created_at) VALUES (?1, ?2, ?1, 0, CURRENT_TIMESTAMP)",
                    params![name, format!("https://bandcamp.com/{name}")],
                )?;
                Ok(t.last_insert_rowid())
            })
            .expect("fan")
    }

    pub fn release_by_url(&self, url: &str) -> Option<i64> {
        let key = normalise(url);
        self.db
            .read(move |c| Ok(c.query_row("SELECT id FROM releases WHERE bandcamp_url = ?1", [&key], |r| r.get(0)).optional()?))
            .expect("release_by_url")
    }
}

/// `_release(session, title, bandcamp_url=...)`.
pub fn seed_release(db: &Db, title: &str, url: Option<&str>) -> i64 {
    let (title, url) = (title.to_string(), url.map(str::to_string));
    db.write(move |t| {
        t.execute(
            "INSERT INTO releases(title, title_key, kind, bandcamp_url, added_at) VALUES (?1, lower(?1), 'album', ?2, CURRENT_TIMESTAMP)",
            params![title, url],
        )?;
        Ok(t.last_insert_rowid())
    })
    .expect("seed_release")
}

// -- the fake library ---------------------------------------------------------------------------

/// `LibraryPort` over raw SQL: enough artist/release/track/file rows for the worker's label / URL /
/// shelf logic to be observable. Artist = grandparent dir, album = parent dir of each file.
pub struct FakeLibrary {
    db: Db,
    /// `adopt_for_urls` never returns (a cancel must still settle the item).
    pub stall_adopt: AtomicBool,
    pub ingests: AtomicUsize,
    pub expected: parking_lot::Mutex<Vec<(i64, i64)>>,
    pub disowned: parking_lot::Mutex<Vec<(i64, String, i64)>>,
}

impl FakeLibrary {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            stall_adopt: AtomicBool::new(false),
            ingests: AtomicUsize::new(0),
            expected: Default::default(),
            disowned: Default::default(),
        }
    }
}

fn name_key(s: &str) -> String {
    bc_db::util::name_key(s)
}

#[async_trait]
impl LibraryPort for FakeLibrary {
    async fn ingest(&self, base: &Path, files: &[PathBuf], source_fan_id: Option<i64>) -> HResult<IngestResult> {
        self.ingests.fetch_add(1, Ordering::SeqCst);
        let base = base.to_path_buf();
        let files = files.to_vec();
        Ok(self
            .db
            .write_async(move |tx| {
                let base_s = base.to_string_lossy().into_owned();
                let root_id: i64 = match tx
                    .query_row("SELECT id FROM library_roots WHERE path = ?1", [&base_s], |r| r.get(0))
                    .optional()?
                {
                    Some(id) => id,
                    None => {
                        tx.execute("INSERT INTO library_roots(path, kind, watch, enabled) VALUES (?1, 'downloads', 0, 1)", [&base_s])?;
                        tx.last_insert_rowid()
                    }
                };
                let mut out = IngestResult::default();
                for f in &files {
                    let album_dir = f.parent().unwrap_or(&base).to_path_buf();
                    let album = album_dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let artist = album_dir
                        .parent()
                        .and_then(|p| p.file_name())
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let stem = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    let title = stem.split_once(" - ").map(|(_, t)| t.to_string()).unwrap_or(stem.clone());

                    let artist_id: i64 = match tx
                        .query_row("SELECT id FROM artists WHERE name_key = ?1", [name_key(&artist)], |r| r.get(0))
                        .optional()?
                    {
                        Some(id) => id,
                        None => {
                            tx.execute(
                                "INSERT INTO artists(name, name_key, created_at) VALUES (?1, ?2, CURRENT_TIMESTAMP)",
                                params![artist, name_key(&artist)],
                            )?;
                            tx.last_insert_rowid()
                        }
                    };
                    let existing: Option<i64> = tx
                        .query_row(
                            "SELECT id FROM releases WHERE artist_id = ?1 AND title_key = ?2",
                            params![artist_id, name_key(&album)],
                            |r| r.get(0),
                        )
                        .optional()?;
                    let release_id = match existing {
                        Some(id) => id,
                        None => {
                            tx.execute(
                                "INSERT INTO releases(title, title_key, artist_id, kind, folder_path, added_at, source_fan_id) \
                                 VALUES (?1, ?2, ?3, 'album', ?4, CURRENT_TIMESTAMP, ?5)",
                                params![album, name_key(&album), artist_id, album_dir.to_string_lossy(), source_fan_id],
                            )?;
                            let id = tx.last_insert_rowid();
                            out.releases_created.push(id);
                            id
                        }
                    };
                    if !out.release_ids.contains(&release_id) {
                        out.release_ids.push(release_id);
                    }
                    let path_s = f.to_string_lossy().into_owned();
                    let known: Option<i64> =
                        tx.query_row("SELECT track_id FROM files WHERE path = ?1", [&path_s], |r| r.get(0)).optional()?.flatten();
                    let track_id = match known {
                        Some(id) => id,
                        None => {
                            tx.execute(
                                "INSERT INTO tracks(release_id, artist_id, title, title_key, loved, play_count, skip_count, added_at) \
                                 VALUES (?1, ?2, ?3, ?4, 0, 0, 0, CURRENT_TIMESTAMP)",
                                params![release_id, artist_id, title, name_key(&title)],
                            )?;
                            let id = tx.last_insert_rowid();
                            let size = std::fs::metadata(f).map(|m| m.len() as i64).unwrap_or(0);
                            tx.execute(
                                "INSERT INTO files(track_id, root_id, path, rel_path, ext, size_bytes, mtime_ns, first_seen_at, last_seen_at) \
                                 VALUES (?1, ?2, ?3, ?4, 'mp3', ?5, 0, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
                                params![
                                    id,
                                    root_id,
                                    path_s,
                                    f.strip_prefix(&base).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
                                    size
                                ],
                            )?;
                            out.tracks_added += 1;
                            id
                        }
                    };
                    out.track_ids.push(track_id);
                }
                Ok(out)
            })
            .await?)
    }

    async fn record_expected(&self, release_id: i64, expected: i64) -> HResult<()> {
        self.expected.lock().push((release_id, expected));
        self.db
            .write_async(move |tx| {
                tx.execute("UPDATE releases SET expected_track_count = ?2 WHERE id = ?1", params![release_id, expected])?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    async fn disown_url(&self, release_id: i64, url: &str, landed_on: i64) -> HResult<()> {
        self.disowned.lock().push((release_id, url.to_string(), landed_on));
        Ok(())
    }

    async fn adopt_for_urls(&self, urls: &[String]) -> HResult<i64> {
        if self.stall_adopt.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        let keys: HashSet<String> = urls.iter().map(|u| normalise(u).to_lowercase()).collect();
        Ok(self
            .db
            .write_async(move |tx| {
                let mut n = 0i64;
                let rows: Vec<(i64, String)> = {
                    let mut st = tx.prepare("SELECT id, bandcamp_url FROM releases WHERE bandcamp_url IS NOT NULL AND source_fan_id IS NOT NULL")?;
                    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
                };
                for (id, url) in rows {
                    if keys.contains(&normalise(&url).to_lowercase()) {
                        tx.execute("UPDATE releases SET source_fan_id = NULL WHERE id = ?1", [id])?;
                        n += 1;
                    }
                }
                Ok(n)
            })
            .await?)
    }

    async fn adopt_releases(&self, ids: &[i64]) -> HResult<i64> {
        let ids = ids.to_vec();
        Ok(self
            .db
            .write_async(move |tx| {
                let mut n = 0i64;
                for id in ids {
                    n += tx.execute("UPDATE releases SET source_fan_id = NULL WHERE id = ?1 AND source_fan_id IS NOT NULL", [id])? as i64;
                }
                Ok(n)
            })
            .await?)
    }

    async fn reconcile_loved(&self, _release_id: i64) -> HResult<LovedOutcome> {
        Ok(LovedOutcome::default())
    }
}

// -- downloaders ----------------------------------------------------------------------------------

/// The fake bandcamp-dl in `mode`, wrapped the way the worker uses it.
pub fn bcdl(mode: &str) -> Arc<BcdlDownloader> {
    bcdl_with(FAKE, mode)
}

pub fn bcdl_with(bin: &str, mode: &str) -> Arc<BcdlDownloader> {
    Arc::new(BcdlDownloader::new(
        BandcampDl::new(bin)
            .with_env("FAKE_BCDL_MODE", mode)
            .with_sweep(SweepConfig { probe: Duration::from_millis(50), ..Default::default() }),
    ))
}

fn crash(detail: &str) -> Outcome {
    Outcome { kind: OutcomeKind::Crash, new_files: vec![], tracks_expected: None, availability: None, tracks_finished: 0, retryable: false, detail: detail.into() }
}

/// Runs until cancelled, like an album mid-download.
pub struct HangDownloader {
    pub started: AtomicUsize,
}

impl HangDownloader {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { started: AtomicUsize::new(0) })
    }
}

#[async_trait]
impl Downloader for HangDownloader {
    fn name(&self) -> &'static str {
        "hang"
    }
    async fn download(&self, _spec: &DownloadSpec, _p: ProgressFn<'_>, cancel: &CancellationToken) -> Outcome {
        self.started.fetch_add(1, Ordering::SeqCst);
        cancel.cancelled().await;
        crash("Cancelled.")
    }
}

/// Counts concurrent runs; holds every run until `release` is notified.
pub struct GateDownloader {
    pub live: AtomicUsize,
    pub peak: AtomicUsize,
    pub calls: AtomicUsize,
    pub started: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
    pub open: AtomicBool,
    pub fail: bool,
}

impl GateDownloader {
    pub fn new(fail: bool) -> Arc<Self> {
        Arc::new(Self {
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            open: AtomicBool::new(false),
            fail,
        })
    }
}

#[async_trait]
impl Downloader for GateDownloader {
    fn name(&self) -> &'static str {
        "gate"
    }
    async fn download(&self, _spec: &DownloadSpec, _p: ProgressFn<'_>, _cancel: &CancellationToken) -> Outcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        self.started.notify_one();
        while !self.open.load(Ordering::SeqCst) {
            let n = self.release.notified();
            tokio::pin!(n);
            if self.open.load(Ordering::SeqCst) {
                break;
            }
            let _ = tokio::time::timeout(Duration::from_millis(50), n).await;
        }
        self.live.fetch_sub(1, Ordering::SeqCst);
        if self.fail {
            panic!("boom");
        }
        crash("done")
    }
}

/// Poll `pred` until true (panics after `secs`).
pub async fn wait_for(secs: f64, what: &str, mut pred: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs_f64(secs);
    while !pred() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub fn count_files(dir: &Path, glob_suffix: &str) -> usize {
    fn rec(dir: &Path, suffix: &str, n: &mut usize) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                rec(&p, suffix, n);
            } else if p.to_string_lossy().ends_with(suffix) {
                *n += 1;
            }
        }
    }
    let mut n = 0;
    rec(dir, glob_suffix, &mut n);
    n
}

/// Serve a router on an ephemeral local port; returns the base URL.
pub async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

/// The app as `bc-server` mounts it: `Ctx` + the download worker's services + the generic job
/// routes + the download routes, with the fake library, served on a local port.
pub struct App {
    pub env: Env,
    pub ctx: Arc<bc_bandcamp::service::Ctx>,
    pub base: String,
    pub http: reqwest::Client,
}

impl App {
    /// `downloader`: `Some` pins the downloader (otherwise the setting decides).
    pub async fn new(downloader: Option<Arc<dyn Downloader>>) -> Self {
        Self::with(Env::new(), downloader).await
    }

    pub async fn with(env: Env, downloader: Option<Arc<dyn Downloader>>) -> Self {
        let ctx = bc_bandcamp::service::Ctx::new(env.db.clone(), env.bus.clone(), env.cfg.clone(), env.jobs.clone());
        // `init` registers `DownloadServices` (worker + hooks); swap in the fake library and, when
        // asked, the pinned downloader by re-initialising from our own deps.
        let mut deps = DownloadDeps::new(env.db.clone(), env.bus.clone(), env.cfg.clone(), Some(ctx.client.clone()))
            .with_library(Arc::new(FakeLibrary::new(env.db.clone())));
        if let Some(d) = downloader {
            deps = deps.with_downloader(d);
        }
        let deps = Arc::new(deps);
        let worker = bc_bandcamp::download::worker::DownloadWorker::new(deps.clone(), env.store().clone());
        env.jobs.add_hooks(worker.hooks());
        ctx.put(Arc::new(bc_bandcamp::download::worker::DownloadServices { worker, deps }));
        let router = bc_bandcamp::api::downloads::router(ctx.clone()).merge(env.jobs.router());
        let base = serve(router).await;
        Self { env, ctx, base, http: reqwest::Client::new() }
    }

    pub fn worker(&self) -> Arc<bc_bandcamp::download::worker::DownloadWorker> {
        self.ctx.expect::<bc_bandcamp::download::worker::DownloadServices>().worker.clone()
    }

    pub async fn start(&self) {
        self.env.jobs.recover().await;
        self.worker().start().await;
    }

    pub async fn get(&self, path: &str) -> (u16, serde_json::Value) {
        let r = self.http.get(format!("{}{path}", self.base)).send().await.expect("get");
        (r.status().as_u16(), r.json().await.unwrap_or(serde_json::Value::Null))
    }

    pub async fn post(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let r = self.http.post(format!("{}{path}", self.base)).json(&body).send().await.expect("post");
        (r.status().as_u16(), r.json().await.unwrap_or(serde_json::Value::Null))
    }

    pub async fn put(&self, path: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let r = self.http.put(format!("{}{path}", self.base)).json(&body).send().await.expect("put");
        (r.status().as_u16(), r.json().await.unwrap_or(serde_json::Value::Null))
    }

    pub async fn delete(&self, path: &str) -> u16 {
        self.http.delete(format!("{}{path}", self.base)).send().await.expect("delete").status().as_u16()
    }
}
