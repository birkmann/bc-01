//! The native Rust downloader (PLAN §8): the page comes through the shared,
//! rate-limited [`BandcampClient`]; the audio bytes come through its cookie-less,
//! unthrottled CDN client, one track at a time (one connection per album).
//!
//! Layout and naming match bandcamp-dl exactly (`slug::expand_template`,
//! `strip_artist_prefix`, two-digit track numbers, `Single`) so that dedup keeps
//! recognising files either engine produced.
//!
//! Crash safety: a track is streamed to `<final>.part`, tagged *in place*,
//! `sync_all`'d, atomically renamed to the final name, and the parent directory
//! is fsynced. A killed process can therefore only ever leave a `.part`, never a
//! truncated or untagged final file; the next attempt purges stale `*.part` and
//! `*.tmp` files before it starts.
//!
//! Tags (ID3v2.4, what mutagen writes by default): TIT2, TPE1 (the track artist,
//! else the album artist), TALB, TPE2, TRCK (`n/total`), TDRC (the release date),
//! TCON (the release tags joined with `,` in ONE frame, as bandcamp-dl's
//! `--embed-genres` does), a comment holding the release URL, and an APIC
//! front-cover (`image/jpeg`, description `Cover`). The cover is embedded only;
//! no `cover.jpg` is left in the album folder (bandcamp-dl removes it too).
//!
//! With a format set (`DownloadSpec::format`) and a cookie, a release the cookie's owner bought
//! comes from their collection in that format instead ([`super::owned`]); anything else still
//! comes from the stream, and the outcome says why.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::{Accessor, ItemKey, TagExt};
use lofty::probe::Probe;
use lofty::tag::{Tag, TagItem, TagType};
use parking_lot::Mutex;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::bcdl::is_downloadable;
use super::owned::{self, Layout, Purchases};
use super::slug::{self, TrackMeta};
use super::{DownloadSpec, Downloader, Outcome, OutcomeKind, Progress, ProgressFn};
use crate::error::HarvestError;
use crate::extract::{HarvestedRelease, HarvestedTrack};
use crate::net::BandcampClient;
use crate::{sources, urls};

/// Largest cover we will embed.
const MAX_ART_BYTES: usize = 16 * 1024 * 1024;
const PARTIAL_STREAM_DETAIL: &str = "Only part of this release is publicly streamable; the rest is download-only on Bandcamp.";
const NONE_STREAMABLE_DETAIL: &str = "None of this release is publicly streamable.";

pub struct NativeDownloader {
    client: BandcampClient,
    template: String,
    purchases: Purchases,
}

impl NativeDownloader {
    /// `client` is the shared client (pages through its token bucket, audio
    /// through `client.cdn_client()`); `template` the default layout.
    pub fn new(client: BandcampClient, template: impl Into<String>) -> Self {
        Self { client, template: template.into(), purchases: Purchases::default() }
    }
}

/// How the owned-quality attempt ended.
enum OwnedRun {
    /// It ran: this is the item's outcome (success or a real failure).
    Done(Outcome),
    /// It does not apply (not a purchase, no cookie, nothing offered yet): use the stream.
    Skip(String),
}

/// A failure while fetching one track.
#[derive(Debug)]
enum TrackError {
    /// Transport, HTTP status or truncated body: retryable.
    Network(String),
    /// Local filesystem problem: not fixed by retrying.
    Io(String),
}

impl From<std::io::Error> for TrackError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// What the run has done so far; read by the cancel/timeout paths after the
/// download future has been dropped (which is exactly what a kill looks like).
#[derive(Default)]
struct State {
    new_files: Vec<PathBuf>,
    present: u32,
    current_part: Option<PathBuf>,
}

impl State {
    fn finished(&self) -> u32 {
        self.new_files.len() as u32 + self.present
    }
}

struct Planned {
    index: u32,
    track: HarvestedTrack,
    meta: TrackMeta,
    dest: PathBuf,
    stream_url: String,
}

