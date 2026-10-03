//! Subprocess wrapper around the `bandcamp-dl` CLI.
//!
//! Five defects were verified by reading the installed 0.0.17 source, and each
//! shapes this module:
//!
//! 1. **Exit code is not a success signal.** `download_album()` returns `False`
//!    on failure but `main()` discards it, so failures exit 0. Conversely an
//!    album with no art raises `AttributeError` on the closing
//!    `os.remove(self.album_art)` *after* every track downloaded fine, exiting
//!    non-zero on a successful download. Success is therefore decided by
//!    observing the filesystem -- see [`super::verify::classify`].
//!
//! 2. **A stale `.tmp` silently truncates.** If `<name>.mp3.tmp` exists the
//!    downloader skips fetching and renames the partial straight to `.mp3`. Any
//!    killed process leaves one, so the retry "succeeds" into a corrupt track.
//!    Purging `*.tmp` before every attempt is the single most important fix --
//!    but it has to distinguish an abandoned partial from one a concurrent
//!    download is still writing. See [`sweep_before_attempt`].
//!
//! 3. **A `cover.jpg` already in the folder kills the whole album.** With `-r`,
//!    `self.album_art` is assigned *only* inside
//!    `if album['art'] and not os.path.exists(dirname + "/cover.jpg")`, but
//!    `write_id3_tags` then does `open(self.album_art, 'rb')` for every track.
//!    So a folder that already holds a `cover.jpg` raises `AttributeError` on
//!    **track 1** and the album produces nothing at all. This is why thousands
//!    of album folders in the legacy library contain a cover and no music: the
//!    earlier prototypes left the cover behind, so every later attempt crashed
//!    the same way. See [`sweep_before_attempt`].
//!
//! 4. **Progress has no newlines.** `print_clean` uses `print(..., end='')` with
//!    a leading `\r`, padded to terminal width. `readline()` would block until
//!    process exit, so stdout is read in fixed-size chunks and split on `\r`,
//!    and the **last record is parsed at EOF** (it has no terminator).
//!
//! 5. **Non album/track URLs are skipped in silence** (empty work list, exit 0).
//!    [`BandcampDl::build_command`] refuses them.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use regex::Regex;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::dedup::{UrlKind, classify_url, normalise};
pub use super::slug::{DEFAULT_TEMPLATE, FLAT_TEMPLATE};
use super::verify::classify;
use super::{DownloadSpec, Downloader, Outcome, OutcomeKind, Progress, ProgressFn};

/// `services/metadata/model.py::COVER_EXTENSIONS`.
pub const COVER_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".gif", ".webp"];
/// `services/metadata/model.py::COVER_STEMS`.
pub const COVER_STEMS: &[&str] = &["cover", "folder", "front", "album", "artwork"];

/// Lowercased `.ext` of a file name (Python `Path(name).suffix.lower()`).
fn suffix_lower(name: &str) -> String {
    match Path::new(name).extension() {
        Some(e) => format!(".{}", e.to_string_lossy().to_lowercase()),
        None => String::new(),
    }
}

/// Whether a (relative) path names an audio file by extension.
pub fn is_audio_name(name: &str) -> bool {
    bc_core::audio::is_audio_path(Path::new(name))
}

fn is_cover_name(name: &str) -> bool {
    let stem = Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    COVER_STEMS.contains(&stem.as_str()) && COVER_EXTENSIONS.contains(&suffix_lower(name).as_str())
}

/// Whether bandcamp-dl will do anything at all with this URL.
///
/// The installed 0.0.17 acts on `/album/` and `/track/` pages only, and skips
/// anything else in silence rather than complaining.
pub fn is_downloadable(url: &str) -> bool {
    matches!(classify_url(&normalise(url)), UrlKind::Album | UrlKind::Track)
}

// (3/12) [==========          ] :: Downloading: some-track-name
static PROGRESS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\((?P<n>\d+)/(?P<total>\d+)\)\s+\[(?P<bar>[=\s]*)\]\s*::\s*(?P<phase>Downloading|Encoding|Finished):\s*(?P<name>.*?)\s*$",
    )
    .expect("static progress regex")
});

