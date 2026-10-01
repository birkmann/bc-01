//! The three-stage incremental scanner (PLAN §3.4).
//!
//! 1. **Walk + stat** a root in parallel (rayon over directories; `DirEntry::metadata` is one
//!    `lstat`, free on a warm cache).
//! 2. Compare against `files` loaded in ONE query: only new/changed files are read.
//! 3. **Read** tags + cover with rayon outside any transaction, then **write** in batches of
//!    ~200 rows (see [`crate::ingest::run_pipeline`]).
//!
//! Unchanged files are not written at all: not even `last_seen_at`. Files that vanished are
//! marked `missing_since` (never deleted) only when the walk was complete and the root itself
//! is reachable.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use bc_db::rusqlite::params;
use bc_db::util::now_db;
use bc_libcore::{ApiError, ApiResult, Ctx};
use bc_types::library::ScanResult;
use rayon::prelude::*;

use crate::ingest::{IngestOptions, RootInfo, WorkItem, load_roots, run_pipeline};

/// A directory that could not be read during a walk.
#[derive(Debug, Clone)]
pub struct WalkFailure {
    pub dir: PathBuf,
    pub error: String,
}

fn walk_dir(dir: &Path, fails: &parking_lot::Mutex<Vec<WalkFailure>>) -> Vec<WorkItem> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "cannot read directory");
            fails.lock().push(WalkFailure { dir: dir.to_path_buf(), error: e.to_string() });
            return Vec::new();
        }
    };
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    for entry in rd {
        let Ok(entry) = entry else {
            fails.lock().push(WalkFailure { dir: dir.to_path_buf(), error: "directory entry error".into() });
            continue;
        };
        let name = entry.file_name();
        // Hidden entries are skipped (legacy rule).
        if name.as_encoded_bytes().first() == Some(&b'.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            subdirs.push(entry.path());
        } else if ft.is_file() {
            let path = entry.path();
            if !crate::media::is_audio_path(&path) {
                continue;
            }
            let Ok(m) = entry.metadata() else { continue };
            files.push(WorkItem {
                path,
                size: m.len() as i64,
                mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
                inode: Some(m.ino() as i64),
            });
        }
    }
    let nested: Vec<Vec<WorkItem>> = subdirs.par_iter().map(|d| walk_dir(d, fails)).collect();
    for n in nested {
        files.extend(n);
    }
    files
}

/// Stage a: every audio file under `root` (hidden entries skipped, symlinks not followed),
/// plus the directories that could not be read.
pub fn walk_audio(root: &Path) -> (Vec<WorkItem>, Vec<WalkFailure>) {
    let fails = parking_lot::Mutex::new(Vec::new());
    let items = walk_dir(root, &fails);
    (items, fails.into_inner())
}

#[derive(Debug, Clone)]
struct Known {
    id: i64,
    size: i64,
    mtime_ns: i64,
    inode: Option<i64>,
    missing: bool,
}

/// Hooks for a scan run: progress callback and cooperative cancel.
pub struct ScanHooks<'a> {
    /// `(phase, seen, total)`; called on the scanning thread, rate limiting is the caller's job.
    pub progress: Option<&'a mut dyn FnMut(&'static str, i64, i64)>,
    pub cancel: &'a AtomicBool,
}

impl ScanHooks<'static> {
    pub fn none() -> ScanHooks<'static> {
        static NEVER: AtomicBool = AtomicBool::new(false);
        ScanHooks { progress: None, cancel: &NEVER }
    }
}

fn unchanged(k: &Known, w: &WorkItem) -> bool {
    k.size == w.size && k.mtime_ns == w.mtime_ns && !k.missing && (k.inode.is_none() || k.inode == w.inode)
}

