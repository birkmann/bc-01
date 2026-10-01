//! The analysis runner: claims `analyze` job items from the durable queue (bc-jobs), fans them
//! out to the niced rayon pool, and writes results back in batches of ~200 through
//! `Db::write`. Priority comes from the job (PLAN 7.3): deck 0, new downloads 50, queue/set 100,
//! backfill 200.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_jobs::{Claimed, Complete, JobStore};
use bc_types::analysis::{
    AnalysisBatchEvent, AnalysisItemEvent, AnalysisProgressEvent, TOPIC_ANALYSIS_BATCH, TOPIC_ANALYSIS_ITEM,
    TOPIC_ANALYSIS_PROGRESS, WaveformMeta,
};
use bc_waveform::store::WaveformStore;
use parking_lot::RwLock;
use tokio::sync::mpsc;

use crate::persist::{WorkerOutput, persist_batch, read_policy};
use crate::pipeline::{AnalyzeOptions, analyze_file};

pub const KIND: &str = "analyze";
pub const BATCH: usize = 200;
const LEASE_SECS: f64 = 900.0;

pub const PRIORITY_DECK: i64 = 0;
pub const PRIORITY_NEW_DOWNLOAD: i64 = 50;
pub const PRIORITY_QUEUE: i64 = 100;
pub const PRIORITY_BACKFILL: i64 = 200;

struct Done {
    item_id: i64,
    job_id: String,
    track_id: i64,
    out: Result<WorkerOutput, String>,
}

pub struct Runner {
    pub db: Db,
    pub store: JobStore,
    pub bus: Arc<EventBus>,
    pub waveforms: Arc<WaveformStore>,
    pub pool: Arc<rayon::ThreadPool>,
    pub opts: RwLock<Arc<AnalyzeOptions>>,
    pub threads: usize,
    pub sidecar: Option<Arc<crate::sidecar::EssentiaSidecar>>,
    stop: AtomicBool,
}

/// Analyse one file and write its waveform; the DB is untouched. Panics become errors.
pub fn work(
    track_id: i64,
    path: &std::path::Path,
    opts: &AnalyzeOptions,
    wfs: &WaveformStore,
    sidecar: Option<&crate::sidecar::EssentiaSidecar>,
) -> Result<WorkerOutput, String> {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| analyze_file(path, opts)));
    let mut analysis = match res {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("analysis panicked".into()),
    };
    let mut wf_meta: Option<WaveformMeta> = None;
    if let Some(wf) = analysis.waveform.take() {
        match wfs.put(track_id, &wf) {
            Ok(bytes) => {
                wf_meta = Some(wf.meta_with_bytes(track_id, bytes as i64));
            }
            Err(e) => tracing::warn!("waveform write failed for track {track_id}: {e}"),
        }
    }
    // essentia only for tracks that would otherwise have no (or only native) BPM/key
    let sidecar = sidecar.and_then(|s| match s.analyze(path) {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!("essentia sidecar failed for track {track_id}: {e}; keeping native BPM/key");
            None
        }
    });
    Ok(WorkerOutput { analysis, wf_meta, sidecar })
}

impl Runner {
    pub fn new(
        db: Db,
        store: JobStore,
        bus: Arc<EventBus>,
        cfg: &Config,
        waveforms: Arc<WaveformStore>,
        opts: AnalyzeOptions,
    ) -> Arc<Self> {
        let threads = cfg.analysis_threads();
        Arc::new(Self {
            db,
            store,
            bus,
            waveforms,
            pool: crate::pool::build_pool(threads),
            opts: RwLock::new(Arc::new(opts)),
            threads,
            sidecar: crate::sidecar::EssentiaSidecar::detect((threads / 6).clamp(1, 4)).map(Arc::new),
            stop: AtomicBool::new(false),
        })
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.store.notify();
    }