/// The progress regex (exposed for tests).
pub fn progress_regex() -> &'static Regex {
    &PROGRESS_RE
}

/// `bandcampdownloader.py`: `File: {name} already exists and is complete, skipping..`
/// -- one line per track, because `--overwrite` is never passed. This is the
/// downloader stating outright that it already has the album, which beats
/// inferring the same thing from the filesystem.
pub const SKIP_MARKER: &str = "already exists and is complete";

/// `__main__.py`: `Full album not available. Skipping <title> ...` -- what `-f`
/// does when any one track on the release has no public stream. The album is
/// dropped from the work list before a single byte is fetched, so the run
/// produces nothing at all and exits 0. Availability is a property of the
/// release, so retrying is futile; the caller re-runs without `-f` instead.
pub const FULL_ALBUM_SKIP_MARKER: &str = "Full album not available";

const NETWORK_HINTS: &[&str] = &[
    "connection",
    "timed out",
    "timeout",
    "temporary failure",
    "name resolution",
    "network",
    "max retries",
    "ssl",
];

/// Relative path -> (size, mtime_ns), for outcome diffing.
pub type SnapshotMap = HashMap<String, (u64, i64)>;

const TAIL_LINES: usize = 200;
const PROGRESS_COALESCE: Duration = Duration::from_millis(250);
const KILL_GRACE: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// The raw facts of one bandcamp-dl run, before classification.
#[derive(Debug, Clone, Default)]
pub struct RawRun {
    pub exit_code: i32,
    pub timed_out: bool,
    /// The caller's cancel token fired and the process group was killed.
    pub cancelled: bool,
    pub track_total: Option<u32>,
    pub finished_count: u32,
    /// Tracks bandcamp-dl declined to fetch because the file was already there
    /// and complete. Counted as it streams: `stdout_tail` is bounded, so on a
    /// long album these lines are evicted long before the run ends.
    pub skipped_existing: u32,
    /// `-f` dropped the album because not every track has a public stream.
    pub full_album_skipped: bool,
    pub stdout_tail: Vec<String>,
    pub stderr_tail: Vec<String>,
    pub duration_s: f64,
}

impl RawRun {
    pub fn new(exit_code: i32, timed_out: bool, track_total: Option<u32>, finished_count: u32) -> Self {
        Self { exit_code, timed_out, track_total, finished_count, ..Default::default() }
    }

    pub fn stderr_text(&self) -> String {
        self.stderr_tail.join("\n")
    }

    pub fn has_network_error(&self) -> bool {
        let blob = format!("{}{}", self.stderr_text(), self.stdout_tail.join("\n")).to_lowercase();
        NETWORK_HINTS.iter().any(|h| blob.contains(h))
    }
}

/// Errors from [`BandcampDl::run`] / [`BandcampDl::build_command`].
#[derive(Debug, thiserror::Error)]
pub enum BcdlError {
    #[error("bandcamp-dl cannot download {0}: not an album or track page")]
    NotDownloadable(String),
    #[error("could not start bandcamp-dl ({binary}): {source}")]
    Spawn { binary: String, source: std::io::Error },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

/// Visit every non-directory entry below `dir` (symlinked directories are not
/// followed, like `os.walk`). `visit(parent, names)` is called once per directory.
fn walk(dir: &Path, visit: &mut dyn FnMut(&Path, &[String])) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut names = Vec::new();
    let mut subdirs = Vec::new();
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if ft.is_dir() {
            subdirs.push(entry.path());
        } else {
            names.push(name);
        }
    }
    visit(dir, &names);
    for sub in subdirs {
        walk(&sub, visit);
    }
}