/// Scan one root. Never deletes; see the module docs. Blocking.
pub fn scan_root(ctx: &Ctx, root_id: i64, hooks: ScanHooks<'_>) -> ApiResult<ScanResult> {
    let started = Instant::now();
    let root = ctx
        .read(|c| Ok(load_roots(c)?.into_iter().find(|r| r.id == root_id)))?
        .ok_or_else(|| ApiError::not_found(format!("root {root_id} not found")))?;
    let mut result = ScanResult { root_id, root_path: root.path.to_string_lossy().into_owned(), ..Default::default() };
    let ScanHooks { mut progress, cancel } = hooks;
    let mut tick = |ph: &'static str, a: i64, b: i64| {
        if let Some(p) = progress.as_mut() {
            p(ph, a, b);
        }
    };

    if !root.path.is_dir() {
        result.errors.push(format!("root does not exist: {}", root.path.display()));
        result.duration_ms = started.elapsed().as_millis() as i64;
        return Ok(result);
    }
    // The root itself must be readable (an unmounted drive leaves an empty mountpoint, which
    // is caught by the zero-files guard below).
    tick("walk", 0, 0);
    let (items, failures) = walk_audio(&root.path);
    result.files_seen = items.len() as i64;
    for f in &failures {
        result.errors.push(format!("cannot read {}: {}", f.dir.display(), f.error));
    }
    let root_unreadable = failures.iter().any(|f| f.dir == root.path);

    let rid = root.id;
    let known: HashMap<String, Known> = ctx.read(|c| {
        let mut st = c.prepare(
            "SELECT path, id, size_bytes, mtime_ns, inode, missing_since IS NOT NULL FROM files WHERE root_id = ?1",
        )?;
        let rows = st.query_map([rid], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Known { id: r.get(1)?, size: r.get(2)?, mtime_ns: r.get(3)?, inode: r.get(4)?, missing: r.get(5)? },
            ))
        })?;
        let mut m = HashMap::with_capacity(200_000);
        for row in rows {
            let (p, k) = row?;
            m.insert(p, k);
        }
        Ok(m)
    })?;

    let mut work: Vec<WorkItem> = Vec::new();
    let mut seen_known: HashSet<i64> = HashSet::with_capacity(known.len());
    let mut reappeared_only = 0usize;
    for it in items {
        let Some(path) = it.path.to_str() else {
            result.errors.push(format!("non-UTF-8 path skipped: {}", it.path.display()));
            continue;
        };
        match known.get(path) {
            Some(k) => {
                seen_known.insert(k.id);
                if unchanged(k, &it) {
                    result.files_unchanged += 1;
                } else {
                    result.files_updated += 1;
                    if k.missing && k.size == it.size && k.mtime_ns == it.mtime_ns {
                        reappeared_only += 1;
                    }
                    work.push(it);
                }
            }
            None => {
                result.files_added += 1;
                work.push(it);
            }
        }
    }
    let _ = reappeared_only;

    let opts = IngestOptions::default();
    let out = run_pipeline(ctx, &root, work, &opts, cancel, &mut tick)?;
    result.tracks_added = out.tally.tracks_added;
    result.errors.extend(out.tally.errors.iter().cloned());
    let cancelled = out.cancelled;

    // Mark vanished files. Never delete: an unmounted drive must not destroy playlists,
    // ratings or play history.
    let active_known = known.values().filter(|k| !k.missing).count();
    let vanished: Vec<i64> = known
        .iter()
        .filter(|(p, k)| !k.missing && !seen_known.contains(&k.id) && !under_failed(p, &failures))
        .map(|(_, k)| k.id)
        .collect();
    if cancelled {
        // An aborted scan proves nothing about what is gone.
    } else if root_unreadable {
        result.errors.push(format!("root unreadable, nothing marked missing: {}", root.path.display()));
    } else if result.files_seen == 0 && active_known > 0 {
        result.errors.push(format!(
            "walk found no audio files but {active_known} are known; is the drive mounted? nothing marked missing"
        ));
    } else if !vanished.is_empty() {
        let now = now_db();
        let n = vanished.len() as i64;
        ctx.db
            .write_chunks(vanished, crate::ingest::COMMIT_BATCH, move |tx, ids| {
                let ph = vec!["?"; ids.len()].join(",");
                let mut args: Vec<&dyn bc_db::rusqlite::ToSql> = vec![&now];
                args.extend(ids.iter().map(|i| i as &dyn bc_db::rusqlite::ToSql));
                tx.execute(&format!("UPDATE files SET missing_since = ?1 WHERE missing_since IS NULL AND id IN ({ph})"), args.as_slice())?;
                Ok(())
            })
            .map_err(ApiError::from)?;
        result.files_missing = n;
    }

    let ms = started.elapsed().as_millis() as i64;
    let now = now_db();
    ctx.db
        .write_with::<_, ApiError>(move |tx| {
            tx.execute("UPDATE library_roots SET last_scan_at = ?1, last_scan_ms = ?2 WHERE id = ?3", params![now, ms, rid])?;
            Ok(())
        })?;
    result.duration_ms = started.elapsed().as_millis() as i64;
    tick("done", result.files_seen, result.files_seen);

    if !out.tally.track_ids.is_empty() {
        ctx.bus.invalidate("track", out.tally.track_ids.clone());
        ctx.bus.invalidate("release", out.tally.releases_touched.clone());
    }
    Ok(result)
}

fn under_failed(path: &str, failures: &[WalkFailure]) -> bool {
    failures.iter().any(|f| Path::new(path).starts_with(&f.dir))
}