fn outcome(kind: OutcomeKind, retryable: bool, detail: impl Into<String>) -> Outcome {
    Outcome { kind, new_files: Vec::new(), tracks_expected: None, availability: None, tracks_finished: 0, retryable, detail: detail.into() }
}

#[async_trait]
impl Downloader for NativeDownloader {
    fn name(&self) -> &'static str {
        "native"
    }

    async fn download(&self, spec: &DownloadSpec, progress: ProgressFn<'_>, cancel: &CancellationToken) -> Outcome {
        let state = Arc::new(Mutex::new(State::default()));
        let template = spec.template.clone().unwrap_or_else(|| self.template.clone());

        enum Ended {
            Done(Outcome),
            Cancelled,
            TimedOut,
        }
        let ended = {
            let work = self.run(spec, &template, progress, &state);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Ended::Cancelled,
                _ = tokio::time::sleep(spec.timeout) => Ended::TimedOut,
                o = work => Ended::Done(o),
            }
        };
        match ended {
            Ended::Done(o) => o,
            Ended::Cancelled | Ended::TimedOut => {
                // The future is gone, as if the process had been killed; the only
                // thing it can have left is the `.part`, which we remove now
                // (a hard kill leaves it for the next attempt's purge instead).
                let (part, new_files, finished) = {
                    let st = state.lock();
                    (st.current_part.clone(), st.new_files.clone(), st.finished())
                };
                if let Some(p) = part {
                    let _ = std::fs::remove_file(&p);
                }
                if matches!(ended, Ended::TimedOut) {
                    Outcome {
                        kind: OutcomeKind::Timeout,
                        new_files,
                        tracks_expected: None,
                        availability: None,
                        tracks_finished: finished,
                        retryable: true,
                        detail: format!("Timed out after {:.0}s.", spec.timeout.as_secs_f64()),
                    }
                } else {
                    Outcome {
                        kind: OutcomeKind::Crash,
                        new_files,
                        tracks_expected: None,
                        availability: None,
                        tracks_finished: finished,
                        retryable: false,
                        detail: "Cancelled.".into(),
                    }
                }
            }
        }
    }
}

impl NativeDownloader {
    async fn run(&self, spec: &DownloadSpec, template: &str, progress: ProgressFn<'_>, state: &Mutex<State>) -> Outcome {
        if !is_downloadable(&spec.url) {
            return outcome(
                OutcomeKind::NoOutput,
                false,
                format!("Cannot download {}: not an album or track page.", spec.url),
            );
        }

        let release = match sources::fetch_release(&self.client, &spec.url).await {
            Ok(r) => r,
            Err(e) => return outcome_for_fetch_error(&e),
        };
        let Some(format) = spec.format.as_deref() else {
            return self.run_stream(spec, template, &release, progress, state).await;
        };
        let why = match self.run_owned(spec, template, format, &release, &mut *progress, state).await {
            OwnedRun::Done(o) => return o,
            OwnedRun::Skip(why) => why,
        };
        info!("native: {} not downloaded in {format}: {why}", spec.url);
        let mut o = self.run_stream(spec, template, &release, progress, state).await;
        if o.ok() {
            o.detail = format!("{} {} was not used ({why}), so this is the public stream.", o.detail, owned::label_of(format));
        }
        o
    }