/// Delete `*.tmp` under `directory`. See defect 2 in the module docs.
///
/// `older_than` guards the startup sweep against a download still running in
/// another process. It is the wrong guard for the pre-attempt sweep -- see
/// [`sweep_before_attempt`].
pub fn purge_tmp_files(directory: &Path, older_than: Option<Duration>) -> usize {
    if !directory.is_dir() {
        return 0;
    }
    let now = SystemTime::now();
    let mut removed = 0;
    walk(directory, &mut |parent, names| {
        for name in names {
            if !name.ends_with(".tmp") {
                continue;
            }
            let path = parent.join(name);
            if let Some(min_age) = older_than {
                match std::fs::metadata(&path).and_then(|m| m.modified()) {
                    Ok(mtime) => {
                        if now.duration_since(mtime).unwrap_or_default() < min_age {
                            continue;
                        }
                    }
                    Err(_) => continue,
                }
            }
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    });
    if removed > 0 {
        info!("purged {removed} stale .tmp file(s) under {}", directory.display());
    }
    removed
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub tmp_removed: usize,
    pub covers_removed: usize,
}

/// Timing knobs of [`sweep_before_attempt`].
#[derive(Debug, Clone, Copy)]
pub struct SweepConfig {
    /// How long to wait before re-measuring every `.tmp`.
    pub probe: Duration,
    /// Minimum age of an orphaned cover before it is deleted.
    pub cover_min_age: Duration,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self { probe: Duration::from_millis(500), cover_min_age: Duration::from_secs(300) }
    }
}

/// Clear the two leftovers that would make this attempt fail (defects 2, 3).
///
/// One walk, because `directory` is the shared downloads root -- 15k entries in
/// the legacy library -- and this runs before every attempt.
///
/// **Abandoned `*.tmp`.** Deleting every `*.tmp` is right only while each item
/// downloads into its own folder. Against one shared root each starting
/// download would delete its siblings' partial files: bandcamp-dl keeps writing
/// to the unlinked inode, then raises `FileNotFoundError` on the closing
/// rename. An age guard cannot separate the two cases -- retry backoff starts
/// at 10 s, well inside any useful window, so the stale file defect 2 is about
/// would slip through and truncate a track. *Growth* separates them:
/// bandcamp-dl streams in small chunks throughout, so a file another run owns
/// changes size across the probe and an abandoned one does not.
///
/// **Orphaned `cover.jpg`.** A cover with no audio beside it is a failed
/// download, and leaving it makes every retry crash on track 1 (defect 3). Only
/// folders with no audio are touched, so real art next to real music is never
/// at risk. `cover_min_age` keeps the sweep off a sibling that has just written
/// its cover and not yet its first track.
///
/// Blocking (it sleeps for the probe); call from `spawn_blocking`.
pub fn sweep_before_attempt(directory: &Path, cfg: SweepConfig) -> SweepReport {
    if !directory.is_dir() {
        return SweepReport::default();
    }
    let mut tmp_sizes: Vec<(PathBuf, u64)> = Vec::new();
    let mut orphan_covers: Vec<PathBuf> = Vec::new();
    let cover_cutoff = SystemTime::now().checked_sub(cfg.cover_min_age);

    walk(directory, &mut |parent, names| {
        let mut covers = Vec::new();
        let mut has_audio = false;
        for name in names {
            if name.ends_with(".tmp") {
                let path = parent.join(name);
                if let Ok(m) = std::fs::metadata(&path) {
                    tmp_sizes.push((path, m.len()));
                }
                continue;
            }
            if is_audio_name(name) {
                has_audio = true;
            } else if is_cover_name(name) {
                covers.push(parent.join(name));
            }
        }
        if !has_audio {
            orphan_covers.extend(covers);
        }
    });

    let mut covers_removed = 0;
    for path in orphan_covers {
        let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else { continue };
        if let Some(cutoff) = cover_cutoff
            && mtime > cutoff
        {
            continue; // a sibling run may have just written it
        }
        if std::fs::remove_file(&path).is_ok() {
            covers_removed += 1;
        }
    }

    let mut tmp_removed = 0;
    if !tmp_sizes.is_empty() {
        std::thread::sleep(cfg.probe);
        for (path, size) in tmp_sizes {
            match std::fs::metadata(&path) {
                Ok(m) if m.len() == size => {
                    if std::fs::remove_file(&path).is_ok() {
                        tmp_removed += 1;
                    }
                }
                _ => continue, // still growing (another run owns it) or gone
            }
        }
    }

    if tmp_removed > 0 || covers_removed > 0 {
        info!(
            "swept {}: {tmp_removed} abandoned .tmp, {covers_removed} orphaned cover(s)",
            directory.display()
        );
    }
    SweepReport { tmp_removed, covers_removed }
}

