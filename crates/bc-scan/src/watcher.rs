//! Filesystem watcher for hot roots (`library_roots.watch = 1`, any kind).
//!
//! `notify` v8 recommended watcher; raw events are debounced (quiet period, default 2 s, with a
//! hard cap so a constant stream still flushes), coalesced per root and handed to
//! [`crate::scanner::scan_paths`] as incremental ingests. Adding/removing/patching a root
//! (routes bump [`crate::roots::ROOTS_EPOCH`]) re-syncs the watched set within a second.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bc_libcore::Ctx;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

use crate::ingest::{RootInfo, best_root};
use crate::roots::ROOTS_EPOCH;

#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Quiet period after the last event before changes are flushed.
    pub debounce: Duration,
    /// A burst longer than this is flushed anyway.
    pub max_wait: Duration,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self { debounce: Duration::from_secs(2), max_wait: Duration::from_secs(30) }
    }
}

/// Handle to the running watcher thread. Dropping it stops the watcher.
pub struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    /// Start watching every root with `watch = 1` (and `enabled = 1`).
    pub fn start(ctx: &Ctx) -> Watcher {
        Self::start_with(ctx, WatchConfig::default())
    }

    pub fn start_with(ctx: &Ctx, cfg: WatchConfig) -> Watcher {
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, ctx2) = (stop.clone(), ctx.clone());
        let thread = std::thread::Builder::new()
            .name("bc-watcher".into())
            .spawn(move || watch_loop(ctx2, cfg, stop2))
            .ok();
        Watcher { stop, thread }
    }

    /// Stop the watcher and wait for its thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn hot_roots(ctx: &Ctx) -> Vec<RootInfo> {
    ctx.read(|c| {
        let mut st = c.prepare("SELECT id, path, kind FROM library_roots WHERE watch = 1 AND enabled = 1")?;
        Ok(st
            .query_map([], |r| Ok(RootInfo { id: r.get(0)?, path: PathBuf::from(r.get::<_, String>(1)?), kind: r.get(2)? }))?
            .collect::<Result<Vec<_>, _>>()?)
    })
    .unwrap_or_default()
}

fn watch_loop(ctx: Ctx, cfg: WatchConfig, stop: Arc<AtomicBool>) {
    let (tx, rx) = crossbeam_channel::unbounded::<notify::Result<notify::Event>>();
    let mut watcher: RecommendedWatcher = match notify::recommended_watcher(move |ev| {
        let _ = tx.send(ev);
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(error = %e, "cannot create filesystem watcher");
            return;
        }
    };
    let mut epoch = u64::MAX;
    let mut roots: Vec<RootInfo> = Vec::new();
    let mut pending: HashMap<i64, BTreeSet<PathBuf>> = HashMap::new();
    let mut first: Option<Instant> = None;
    let mut last: Option<Instant> = None;

    while !stop.load(Ordering::SeqCst) {
        let e = ROOTS_EPOCH.load(Ordering::SeqCst);
        if e != epoch {
            epoch = e;
            for r in &roots {
                let _ = watcher.unwatch(&r.path);
            }
            roots = hot_roots(&ctx);
            for r in &roots {
                match watcher.watch(&r.path, RecursiveMode::Recursive) {
                    Ok(()) => tracing::info!(root = %r.path.display(), "watching"),
                    Err(err) => tracing::warn!(root = %r.path.display(), error = %err, "cannot watch root"),
                }
            }
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(ev)) => {
                if matches!(ev.kind, EventKind::Access(_)) {
                    continue;
                }
                for p in ev.paths {
                    if let Some(r) = best_root(&roots, &p) {
                        // Only audio files or directories matter; a vanished path may have been either.
                        if p.is_dir() || crate::media::is_audio_path(&p) || !p.exists() {
                            pending.entry(r.id).or_default().insert(p);
                            let now = Instant::now();
                            first.get_or_insert(now);
                            last = Some(now);
                        }
                    }
                }
            }
            Ok(Err(err)) => tracing::warn!(error = %err, "watcher error"),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
        if let (Some(f), Some(l)) = (first, last)
            && (l.elapsed() >= cfg.debounce || f.elapsed() >= cfg.max_wait)
        {
            for (root_id, paths) in pending.drain() {
                let paths: Vec<PathBuf> = paths.into_iter().collect();
                match crate::scanner::scan_paths(&ctx, root_id, &paths) {
                    Ok(r) => tracing::info!(root_id, added = r.files_added, updated = r.files_updated, missing = r.files_missing, "watch ingest"),
                    Err(err) => tracing::warn!(root_id, error = %err, "watch ingest failed"),
                }
            }
            first = None;
            last = None;
        }
    }
}