    /// The purchase in `format` (or the best format offered), unpacked into the stream layout.
    async fn run_owned(
        &self,
        spec: &DownloadSpec,
        template: &str,
        format: &str,
        release: &HarvestedRelease,
        progress: ProgressFn<'_>,
        state: &Mutex<State>,
    ) -> OwnedRun {
        if !self.client.has_cookie() {
            return OwnedRun::Skip("no Bandcamp cookie is set".into());
        }
        let link = match self.purchases.link(&self.client, &release.url).await {
            Ok(Some(l)) => l,
            Ok(None) => return OwnedRun::Skip("not in your collection".into()),
            Err(e) => return OwnedRun::Skip(format!("your collection could not be read: {e}")),
        };
        let resolved = match owned::resolve(&self.client, &link, format).await {
            Ok(r) => r,
            Err(e) => return OwnedRun::Skip(e.to_string()),
        };
        let layout = match Layout::new(release, template, &spec.base_dir) {
            Ok(l) => l,
            Err(e) => return OwnedRun::Done(outcome(OutcomeKind::Crash, false, format!("Refusing to write outside the download directory: {e}"))),
        };
        if let Err(e) = tokio::fs::create_dir_all(&spec.base_dir).await {
            return OwnedRun::Done(finish_err(state, release, TrackError::Io(e.to_string())));
        }
        purge_partials(&spec.base_dir);
        let total = release.tracks.len() as u32;
        let label = owned::label_of(&resolved.format);
        let mut emit = |phase: &str, fraction: f64| {
            progress(Progress {
                track_index: 1,
                track_total: total,
                phase: phase.to_string(),
                track_name: format!("{} ({label})", release.title),
                fraction,
            })
        };
        emit("Downloading", 0.0);
        let archive = spec.base_dir.join(".owned-download.part");
        state.lock().current_part = Some(archive.clone());
        if let Err(e) = self.fetch_track(&resolved.url, &archive, |f| emit("Downloading", f * 0.95)).await {
            let _ = tokio::fs::remove_file(&archive).await;
            state.lock().current_part = None;
            warn!("native: owned download of {} failed: {e:?}", spec.url);
            return OwnedRun::Done(finish_err(state, release, e));
        }
        emit("Encoding", 0.95);
        let (a, fmt) = (archive.clone(), resolved.format.clone());
        let unpacked = tokio::task::spawn_blocking(move || owned::unpack(&a, &fmt, &layout, existing_is_complete)).await;
        let _ = tokio::fs::remove_file(&archive).await;
        let unpacked = match unpacked {
            Ok(Ok(u)) => u,
            Ok(Err(e)) => {
                state.lock().current_part = None;
                return OwnedRun::Done(finish_err(state, release, TrackError::Io(format!("unpacking the download failed: {e}"))));
            }
            Err(e) => {
                state.lock().current_part = None;
                return OwnedRun::Done(finish_err(state, release, TrackError::Io(format!("unpack task failed: {e}"))));
            }
        };
        let (new, present) = {
            let mut st = state.lock();
            st.current_part = None;
            st.new_files.extend(unpacked.new_files.iter().cloned());
            st.present += unpacked.present;
            (st.new_files.clone(), st.finished())
        };
        emit("Finished", 1.0);
        let mut o = Outcome {
            tracks_expected: Some(total),
            availability: Some(release.availability()),
            tracks_finished: present,
            new_files: new,
            ..outcome(OutcomeKind::Ok, false, "")
        };
        if o.new_files.is_empty() && unpacked.present == 0 {
            o.kind = OutcomeKind::NoOutput;
            o.detail = format!("The {label} download held no audio files.");
        } else if o.new_files.is_empty() {
            o.kind = OutcomeKind::AlreadyHave;
            o.detail = format!("Already downloaded: {} file(s) were already present and complete.", unpacked.present);
        } else {
            o.detail = format!("Downloaded {} file(s) in {label} from your collection.", o.new_files.len());
        }
        info!("native: {} -> {:?} ({label})", spec.url, o.kind);
        OwnedRun::Done(o)
    }