/// Map of relative path -> (size, mtime_ns) for outcome diffing.
pub fn snapshot_tree(directory: &Path) -> SnapshotMap {
    use std::os::unix::fs::MetadataExt;
    let mut out = SnapshotMap::new();
    if !directory.is_dir() {
        return out;
    }
    walk(directory, &mut |parent, names| {
        for name in names {
            let path = parent.join(name);
            // `metadata` follows symlinks, like Path.is_file()/stat().
            let Ok(m) = std::fs::metadata(&path) else { continue };
            if !m.is_file() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(directory) else { continue };
            let mtime_ns = m.mtime().saturating_mul(1_000_000_000).saturating_add(m.mtime_nsec());
            out.insert(rel.to_string_lossy().into_owned(), (m.len(), mtime_ns));
        }
    });
    out
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

/// Kills the whole process group on drop unless disarmed: a dropped future (a
/// cancelled task) must not orphan bandcamp-dl and its children.
struct GroupGuard(Option<i32>);

impl GroupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            killpg(pgid, libc::SIGKILL);
        }
    }
}

fn killpg(pgid: i32, sig: i32) {
    if pgid > 1 {
        // SAFETY: plain syscall; errors (ESRCH, EPERM) are deliberately ignored.
        unsafe {
            libc::killpg(pgid, sig);
        }
    }
}

/// SIGTERM the whole process group, then SIGKILL if it lingers past 5 s.
async fn kill_group(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let Some(pid) = child.id() else { return };
    let pgid = pid as i32; // the child leads its own group (process_group(0))
    killpg(pgid, libc::SIGTERM);
    if tokio::time::timeout(KILL_GRACE, child.wait()).await.is_err() {
        killpg(pgid, libc::SIGKILL);
        let _ = child.wait().await;
    }
}

/// Resolve a binary like `shutil.which`: a path with a separator is used as is,
/// a bare name is searched on `PATH`.
pub fn which(binary: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let is_exec = |p: &Path| std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false);
    if binary.contains('/') {
        let p = PathBuf::from(binary);
        return is_exec(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(binary)).find(|p| is_exec(p))
}

fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|s| -s)).unwrap_or(-1)
}

// ---------------------------------------------------------------------------
// Streaming parser
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StreamState {
    total: Option<u32>,
    finished: u32,
    skipped: u32,
    full_skipped: bool,
    stdout_tail: VecDeque<String>,
    last_emit: Option<Instant>,
}

impl StreamState {
    /// One `\r`-delimited record. May itself hold several newline-terminated
    /// lines (the skip notices are `\n`-terminated, progress is `\r`-separated,
    /// so both can arrive inside one chunk); each is handled on its own.
    fn handle_record(&mut self, raw: &str, on_progress: &mut Option<ProgressFn<'_>>) {
        for line in raw.split('\n') {
            self.handle_line(line, on_progress);
        }
    }

    fn handle_line(&mut self, raw: &str, on_progress: &mut Option<ProgressFn<'_>>) {
        let line = raw.trim();
        if line.is_empty() {
            return;
        }
        if self.stdout_tail.len() >= TAIL_LINES {
            self.stdout_tail.pop_front();
        }
        self.stdout_tail.push_back(line.to_string());

        // Counted per occurrence, not per line.
        self.skipped += line.matches(SKIP_MARKER).count() as u32;

        // Newline-terminated and never followed by a \r, so this arrives in the
        // tail flush at EOF rather than as its own chunk -- hence the flush.
        if line.contains(FULL_ALBUM_SKIP_MARKER) {
            self.full_skipped = true;
        }

        let Some(m) = PROGRESS_RE.captures(line) else { return };
        let n: u32 = m["n"].parse().unwrap_or(0);
        let total: u32 = m["total"].parse().unwrap_or(0);
        let phase = &m["phase"];
        self.total = Some(total);
        if phase == "Finished" {
            self.finished = self.finished.max(n);
        }
        let bar = &m["bar"];
        let bar_len = bar.chars().count();
        let within = if bar_len > 0 { bar.matches('=').count() as f64 / bar_len as f64 } else { 0.0 };
        let done_part = if phase != "Downloading" { 1.0 } else { within };
        let fraction = ((n as f64 - 1.0 + done_part) / total.max(1) as f64).clamp(0.0, 1.0);

        // Coalesce: a 50-track album would otherwise emit thousands of events
        // and make the UI main thread the bottleneck.
        let now = Instant::now();
        let due = self.last_emit.is_none_or(|t| now.duration_since(t) >= PROGRESS_COALESCE);
        if let Some(cb) = on_progress.as_mut()
            && (due || phase == "Finished")
        {
            self.last_emit = Some(now);
            cb(Progress {
                track_index: n,
                track_total: total,
                phase: phase.to_string(),
                track_name: m["name"].to_string(),
                fraction,
            });
        }
    }

