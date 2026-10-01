//! Downloads (PLAN §8): the [`Downloader`] trait, the shared outcome types, and
//! the two implementations (native Rust downloader, `bandcamp-dl` adapter).
//!
//! File ownership while the crate is built in parallel:
//! * this file — trait + shared types (WS2 lead)
//! * `slug`, `verify`, `bcdl`, `dedup`, `diskguard` — wave 1 (downloads part 1)
//! * `native`, `worker`, `staging` — wave 2 (downloads part 2)

pub mod bcdl;
pub mod dedup;
pub mod diskguard;
pub mod library_port;
pub mod native;
pub mod slug;
pub mod verify;
pub mod worker;

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// Outcome classes. Decided by looking at the filesystem (never the exit code):
/// exit 1 with a complete track set is `Ok`; exit 0 with no output is `NoOutput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutcomeKind {
    Ok,
    AlreadyHave,
    Partial,
    NoOutput,
    NotFound,
    Network,
    Timeout,
    Crash,
}

impl OutcomeKind {
    /// The `error_class` string stored on the job item / sent in `job.item.failed`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::AlreadyHave => "already_have",
            Self::Partial => "partial",
            Self::NoOutput => "no_output",
            Self::NotFound => "not_found",
            Self::Network => "network",
            Self::Timeout => "timeout",
            Self::Crash => "crash",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub kind: OutcomeKind,
    /// New or changed audio files, absolute paths.
    pub new_files: Vec<PathBuf>,
    pub tracks_expected: Option<u32>,
    /// What the tralbum this run fetched said is out now (pre-order state); `None` when the
    /// downloader did not read the page itself (bandcamp-dl subprocess).
    pub availability: Option<bc_types::library::ReleaseAvailability>,
    pub tracks_finished: u32,
    pub retryable: bool,
    pub detail: String,
}

impl Outcome {
    /// Both a fresh download and an already-complete one are successes.
    pub fn ok(&self) -> bool {
        matches!(self.kind, OutcomeKind::Ok | OutcomeKind::AlreadyHave)
    }
}

/// One progress record (from bandcamp-dl's `\r` stream, or the native streamer).
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub track_index: u32,
    pub track_total: u32,
    /// `Downloading` | `Encoding` | `Finished`
    pub phase: String,
    pub track_name: String,
    /// 0..=1 over the whole release.
    pub fraction: f64,
}

/// What to download and where.
#[derive(Debug, Clone)]
pub struct DownloadSpec {
    /// Album or track page URL (anything else is refused by `bandcamp-dl`).
    pub url: String,
    /// Staging directory the files land in (per item).
    pub base_dir: PathBuf,
    /// Layout template, e.g. `%{artist}/%{album}/%{track} - %{title}`; `None` = default.
    pub template: Option<String>,
    /// bandcamp-dl `-f`: refuse an album unless every track streams. The worker
    /// retries once without it when the run reports "Full album not available".
    pub full_album: bool,
    pub timeout: Duration,
    /// Download a /track/ URL as that single track.
    pub tracks_only: bool,
}

impl DownloadSpec {
    pub fn new(url: impl Into<String>, base_dir: impl AsRef<Path>) -> Self {
        Self {
            url: url.into(),
            base_dir: base_dir.as_ref().to_path_buf(),
            template: None,
            full_album: true,
            timeout: Duration::from_secs(2700),
            tracks_only: false,
        }
    }
}

/// Progress callback (called from the downloader's task; must be cheap).
pub type ProgressFn<'a> = &'a mut (dyn FnMut(Progress) + Send);

#[async_trait]
pub trait Downloader: Send + Sync {
    fn name(&self) -> &'static str;

    /// Download one release into `spec.base_dir`. Purges stale `*.tmp` first,
    /// classifies the result by filesystem diff, and kills the whole process
    /// tree (if any) on `cancel`. Never panics on I/O or network failures: they
    /// come back as an [`Outcome`] with the right `kind`.
    async fn download(&self, spec: &DownloadSpec, progress: ProgressFn<'_>, cancel: &CancellationToken) -> Outcome;
}