    /// The release's public streams, one MP3 per streamable track.
    async fn run_stream(
        &self,
        spec: &DownloadSpec,
        template: &str,
        release: &HarvestedRelease,
        progress: ProgressFn<'_>,
        state: &Mutex<State>,
    ) -> Outcome {
        let total_tracks = release.tracks.len() as u32;

        let mut plan = Vec::new();
        for (i, t) in release.tracks.iter().enumerate() {
            let Some(stream_url) = t.stream_url.clone().filter(|u| !u.trim().is_empty()) else { continue };
            let meta = track_meta(release, t);
            let rel = {
                let mut p = slug::expand_template(template, &meta).into_os_string();
                p.push(".mp3");
                PathBuf::from(p)
            };
            let dest = match bc_core::paths::safe_join(&spec.base_dir, &rel) {
                Ok(p) => p,
                Err(e) => {
                    return outcome(OutcomeKind::Crash, false, format!("Refusing to write outside the download directory: {e}"));
                }
            };
            plan.push(Planned { index: i as u32 + 1, track: t.clone(), meta, dest, stream_url });
        }

        let mut result = Outcome { tracks_expected: Some(total_tracks), availability: Some(release.availability()), ..outcome(OutcomeKind::Ok, false, "") };
        if plan.is_empty() {
            result.kind = OutcomeKind::NoOutput;
            result.detail = NONE_STREAMABLE_DETAIL.into();
            return result;
        }
        let partial_stream = plan.len() < release.tracks.len();

        // Purge stale partials in every directory we will touch, before any attempt.
        let mut dirs: Vec<PathBuf> = plan.iter().filter_map(|p| p.dest.parent().map(Path::to_path_buf)).collect();
        dirs.sort();
        dirs.dedup();
        for d in &dirs {
            purge_partials(d);
        }

        let n = plan.len() as f64;
        let mut art: Option<Option<Arc<Vec<u8>>>> = None; // fetched lazily, once
        for (pos, p) in plan.iter().enumerate() {
            let name = p.dest.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let emit = |progress: &mut (dyn FnMut(Progress) + Send), phase: &str, within: f64| {
                progress(Progress {
                    track_index: p.index,
                    track_total: total_tracks,
                    phase: phase.to_string(),
                    track_name: name.clone(),
                    fraction: ((pos as f64 + within) / n).clamp(0.0, 1.0),
                });
            };

            if existing_is_complete(&p.dest) {
                debug!("native: {} already present", p.dest.display());
                state.lock().present += 1;
                emit(&mut *progress, "Finished", 1.0);
                continue;
            }
            if let Some(dir) = p.dest.parent() {
                if let Err(e) = tokio::fs::create_dir_all(dir).await {
                    return finish_err(state, release, TrackError::Io(e.to_string()));
                }
            }
            // A final file that exists but is unreadable is replaced (atomic rename).
            if art.is_none() {
                art = Some(self.fetch_art(release).await.map(Arc::new));
            }
            let cover = art.clone().flatten();

            emit(&mut *progress, "Downloading", 0.0);
            let part = part_path(&p.dest);
            state.lock().current_part = Some(part.clone());
            let res = self
                .fetch_track(&p.stream_url, &part, |frac| emit(&mut *progress, "Downloading", frac * 0.999))
                .await;
            let res = match res {
                Ok(()) => finalize(release, p, total_tracks, &part, cover).await,
                Err(e) => Err(e),
            };
            match res {
                Ok(()) => {
                    {
                        let mut st = state.lock();
                        st.current_part = None;
                        st.new_files.push(p.dest.clone());
                    }
                    emit(&mut *progress, "Finished", 1.0);
                }
                Err(e) => {
                    let _ = tokio::fs::remove_file(&part).await;
                    state.lock().current_part = None;
                    warn!("native: track {} of {} failed: {e:?}", p.index, spec.url);
                    return finish_err(state, release, e);
                }
            }
        }

        let st = state.lock();
        result.new_files = st.new_files.clone();
        result.tracks_finished = st.finished();
        if st.new_files.is_empty() {
            result.kind = OutcomeKind::AlreadyHave;
            result.detail = if partial_stream {
                PARTIAL_STREAM_DETAIL.to_string()
            } else {
                format!("Already downloaded: {} file(s) were already present and complete.", st.present)
            };
        } else {
            result.detail = if partial_stream {
                PARTIAL_STREAM_DETAIL.to_string()
            } else {
                format!("Downloaded {} file(s).", st.new_files.len())
            };
        }
        info!("native: {} -> {:?}", spec.url, result.kind);
        result
    }