    /// Feed bytes; complete `\r` records are handled, the rest stays in `buf`.
    fn feed(&mut self, buf: &mut Vec<u8>, chunk: &[u8], on_progress: &mut Option<ProgressFn<'_>>) {
        buf.extend_from_slice(chunk);
        while let Some(pos) = buf.iter().position(|&b| b == b'\r') {
            let record: Vec<u8> = buf.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&record[..record.len() - 1]).into_owned();
            self.handle_record(&text, on_progress);
        }
    }

    /// Progress lines are `\r`-*prefixed*, not `\r`-terminated, so the final
    /// record has no separator after it and is still sitting in `buf` at EOF.
    /// Dropping it loses the last "Finished:" line, which makes a complete album
    /// look like it downloaded one track short -- misclassified as `partial` and
    /// needlessly retried.
    fn flush(&mut self, buf: &mut Vec<u8>, on_progress: &mut Option<ProgressFn<'_>>) {
        if !buf.is_empty() {
            let text = String::from_utf8_lossy(buf).into_owned();
            buf.clear();
            self.handle_record(&text, on_progress);
        }
    }
}

fn push_stderr(tail: &mut VecDeque<String>, buf: &mut Vec<u8>, chunk: &[u8], flush: bool) {
    buf.extend_from_slice(chunk);
    let take = |tail: &mut VecDeque<String>, bytes: &[u8]| {
        let text = String::from_utf8_lossy(bytes).into_owned();
        let line = text.trim();
        if !line.is_empty() {
            if tail.len() >= TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line.to_string());
        }
    };
    while let Some(pos) = buf.iter().position(|&b| b == b'\n' || b == b'\r') {
        let record: Vec<u8> = buf.drain(..=pos).collect();
        take(tail, &record[..record.len() - 1]);
    }
    if flush && !buf.is_empty() {
        let rest = std::mem::take(buf);
        take(tail, &rest);
    }
}

// ---------------------------------------------------------------------------
// The adapter
// ---------------------------------------------------------------------------

/// Per-run options of [`BandcampDl::run`].
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub timeout: Duration,
    /// Pass `-f`. `false` yields the streamable tracks of a part-streamable
    /// release instead of nothing; it is the second attempt, never the first.
    pub full_album: bool,
    /// Overrides the configured layout for this run (flat-folder jobs).
    pub template: Option<String>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self { timeout: Duration::from_secs(2700), full_album: true, template: None }
    }
}

/// Adapter for the bandcamp-dl binary.
pub struct BandcampDl {
    pub binary: String,
    pub template: String,
    flags: tokio::sync::OnceCell<HashSet<String>>,
    sweep: SweepConfig,
    /// Extra environment for every spawned process (tests drive the fake with it).
    extra_env: Vec<(String, String)>,
}

