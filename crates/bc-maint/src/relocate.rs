//! Move a library root to another location, including another drive (port of
//! `services/library/relocate.py`).
//!
//! Three things make this more than a `mv`:
//!
//! * **The database stores absolute paths.** Every `files.path` under the root has to follow.
//! * **Across drives there is no atomic rename.** `rename` fails with `EXDEV`, so it becomes
//!   copy - fsync - verify size - rename into place - delete, one file at a time; a crash
//!   mid-copy leaves a `.part` file and the original intact.
//! * **It can be interrupted.** Each file is moved and its row updated before the next file is
//!   touched. If the process dies, rows that moved point at the new drive and the rest still point
//!   at the old one -- both are true, nothing is lost, and re-running finishes the job.

use std::path::{Path, PathBuf};

use bc_db::rusqlite::OptionalExtension;
use bc_libcore::{ApiError, Ctx};

use crate::util::canon;

/// The move cannot safely proceed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RelocateError(pub String);

impl From<bc_libcore::ApiError> for RelocateError {
    fn from(e: bc_libcore::ApiError) -> Self {
        RelocateError(e.to_string())
    }
}
impl From<RelocateError> for ApiError {
    fn from(e: RelocateError) -> Self {
        ApiError::bad(e.0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelocatePlan {
    pub root_id: i64,
    pub source: String,
    pub target: String,
    pub file_count: i64,
    pub total_bytes: i64,
    pub free_bytes: i64,
    pub same_filesystem: bool,
    pub target_exists: bool,
    pub target_empty: bool,
    pub warnings: Vec<String>,
}

impl RelocatePlan {
    /// Same-filesystem moves are renames and consume no extra space; otherwise 5% head-room (a
    /// full destination drive mid-move is the worst outcome).
    pub fn fits(&self) -> bool {
        self.same_filesystem || (self.free_bytes as f64) > (self.total_bytes as f64) * 1.05
    }
    pub fn ok(&self) -> bool {
        self.fits() && !self.warnings.iter().any(|w| w.starts_with("BLOCK:"))
    }
    pub fn out(&self) -> bc_types::library::MovePlanOut {
        bc_types::library::MovePlanOut {
            source: self.source.clone(),
            target: self.target.clone(),
            file_count: self.file_count,
            total_bytes: self.total_bytes,
            free_bytes: self.free_bytes,
            same_filesystem: self.same_filesystem,
            target_exists: self.target_exists,
            target_empty: self.target_empty,
            fits: self.fits(),
            ok: self.ok(),
            warnings: self.warnings.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RelocateProgress {
    pub moved: i64,
    pub total: i64,
    pub bytes_moved: i64,
    pub total_bytes: i64,
    pub current: String,
    pub errors: Vec<String>,
    pub done: bool,
}

fn expanduser(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| (p == "~").then_some(""))
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

fn free_space(p: &Path) -> Option<i64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` is a properly sized, zeroed out-parameter.
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        Some((st.f_bavail as u128 * st.f_frsize as u128).min(i64::MAX as u128) as i64)
    }
}

fn writable(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes())
        // SAFETY: valid NUL-terminated path.
        .map(|c| unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0)
        .unwrap_or(false)
}

/// Validate a move before touching anything.
pub fn plan(ctx: &Ctx, root_id: i64, target_path: &str) -> Result<RelocatePlan, RelocateError> {
    let source_s: String = ctx
        .read(|c| Ok(c.query_row("SELECT path FROM library_roots WHERE id = ?1", [root_id], |r| r.get::<_, String>(0)).optional()?))?
        .ok_or_else(|| RelocateError(format!("library root {root_id} not found")))?;
    let (file_count, total_bytes): (i64, i64) = ctx.read(|c| {
        Ok(c.query_row("SELECT COUNT(*), COALESCE(SUM(size_bytes),0) FROM files WHERE root_id = ?1", [root_id], |r| Ok((r.get(0)?, r.get(1)?)))?)
    })?;

    let source = PathBuf::from(&source_s);
    let target = expanduser(target_path);
    let mut warnings = Vec::new();
    if !source.is_dir() {
        warnings.push(format!("BLOCK: the current location {} is not reachable \u{2014} is the drive mounted?", source.display()));
    }
    let (rt, rs) = (canon(&target), canon(&source));
    if rt == rs {
        warnings.push("BLOCK: the target is the same as the current location.".into());
    } else if rt.starts_with(&rs) {
        // Moving a folder inside itself would recurse forever.
        warnings.push("BLOCK: the target is inside the folder being moved.".into());
    }
    // The target need not exist yet, but its parent must, so we can create it.
    let probe: PathBuf = if target.exists() { target.clone() } else { target.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(".")) };
    if !probe.exists() {
        warnings.push(format!(
            "BLOCK: {} does not exist \u{2014} create it first.",
            target.parent().map(|p| p.display().to_string()).unwrap_or_default()
        ));
    } else if !writable(&probe) {
        warnings.push(format!("BLOCK: no write permission for {}.", probe.display()));
    }
    let target_exists = target.exists();
    let mut target_empty = true;
    if target_exists && target.is_dir() {
        target_empty = std::fs::read_dir(&target).map(|mut d| d.next().is_none()).unwrap_or(true);
        if !target_empty {
            warnings.push("The target folder is not empty. Existing files will be kept; anything with the same name will be overwritten.".into());
        }
    }
    let free_bytes = match if probe.exists() { free_space(&probe) } else { None } {
        Some(f) => f,
        None => {
            warnings.push("Could not read free space on the target drive.".into());
            0
        }
    };
    let same_filesystem = {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(&source), std::fs::metadata(&probe)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev(),
            _ => false,
        }
    };
    let mut p = RelocatePlan {
        root_id,
        source: source.display().to_string(),
        target: target.display().to_string(),
        file_count,
        total_bytes,
        free_bytes,
        same_filesystem,
        target_exists,
        target_empty,
        warnings,
    };
    if !p.fits() {
        p.warnings.push(format!(
            "BLOCK: not enough space \u{2014} need about {:.1} GB, {:.1} GB free.",
            total_bytes as f64 / 1e9,
            free_bytes as f64 / 1e9
        ));
    }
    Ok(p)
}

/// Move one file, returning its size. Never deletes before the copy lands.
pub fn move_file(source: &Path, target: &Path, same_fs: bool) -> std::io::Result<u64> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let meta = std::fs::metadata(source)?;
    let size = meta.len();
    if same_fs {
        match std::fs::rename(source, target) {
            Ok(()) => return Ok(size),
            // A bind mount can share a device number and still refuse a rename: copy instead.
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {}
            Err(e) => return Err(e),
        }
    }
    // Cross-device: copy to a temp name, fsync, rename into place, then remove the original. A
    // crash mid-copy leaves a .part file and the original intact -- never a truncated file where
    // the real one used to be.
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let staging = target.with_file_name(format!(".{name}.part"));
    let attempt = || -> std::io::Result<()> {
        std::fs::copy(source, &staging)?;
        let f = std::fs::OpenOptions::new().write(true).open(&staging)?;
        if let Ok(m) = meta.modified() {
            let _ = f.set_modified(m); // keep mtime like copy2, so a rescan sees the file unchanged
        }
        f.sync_all()?;
        drop(f);
        if std::fs::metadata(&staging)?.len() != size {
            return Err(std::io::Error::other(format!("size mismatch after copying {name}")));
        }
        std::fs::rename(&staging, target)
    };
    if let Err(e) = attempt() {
        let _ = std::fs::remove_file(&staging);
        return Err(e);
    }
    match std::fs::remove_file(source) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    Ok(size)
}

/// Move the root, updating the database as each file lands. `on_progress` is called after every
/// file (and once more when done); returning `false` stops the move there (an interruption: the
/// database already agrees with the disk for every file processed so far). Runs blocking -- call
/// it from `spawn_blocking`.
pub fn execute(
    ctx: &Ctx,
    root_id: i64,
    target_path: &str,
    mut on_progress: impl FnMut(&RelocateProgress) -> bool,
) -> Result<RelocateProgress, RelocateError> {
    let checked = plan(ctx, root_id, target_path)?;
    if !checked.ok() {
        let msg = checked.warnings.iter().filter(|w| w.starts_with("BLOCK:")).map(|w| w.trim_start_matches("BLOCK: ").to_string()).collect::<Vec<_>>().join("; ");
        return Err(RelocateError(msg));
    }
    let source = PathBuf::from(&checked.source);
    let target = expanduser(target_path);
    std::fs::create_dir_all(&target).map_err(|e| RelocateError(format!("cannot create {}: {e}", target.display())))?;

    let files: Vec<(i64, String, String)> = ctx.read(|c| {
        let mut st = c.prepare("SELECT id, path, rel_path FROM files WHERE root_id = ?1 ORDER BY id")?;
        Ok(st.query_map([root_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?)
    })?;
    let mut progress = RelocateProgress { total: files.len() as i64, total_bytes: checked.total_bytes, ..Default::default() };

    for (id, old_s, rel_path) in files {
        let old = PathBuf::from(&old_s);
        let relative = if !rel_path.is_empty() {
            rel_path
        } else if let Ok(r) = old.strip_prefix(&source) {
            r.to_string_lossy().into_owned()
        } else {
            old.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        };
        let new = target.join(&relative);
        progress.current = relative.clone();
        let step = (|| -> Result<(), String> {
            if old.exists() {
                progress.bytes_moved += move_file(&old, &new, checked.same_filesystem).map_err(|e| e.to_string())? as i64;
            } else if !new.exists() {
                // Neither location has it: it was already missing.
                progress.errors.push(format!("{relative}: missing on disk"));
            }
            // Update the row immediately, so an interruption leaves the database agreeing with the
            // filesystem for every file processed so far.
            let (np, rel) = (new.to_string_lossy().into_owned(), relative.clone());
            ctx.write(move |t| {
                t.execute("UPDATE files SET path = ?1, rel_path = ?2 WHERE id = ?3", (np, rel, id))?;
                Ok(())
            })
            .map_err(|e| e.to_string())
        })();
        if let Err(e) = step {
            tracing::warn!(file = %relative, error = %e, "relocate failed");
            progress.errors.push(format!("{relative}: {e}"));
        }
        progress.moved += 1;
        if !on_progress(&progress) {
            return Ok(progress);
        }
    }

    // Repoint the root and any release folder paths that referenced it.
    let (src_s, tgt_s) = (source.to_string_lossy().into_owned(), target.to_string_lossy().into_owned());
    ctx.write(move |t| {
        t.execute("UPDATE library_roots SET path = ?1 WHERE id = ?2", (&tgt_s, root_id))?;
        let n = src_s.len() as i64;
        t.execute(
            "UPDATE releases SET folder_path = ?2 || substr(folder_path, ?3 + 1)
              WHERE folder_path = ?1 OR substr(folder_path, 1, ?3 + 1) = ?1 || '/'",
            (&src_s, &tgt_s, n),
        )?;
        Ok(())
    })?;
    cleanup_empty_dirs(&source);
    progress.done = true;
    on_progress(&progress);
    Ok(progress)
}

/// Remove directories left behind, deepest first. Never removes the root itself, and never
/// anything still holding a file.
pub fn cleanup_empty_dirs(root: &Path) {
    if !root.is_dir() {
        return;
    }
    for entry in walkdir::WalkDir::new(root).contents_first(true).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_dir() && entry.path() != root {
            let _ = std::fs::remove_dir(entry.path()); // only succeeds when empty
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    /// A library root with three "tracks" under `Somatic/Grid Failure/` (the legacy test scans real
    /// mp3s; the move itself only needs the rows and bytes).
    struct Lib {
        env: TestEnv,
        tmp: tempfile::TempDir,
        old: PathBuf,
        root_id: i64,
    }

    fn lib() -> Lib {
        let env = test_env();
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old-drive").join("music");
        let folder = old.join("Somatic").join("Grid Failure");
        std::fs::create_dir_all(&folder).unwrap();
        let root_id = seed_root(&env.db, old.to_str().unwrap(), "library");
        let rid = seed_release(&env.db, "Somatic", "Grid Failure", None, None);
        exec(&env.db, &format!("UPDATE releases SET folder_path='{}' WHERE id={rid}", folder.display()));
        for n in 1..=3 {
            let rel = format!("Somatic/Grid Failure/0{n} - track.mp3");
            let p = old.join(&rel);
            std::fs::write(&p, vec![n as u8; 1000 * n]).unwrap();
            let t = seed_track(&env.db, rid, &format!("t{n}"), Some(n as i64));
            seed_file(&env.db, t, root_id, p.to_str().unwrap(), &rel, 1000 * n as i64);
        }
        Lib { env, tmp, old, root_id }
    }

    fn mp3s(dir: &Path) -> usize {
        walkdir::WalkDir::new(dir).into_iter().filter_map(Result::ok).filter(|e| e.path().extension().is_some_and(|x| x == "mp3")).count()
    }

    fn run(l: &Lib, target: &Path) -> Result<RelocateProgress, RelocateError> {
        execute(&l.env, l.root_id, target.to_str().unwrap(), |_| true)
    }

    fn new_target(l: &Lib) -> PathBuf {
        let parent = l.tmp.path().join("new-drive");
        std::fs::create_dir_all(&parent).unwrap();
        parent.join("music")
    }

    #[test]
    fn plan_reports_size_and_space() {
        let l = lib();
        let target = new_target(&l);
        let p = plan(&l.env, l.root_id, target.to_str().unwrap()).unwrap();
        assert_eq!(p.file_count, 3);
        assert!(p.total_bytes > 0);
        assert!(p.ok() && p.fits());
    }

    #[test]
    fn plan_refuses_moving_into_itself() {
        let l = lib();
        let p = plan(&l.env, l.root_id, l.old.join("nested").to_str().unwrap()).unwrap();
        assert!(!p.ok());
        assert!(p.warnings.iter().any(|w| w.contains("inside the folder")));
    }

    #[test]
    fn plan_refuses_the_same_location() {
        let l = lib();
        assert!(!plan(&l.env, l.root_id, l.old.to_str().unwrap()).unwrap().ok());
    }

    #[test]
    fn plan_refuses_a_missing_parent() {
        let l = lib();
        let p = plan(&l.env, l.root_id, l.tmp.path().join("no/such/place").to_str().unwrap()).unwrap();
        assert!(!p.ok());
        assert!(p.warnings.iter().any(|w| w.contains("does not exist")));
    }

    #[test]
    fn plan_warns_when_the_target_is_not_empty() {
        let l = lib();
        let target = l.tmp.path().join("occupied");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("something.txt"), "already here").unwrap();
        let p = plan(&l.env, l.root_id, target.to_str().unwrap()).unwrap();
        assert!(!p.target_empty);
        assert!(p.warnings.iter().any(|w| w.contains("not empty")));
        assert!(p.ok(), "a non-empty target is a warning, not a blocker");
    }

    #[test]
    fn move_relocates_files_and_repoints_the_database() {
        let l = lib();
        let target = new_target(&l);
        let r = run(&l, &target).unwrap();
        assert_eq!(r.moved, 3);
        assert!(r.errors.is_empty() && r.done);
        assert_eq!(mp3s(&target), 3);
        assert_eq!(mp3s(&l.old), 0);
        assert!(target.join("Somatic/Grid Failure").is_dir(), "the relative structure is preserved");
        assert_eq!(q_str(&l.env.db, &format!("SELECT path FROM library_roots WHERE id={}", l.root_id)).unwrap(), target.to_str().unwrap());
        // Every file row follows, and so does the release folder.
        assert_eq!(q_i64(&l.env.db, &format!("SELECT COUNT(*) FROM files WHERE path LIKE '{}/%'", target.display())), 3);
        assert_eq!(q_str(&l.env.db, "SELECT folder_path FROM releases").unwrap(), target.join("Somatic/Grid Failure").to_str().unwrap());
    }

    #[test]
    fn tracks_remain_reachable_after_a_move() {
        // The point of the whole operation: stored paths must follow the files.
        let l = lib();
        let target = new_target(&l);
        run(&l, &target).unwrap();
        for p in q_paths(&l.env.db) {
            assert!(Path::new(&p).is_file(), "{p}");
        }
    }

    fn q_paths(db: &bc_db::Db) -> Vec<String> {
        db.read(|c| {
            let mut st = c.prepare("SELECT path FROM files")?;
            Ok(st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap()
    }

    #[test]
    fn library_contents_are_unchanged_by_a_move() {
        let l = lib();
        let before = (q_i64(&l.env.db, "SELECT COUNT(*) FROM tracks"), q_i64(&l.env.db, "SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL"));
        run(&l, &new_target(&l)).unwrap();
        let after = (q_i64(&l.env.db, "SELECT COUNT(*) FROM tracks"), q_i64(&l.env.db, "SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL"));
        assert_eq!(before, after, "nothing should look missing after a move");
    }

    #[test]
    fn a_rescan_after_moving_finds_no_changes() {
        // The scanner compares (size, mtime_ns); a cross-device copy must keep mtime and the
        // rows keep their size, so nothing reads as changed.
        let l = lib();
        let src = l.old.join("Somatic/Grid Failure/01 - track.mp3");
        let before = std::fs::metadata(&src).unwrap().modified().unwrap();
        let target = new_target(&l);
        run(&l, &target).unwrap();
        let after = std::fs::metadata(target.join("Somatic/Grid Failure/01 - track.mp3")).unwrap().modified().unwrap();
        assert_eq!(before, after);
        assert_eq!(q_i64(&l.env.db, "SELECT size_bytes FROM files WHERE rel_path LIKE '%01 - track.mp3'"), 1000);
    }

    #[test]
    fn move_creates_the_target_if_absent() {
        let l = lib();
        let target = l.tmp.path().join("new-drive");
        std::fs::create_dir_all(&target).unwrap();
        let target = target.join("fresh");
        assert!(!target.exists());
        assert_eq!(run(&l, &target).unwrap().moved, 3);
        assert!(target.is_dir());
    }

    #[test]
    fn empty_directories_are_cleaned_up() {
        let l = lib();
        run(&l, &new_target(&l)).unwrap();
        assert!(!l.old.join("Somatic/Grid Failure").exists());
    }

    #[test]
    fn a_blocked_move_changes_nothing() {
        let l = lib();
        let err = run(&l, &l.tmp.path().join("nope/deeper")).unwrap_err();
        assert!(err.0.contains("does not exist"));
        assert_eq!(q_str(&l.env.db, &format!("SELECT path FROM library_roots WHERE id={}", l.root_id)).unwrap(), l.old.to_str().unwrap());
        assert_eq!(mp3s(&l.old), 3);
    }

    #[test]
    fn partial_move_leaves_a_consistent_database() {
        // Stopping halfway must leave every processed row agreeing with the disk.
        let l = lib();
        let target = new_target(&l);
        let stopped = execute(&l.env, l.root_id, target.to_str().unwrap(), |_| false).unwrap(); // exactly one file
        assert_eq!(stopped.moved, 1);
        assert!(!stopped.done);
        for p in q_paths(&l.env.db) {
            assert!(Path::new(&p).exists(), "{p} is recorded but not on disk -- the database and filesystem disagree after an interrupted move");
        }
    }

    #[test]
    fn resuming_an_interrupted_move_completes_it() {
        let l = lib();
        let target = new_target(&l);
        execute(&l.env, l.root_id, target.to_str().unwrap(), |_| false).unwrap();
        // Re-run: the already-moved file is simply skipped.
        let r = run(&l, &target).unwrap();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(mp3s(&target), 3);
        assert_eq!(q_str(&l.env.db, &format!("SELECT path FROM library_roots WHERE id={}", l.root_id)).unwrap(), target.to_str().unwrap());
    }

    #[test]
    fn cross_device_copy_verifies_size_before_deleting() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("src.mp3");
        let mut data = vec![0xff, 0xfb];
        data.extend(vec![0u8; 4096]);
        std::fs::write(&source, &data).unwrap();
        let target = tmp.path().join("out").join("dest.mp3");
        let size = move_file(&source, &target, false).unwrap();
        assert_eq!(size, 4098);
        assert_eq!(std::fs::metadata(&target).unwrap().len(), 4098);
        assert!(!source.exists());
        // No staging file left behind.
        let leftovers = std::fs::read_dir(target.parent().unwrap()).unwrap().filter_map(Result::ok).filter(|e| e.file_name().to_string_lossy().ends_with(".part")).count();
        assert_eq!(leftovers, 0);
    }
}