    /// True when the track has no BPM/key from essentia (no row, or no values): the sidecar is
    /// worth its cost only then.
    pub fn wants_sidecar(&self, track_id: i64) -> Option<Arc<crate::sidecar::EssentiaSidecar>> {
        let sc = self.sidecar.clone()?;
        let has: bool = self
            .db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT bpm IS NOT NULL AND camelot IS NOT NULL AND (analyzer IN ('essentia-import','essentia-sidecar') OR (analyzer IS NULL AND backend='essentia')) FROM analysis WHERE track_id = ?1",
                    [track_id],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(false))
            })
            .unwrap_or(false);
        (!has).then_some(sc)
    }

    fn file_for(&self, track_id: i64) -> Option<PathBuf> {
        self.db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT path FROM files WHERE track_id = ?1 AND missing_since IS NULL ORDER BY id LIMIT 1",
                    [track_id],
                    |r| r.get::<_, String>(0),
                )
                .ok())
            })
            .ok()
            .flatten()
            .map(PathBuf::from)
    }

    /// Main loop; spawn with `tokio::spawn(runner.clone().run())`.
    pub async fn run(self: Arc<Self>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<Done>();
        let mut wake = self.store.subscribe_wake();
        let target = self.threads * 3;
        let mut in_flight: HashMap<i64, String> = HashMap::new(); // item id -> job id
        let mut buf: Vec<Done> = Vec::new();
        let mut last_flush = Instant::now();
        let mut last_beat = Instant::now();
        let mut flushes = 0u32;
        let mut batch_done: u32 = 0;
        let mut batch_total: u32 = 0;
        let pid = std::process::id() as i64;
        tracing::info!("analysis runner started with {} workers (nice {})", self.threads, crate::pool::WORKER_NICE);

        loop {
            if self.stop.load(Ordering::SeqCst) && in_flight.is_empty() && buf.is_empty() {
                break;
            }
            // fill the pool
            let mut claimed_now: Vec<i64> = Vec::new();
            while !self.stop.load(Ordering::SeqCst) && in_flight.len() < target {
                let claim = self.store.run(move |s| s.claim_item(KIND, pid, LEASE_SECS)).await;
                let Ok(Some(Claimed { item, job })) = claim else { break };
                let Some(track_id) = item.track_id else {
                    let id = item.id;
                    let _ = self.store.run(move |s| s.fail_item(id, "item has no track", "analysis", false)).await;
                    continue;
                };
                let Some(path) = self.file_for(track_id) else {
                    let id = item.id;
                    let _ = self.store.run(move |s| s.fail_item(id, "no available file", "missing_file", false)).await;
                    continue;
                };
                if in_flight.is_empty() && buf.is_empty() {
                    batch_done = 0;
                    batch_total = 0;
                }
                batch_total += 1;
                in_flight.insert(item.id, job.id.clone());
                claimed_now.push(track_id);
                let (item_id, job_id) = (item.id, job.id.clone());
                let (opts, wfs, txc) = (self.opts.read().clone(), self.waveforms.clone(), tx.clone());
                let sidecar = self.wants_sidecar(track_id);
                self.pool.spawn(move || {
                    let out = work(track_id, &path, &opts, &wfs, sidecar.as_deref());
                    let _ = txc.send(Done { item_id, job_id, track_id, out });
                });
            }
            if !claimed_now.is_empty() {
                self.bus.publish(TOPIC_ANALYSIS_BATCH, &AnalysisBatchEvent { tracks: claimed_now, status: "running".into() });
            }

            let tick = tokio::time::sleep(Duration::from_millis(400));
            tokio::select! {
                Some(done) = rx.recv() => {
                    in_flight.remove(&done.item_id);
                    batch_done += 1;
                    let ev = match &done.out {
                        Ok(o) => AnalysisItemEvent {
                            track_id: done.track_id,
                            status: if o.analysis.tempo.is_some() && o.analysis.key.is_some() { "ok" } else { "partial" }.into(),
                            bpm: o.sidecar.as_ref().and_then(|s| s.bpm).or(o.analysis.tempo.as_ref().map(|t| t.bpm)),
                            camelot: o.sidecar.as_ref().and_then(|s| s.camelot()).or(o.analysis.key.as_ref().map(|k| k.result.camelot)).map(|c| c.to_string()),
                            key: o.analysis.key.as_ref().map(|k| bc_music::camelot::key_name(k.result.pitch_class as i32, if k.result.minor { bc_music::camelot::Mode::Minor } else { bc_music::camelot::Mode::Major })),
                            energy: Some(o.analysis.energy_legacy),
                            error: None,
                            done: batch_done,
                            batch: batch_total,
                        },
                        Err(e) => AnalysisItemEvent {
                            track_id: done.track_id, status: "failed".into(), bpm: None, camelot: None, key: None,
                            energy: None, error: Some(e.clone()), done: batch_done, batch: batch_total,
                        },
                    };
                    self.bus.publish(TOPIC_ANALYSIS_ITEM, &ev);
                    buf.push(done);
                    if buf.len() >= BATCH {
                        self.flush(&mut buf).await;
                        self.maybe_evict(&mut flushes);
                        last_flush = Instant::now();
                    }
                }
                _ = wake.changed() => {}
                _ = tick => {
                    if !buf.is_empty() && (in_flight.is_empty() || last_flush.elapsed() > Duration::from_secs(2)) {
                        self.flush(&mut buf).await;
                        last_flush = Instant::now();
                    }
                }
            }
            if !in_flight.is_empty() && last_beat.elapsed() > Duration::from_secs(30) {
                for id in in_flight.keys().copied().collect::<Vec<_>>() {
                    let _ = self.store.run(move |s| s.heartbeat(id, LEASE_SECS)).await;
                }
                last_beat = Instant::now();
            }
        }
        tracing::info!("analysis runner stopped");
    }

    /// LRU size cap of the waveform detail cache, enforced every 20 batches.
    fn maybe_evict(&self, flushes: &mut u32) {
        *flushes += 1;
        if *flushes % 20 == 1 {
            let wf = self.waveforms.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = wf.evict_to_cap() {
                    tracing::warn!("waveform eviction failed: {e}");
                }
            });
        }
    }

    async fn flush(&self, buf: &mut Vec<Done>) {
        let items = std::mem::take(buf);
        if items.is_empty() {
            return;
        }
        let policy = read_policy(&self.db);
        let rows: Vec<(i64, Result<WorkerOutput, String>)> =
            items.iter().map(|d| (d.track_id, d.out.clone())).collect();
        let db = self.db.clone();
        let write = tokio::task::spawn_blocking(move || persist_batch(&db, rows, policy)).await;
        let write_ok = matches!(write, Ok(Ok(())));
        if !write_ok {
            tracing::error!("analysis batch write failed: {:?}", write);
        }
        let mut jobs: HashMap<String, ()> = HashMap::new();
        let mut ids = Vec::new();
        for d in &items {
            jobs.insert(d.job_id.clone(), ());
            let id = d.item_id;
            match (&d.out, write_ok) {
                (Ok(o), true) => {
                    ids.push(d.track_id);
                    let msg = match (&o.analysis.tempo, &o.analysis.key) {
                        (Some(t), Some(k)) => format!("{:.0} BPM {}", t.bpm, k.result.camelot),
                        _ => "partial".to_string(),
                    };
                    let _ = self.store.run(move |s| s.complete_item(id, Complete { message: Some(msg), result: None, release_id: None })).await;
                }
                (Ok(_), false) => {
                    let _ = self.store.run(move |s| s.fail_item(id, "database write failed", "db", true)).await;
                }
                (Err(e), _) => {
                    let e = e.clone();
                    // a corrupt file fails identically forever: never retry
                    let _ = self.store.run(move |s| s.fail_item(id, &e, "analysis", false)).await;
                }
            }
        }
        if !ids.is_empty() {
            self.bus.invalidate("track", ids);
        }
        for job_id in jobs.into_keys() {
            let jid = job_id.clone();
            if let Ok(Some(job)) = self.store.run(move |s| s.get_job(&jid)).await {
                self.bus.publish(
                    TOPIC_ANALYSIS_PROGRESS,
                    &AnalysisProgressEvent {
                        job_id: job.id.clone(),
                        status: job.status.clone(),
                        total: job.total,
                        completed: job.completed,
                        failed: job.failed,
                    },
                );
            }
        }
    }
}
