//! Per-item staging directories (`<base>/.staging/item-<id>`): merge into the library tree, purge
//! the stale ones, and clean up after a crash (partials, orphaned `bandcamp-dl` children).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use bc_db::Db;

use crate::download::bcdl::is_audio_name;
use crate::download::dedup::url_key;

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
/// folders. `rename` is atomic on one filesystem, and overwriting this release's own files is
/// correct -- a fresh download beats whatever stale file was there.
///
/// With `owner` (the library and the URL being downloaded), files the library files under a
/// *different* release are never overwritten. Two records whose names slugify alike ("Untitled",
/// "E.P." and "EP") get the same folder; the second one lands in `<album>-2` instead of renaming
/// over the first one's tracks, and a lone colliding file becomes `<name> (2).<ext>`.
pub fn merge_staging(staging: &Path, target: &Path, owner: Option<(&Db, &str)>) -> Vec<PathBuf> {
    if !staging.is_dir() {
        return Vec::new();
    }
    let mut files = Vec::new();
    walk_files(staging, &mut files);
    files.retain(|p| !is_partial(&p.to_string_lossy()));
    files.sort();
    let mut audio = Vec::new();
    if files.iter().any(|p| is_audio_name(&p.to_string_lossy())) {
        let guard = owner.map(|(db, url)| Guard { db, url: url_key(url) });
        let mut dirs: HashMap<PathBuf, PathBuf> = HashMap::new();
        for src in &files {
            let Ok(rel) = src.strip_prefix(staging) else { continue };
            let Some(name) = rel.file_name() else { continue };
            let rel_dir = rel.parent().unwrap_or(Path::new(""));
            let dir = dirs
                .entry(rel_dir.to_path_buf())
                .or_insert_with(|| match &guard {
                    Some(g) => g.free_dir(target, rel_dir),
                    None => target.join(rel_dir),
                })
                .clone();
            if let Err(e) = std::fs::create_dir_all(&dir) {
                tracing::warn!("cannot create {}: {e}", dir.display());
                continue;
            }
            let dest = match &guard {
                Some(g) => g.free_file(&dir, Path::new(name)),
                None => dir.join(name),
            };
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

/// Who the library says a folder or file already belongs to.
#[derive(Debug, PartialEq)]
enum Claim {
    /// Nothing on disk, or nothing the library knows about: safe to write into.
    Free,
    /// The release being downloaded: overwriting it is a refresh.
    Ours,
    /// Another release (or one with no URL, which might be anything): hands off.
    Theirs,
}

struct Guard<'a> {
    db: &'a Db,
    /// `url_key` of the release being downloaded.
    url: String,
}

impl Guard<'_> {
    /// `target/rel_dir`, or the first `<last>-N` sibling of it that is not another release's.
    /// Only the album folder moves: `target` itself (a shelf, a flat batch) is shared by design.
    fn free_dir(&self, target: &Path, rel_dir: &Path) -> PathBuf {
        let wanted = target.join(rel_dir);
        let Some(leaf) = rel_dir.file_name().map(|n| n.to_string_lossy().into_owned()) else { return wanted };
        if self.dir_claim(&wanted) != Claim::Theirs {
            return wanted;
        }
        let parent = wanted.parent().unwrap_or(target).to_path_buf();
        for n in 2.. {
            let cand = parent.join(format!("{leaf}-{n}"));
            if !cand.exists() || self.dir_claim(&cand) == Claim::Ours {
                tracing::warn!("{} belongs to another release; {} goes to {}", wanted.display(), self.url, cand.display());
                return cand;
            }
        }
        unreachable!()
    }

    /// `dir/name`, or `dir/<stem> (N).<ext>` when `dir/name` is another release's file.
    fn free_file(&self, dir: &Path, name: &Path) -> PathBuf {
        let wanted = dir.join(name);
        if !wanted.exists() || self.file_claim(&wanted) != Claim::Theirs {
            return wanted;
        }
        let stem = name.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = name.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        for n in 2.. {
            let cand = dir.join(format!("{stem} ({n}){ext}"));
            if !cand.exists() || self.file_claim(&cand) == Claim::Ours {
                tracing::warn!("{} belongs to another release; kept it and wrote {}", wanted.display(), cand.display());
                return cand;
            }
        }
        unreachable!()
    }

    fn dir_claim(&self, dir: &Path) -> Claim {
        if !dir.exists() {
            return Claim::Free;
        }
        self.claim("SELECT bandcamp_url FROM releases WHERE folder_path IN (?1, ?2)", dir)
    }

    fn file_claim(&self, file: &Path) -> Claim {
        self.claim(
            "SELECT r.bandcamp_url FROM files f JOIN tracks t ON t.id = f.track_id \
             JOIN releases r ON r.id = t.release_id WHERE f.path IN (?1, ?2)",
            file,
        )
    }

    /// The release URLs `sql` finds for `path` (as given and canonicalised -- rows store
    /// whichever form the ingest saw), boiled down to one verdict. Unreadable means `Free`: the
    /// guard never makes a download fail, it only steers where the files go.
    fn claim(&self, sql: &str, path: &Path) -> Claim {
        let raw = path.to_string_lossy().into_owned();
        let canon = std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| raw.clone());
        let urls = self
            .db
            .read(|c| {
                let mut st = c.prepare(sql)?;
                let v = st.query_map([&raw, &canon], |r| r.get::<_, Option<String>>(0))?.collect::<Result<Vec<_>, _>>()?;
                Ok(v)
            })
            .unwrap_or_default();
        if urls.is_empty() {
            Claim::Free
        } else if urls.iter().flatten().any(|u| url_key(u) == self.url) {
            Claim::Ours
        } else {
            Claim::Theirs
        }
    }
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