impl BandcampDl {
    /// `binary` is a path, or a bare name resolved through `PATH`.
    pub fn new(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
            template: DEFAULT_TEMPLATE.to_string(),
            flags: tokio::sync::OnceCell::new(),
            sweep: SweepConfig::default(),
            extra_env: Vec::new(),
        }
    }

    /// Add an environment variable for every process this adapter spawns.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    pub fn with_template(mut self, template: impl Into<String>) -> Self {
        self.template = template.into();
        self
    }

    /// Override the pre-attempt sweep timing (tests).
    pub fn with_sweep(mut self, sweep: SweepConfig) -> Self {
        self.sweep = sweep;
        self
    }

    /// Parse `--help` once and cache the result.
    ///
    /// The gen-2 scripts hardcoded `--embed-genres` and `--no-confirm`, which
    /// are not in every published option list. Probing the *installed* binary
    /// avoids passing a flag it will reject. An unresolvable or unresponsive
    /// binary yields an empty set (cached, like the Python).
    pub async fn supported_flags(&self) -> &HashSet<String> {
        self.flags
            .get_or_init(|| async {
                let Some(path) = which(&self.binary) else { return HashSet::new() };
                let out = Command::new(&path)
                    .arg("--help")
                    .envs(self.extra_env.iter().map(|(k, v)| (k, v)))
                    .stdin(Stdio::null())
                    .kill_on_drop(true)
                    .output();
                let Ok(Ok(out)) = tokio::time::timeout(PROBE_TIMEOUT, out).await else {
                    return HashSet::new();
                };
                // stdout and stderr are merged in the Python probe.
                let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                text.push('\n');
                text.push_str(&String::from_utf8_lossy(&out.stderr));
                static FLAG_RE: LazyLock<Regex> =
                    LazyLock::new(|| Regex::new(r"--[a-z][a-z0-9-]+").expect("static flag regex"));
                let flags: HashSet<String> = FLAG_RE.find_iter(&text).map(|m| m.as_str().to_string()).collect();
                debug!("bandcamp-dl flags: {flags:?}");
                flags
            })
            .await
    }

    /// First line of `--version`, if the binary answers.
    pub async fn version(&self) -> Option<String> {
        let path = which(&self.binary)?;
        let out = Command::new(path).arg("--version").envs(self.extra_env.iter().map(|(k, v)| (k, v))).stdin(Stdio::null()).kill_on_drop(true).output();
        let out = tokio::time::timeout(PROBE_TIMEOUT, out).await.ok()?.ok()?;
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        text.trim().lines().next().map(str::to_string)
    }

    /// The argv for one run (`argv[0]` is the configured binary).
    pub async fn build_command(
        &self,
        url: &str,
        base_dir: &Path,
        full_album: bool,
        template: Option<&str>,
    ) -> Result<Vec<String>, BcdlError> {
        if !is_downloadable(url) {
            // Defect 5, and the quietest of them: `__main__` walks its
            // positional URLs with `if "/album/" not in url and "/track/" not
            // in url: continue`, so a discography or profile page is dropped
            // before a single request -- empty work list, exit 0, no output.
            // `classify` can only read that as `no_output`, which is how one
            // queued `/music` page failed "No audio files were produced."
            // three identical times. Refused at the one point a URL becomes
            // argv, so the invariant holds for every caller present and future.
            return Err(BcdlError::NotDownloadable(url.to_string()));
        }
        let flags = self.supported_flags().await;
        let mut cmd = vec![
            self.binary.clone(),
            "--template".into(),
            template.unwrap_or(&self.template).to_string(),
            "--base-dir".into(),
            base_dir.to_string_lossy().into_owned(),
        ];
        // -r / --embed-art: embed cover art (bandcamp-dl then deletes cover.jpg,
        // so ingestion re-extracts it from the APIC frame).
        cmd.push("-r".into());
        // -f / --full-album: refuse an album unless *every* track streams. The
        // default, because a half-album filed as a whole one is worse than no
        // album -- but see `download`: dropped to fetch what is streamable when
        // the alternative is nothing at all.
        if full_album {
            cmd.push("-f".into());
        }
        if flags.contains("--no-confirm") {
            cmd.push("--no-confirm".into());
        }
        if flags.contains("--embed-genres") {
            // Bandcamp tags -> TCON. This is the recommendation signal; without
            // it the library loses its most useful metadata.
            cmd.push("--embed-genres".into());
        }
        cmd.push(url.to_string());
        Ok(cmd)
    }

    /// Run bandcamp-dl once.
    ///
    /// Sweeps `base_dir` first (abandoned `.tmp`, orphaned covers), runs the
    /// binary in its own process group with `PYTHONUNBUFFERED=1`, `COLUMNS=200`,
    /// `LC_ALL=C.UTF-8`, streams stdout in 4 KiB chunks split on `\r`, and kills
    /// the whole group (SIGTERM, then SIGKILL after 5 s) on timeout or cancel.
    pub async fn run(
        &self,
        url: &str,
        base_dir: &Path,
        opts: &RunOptions,
        mut on_progress: Option<ProgressFn<'_>>,
        cancel: &CancellationToken,
    ) -> Result<RawRun, BcdlError> {
        // Validate before touching the disk.
        let cmd = self.build_command(url, base_dir, opts.full_album, opts.template.as_deref()).await?;
        tokio::fs::create_dir_all(base_dir).await?;
        // Off the runtime: this walks the whole downloads root and deliberately
        // sleeps to watch for growth.
        let dir = base_dir.to_path_buf();
        let sweep = self.sweep;
        let _ = tokio::task::spawn_blocking(move || sweep_before_attempt(&dir, sweep)).await;

        let started = Instant::now();
        let program = which(&self.binary).unwrap_or_else(|| PathBuf::from(&self.binary));
        let mut command = Command::new(&program);
        command
            .args(&cmd[1..])
            .env("PYTHONUNBUFFERED", "1") // else stdout to a pipe is block-buffered
            .env("COLUMNS", "200") // print_clean pads to the terminal width
            .env("LC_ALL", "C.UTF-8")
            .envs(self.extra_env.iter().map(|(k, v)| (k, v)))
            .current_dir(base_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group, so a timeout or cancel kills the whole tree
            // rather than orphaning children.
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|source| BcdlError::Spawn { binary: self.binary.clone(), source })?;
        let mut guard = GroupGuard(child.id().map(|p| p as i32));

        let mut state = StreamState::default();
        let mut stderr_tail: VecDeque<String> = VecDeque::new();
        let (mut out_buf, mut err_buf) = (Vec::new(), Vec::new());

        // Each pipe is read by its own task in 4 KiB chunks (never `readline`:
        // the progress stream has no newlines) and forwarded here, so the parse
        // state and the progress callback live on this one task.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(bool, Vec<u8>)>();
        let mut readers = Vec::new();
        if let Some(pipe) = child.stdout.take() {
            readers.push(tokio::spawn(pump_pipe(pipe, true, tx.clone())));
        }
        if let Some(pipe) = child.stderr.take() {
            readers.push(tokio::spawn(pump_pipe(pipe, false, tx.clone())));
        }
        drop(tx);

        let deadline = tokio::time::Instant::now() + opts.timeout;
        let mut timed_out = false;
        let mut cancelled = false;

        // Phase 1: until both pipes reach EOF (an empty chunk marks EOF).
        let mut open_pipes = readers.len();
        while open_pipes > 0 {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => { cancelled = true; break; }
                _ = tokio::time::sleep_until(deadline) => { timed_out = true; break; }
                msg = rx.recv() => match msg {
                    None => break,
                    Some((is_stdout, chunk)) if chunk.is_empty() => {
                        open_pipes -= 1;
                        if is_stdout {
                            state.flush(&mut out_buf, &mut on_progress);
                        } else {
                            push_stderr(&mut stderr_tail, &mut err_buf, &[], true);
                        }
                    }
                    Some((true, chunk)) => state.feed(&mut out_buf, &chunk, &mut on_progress),
                    Some((false, chunk)) => push_stderr(&mut stderr_tail, &mut err_buf, &chunk, false),
                },
            }
        }

        // Phase 2: reap.
        let mut exit_code = -1;
        if timed_out || cancelled {
            kill_group(&mut child).await;
            // Whatever the child managed to print before it died.
            state.flush(&mut out_buf, &mut on_progress);
            push_stderr(&mut stderr_tail, &mut err_buf, &[], true);
            if let Ok(Some(status)) = child.try_wait() {
                exit_code = exit_code_of(status);
            }
        } else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => { cancelled = true; }
                _ = tokio::time::sleep_until(deadline) => { timed_out = true; }
                st = child.wait() => {
                    if let Ok(status) = st { exit_code = exit_code_of(status); }
                }
            }
            if timed_out || cancelled {
                kill_group(&mut child).await;
                if let Ok(Some(status)) = child.try_wait() {
                    exit_code = exit_code_of(status);
                }
            }
        }
        guard.disarm();
        for r in readers {
            r.abort();
        }

        Ok(RawRun {
            exit_code,
            timed_out,
            cancelled,
            track_total: state.total,
            finished_count: state.finished,
            skipped_existing: state.skipped,
            full_album_skipped: state.full_skipped,
            stdout_tail: state.stdout_tail.into_iter().collect(),
            stderr_tail: stderr_tail.into_iter().collect(),
            duration_s: started.elapsed().as_secs_f64(),
        })
    }
}

