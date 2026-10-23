//! Move a library root to another location, including another drive (port of
//! `services/library/relocate.py`).
//!
//! Four things make this more than a `mv`:
//!
//! * **The database stores absolute paths.** Every `files.path` under the root has to follow.
//! * **Across drives there is no atomic rename.** `rename` fails with `EXDEV`, so it becomes
//!   copy - fsync - verify size - rename into place - delete, one file at a time; a crash
//!   mid-copy leaves a `.part` file and the original intact.
//! * **It can be interrupted.** Each file is moved and its row updated before the next file is
//!   touched. If the process dies, rows that moved point at the new drive and the rest still point
//!   at the old one -- both are true, nothing is lost, and re-running finishes the job. Every step
//!   is idempotent: a row already at its destination is not touched again, and a file is never
//!   moved onto itself (a cross-drive "move" of a file onto itself copies it over itself and then
//!   deletes it).
//! * **The target can be another root.** Moving a downloads folder into the library folder (or a
//!   folder inside it) *merges*: the files join that root and this root keeps its path, so new
//!   downloads still have somewhere to land. A file is never written over another one: an
//!   identical copy already at the destination counts as moved, and a file another row already
//!   has there is a duplicate (the same record downloaded twice): its track is folded into that
//!   row's, user data and all, and the source copy goes, so the source ends up clean. Anything
//!   else (an untracked, different file in the way) is a conflict and stays where it is.
//!
//! The caller holds both roots in [`bc_libcore::root_lock`] for the whole move, so no scan, watcher
//! ingest or second move works on them meanwhile.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use bc_db::rusqlite::OptionalExtension;
use bc_db::util::now_db;
use bc_libcore::{ApiError, Ctx};
use bc_types::library::MoveOutcome;

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

