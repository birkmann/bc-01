//! Per-item staging directories (`<base>/.staging/item-<id>`): merge into the library tree, purge
//! the stale ones, and clean up after a crash (partials, orphaned `bandcamp-dl` children).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use bc_db::Db;

use crate::download::bcdl::is_audio_name;

pub const STAGING_DIRNAME: &str = ".staging";

/// `<base>/.staging/item-<id>`.
pub fn staging_dir(base: &Path, item_id: i64) -> PathBuf {
    base.join(STAGING_DIRNAME).join(format!("item-{item_id}"))
}

/// A leftover of an interrupted download: never moved into the library.
fn is_partial(name: &str) -> bool {
    name.ends_with(".tmp") || name.ends_with(".part")
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            walk_files(&entry.path(), out);
        } else if ft.is_file() {
            out.push(entry.path());
        }
    }
}

/// Move a finished staging tree into the real downloads tree (`_merge_staging`).
///
/// Returns the final paths of the audio files moved; non-audio siblings (covers, playlists) move
/// with them, `.tmp`/`.part` leftovers do not. A staging tree with no audio at all is just
/// deleted: moving an orphaned cover into the library would litter the tree with empty album
/// folders. `rename` is atomic on one filesystem, and overwriting is correct -- a fresh download
/// beats whatever stale file was there.
pub fn merge_staging(staging: &Path, target: &Path) -> Vec<PathBuf> {
    if !staging.is_dir() {
        return Vec::new();
    }
    let mut files = Vec::new();
    walk_files(staging, &mut files);
    files.retain(|p| !is_partial(&p.to_string_lossy()));
    files.sort();
    let mut audio = Vec::new();
    if files.iter().any(|p| is_audio_name(&p.to_string_lossy())) {
        for src in &files {
            let Ok(rel) = src.strip_prefix(staging) else { continue };
            let dest = target.join(rel);
            if let Some(parent) = dest.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!("cannot create {}: {e}", parent.display());
                    continue;
                }
            }
            match std::fs::rename(src, &dest) {
                Ok(()) => {
                    if is_audio_name(&dest.to_string_lossy()) {
                        audio.push(dest);
                    }
                }
                Err(e) => tracing::warn!("cannot move {} to {}: {e}", src.display(), dest.display()),
            }
        }
    }
    let _ = std::fs::remove_dir_all(staging);
    audio
}

/// Ids of the download items that still need their staging dir (`pending` or `running`).
fn active_item_ids(db: &Db) -> HashSet<i64> {
    db.read(|c| {
        let mut st = c.prepare(
            "SELECT ji.id FROM job_items ji JOIN jobs j ON j.id = ji.job_id \
             WHERE j.kind = 'download' AND ji.status IN ('pending','running')",
        )?;
        let ids = st.query_map([], |r| r.get::<_, i64>(0))?.collect::<Result<HashSet<_>, _>>()?;
        Ok(ids)
    })
    .unwrap_or_default()
}

/// Delete per-item staging dirs whose item is no longer pending or running (`purge_stale_staging`).
///
/// Runs after crash recovery, so an interrupted item that was requeued keeps its staging and
/// resumes instead of re-downloading every track. Only `item-*` dirs are touched.
pub fn purge_stale_staging(db: &Db, base: &Path) -> usize {
    let root = base.join(STAGING_DIRNAME);
    let Ok(rd) = std::fs::read_dir(&root) else { return 0 };
    let active = active_item_ids(db);
    let mut removed = 0;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) || !name.starts_with("item-") {
            continue;
        }
        let id = name.strip_prefix("item-").and_then(|s| s.parse::<i64>().ok());
        if id.is_some_and(|i| active.contains(&i)) {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
        removed += 1;
    }
    if removed > 0 {
        tracing::info!("removed {removed} stale staging dir(s) under {}", root.display());
    }
    removed
}

/// Delete `*.tmp` / `*.part` below `dir` (crash recovery: a partial left by a killed download).
pub fn purge_partials(dir: &Path) -> usize {
    let mut files = Vec::new();
    walk_files(dir, &mut files);
    files
        .into_iter()
        .filter(|p| is_partial(&p.to_string_lossy()))
        .filter(|p| std::fs::remove_file(p).is_ok())
        .count()
}

/// Recovery over every surviving `item-*` staging dir: kill orphaned `bandcamp-dl` children, then
/// remove their partials. Returns `(orphans killed, partials removed)`.
pub fn recover_staging(base: &Path, binary: &str) -> (usize, usize) {
    let root = base.join(STAGING_DIRNAME);
    let Ok(rd) = std::fs::read_dir(&root) else { return (0, 0) };
    let dirs: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("item-") && e.path().is_dir())
        .map(|e| e.path())
        .collect();
    let killed = kill_orphans(&dirs, binary);
    let partials = dirs.iter().map(|d| purge_partials(d)).sum();
    (killed, partials)
}

/// Kill `bandcamp-dl` processes a SIGKILLed server left running against these staging dirs.
///
/// bandcamp-dl runs in its own process group, so a SIGKILL of the server leaves it going (and
/// writing `.tmp` files into the staging tree). The child's argv carries `--base-dir <staging>`
/// -- unique per item -- so the live process is found by scanning `/proc` for exactly that and for
/// the binary's name; nothing else is ever signalled. (A pid file would need the child's pid, which
/// the public `BandcampDl` API does not expose, and would be exposed to pid reuse.) The whole
/// process group is killed when the process leads its own group (which `BandcampDl` guarantees).
/// Linux only; a no-op elsewhere.
pub fn kill_orphans(staging_dirs: &[PathBuf], binary: &str) -> usize {
    #[cfg(target_os = "linux")]
    {
        kill_orphans_linux(staging_dirs, binary)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (staging_dirs, binary);
        0
    }
}

#[cfg(target_os = "linux")]
fn kill_orphans_linux(staging_dirs: &[PathBuf], binary: &str) -> usize {
    if staging_dirs.is_empty() {
        return 0;
    }
    let wanted: Vec<String> = staging_dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect();
    let name = Path::new(binary).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| binary.to_string());
    let me = std::process::id() as i32;
    let Ok(proc) = std::fs::read_dir("/proc") else { return 0 };
    let mut victims: Vec<i32> = Vec::new();
    for entry in proc.flatten() {
        let Some(pid) = entry.file_name().to_string_lossy().parse::<i32>().ok() else { continue };
        if pid == me || pid <= 1 {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else { continue };
        let args: Vec<String> = raw.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
        if !args.iter().any(|a| a.contains(&name)) {
            continue;
        }
        let in_staging = args.windows(2).any(|w| w[0] == "--base-dir" && wanted.contains(&w[1]));
        if !in_staging {
            continue;
        }
        victims.push(pid);
    }
    let mut killed = 0;
    for pid in &victims {
        // SAFETY: plain signalling syscalls on pids just verified against /proc.
        unsafe {
            let pgid = libc::getpgid(*pid);
            if pgid == *pid {
                libc::killpg(pgid, libc::SIGKILL);
            } else {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
        killed += 1;
        tracing::warn!("killed orphaned {name} (pid {pid}) left by a previous run");
    }
    // Let them die before the caller deletes their files.
    for pid in victims {
        for _ in 0..40 {
            if !Path::new(&format!("/proc/{pid}")).exists() || is_zombie(pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    killed
}

#[cfg(target_os = "linux")]
fn is_zombie(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map(|s| s.rsplit(')').next().is_some_and(|rest| rest.trim_start().starts_with('Z'))).unwrap_or(true)
}