/// Forward a pipe in 4 KiB chunks; an empty chunk signals EOF (or a read error).
async fn pump_pipe<R: tokio::io::AsyncRead + Unpin>(
    mut pipe: R,
    is_stdout: bool,
    tx: tokio::sync::mpsc::UnboundedSender<(bool, Vec<u8>)>,
) {
    let mut chunk = [0u8; 4096];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => {
                let _ = tx.send((is_stdout, Vec::new()));
                return;
            }
            Ok(n) => {
                if tx.send((is_stdout, chunk[..n].to_vec())).is_err() {
                    return;
                }
            }
        }
    }
}

fn outcome_for_error(err: &BcdlError) -> Outcome {
    let (kind, retryable) = match err {
        BcdlError::NotDownloadable(_) => (OutcomeKind::NoOutput, false),
        _ => (OutcomeKind::Crash, false),
    };
    Outcome {
        kind,
        new_files: Vec::new(),
        tracks_expected: None,
        availability: None,
        tracks_finished: 0,
        retryable,
        detail: err.to_string(),
    }
}

async fn snapshot_async(dir: PathBuf) -> SnapshotMap {
    tokio::task::spawn_blocking(move || snapshot_tree(&dir)).await.unwrap_or_default()
}

#[async_trait]
impl Downloader for BandcampDl {
    fn name(&self) -> &'static str {
        "bandcamp-dl"
    }

    async fn download(&self, spec: &DownloadSpec, progress: ProgressFn<'_>, cancel: &CancellationToken) -> Outcome {
        let base = spec.base_dir.clone();
        let mut opts = RunOptions {
            timeout: spec.timeout,
            full_album: spec.full_album,
            template: spec.template.clone(),
        };
        // Snapshot after the sweep would hide nothing (the sweep only deletes
        // junk), and before it keeps the diff honest about what this call did.
        let before = snapshot_async(base.clone()).await;

        let mut run = match self.run(&spec.url, &base, &opts, Some(&mut *progress), cancel).await {
            Ok(run) => run,
            Err(e) => {
                warn!("bandcamp-dl run failed: {e}");
                return outcome_for_error(&e);
            }
        };

        // `-f` dropped the album because some track has no public stream: the
        // run produced nothing and exits 0, and retrying with `-f` is futile.
        // Availability is a property of the release, so fetch what does stream.
        if opts.full_album && run.full_album_skipped && !run.cancelled {
            info!("{}: full album not available; retrying without -f", spec.url);
            opts.full_album = false;
            run = match self.run(&spec.url, &base, &opts, Some(&mut *progress), cancel).await {
                Ok(run) => run,
                Err(e) => return outcome_for_error(&e),
            };
        }

        if run.cancelled {
            return Outcome {
                kind: OutcomeKind::Crash,
                new_files: Vec::new(),
                tracks_expected: run.track_total,
                availability: None,
                tracks_finished: run.finished_count,
                retryable: false,
                detail: "Cancelled.".into(),
            };
        }

        let after = snapshot_async(base.clone()).await;
        classify(&before, &after, &run, &base)
    }
}