/// The other root a move merges into.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeInto {
    pub root_id: i64,
    /// As stored in `library_roots.path`.
    pub root_path: PathBuf,
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
    pub merge_into: Option<MergeInto>,
    /// Rows whose destination path another row already has: duplicates, folded into that row.
    pub conflicts: i64,
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
    /// The roots the move works on (source, and the merge target).
    pub fn roots(&self) -> Vec<i64> {
        std::iter::once(self.root_id).chain(self.merge_into.as_ref().map(|m| m.root_id)).collect()
    }
    /// The destination of a file at `relative` (to the source root): its path and the
    /// `(root_id, rel_path)` its row gets.
    fn destination(&self, target: &Path, relative: &str) -> (PathBuf, i64, String) {
        let new = target.join(relative);
        match &self.merge_into {
            Some(m) => {
                let rel = new.strip_prefix(&m.root_path).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| relative.to_string());
                (new, m.root_id, rel)
            }
            None => (new, self.root_id, relative.to_string()),
        }
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
            merge_into: self.merge_into.as_ref().map(|m| m.root_path.display().to_string()),
            conflicts: self.conflicts,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RelocateProgress {
    /// Rows processed (moved, already in place, missing, conflicting or failed).
    pub moved: i64,
    /// Rows to process; grows if files arrive in the source root during the move.
    pub total: i64,
    pub bytes_moved: i64,
    pub total_bytes: i64,
    pub current: String,
    pub errors: Vec<String>,
    /// Left where they are: the destination already holds a different file.
    pub conflicts: Vec<String>,
    /// Rows that were duplicates of a row already at the destination, folded into it.
    pub duplicates: i64,
    pub done: bool,
    /// Set once `done`.
    pub outcome: Option<MoveOutcome>,
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

/// A file's path relative to the root it was recorded under.
fn relative_of(old: &Path, rel_path: String, source: &Path) -> String {
    if !rel_path.is_empty() {
        rel_path
    } else if let Ok(r) = old.strip_prefix(source) {
        r.to_string_lossy().into_owned()
    } else {
        old.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// Validate a move before touching anything.
pub fn plan(ctx: &Ctx, root_id: i64, target_path: &str) -> Result<RelocatePlan, RelocateError> {
    let source_s: String = ctx
        .read(|c| Ok(c.query_row("SELECT path FROM library_roots WHERE id = ?1", [root_id], |r| r.get::<_, String>(0)).optional()?))?
        .ok_or_else(|| RelocateError(format!("library root {root_id} not found")))?;
    let (file_count, total_bytes): (i64, i64) = ctx.read(|c| {
        Ok(c.query_row("SELECT COUNT(*), COALESCE(SUM(size_bytes),0) FROM files WHERE root_id = ?1", [root_id], |r| Ok((r.get(0)?, r.get(1)?)))?)
    })?;
    let others: Vec<(i64, String)> = ctx.read(|c| {
        let mut st = c.prepare("SELECT id, path FROM library_roots WHERE id != ?1 ORDER BY id")?;
        Ok(st.query_map([root_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?)
    })?;

    let source = PathBuf::from(&source_s);
    let mut target = expanduser(target_path);
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
    // The target is another root, or inside one: merge into it. A target that holds another root
    // would nest that root inside this one, and every file there would be indexed twice.
    let mut merge_into = None;
    for (id, path) in &others {
        let ro = canon(Path::new(path));
        if rt.starts_with(&ro) {
            // Spell the destination the way that root is stored, so rel paths come out right.
            let sub = rt.strip_prefix(&ro).unwrap_or(Path::new(""));
            target = if sub.as_os_str().is_empty() { PathBuf::from(path) } else { PathBuf::from(path).join(sub) };
            merge_into = Some(MergeInto { root_id: *id, root_path: PathBuf::from(path) });
            break;
        } else if ro.starts_with(&rt) {
            warnings.push(format!("BLOCK: the target contains another library folder ({path}). Pick that folder itself to merge into it."));
        }
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
    }
    if let Some(m) = &merge_into {
        warnings.push(format!(
            "This merges into the library folder {}: the files join it, and this folder stays where it is{}.",
            m.root_path.display(),
            if file_count > 0 { " (empty, so new downloads still have a home)" } else { "" }
        ));
    } else if !target_empty {
        warnings.push("The target folder is not empty. Nothing there is overwritten: an identical copy counts as moved, a different file with the same name stays where it is.".into());
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
        merge_into,
        conflicts: 0,
        warnings,
    };
    if target_exists {
        p.conflicts = count_conflicts(ctx, &p, &source, &target)?;
        if p.conflicts > 0 {
            p.warnings.push(format!(
                "{} file(s) are already in the library under the same name. The library's copy stays, taking over plays, loves and playlist entries, and the duplicate is removed from this folder.",
                p.conflicts
            ));
        }
    }
    if !p.fits() {
        p.warnings.push(format!(
            "BLOCK: not enough space \u{2014} need about {:.1} GB, {:.1} GB free.",
            total_bytes as f64 / 1e9,
            free_bytes as f64 / 1e9
        ));
    }
    Ok(p)
}

/// Rows of the source root whose destination path another row already has.
fn count_conflicts(ctx: &Ctx, p: &RelocatePlan, source: &Path, target: &Path) -> Result<i64, RelocateError> {
    let root_id = p.root_id;
    let rows: Vec<(i64, String, String)> = ctx.read(move |c| {
        let mut st = c.prepare("SELECT id, path, rel_path FROM files WHERE root_id = ?1")?;
        Ok(st.query_map([root_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?)
    })?;
    let dests: Vec<(i64, String)> = rows
        .into_iter()
        .map(|(id, old, rel)| {
            let relative = relative_of(Path::new(&old), rel, source);
            (id, p.destination(target, &relative).0.to_string_lossy().into_owned())
        })
        .collect();
    Ok(ctx.read(move |c| {
        let mut n = 0i64;
        for chunk in dests.chunks(400) {
            let ph = vec!["(?, ?)"; chunk.len()].join(",");
            let args: Vec<&dyn bc_db::rusqlite::ToSql> = chunk.iter().flat_map(|(id, d)| [id as &dyn bc_db::rusqlite::ToSql, d]).collect();
            n += c.query_row(
                &format!("WITH d(id, path) AS (VALUES {ph}) SELECT COUNT(*) FROM d JOIN files f ON f.path = d.path AND f.id != d.id"),
                args.as_slice(),
                |r| r.get::<_, i64>(0),
            )?;
        }
        Ok(n)
    })?)
}

/// Same bytes? (Sizes first; then a streaming compare.)
fn same_content(a: &Path, b: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    if std::fs::metadata(a)?.len() != std::fs::metadata(b)?.len() {
        return Ok(false);
    }
    let (mut fa, mut fb) = (std::io::BufReader::new(std::fs::File::open(a)?), std::io::BufReader::new(std::fs::File::open(b)?));
    let (mut ba, mut bb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        let n = fa.read(&mut ba)?;
        if n == 0 {
            return Ok(true);
        }
        fb.read_exact(&mut bb[..n])?;
        if ba[..n] != bb[..n] {
            return Ok(false);
        }
    }
}

/// Is `a` the very same file as `b` (same path, or a hard link / other spelling of it)?
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if a == b {
        return true;
    }
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Move one file, returning its size. Never deletes before the copy lands, never moves a file
/// onto itself and never writes over an existing file.
pub fn move_file(source: &Path, target: &Path, same_fs: bool) -> std::io::Result<u64> {
    let meta = std::fs::metadata(source)?;
    let size = meta.len();
    if same_file(source, target) {
        // Already there. Copying it "across" would end by deleting the only copy.
        return Ok(size);
    }
    if target.exists() {
        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("{} already exists", target.display())));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
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

/// What became of one row.
enum Step {
    /// At its destination (moved now, earlier, or missing at both ends); `bytes` moved now.
    Placed { bytes: u64 },
    /// A duplicate of the row already at the destination: folded into it, its source copy gone.
    Folded { bytes: u64 },
    /// Destination taken by a different file or by another row: left as it was.
    Conflict(String),
}

/// Move the root, updating the database as each file lands. `on_progress` is called after every
/// file (and once more when done); returning `false` stops the move there (an interruption: the
/// database already agrees with the disk for every file processed so far). Runs blocking -- call
/// it from `spawn_blocking`, holding [`RelocatePlan::roots`] in `root_lock`.
pub fn execute(
    ctx: &Ctx,
    root_id: i64,
    target_path: &str,
    on_progress: impl FnMut(&RelocateProgress) -> bool,
) -> Result<RelocateProgress, RelocateError> {
    execute_with(ctx, root_id, target_path, None, on_progress)
}

/// [`execute`], optionally forcing the copy path (`same_fs = Some(false)`; tests run on one drive).
fn execute_with(
    ctx: &Ctx,
    root_id: i64,
    target_path: &str,
    same_fs: Option<bool>,
    mut on_progress: impl FnMut(&RelocateProgress) -> bool,
) -> Result<RelocateProgress, RelocateError> {
    let checked = plan(ctx, root_id, target_path)?;
    if !checked.ok() {
        let msg = checked.warnings.iter().filter(|w| w.starts_with("BLOCK:")).map(|w| w.trim_start_matches("BLOCK: ").to_string()).collect::<Vec<_>>().join("; ");
        return Err(RelocateError(msg));
    }
    let same_fs = same_fs.unwrap_or(checked.same_filesystem);
    let source = PathBuf::from(&checked.source);
    let target = PathBuf::from(&checked.target);
    std::fs::create_dir_all(&target).map_err(|e| RelocateError(format!("cannot create {}: {e}", target.display())))?;

    let mut progress = RelocateProgress { total_bytes: checked.total_bytes, ..Default::default() };
    let mut tried: HashSet<i64> = HashSet::new();
    let merged = checked.merge_into.is_some();
    let (src_s, tgt_s) = (source.to_string_lossy().into_owned(), target.to_string_lossy().into_owned());
    loop {
        // Downloads can land in the source root while the move runs: after each pass, pick up
        // rows that arrived meanwhile, until a pass finds none.
        loop {
            let pending = pending_rows(ctx, root_id, &tried)?;
            if pending.is_empty() {
                break;
            }
            progress.total += pending.len() as i64;
            for id in pending {
                tried.insert(id);
                // Read the row now, not from a list taken at the start: it is the truth as of this file.
                let row = ctx.read(move |c| {
                    Ok(c.query_row("SELECT path, rel_path FROM files WHERE id = ?1 AND root_id = ?2", (id, root_id), |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })
                    .optional()?)
                })?;
                if let Some((old_s, rel_path)) = row {
                    let old = PathBuf::from(&old_s);
                    let relative = match old.strip_prefix(&target) {
                        // Already moved by an earlier, interrupted run.
                        Ok(r) if !old.starts_with(&source) => r.to_string_lossy().into_owned(),
                        _ => relative_of(&old, rel_path, &source),
                    };
                    let (new, new_root, new_rel) = checked.destination(&target, &relative);
                    progress.current = relative.clone();
                    match move_one(ctx, id, &old, &new, new_root, &new_rel, same_fs) {
                        Ok(Step::Placed { bytes }) => progress.bytes_moved += bytes as i64,
                        Ok(Step::Folded { bytes }) => {
                            progress.bytes_moved += bytes as i64;
                            progress.duplicates += 1;
                        }
                        Ok(Step::Conflict(why)) => progress.conflicts.push(format!("{relative}: {why}")),
                        Err(e) => {
                            tracing::warn!(file = %relative, error = %e, "relocate failed");
                            progress.errors.push(format!("{relative}: {e}"));
                        }
                    }
                } // else: deleted (or moved out of the root) meanwhile
                progress.moved += 1;
                if !on_progress(&progress) {
                    return Ok(progress);
                }
            }
        }

        // A root pointed at the target with files still at the source would lose them to the next
        // scan, so it is only repointed when nothing was left behind. A merge leaves the root
        // where it is (conflicts stay in it, as do downloads that land during the move).
        let left_behind = !progress.errors.is_empty() || (!merged && !progress.conflicts.is_empty());
        if left_behind && !merged {
            break;
        }
        let (src_q, tgt_q) = (src_s.clone(), tgt_s.clone());
        let late = ctx.write(move |t| {
            if !merged {
                let rows: i64 = t.query_row(
                    "SELECT COUNT(*) FROM files WHERE root_id = ?1 AND (path = ?2 OR substr(path, 1, length(?2) + 1) = ?2 || '/')",
                    (root_id, &src_q),
                    |r| r.get(0),
                )?;
                if rows > 0 {
                    return Ok(true); // arrived after the last pass: go round again
                }
                t.execute("UPDATE library_roots SET path = ?1 WHERE id = ?2", (&tgt_q, root_id))?;
            }
            let n = src_q.len() as i64;
            t.execute(
                "UPDATE releases SET folder_path = ?2 || substr(folder_path, ?3 + 1)
                  WHERE folder_path = ?1 OR substr(folder_path, 1, ?3 + 1) = ?1 || '/'",
                (&src_q, &tgt_q, n),
            )?;
            Ok(false)
        })?;
        if !late {
            cleanup_empty_dirs(&source);
            break;
        }
    }
    let left_behind = !progress.errors.is_empty() || (!merged && !progress.conflicts.is_empty());
    progress.done = true;
    progress.outcome = Some(if left_behind { MoveOutcome::Incomplete } else { MoveOutcome::Complete });
    on_progress(&progress);
    Ok(progress)
}

/// Rows of the root not yet processed in this run.
fn pending_rows(ctx: &Ctx, root_id: i64, tried: &HashSet<i64>) -> Result<Vec<i64>, RelocateError> {
    let all: Vec<i64> = ctx.read(move |c| {
        let mut st = c.prepare("SELECT id FROM files WHERE root_id = ?1 ORDER BY id")?;
        Ok(st.query_map([root_id], |r| r.get(0))?.collect::<Result<_, _>>()?)
    })?;
    Ok(all.into_iter().filter(|id| !tried.contains(id)).collect())
}

/// Bring one row to `new`: move its file if it is still at `old`, then rewrite the row.
fn move_one(ctx: &Ctx, id: i64, old: &Path, new: &Path, new_root: i64, new_rel: &str, same_fs: bool) -> Result<Step, String> {
    let new_s = new.to_string_lossy().into_owned();
    // Another row has this path: the same record, already in the library.
    let owner: Option<i64> = {
        let p = new_s.clone();
        ctx.read(move |c| Ok(c.query_row("SELECT id FROM files WHERE path = ?1 AND id != ?2", (p, id), |r| r.get(0)).optional()?))
            .map_err(|e| e.to_string())?
    };
    if let Some(owner) = owner {
        return fold_duplicate(ctx, id, owner, old, new, same_fs);
    }
    let mut bytes = 0;
    if old.exists() && !same_file(old, new) {
        if new.exists() {
            // An untracked file is in the way: the copy of an interrupted earlier run, or something else.
            if same_content(old, new).map_err(|e| e.to_string())? {
                std::fs::remove_file(old).map_err(|e| e.to_string())?;
            } else {
                return Ok(Step::Conflict(format!("a different file is already at {new_s}")));
            }
        } else {
            bytes = move_file(old, new, same_fs).map_err(|e| e.to_string())?;
        }
    }
    // Update the row immediately, so an interruption leaves the database agreeing with the
    // filesystem for every file processed so far. Missing at both ends stays (or becomes) missing.
    let (rel, present, now) = (new_rel.to_string(), new.exists(), now_db());
    ctx.write(move |t| {
        t.execute(
            "UPDATE files SET path = ?1, rel_path = ?2, root_id = ?3,
                    missing_since = CASE WHEN ?4 THEN NULL ELSE COALESCE(missing_since, ?5) END
              WHERE id = ?6",
            (new_s, rel, new_root, present, now, id),
        )?;
        Ok(())
    })
    .map_err(|e| e.to_string())?;
    Ok(Step::Placed { bytes })
}

/// Fold row `id` (at `old`) into row `owner`, which already has its destination `new`: the same
/// record downloaded twice. The owner's track keeps the user data of both ([`fold_user_data`]); the
/// duplicate's file row and track go (its release too, once empty); and the source copy goes --
/// moved into place first when the owner's own file is missing, so the last copy is never lost.
fn fold_duplicate(ctx: &Ctx, id: i64, owner: i64, old: &Path, new: &Path, same_fs: bool) -> Result<Step, String> {
    let mut bytes = 0;
    if old.exists() && !new.exists() {
        bytes = move_file(old, new, same_fs).map_err(|e| e.to_string())?;
        let m = std::fs::metadata(new).map_err(|e| e.to_string())?;
        let now = now_db();
        ctx.write(move |t| {
            use std::os::unix::fs::MetadataExt;
            t.execute(
                "UPDATE files SET size_bytes = ?1, mtime_ns = ?2, inode = ?3, last_seen_at = ?4, missing_since = NULL WHERE id = ?5",
                (m.len() as i64, m.mtime() * 1_000_000_000 + m.mtime_nsec(), m.ino() as i64, now, owner),
            )?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    }
    let emptied = ctx
        .write(move |t| {
            let track_of = |file: i64| t.query_row("SELECT track_id FROM files WHERE id = ?1", [file], |r| r.get::<_, i64>(0));
            let (dup, keeper) = (track_of(id)?, track_of(owner)?);
            t.execute("DELETE FROM files WHERE id = ?1", [id])?;
            if dup == keeper || t.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE track_id = ?1)", [dup], |r| r.get::<_, bool>(0))? {
                return Ok(None);
            }
            crate::strays::fold_user_data(t, dup, keeper)?;
            let (release, keeper_release): (Option<i64>, Option<i64>) = (
                t.query_row("SELECT release_id FROM tracks WHERE id = ?1", [dup], |r| r.get(0))?,
                t.query_row("SELECT release_id FROM tracks WHERE id = ?1", [keeper], |r| r.get(0))?,
            );
            crate::delete::release_tag_counts(t, &[dup])?;
            bc_db::fts::remove_tracks(t, &[dup]).map_err(ApiError::from)?;
            t.execute("DELETE FROM tracks WHERE id = ?1", [dup])?;
            let Some(rid) = release.filter(|r| Some(*r) != keeper_release) else { return Ok(None) };
            if t.query_row("SELECT EXISTS(SELECT 1 FROM tracks WHERE release_id = ?1)", [rid], |r| r.get::<_, bool>(0))? {
                return Ok(None);
            }
            crate::strays::repoint_inbox(t, rid, keeper_release)?;
            crate::tidy::delete_artwork_rows(t, &[rid])?;
            t.execute("DELETE FROM releases WHERE id = ?1", [rid])?;
            Ok(Some(rid))
        })
        .map_err(|e| e.to_string())?;
    if let Some(rid) = emptied {
        crate::tidy::delete_artwork(&ctx.config.art_dir(), &[rid]);
    }
    // The library has its own copy at `new`: this one is surplus.
    if old.exists() && !same_file(old, new) {
        std::fs::remove_file(old).map_err(|e| format!("folded, but could not remove the source copy: {e}"))?;
    }
    Ok(Step::Folded { bytes })
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

    fn run_copying(l: &Lib, target: &Path) -> Result<RelocateProgress, RelocateError> {
        execute_with(&l.env, l.root_id, target.to_str().unwrap(), Some(false), |_| true)
    }

    fn root_path(l: &Lib) -> String {
        q_str(&l.env.db, &format!("SELECT path FROM library_roots WHERE id={}", l.root_id)).unwrap()
    }

    #[test]
    fn move_across_drives_relocates_files_and_repoints_the_database() {
        let l = lib();
        let target = new_target(&l);
        let r = run_copying(&l, &target).unwrap();
        assert!(r.errors.is_empty() && r.done && r.outcome == Some(MoveOutcome::Complete), "{r:?}");
        assert_eq!(mp3s(&target), 3);
        assert_eq!(mp3s(&l.old), 0);
        assert_eq!(root_path(&l), target.to_str().unwrap());
        for p in q_paths(&l.env.db) {
            assert!(Path::new(&p).is_file(), "{p}");
        }
    }

    #[test]
    fn moving_a_file_onto_itself_keeps_it() {
        // The cross-drive path copies to a staging file, renames it into place, then deletes the
        // source -- which, for a file "moved" onto itself, was the only copy.
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("track.mp3");
        std::fs::write(&f, vec![7u8; 2048]).unwrap();
        assert_eq!(move_file(&f, &f, false).unwrap(), 2048);
        assert_eq!(std::fs::read(&f).unwrap(), vec![7u8; 2048]);
    }

    #[test]
    fn move_file_never_writes_over_an_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("a.mp3"), tmp.path().join("b.mp3"));
        std::fs::write(&a, b"new").unwrap();
        std::fs::write(&b, b"keep").unwrap();
        for same_fs in [true, false] {
            assert_eq!(move_file(&a, &b, same_fs).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
            assert_eq!(std::fs::read(&b).unwrap(), b"keep");
            assert!(a.exists());
        }
    }

    #[test]
    fn re_running_over_rows_already_at_the_target_deletes_nothing() {
        // What two concurrent runs did: the second one's rows already pointed at the target.
        let l = lib();
        let target = new_target(&l);
        execute_with(&l.env, l.root_id, target.to_str().unwrap(), Some(false), |p| p.moved < 2).unwrap();
        assert_eq!(mp3s(&target), 2);
        let r = run_copying(&l, &target).unwrap();
        assert!(r.errors.is_empty() && r.conflicts.is_empty(), "{r:?}");
        assert_eq!(mp3s(&target), 3, "every file survives the second run");
        assert_eq!(r.bytes_moved, 3000, "only the file still at the source is copied");
        for p in q_paths(&l.env.db) {
            assert!(Path::new(&p).is_file(), "{p}");
        }
    }

    #[test]
    fn an_identical_copy_at_the_target_counts_as_moved() {
        // An earlier run copied the file and was cut off before removing the original.
        let l = lib();
        let target = new_target(&l);
        let rel = "Somatic/Grid Failure/02 - track.mp3";
        std::fs::create_dir_all(target.join("Somatic/Grid Failure")).unwrap();
        std::fs::copy(l.old.join(rel), target.join(rel)).unwrap();
        let r = run_copying(&l, &target).unwrap();
        assert!(r.conflicts.is_empty() && r.errors.is_empty(), "{r:?}");
        assert!(!l.old.join(rel).exists());
        assert_eq!(mp3s(&target), 3);
        assert_eq!(root_path(&l), target.to_str().unwrap());
    }

    #[test]
    fn a_different_file_at_the_target_is_a_conflict_and_the_root_stays_put() {
        let l = lib();
        let target = new_target(&l);
        let rel = "Somatic/Grid Failure/02 - track.mp3";
        std::fs::create_dir_all(target.join("Somatic/Grid Failure")).unwrap();
        std::fs::write(target.join(rel), b"someone else's file").unwrap();
        let r = run_copying(&l, &target).unwrap();
        assert_eq!(r.conflicts.len(), 1, "{r:?}");
        assert_eq!(r.outcome, Some(MoveOutcome::Incomplete));
        assert_eq!(std::fs::read(target.join(rel)).unwrap(), b"someone else's file", "never overwritten");
        assert!(l.old.join(rel).is_file(), "the conflicting file stays at the source");
        assert_eq!(root_path(&l), l.old.to_str().unwrap(), "not repointed while a file is left behind");
        for p in q_paths(&l.env.db) {
            assert!(Path::new(&p).is_file(), "{p}");
        }
    }

    /// `lib()` plus a second, `library` root at `<tmp>/new-drive/library` holding one file of its
    /// own, which shares a relative path with the first root's track 3.
    fn with_library_root(l: &Lib) -> (PathBuf, i64, i64) {
        let lib_dir = l.tmp.path().join("new-drive").join("library");
        std::fs::create_dir_all(lib_dir.join("Somatic/Grid Failure")).unwrap();
        let lib_root = seed_root(&l.env.db, lib_dir.to_str().unwrap(), "library");
        let rel = "Somatic/Grid Failure/03 - track.mp3";
        std::fs::write(lib_dir.join(rel), b"the library's own copy").unwrap();
        let rid = seed_release(&l.env.db, "Somatic", "Grid Failure (library)", None, None);
        let t = seed_track(&l.env.db, rid, "t3 (library)", Some(3));
        let fid = seed_file(&l.env.db, t, lib_root, lib_dir.join(rel).to_str().unwrap(), rel, 22);
        (lib_dir, lib_root, fid)
    }

    #[test]
    fn moving_into_another_root_merges_into_it() {
        let l = lib();
        let (lib_dir, lib_root, lib_file) = with_library_root(&l);
        let p = plan(&l.env, l.root_id, lib_dir.to_str().unwrap()).unwrap();
        assert!(p.ok(), "{:?}", p.warnings);
        assert_eq!(p.merge_into.as_ref().map(|m| m.root_id), Some(lib_root));
        assert_eq!(p.conflicts, 1);
        let dup_track = q_i64(&l.env.db, &format!("SELECT track_id FROM files WHERE root_id={} AND rel_path LIKE '%03 - track.mp3'", l.root_id));
        let keeper = q_i64(&l.env.db, &format!("SELECT track_id FROM files WHERE id={lib_file}"));
        exec(&l.env.db, &format!("UPDATE tracks SET play_count = 5, loved = 1 WHERE id = {dup_track}"));
        exec(&l.env.db, "INSERT INTO playlists(id, name, kind, created_at, updated_at) VALUES (1, 'set', 'manual', '2024-01-01 00:00:00', '2024-01-01 00:00:00')");
        exec(&l.env.db, &format!("INSERT INTO playlist_items(playlist_id, track_id, position, added_at) VALUES (1, {dup_track}, 1, '2024-01-01 00:00:00')"));

        let r = run_copying(&l, &lib_dir).unwrap();
        assert_eq!(r.outcome, Some(MoveOutcome::Complete), "{r:?}");
        assert!(r.conflicts.is_empty(), "{r:?}");
        assert_eq!(r.duplicates, 1, "track 3 was already in the library");
        // Everything now belongs to the library root, relative to it; the source root is empty.
        assert_eq!(q_i64(&l.env.db, &format!("SELECT COUNT(*) FROM files WHERE root_id={lib_root}")), 3);
        assert_eq!(q_i64(&l.env.db, &format!("SELECT COUNT(*) FROM files WHERE root_id={}", l.root_id)), 0);
        assert_eq!(
            q_str(&l.env.db, &format!("SELECT rel_path FROM files WHERE root_id={lib_root} AND path LIKE '%01 - track.mp3'")).unwrap(),
            "Somatic/Grid Failure/01 - track.mp3"
        );
        // The duplicate is gone from disk and database; the library's copy is untouched and has its data.
        assert_eq!(mp3s(&l.old), 0, "the source is clean");
        assert_eq!(std::fs::read(lib_dir.join("Somatic/Grid Failure/03 - track.mp3")).unwrap(), b"the library's own copy");
        assert_eq!(q_i64(&l.env.db, &format!("SELECT COUNT(*) FROM tracks WHERE id={dup_track}")), 0);
        assert_eq!(q_i64(&l.env.db, &format!("SELECT play_count FROM tracks WHERE id={keeper}")), 5);
        assert_eq!(q_i64(&l.env.db, &format!("SELECT loved FROM tracks WHERE id={keeper}")), 1);
        assert_eq!(q_i64(&l.env.db, "SELECT track_id FROM playlist_items"), keeper, "the playlist entry follows");
        assert_eq!(q_i64(&l.env.db, "SELECT COUNT(*) FROM releases WHERE title = 'Grid Failure'"), 1, "a release with other tracks stays");
        // The merged root keeps its path (new downloads still land there).
        assert_eq!(root_path(&l), l.old.to_str().unwrap());
        assert!(l.old.is_dir());
        for p in q_paths(&l.env.db) {
            assert!(Path::new(&p).is_file(), "{p}");
        }
    }

    #[test]
    fn a_duplicate_whose_library_copy_is_missing_takes_its_place() {
        let l = lib();
        let (lib_dir, _, lib_file) = with_library_root(&l);
        let rel = "Somatic/Grid Failure/03 - track.mp3";
        std::fs::remove_file(lib_dir.join(rel)).unwrap();
        exec(&l.env.db, &format!("UPDATE files SET missing_since = '2026-01-01 00:00:00' WHERE id = {lib_file}"));
        let r = run_copying(&l, &lib_dir).unwrap();
        assert_eq!((r.duplicates, r.outcome), (1, Some(MoveOutcome::Complete)), "{r:?}");
        assert_eq!(std::fs::read(lib_dir.join(rel)).unwrap(), vec![3u8; 3000], "the only copy moved into place");
        assert!(!l.old.join(rel).exists());
        assert_eq!(q_i64(&l.env.db, &format!("SELECT missing_since IS NULL FROM files WHERE id={lib_file}")), 1);
        assert_eq!(q_i64(&l.env.db, &format!("SELECT size_bytes FROM files WHERE id={lib_file}")), 3000);
    }

    #[test]
    fn a_duplicate_alone_in_its_release_takes_the_release_with_it() {
        let l = lib();
        let (lib_dir, _, _) = with_library_root(&l);
        // Track 3 on a release of its own.
        let solo = seed_release(&l.env.db, "Somatic", "Grid Failure (single)", None, None);
        exec(&l.env.db, &format!("UPDATE tracks SET release_id = {solo} WHERE id = (SELECT track_id FROM files WHERE root_id={} AND rel_path LIKE '%03 - track.mp3')", l.root_id));
        let r = run_copying(&l, &lib_dir).unwrap();
        assert_eq!(r.duplicates, 1, "{r:?}");
        assert_eq!(q_i64(&l.env.db, &format!("SELECT COUNT(*) FROM releases WHERE id={solo}")), 0);
    }

    #[test]
    fn a_merge_into_a_subfolder_of_another_root_keeps_rel_paths_relative_to_it() {
        let l = lib();
        let (lib_dir, lib_root, _) = with_library_root(&l);
        let sub = lib_dir.join("from-downloads");
        let r = run_copying(&l, &sub).unwrap();
        assert!(r.conflicts.is_empty() && r.errors.is_empty(), "{r:?}");
        assert_eq!(
            q_str(&l.env.db, &format!("SELECT rel_path FROM files WHERE root_id={lib_root} AND path LIKE '%from-downloads%01 - track.mp3'")).unwrap(),
            "from-downloads/Somatic/Grid Failure/01 - track.mp3"
        );
    }

    #[test]
    fn a_target_that_holds_another_root_is_refused() {
        let l = lib();
        let (_, _, _) = with_library_root(&l);
        let p = plan(&l.env, l.root_id, l.tmp.path().join("new-drive").to_str().unwrap()).unwrap();
        assert!(!p.ok());
        assert!(p.warnings.iter().any(|w| w.contains("contains another library folder")), "{:?}", p.warnings);
    }

    #[test]
    fn a_file_missing_at_both_ends_moves_as_missing() {
        let l = lib();
        let target = new_target(&l);
        std::fs::remove_file(l.old.join("Somatic/Grid Failure/01 - track.mp3")).unwrap();
        let r = run_copying(&l, &target).unwrap();
        assert_eq!(r.outcome, Some(MoveOutcome::Complete), "{r:?}");
        assert_eq!(q_i64(&l.env.db, "SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL"), 1);
        assert_eq!(q_i64(&l.env.db, &format!("SELECT COUNT(*) FROM files WHERE path LIKE '{}/%'", target.display())), 3);
    }

    #[test]
    fn rows_that_arrive_during_the_move_go_too() {
        // A download finishing mid-move writes its row under the old path.
        let l = lib();
        let target = new_target(&l);
        let rel = "Late/Arrival/01 - late.mp3";
        let mut added = false;
        let r = execute_with(&l.env, l.root_id, target.to_str().unwrap(), Some(false), |p| {
            if !added && p.moved == 1 {
                added = true;
                std::fs::create_dir_all(l.old.join("Late/Arrival")).unwrap();
                std::fs::write(l.old.join(rel), vec![9u8; 500]).unwrap();
                let rid = seed_release(&l.env.db, "Late", "Arrival", None, None);
                let t = seed_track(&l.env.db, rid, "late", Some(1));
                seed_file(&l.env.db, t, l.root_id, l.old.join(rel).to_str().unwrap(), rel, 500);
            }
            true
        })
        .unwrap();
        assert_eq!((r.moved, r.total, r.outcome), (4, 4, Some(MoveOutcome::Complete)));
        assert!(target.join(rel).is_file());
        assert_eq!(root_path(&l), target.to_str().unwrap());
    }
}