    /// Stream `url` to `part`, verifying the byte count, and `sync_all` it.
    async fn fetch_track(&self, url: &str, part: &Path, mut on_fraction: impl FnMut(f64)) -> Result<(), TrackError> {
        let parsed = url::Url::parse(url).map_err(|e| TrackError::Network(format!("bad stream URL: {e}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(TrackError::Network(format!("unsupported stream URL scheme: {}", parsed.scheme())));
        }
        // The cdn client carries no cookie and no limiter by construction.
        let mut resp = self
            .client
            .cdn_client()
            .get(parsed)
            .send()
            .await
            .map_err(|e| TrackError::Network(format!("request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(TrackError::Network(format!("stream URL answered HTTP {}", status.as_u16())));
        }
        let encoded = resp.headers().contains_key(reqwest::header::CONTENT_ENCODING);
        let expected = if encoded { None } else { resp.content_length() };
        let mut file = tokio::fs::File::create(part).await?;
        let mut written: u64 = 0;
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    file.write_all(&chunk).await?;
                    written += chunk.len() as u64;
                    if let Some(total) = expected.filter(|t| *t > 0) {
                        on_fraction((written as f64 / total as f64).min(1.0));
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(TrackError::Network(format!("body error after {written} bytes: {e}"))),
            }
        }
        file.flush().await?;
        if let Some(total) = expected {
            if written != total {
                return Err(TrackError::Network(format!("incomplete body: got {written} of {total} bytes")));
            }
        }
        if written == 0 {
            return Err(TrackError::Network("empty body".into()));
        }
        file.sync_all().await?;
        Ok(())
    }

    /// Cover art, once per release. Any failure just means "no art".
    async fn fetch_art(&self, release: &HarvestedRelease) -> Option<Vec<u8>> {
        let url = release.art_url.clone().or_else(|| urls::build_art_url(release.art_id))?;
        let go = async {
            let resp = self.client.cdn_client().get(&url).send().await.ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let bytes = resp.bytes().await.ok()?;
            (!bytes.is_empty() && bytes.len() <= MAX_ART_BYTES).then(|| bytes.to_vec())
        };
        let out = go.await;
        if out.is_none() {
            warn!("native: couldn't download album art from {url}; continuing without it");
        }
        out
    }
}

fn finish_err(state: &Mutex<State>, release: &HarvestedRelease, e: TrackError) -> Outcome {
    let st = state.lock();
    let (kind, retryable, detail) = match e {
        TrackError::Network(m) => (OutcomeKind::Partial, true, format!("Download interrupted: {m}")),
        TrackError::Io(m) => (OutcomeKind::Crash, false, format!("Filesystem error: {m}")),
    };
    Outcome {
        kind,
        new_files: st.new_files.clone(),
        tracks_expected: Some(release.tracks.len() as u32),
        availability: Some(release.availability()),
        tracks_finished: st.finished(),
        retryable,
        detail,
    }
}

fn outcome_for_fetch_error(e: &HarvestError) -> Outcome {
    match e {
        HarvestError::Other(m) if m.starts_with("not found") => {
            outcome(OutcomeKind::NotFound, false, "Album or track not found on Bandcamp (HTTP 404).")
        }
        HarvestError::IdentityExpired(m) => outcome(OutcomeKind::Crash, false, m.clone()),
        HarvestError::Cancelled => outcome(OutcomeKind::Crash, false, "Cancelled."),
        HarvestError::Io(_) | HarvestError::Db(_) => outcome(OutcomeKind::Crash, false, e.to_string()),
        HarvestError::Extraction(m) => outcome(OutcomeKind::NoOutput, true, m.clone()),
        other => outcome(OutcomeKind::Network, true, other.to_string()),
    }
}

pub(super) fn track_meta(release: &HarvestedRelease, t: &HarvestedTrack) -> TrackMeta {
    let artist = t.artist.clone().filter(|a| !a.is_empty());
    let title = slug::strip_artist_prefix(&t.title, artist.as_deref());
    // bandcamp-dl: `str(track['track_num'])`, "None" (-> "Single") when absent.
    let track = t.track_num.filter(|n| *n > 0).map(|n| n as u32);
    TrackMeta {
        artist,
        albumartist: release.artist_name.clone(),
        album: release.title.clone(),
        title,
        track,
        date: release.release_date.as_deref().map(|d| d.chars().take(4).collect()).unwrap_or_default(),
        label: release.label_name.clone().unwrap_or_default(),
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

/// Remove `*.part` and `*.tmp` directly inside `dir`.
fn purge_partials(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if (name.ends_with(".part") || name.ends_with(".tmp")) && e.path().is_file() {
            if std::fs::remove_file(e.path()).is_ok() {
                info!("native: purged stale {}", e.path().display());
            }
        }
    }
}

/// Conservative completeness check: non-empty and lofty can parse it.
fn existing_is_complete(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() && m.len() > 0 => {}
        _ => return false,
    }
    Probe::open(path).and_then(|p| p.read()).is_ok()
}

fn build_tag(release: &HarvestedRelease, p: &Planned, total_tracks: u32, cover: Option<&[u8]>) -> Tag {
    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title(p.meta.title.clone());
    let artist = p.meta.artist.clone().unwrap_or_else(|| release.artist_name.clone());
    tag.set_artist(artist);
    tag.set_album(release.title.clone());
    tag.insert_text(ItemKey::AlbumArtist, release.artist_name.clone());
    // bandcamp-dl writes track "1" for singles.
    tag.set_track(p.track.track_num.filter(|n| *n > 0).unwrap_or(1) as u32);
    tag.set_track_total(total_tracks.max(1));
    if let Some(d) = release.release_date.clone().filter(|d| !d.is_empty()) {
        tag.insert_text(ItemKey::RecordingDate, d);
    }
    if !release.tags.is_empty() {
        tag.insert_text(ItemKey::Genre, release.tags.join(","));
    }
    if let Some(l) = release.label_name.clone().filter(|l| !l.is_empty()) {
        tag.insert_text(ItemKey::Label, l);
    }
    tag.push(TagItem::new(ItemKey::Comment, lofty::tag::ItemValue::Text(release.url.clone())));
    if let Some(bytes) = cover {
        let mime = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) { MimeType::Png } else { MimeType::Jpeg };
        tag.push_picture(Picture::unchecked(bytes.to_vec()).pic_type(PictureType::CoverFront).mime_type(mime).description("Cover").build());
    }
    tag
}

/// Tag the `.part`, fsync, atomically rename, fsync the directory.
async fn finalize(
    release: &HarvestedRelease,
    p: &Planned,
    total_tracks: u32,
    part: &Path,
    cover: Option<Arc<Vec<u8>>>,
) -> Result<(), TrackError> {
    let tag = build_tag(release, p, total_tracks, cover.as_deref().map(Vec::as_slice));
    let part = part.to_path_buf();
    let dest = p.dest.clone();
    tokio::task::spawn_blocking(move || -> Result<(), TrackError> {
        tag.save_to_path(&part, WriteOptions::default())
            .map_err(|e| TrackError::Io(format!("tagging failed: {e}")))?;
        std::fs::File::open(&part)?.sync_all()?;
        std::fs::rename(&part, &dest)?;
        if let Some(dir) = dest.parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| TrackError::Io(format!("finalize task failed: {e}")))?
}