/// Re-check explicit paths (the watcher's coalesced events): existing files are stat-compared
/// and ingested when new/changed; paths that no longer exist are marked missing (a vanished
/// directory marks everything beneath it). Directories are walked.
pub fn scan_paths(ctx: &Ctx, root_id: i64, changed: &[PathBuf]) -> ApiResult<ScanResult> {
    let started = Instant::now();
    let root: RootInfo = ctx
        .read(|c| Ok(load_roots(c)?.into_iter().find(|r| r.id == root_id)))?
        .ok_or_else(|| ApiError::not_found(format!("root {root_id} not found")))?;
    let mut result = ScanResult { root_id, root_path: root.path.to_string_lossy().into_owned(), ..Default::default() };

    let mut candidates: Vec<WorkItem> = Vec::new();
    let mut gone: Vec<PathBuf> = Vec::new();
    for p in changed {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => {
                let (items, _) = walk_audio(p);
                candidates.extend(items);
            }
            Ok(m) if m.is_file() => {
                if crate::media::is_audio_path(p) && !hidden(p, &root.path)
                    && let Ok(it) = crate::ingest::stat_item(p)
                {
                    candidates.push(it);
                }
            }
            Ok(_) => {}
            Err(_) => gone.push(p.clone()),
        }
    }
    let mut seen = HashSet::new();
    candidates.retain(|c| seen.insert(c.path.clone()));
    result.files_seen = candidates.len() as i64;

    // Stat-compare the candidates against the DB.
    let paths: Vec<String> = candidates.iter().filter_map(|c| c.path.to_str().map(String::from)).collect();
    let known: HashMap<String, Known> = ctx.read(|c| {
        let mut m = HashMap::new();
        for chunk in paths.chunks(500) {
            let ph = vec!["?"; chunk.len()].join(",");
            let mut st = c.prepare(&format!(
                "SELECT path, id, size_bytes, mtime_ns, inode, missing_since IS NOT NULL FROM files WHERE path IN ({ph})"
            ))?;
            let rows = st.query_map(bc_db::rusqlite::params_from_iter(chunk.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Known { id: r.get(1)?, size: r.get(2)?, mtime_ns: r.get(3)?, inode: r.get(4)?, missing: r.get(5)? },
                ))
            })?;
            for row in rows {
                let (p, k) = row?;
                m.insert(p, k);
            }
        }
        Ok(m)
    })?;
    let mut work = Vec::new();
    for it in candidates {
        match it.path.to_str().and_then(|p| known.get(p)) {
            Some(k) if unchanged(k, &it) => result.files_unchanged += 1,
            Some(_) => {
                result.files_updated += 1;
                work.push(it);
            }
            None => {
                result.files_added += 1;
                work.push(it);
            }
        }
    }
    let cancel = AtomicBool::new(false);
    let mut tick = |_: &'static str, _: i64, _: i64| {};
    let out = run_pipeline(ctx, &root, work, &IngestOptions::default(), &cancel, &mut tick)?;
    result.tracks_added = out.tally.tracks_added;
    result.errors.extend(out.tally.errors.iter().cloned());

    if !gone.is_empty() {
        let now = now_db();
        let gone_s: Vec<String> = gone.iter().filter_map(|g| g.to_str().map(String::from)).collect();
        let n = ctx.db.write_with::<i64, ApiError>(move |tx| {
            let mut n = 0;
            for g in &gone_s {
                let prefix_len = g.chars().count() as i64 + 1;
                let like_prefix = format!("{g}/");
                n += tx.execute(
                    "UPDATE files SET missing_since = ?1 WHERE missing_since IS NULL AND (path = ?2 OR substr(path, 1, ?3) = ?4)",
                    params![now, g, prefix_len, like_prefix],
                )?;
            }
            Ok(n as i64)
        })?;
        result.files_missing = n;
    }
    result.duration_ms = started.elapsed().as_millis() as i64;
    if out.tally.track_ids.len() + result.files_missing as usize > 0 {
        ctx.bus.invalidate("track", out.tally.track_ids.clone());
        ctx.bus.invalidate("release", out.tally.releases_touched.clone());
        let changed = bc_types::library::LibraryChanged {
            added_tracks: if out.tally.tracks_added > 0 { out.tally.track_ids.clone() } else { vec![] },
            changed_tracks: out.tally.track_ids.clone(),
            ..Default::default()
        };
        ctx.bus.publish(bc_types::library::TOPIC_LIBRARY_CHANGED, &changed);
    }
    Ok(result)
}

fn hidden(p: &Path, root: &Path) -> bool {
    p.strip_prefix(root)
        .unwrap_or(p)
        .components()
        .any(|c| c.as_os_str().as_encoded_bytes().first() == Some(&b'.'))
}
