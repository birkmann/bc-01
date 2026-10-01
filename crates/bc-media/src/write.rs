//! DJ-field write-back: fill empty tags, and only empty ones.
//!
//! Port of `services/metadata/{dj_fields,gapfill,atomic}.py` and the writer halves of
//! `mappers.py`.
//!
//! The rule this module exists to enforce: a value already in the file wins. BPM and key from
//! Mixed In Key, Rekordbox or the artist are left exactly as they are; our analysis only fills a
//! blank. The unit of "empty" is the *logical field*, not the frame (`read_tags` already
//! coalesces `TBPM`/`TXXX:BPM`), so when a field is a gap we write *all* of its frames for that
//! format and when it is not we touch none of them.
//!
//! Every write is **atomic**: the file is copied to `.<name>.<random>.bctmp` in the same
//! directory, the copy is edited with lofty, fsynced and renamed over the original, then the
//! directory is fsynced. The original is never opened for writing, so a crash leaves it
//! byte-identical; the temp file is removed on any error. Unlike the Python port there is no
//! in-place "auto" fast path (lofty offers no refuse-to-grow probe), see
//! [`WriteMode`].

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use lofty::TextEncoding;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::id3::v2::{
    ExtendedTextFrame, Frame, FrameId, Id3v2Tag, Id3v2Version, TextInformationFrame,
};
use lofty::iff::aiff::AiffFile;
use lofty::iff::wav::WavFile;
use lofty::mp4::{Atom, AtomData, AtomIdent, DataType, Ilst, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::{OpusFile, VorbisFile};

use crate::camelot::{key_name, to_camelot};
use crate::error::{MediaError, Result};
use crate::tags::{
    Container, FREEFORM_MEAN, TrackTags, container_for, read_tags, to_float, to_int,
};

/// Scratch-file marker. Absent from the audio extension list *and* preceded by a dot in the
/// file name, so the scanner skips an orphan on both counts.
pub const TMP_MARKER: &str = ".bctmp";

// ---------------------------------------------------------------------------
// Field model (dj_fields.py)
// ---------------------------------------------------------------------------

/// A group of logical fields the caller can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldGroup {
    Bpm,
    Key,
    Camelot,
    Energy,
    ReplayGain,
}

impl FieldGroup {
    /// Parse the API spelling (`bpm|key|camelot|energy|replaygain`).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bpm" => Some(Self::Bpm),
            "key" => Some(Self::Key),
            "camelot" => Some(Self::Camelot),
            "energy" => Some(Self::Energy),
            "replaygain" => Some(Self::ReplayGain),
            _ => None,
        }
    }

    /// Logical field names this group covers.
    pub fn fields(self) -> &'static [&'static str] {
        match self {
            Self::Bpm => &["bpm"],
            Self::Key => &["initial_key"],
            Self::Camelot => &["camelot"],
            Self::Energy => &["energy"],
            Self::ReplayGain => &["replaygain_track_gain", "replaygain_track_peak"],
        }
    }
}

/// Every group.
pub const ALL_GROUPS: [FieldGroup; 5] = [
    FieldGroup::Bpm,
    FieldGroup::Key,
    FieldGroup::Camelot,
    FieldGroup::Energy,
    FieldGroup::ReplayGain,
];

/// The default excludes energy. `Analysis.energy` is an RMS proxy that saturates at 1.0, so on
/// modern loud masters it piles up at level 10 -- ask for it explicitly.
pub const DEFAULT_GROUPS: [FieldGroup; 4] = [
    FieldGroup::Bpm,
    FieldGroup::Key,
    FieldGroup::Camelot,
    FieldGroup::ReplayGain,
];

/// Logical fields in the order a plan reports them (`TrackTags` field names).
pub const FIELD_NAMES: [&str; 6] = [
    "bpm",
    "initial_key",
    "camelot",
    "energy",
    "replaygain_track_gain",
    "replaygain_track_peak",
];

/// Union of the field names of `groups`.
pub fn fields_for(groups: &[FieldGroup]) -> Vec<&'static str> {
    FIELD_NAMES
        .iter()
        .copied()
        .filter(|n| groups.iter().any(|g| g.fields().contains(n)))
        .collect()
}

/// Proposed tag values. `None` means "no value to offer for this field".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DjFields {
    pub bpm: Option<f64>,
    pub initial_key: Option<String>,
    pub camelot: Option<String>,
    /// 1-10.
    pub energy: Option<i32>,
    /// dB.
    pub replaygain_track_gain: Option<f64>,
    /// Linear amplitude.
    pub replaygain_track_peak: Option<f64>,
}

impl DjFields {
    /// True when no field has a value.
    pub fn is_empty(&self) -> bool {
        self.bpm.is_none()
            && self.initial_key.is_none()
            && self.camelot.is_none()
            && self.energy.is_none()
            && self.replaygain_track_gain.is_none()
            && self.replaygain_track_peak.is_none()
    }

    /// A copy holding just `names` (logical field names).
    pub fn only(&self, names: &[&str]) -> DjFields {
        let keep = |n: &str| names.contains(&n);
        DjFields {
            bpm: self.bpm.filter(|_| keep("bpm")),
            initial_key: self.initial_key.clone().filter(|_| keep("initial_key")),
            camelot: self.camelot.clone().filter(|_| keep("camelot")),
            energy: self.energy.filter(|_| keep("energy")),
            replaygain_track_gain: self
                .replaygain_track_gain
                .filter(|_| keep("replaygain_track_gain")),
            replaygain_track_peak: self
                .replaygain_track_peak
                .filter(|_| keep("replaygain_track_peak")),
        }
    }

    /// The value of one logical field rendered the way it reads in a tag.
    pub fn rendered(&self, name: &str) -> Option<String> {
        match name {
            "bpm" => self.bpm.map(|v| format!("{v:.2}")),
            "initial_key" => self.initial_key.clone(),
            "camelot" => self.camelot.clone(),
            "energy" => self.energy.map(|v| v.to_string()),
            "replaygain_track_gain" => self.replaygain_track_gain.map(|v| format!("{v:+.2} dB")),
            "replaygain_track_peak" => self.replaygain_track_peak.map(|v| format!("{v:.6}")),
            _ => None,
        }
    }

    /// Parse a journal-rendered value back into `name`. Returns false for unknown names or
    /// unparsable values.
    pub fn set_from_rendered(&mut self, name: &str, text: &str) -> bool {
        match name {
            "bpm" => to_float(text).map(|v| self.bpm = Some(v)).is_some(),
            "initial_key" => {
                self.initial_key = Some(text.to_string());
                true
            }
            "camelot" => {
                self.camelot = Some(text.to_string());
                true
            }
            "energy" => to_int(text).map(|v| self.energy = Some(v)).is_some(),
            "replaygain_track_gain" => to_float(text)
                .map(|v| self.replaygain_track_gain = Some(v))
                .is_some(),
            "replaygain_track_peak" => to_float(text)
                .map(|v| self.replaygain_track_peak = Some(v))
                .is_some(),
            _ => false,
        }
    }
}

/// Plain analysis values handed to [`from_analysis`] (no DB types in this crate).
#[derive(Debug, Clone, Default)]
pub struct AnalysisInput {
    /// A failed analysis offers nothing.
    pub failed: bool,
    pub bpm: Option<f64>,
    pub bpm_confidence: Option<f64>,
    /// Pitch class 0..11, 0 = C.
    pub key_root: Option<i32>,
    /// `"major"` / `"minor"`.
    pub key_mode: Option<String>,
    /// Stored Camelot code, preferred over re-deriving.
    pub camelot: Option<String>,
    pub key_confidence: Option<f64>,
    /// Energy proxy 0..1.
    pub energy: Option<f64>,
    /// ReplayGain gain in dB.
    pub replaygain_gain: Option<f64>,
    /// True peak in dBTP.
    pub true_peak_db: Option<f64>,
}

/// 0-1 float to the 1-10 integer Mixed In Key / Serato / Rekordbox read.
///
/// Never 0: every tool that consumes `EnergyLevel` treats 0 as "unset", so a zero would
/// round-trip as a gap forever and be rewritten on every pass.
pub fn energy_level(value: Option<f64>) -> Option<i32> {
    let v = value?;
    if !v.is_finite() {
        return None;
    }
    Some(((v * 10.0) as i64 + 1).clamp(1, 10) as i32)
}

/// The conventional ReplayGain string, e.g. `-3.42 dB` (signed, like metaflac / foobar2000).
pub fn format_gain(db: Option<f64>) -> Option<String> {
    db.map(|v| format!("{v:+.2} dB"))
}

/// dBTP to the linear amplitude the tag convention stores. Deliberately not clamped to 1.0.
pub fn peak_linear(true_peak_db: Option<f64>) -> Option<f64> {
    true_peak_db.map(|db| 10f64.powf(db / 20.0))
}

fn round_to(v: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (v * f).round() / f
}

/// Turn analysis results into proposed tag values.
///
/// A below-threshold confidence suppresses the *whole* estimate rather than half of it.
pub fn from_analysis(
    row: &AnalysisInput,
    groups: &[FieldGroup],
    min_bpm_confidence: f64,
    min_key_confidence: f64,
) -> DjFields {
    if row.failed {
        return DjFields::default();
    }
    let wanted = fields_for(groups);
    let want = |n: &str| wanted.contains(&n);
    let mut out = DjFields::default();

    let bpm_ok = row.bpm_confidence.is_none_or(|c| c >= min_bpm_confidence);
    if want("bpm")
        && bpm_ok
        && let Some(b) = row.bpm.filter(|b| *b > 0.0)
    {
        out.bpm = Some(round_to(b, 2));
    }

    let key_ok = row.key_confidence.is_none_or(|c| c >= min_key_confidence);
    if key_ok {
        let mode = row.key_mode.as_deref();
        if want("initial_key") {
            out.initial_key = key_name(row.key_root, mode);
        }
        if want("camelot") {
            out.camelot = row
                .camelot
                .clone()
                .filter(|c| !c.is_empty())
                .or_else(|| to_camelot(row.key_root, mode).map(str::to_string));
        }
    }

    if want("energy") {
        out.energy = energy_level(row.energy);
    }
    if want("replaygain_track_gain") {
        out.replaygain_track_gain = row.replaygain_gain.map(|g| round_to(g, 2));
    }
    if want("replaygain_track_peak") {
        out.replaygain_track_peak = peak_linear(row.true_peak_db).map(|p| round_to(p, 6));
    }
    out
}

// ---------------------------------------------------------------------------
// Planning (gapfill.py)
// ---------------------------------------------------------------------------

/// Per-field classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldStatus {
    /// Empty in the file: written.
    Gap,
    /// Already agrees with the proposal.
    Kept,
    /// Disagrees and is still not written -- surfaced for review.
    Conflict,
    /// The analysis had nothing to say.
    NoSource,
}

impl FieldStatus {
    /// API spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gap => "gap",
            Self::Kept => "kept",
            Self::Conflict => "conflict",
            Self::NoSource => "no_source",
        }
    }
}

/// One row of a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldPlan {
    pub field: &'static str,
    pub status: FieldStatus,
    pub existing: Option<String>,
    pub proposed: Option<String>,
}

/// What a write would do to one file.
#[derive(Debug, Clone)]
pub struct TagWritePlan {
    pub path: PathBuf,
    /// False for containers without a writer.
    pub supported: bool,
    pub fields: Vec<FieldPlan>,
    /// Narrowed to the `Gap` fields only, so handing it to a writer cannot overwrite anything.
    pub values: DjFields,
}

impl TagWritePlan {
    /// Names of the fields that will be written.
    pub fn gaps(&self) -> Vec<&'static str> {
        self.names_with(FieldStatus::Gap)
    }
    /// Names of the fields that disagree with the file and are left alone.
    pub fn conflicts(&self) -> Vec<&'static str> {
        self.names_with(FieldStatus::Conflict)
    }
    fn names_with(&self, s: FieldStatus) -> Vec<&'static str> {
        self.fields
            .iter()
            .filter(|f| f.status == s)
            .map(|f| f.field)
            .collect()
    }
    /// Nothing would be written.
    pub fn is_noop(&self) -> bool {
        !self.supported || !self.fields.iter().any(|f| f.status == FieldStatus::Gap)
    }
}

/// Containers [`write_dj_fields`] can write: mp3, aiff, wav (ID3), flac, ogg, opus, m4a/mp4.
pub fn is_writable(path: &Path) -> bool {
    !matches!(container_for(path), Container::Other | Container::Aac)
}

/// Containers that cannot be resized in place (documentation only: every write copies anyway).
pub const COPY_ONLY_EXTENSIONS: [&str; 4] = [".ogg", ".opus", ".wav", ".aiff"];

const BPM_TOLERANCE: f64 = 0.01;
const GAIN_TOLERANCE: f64 = 0.05;
const PEAK_TOLERANCE: f64 = 0.001;

fn existing_f(t: &TrackTags, name: &str) -> Option<f64> {
    match name {
        "bpm" => t.bpm,
        "replaygain_track_gain" => t.replaygain_track_gain,
        "replaygain_track_peak" => t.replaygain_track_peak,
        "energy" => t.energy.map(f64::from),
        _ => None,
    }
}

fn existing_s(t: &TrackTags, name: &str) -> Option<String> {
    match name {
        "initial_key" => t.initial_key.clone(),
        "camelot" => t.camelot.clone(),
        _ => None,
    }
}

/// Render the file's current value for a field the way [`DjFields::rendered`] would.
fn existing_rendered(t: &TrackTags, name: &str) -> Option<String> {
    let d = DjFields {
        bpm: t.bpm,
        initial_key: t.initial_key.clone(),
        camelot: t.camelot.clone(),
        energy: t.energy,
        replaygain_track_gain: t.replaygain_track_gain,
        replaygain_track_peak: t.replaygain_track_peak,
    };
    d.rendered(name)
}

/// Empty, or a sentinel that means empty (BPM 0, energy 0, peak 0; but a gain of 0 or a
/// negative gain is a real value).
fn is_gap(t: &TrackTags, name: &str) -> bool {
    match name {
        "initial_key" | "camelot" => existing_s(t, name).is_none_or(|s| s.trim().is_empty()),
        "bpm" | "energy" | "replaygain_track_peak" => existing_f(t, name).is_none_or(|v| v <= 0.0),
        _ => existing_f(t, name).is_none(),
    }
}

fn agrees(t: &TrackTags, d: &DjFields, name: &str) -> bool {
    match name {
        "bpm" => match (t.bpm, d.bpm) {
            (Some(l), Some(r)) => r > 0.0 && (l - r).abs() / r <= BPM_TOLERANCE,
            _ => false,
        },
        "replaygain_track_gain" => {
            matches!((t.replaygain_track_gain, d.replaygain_track_gain), (Some(l), Some(r)) if (l - r).abs() <= GAIN_TOLERANCE)
        }
        "replaygain_track_peak" => {
            matches!((t.replaygain_track_peak, d.replaygain_track_peak), (Some(l), Some(r)) if (l - r).abs() <= PEAK_TOLERANCE)
        }
        "energy" => t.energy == d.energy,
        "initial_key" => {
            matches!((&t.initial_key, &d.initial_key), (Some(l), Some(r)) if l.trim().to_lowercase() == r.trim().to_lowercase())
        }
        "camelot" => {
            matches!((&t.camelot, &d.camelot), (Some(l), Some(r)) if l.trim().to_lowercase() == r.trim().to_lowercase())
        }
        _ => false,
    }
}

/// Classify every DJ field against already-read `tags`, and narrow the values to the gaps.
pub fn plan_gaps(path: &Path, tags: &TrackTags, values: &DjFields) -> TagWritePlan {
    if !is_writable(path) {
        return TagWritePlan {
            path: path.to_path_buf(),
            supported: false,
            fields: Vec::new(),
            values: DjFields::default(),
        };
    }
    let mut plans = Vec::with_capacity(FIELD_NAMES.len());
    let mut gaps: Vec<&str> = Vec::new();
    for name in FIELD_NAMES {
        let proposed = values.rendered(name);
        let existing = existing_rendered(tags, name);
        let plan = if proposed.is_none() {
            FieldPlan {
                field: name,
                status: FieldStatus::NoSource,
                existing,
                proposed: None,
            }
        } else if is_gap(tags, name) {
            gaps.push(name);
            FieldPlan {
                field: name,
                status: FieldStatus::Gap,
                existing: None,
                proposed,
            }
        } else if agrees(tags, values, name) {
            FieldPlan {
                field: name,
                status: FieldStatus::Kept,
                existing,
                proposed,
            }
        } else {
            FieldPlan {
                field: name,
                status: FieldStatus::Conflict,
                existing,
                proposed,
            }
        };
        plans.push(plan);
    }
    TagWritePlan {
        path: path.to_path_buf(),
        supported: true,
        fields: plans,
        values: values.only(&gaps),
    }
}

/// Read the file's tags and plan a write for the proposal restricted to `groups`.
pub fn plan_write(path: &Path, values: &DjFields, groups: &[FieldGroup]) -> TagWritePlan {
    let narrowed = values.only(&fields_for(groups));
    plan_gaps(path, &read_tags(path), &narrowed)
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// How a write ended (errors are returned as `Err`, see [`write_dj_fields`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteStatus {
    Written,
    /// Nothing to do (no gaps).
    Skipped,
    /// No writer for this container.
    Unsupported,
}

/// How the bytes reached the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    /// Copy, edit, fsync, rename.
    Copy,
    None,
}

/// Write mode. The Python port had `auto` (try an in-place save that refuses to grow the
/// tag block, then fall back to copy) and `atomic`. lofty cannot probe whether a tag fits the
/// padding, so both modes here take the atomic copy path; the enum is kept for API parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteMode {
    #[default]
    Auto,
    Atomic,
}

/// Result of a successful (or skipped) write, including what the caller must store in the same
/// DB transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteOutcome {
    pub status: WriteStatus,
    pub mode: WriteKind,
    /// Logical fields written.
    pub fields: Vec<String>,
    /// Frame / atom keys touched (what the undo journal records).
    pub frames: Vec<String>,
    /// New file mtime in ns since the epoch (only when written).
    pub mtime_ns: Option<i64>,
    /// New file size in bytes (only when written).
    pub size_bytes: Option<u64>,
    /// New `TrackTags::tag_hash` (only when written).
    pub tag_hash: Option<String>,
}

impl WriteOutcome {
    fn none(status: WriteStatus) -> Self {
        Self {
            status,
            mode: WriteKind::None,
            fields: vec![],
            frames: vec![],
            mtime_ns: None,
            size_bytes: None,
            tag_hash: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Atomic copy-edit-rename (atomic.py)
// ---------------------------------------------------------------------------

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_tmp(path: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tag = (nanos as u64)
        ^ (u64::from(std::process::id()) << 40)
        ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{tag:016x}{TMP_MARKER}"))
}

struct TmpGuard(Option<PathBuf>);
impl Drop for TmpGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = fs::remove_file(p);
        }
    }
}

fn fsync_dir(dir: &Path) -> std::io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Run `edit` on a scratch twin of `path`; on success the twin *becomes* `path`.
///
/// The twin is created beside the original (same filesystem, so the rename is atomic), with the
/// original's permissions. On any error the twin is removed and the original is untouched.
/// Symlinks are resolved first so the link itself is preserved.
pub fn atomic_edit<T>(path: &Path, edit: impl FnOnce(&Path) -> Result<T>) -> Result<T> {
    let real = fs::canonicalize(path)?;
    let meta = fs::metadata(&real)?;
    if meta.permissions().readonly() {
        return Err(MediaError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is read-only", real.display()),
        )));
    }
    let tmp = unique_tmp(&real);
    let mut guard = TmpGuard(Some(tmp.clone()));
    fs::copy(&real, &tmp)?;
    let value = edit(&tmp)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&tmp)?
        .sync_all()?;
    fs::rename(&tmp, &real)?;
    guard.0 = None;
    if let Some(parent) = real.parent()
        && let Err(e) = fsync_dir(parent)
    {
        tracing::warn!(dir = %parent.display(), error = %e, "directory fsync failed");
    }
    Ok(value)
}

/// Delete orphaned `.bctmp` scratch files under `root`. Returns how many were removed.
///
/// A killed process leaves one behind. They are invisible to the scanner, so this is
/// housekeeping. `older_than` guards against a write still running in another process.
pub fn purge_tag_tmp(root: &Path, older_than: Option<std::time::Duration>) -> usize {
    fn walk(dir: &Path, older_than: Option<std::time::Duration>, removed: &mut usize) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                walk(&path, older_than, removed);
            } else if ft.is_file() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !(name.starts_with('.') && name.contains(TMP_MARKER)) {
                    continue;
                }
                if let Some(min_age) = older_than {
                    let young = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|m| m.elapsed().ok())
                        .is_some_and(|age| age < min_age);
                    if young {
                        continue;
                    }
                }
                if fs::remove_file(&path).is_ok() {
                    *removed += 1;
                }
            }
        }
    }
    let mut removed = 0;
    walk(root, older_than, &mut removed);
    if removed > 0 {
        tracing::info!(removed, root = %root.display(), "purged orphaned tag temp files");
    }
    removed
}

// ---------------------------------------------------------------------------
// Per-format apply / clear
// ---------------------------------------------------------------------------

const ID3_TXXX_BPM: &str = "BPM";
const ID3_TXXX_KEY: &str = "INITIALKEY";
const ID3_TXXX_CAMELOT: &str = "CAMELOT";
// "EnergyLevel", not "ENERGYLEVEL": Mixed In Key's own spelling.
const ID3_TXXX_ENERGY: &str = "EnergyLevel";
const ID3_TXXX_GAIN: &str = "REPLAYGAIN_TRACK_GAIN";
const ID3_TXXX_PEAK: &str = "REPLAYGAIN_TRACK_PEAK";

fn txxx_desc(field: &str) -> Option<&'static str> {
    Some(match field {
        "bpm" => ID3_TXXX_BPM,
        "initial_key" => ID3_TXXX_KEY,
        "camelot" => ID3_TXXX_CAMELOT,
        "energy" => ID3_TXXX_ENERGY,
        "replaygain_track_gain" => ID3_TXXX_GAIN,
        "replaygain_track_peak" => ID3_TXXX_PEAK,
        _ => return None,
    })
}

fn vorbis_key(field: &str) -> Option<&'static str> {
    Some(match field {
        "bpm" => "BPM",
        "initial_key" => "INITIALKEY",
        "camelot" => "CAMELOT",
        "energy" => "ENERGYLEVEL",
        "replaygain_track_gain" => "REPLAYGAIN_TRACK_GAIN",
        "replaygain_track_peak" => "REPLAYGAIN_TRACK_PEAK",
        _ => return None,
    })
}

fn mp4_freeform_name(field: &str) -> Option<&'static str> {
    Some(match field {
        "bpm" => "BPM",
        "initial_key" => "initialkey",
        "camelot" => "camelot",
        "energy" => "energylevel",
        "replaygain_track_gain" => "replaygain_track_gain",
        "replaygain_track_peak" => "replaygain_track_peak",
        _ => return None,
    })
}

/// ID3v2.4 4.2.3: ground keys A-G, `b`/`#` for half keys, `m` for minor, or `o`.
fn is_tkey(s: &str) -> bool {
    if s == "o" {
        return true;
    }
    let b = s.as_bytes();
    let mut i = 0;
    if b.is_empty() || !(b'A'..=b'G').contains(&b[0]) {
        return false;
    }
    i += 1;
    if i < b.len() && (b[i] == b'b' || b[i] == b'#') {
        i += 1;
    }
    if i < b.len() && b[i] == b'm' {
        i += 1;
    }
    i == b.len()
}

fn text_frame(id: &'static str, value: String) -> Frame<'static> {
    Frame::Text(TextInformationFrame::new(
        FrameId::Valid(Cow::Borrowed(id)),
        TextEncoding::UTF8,
        value,
    ))
}

/// Replace `TXXX:<desc>`, matching the descriptor case-insensitively so a file already carrying
/// `TXXX:bpm` does not end up with two BPM descriptors.
fn set_txxx(tag: &mut Id3v2Tag, desc: &str, value: String) -> String {
    tag.retain(|f| !matches!(f, Frame::UserText(u) if u.description.eq_ignore_ascii_case(desc)));
    tag.insert(Frame::UserText(ExtendedTextFrame::new(
        TextEncoding::UTF8,
        desc.to_string(),
        value,
    )));
    format!("TXXX:{desc}")
}

fn remove_txxx(tag: &mut Id3v2Tag, desc: &str) -> Vec<String> {
    let mut removed = Vec::new();
    tag.retain(|f| match f {
        Frame::UserText(u) if u.description.eq_ignore_ascii_case(desc) => {
            removed.push(format!("TXXX:{}", u.description));
            false
        }
        _ => true,
    });
    removed
}

fn remove_text_frame(tag: &mut Id3v2Tag, id: &'static str) -> bool {
    tag.remove(&FrameId::Valid(Cow::Borrowed(id))).count() > 0
}

fn id3_apply(tag: &mut Id3v2Tag, v: &DjFields) -> Vec<String> {
    let mut touched = Vec::new();
    if let Some(bpm) = v.bpm {
        // TBPM is integer-only by spec; the exact decimal goes alongside in TXXX:BPM.
        tag.insert(text_frame(
            "TBPM",
            format!("{}", bpm.round_ties_even() as i64),
        ));
        touched.push("TBPM".to_string());
        touched.push(set_txxx(tag, ID3_TXXX_BPM, format!("{bpm:.2}")));
    }
    if let Some(key) = &v.initial_key {
        if is_tkey(key) {
            tag.insert(text_frame("TKEY", key.clone()));
            touched.push("TKEY".to_string());
        }
        touched.push(set_txxx(tag, ID3_TXXX_KEY, key.clone()));
    }
    if let Some(c) = &v.camelot {
        touched.push(set_txxx(tag, ID3_TXXX_CAMELOT, c.clone()));
    }
    if let Some(e) = v.energy {
        touched.push(set_txxx(tag, ID3_TXXX_ENERGY, e.to_string()));
    }
    if let Some(g) = v.replaygain_track_gain {
        touched.push(set_txxx(tag, ID3_TXXX_GAIN, format!("{g:+.2} dB")));
    }
    if let Some(p) = v.replaygain_track_peak {
        touched.push(set_txxx(tag, ID3_TXXX_PEAK, format!("{p:.6}")));
    }
    touched
}

fn id3_clear(tag: &mut Id3v2Tag, fields: &[String]) -> Vec<String> {
    let mut removed = Vec::new();
    for f in fields {
        let text = match f.as_str() {
            "bpm" => Some("TBPM"),
            "initial_key" => Some("TKEY"),
            _ => None,
        };
        if let Some(id) = text
            && remove_text_frame(tag, id)
        {
            removed.push(id.to_string());
        }
        if let Some(desc) = txxx_desc(f) {
            removed.extend(remove_txxx(tag, desc));
        }
    }
    removed
}

fn vorbis_apply(vc: &mut VorbisComments, v: &DjFields) -> Vec<String> {
    let mut touched = Vec::new();
    let mut put = |key: &str, value: String| {
        // `insert` removes any case-variant of the key, so duplicates are impossible.
        vc.insert(key.to_string(), value);
        touched.push(key.to_string());
    };
    if let Some(b) = v.bpm {
        put("BPM", format!("{b:.2}"));
    }
    if let Some(k) = &v.initial_key {
        put("INITIALKEY", k.clone());
    }
    if let Some(c) = &v.camelot {
        put("CAMELOT", c.clone());
    }
    if let Some(e) = v.energy {
        put("ENERGYLEVEL", e.to_string());
    }
    if let Some(g) = v.replaygain_track_gain {
        put("REPLAYGAIN_TRACK_GAIN", format!("{g:+.2} dB"));
    }
    if let Some(p) = v.replaygain_track_peak {
        put("REPLAYGAIN_TRACK_PEAK", format!("{p:.6}"));
    }
    touched
}

fn vorbis_clear(vc: &mut VorbisComments, fields: &[String]) -> Vec<String> {
    let mut removed = Vec::new();
    for f in fields {
        if let Some(key) = vorbis_key(f) {
            // `VorbisComments::remove` swaps items around; rebuild to keep the other
            // (multi-value genre ...) items in their original order.
            let items: Vec<(String, String)> = vc.take_items().collect();
            let mut hit = false;
            for (k, v) in items {
                if k.eq_ignore_ascii_case(key) {
                    hit = true;
                } else {
                    vc.push(k, v);
                }
            }
            if hit {
                removed.push(key.to_string());
            }
        }
    }
    removed
}

fn freeform_ident(name: &str) -> AtomIdent<'static> {
    AtomIdent::Freeform {
        mean: Cow::Borrowed(FREEFORM_MEAN),
        name: Cow::Owned(name.to_string()),
    }
}

fn freeform_key(name: &str) -> String {
    format!("----:{FREEFORM_MEAN}:{name}")
}

fn mp4_apply(ilst: &mut Ilst, v: &DjFields) -> Vec<String> {
    let touched = std::cell::RefCell::new(Vec::<String>::new());
    let ff = |ilst: &mut Ilst, field: &str, value: String| {
        let Some(name) = mp4_freeform_name(field) else {
            return;
        };
        // Freeform names are case-sensitive in the file; drop case variants first.
        ilst.retain(|a| {
            !matches!(a.ident(), AtomIdent::Freeform { mean, name: n } if mean == FREEFORM_MEAN && n.eq_ignore_ascii_case(name))
        });
        ilst.insert(Atom::new(freeform_ident(name), AtomData::UTF8(value)));
        touched.borrow_mut().push(freeform_key(name));
    };
    if let Some(b) = v.bpm {
        // tmpo is a 16-bit integer atom; the exact decimal goes freeform.
        let n = (b.round_ties_even() as i64).clamp(0, 65535) as u16;
        ilst.replace_atom(Atom::new(
            AtomIdent::Fourcc(*b"tmpo"),
            AtomData::Unknown {
                code: DataType::BeSignedInteger,
                data: n.to_be_bytes().to_vec(),
            },
        ));
        touched.borrow_mut().push("tmpo".to_string());
        ff(ilst, "bpm", format!("{b:.2}"));
    }
    if let Some(k) = &v.initial_key {
        ff(ilst, "initial_key", k.clone());
    }
    if let Some(c) = &v.camelot {
        ff(ilst, "camelot", c.clone());
    }
    if let Some(e) = v.energy {
        ff(ilst, "energy", e.to_string());
    }
    if let Some(g) = v.replaygain_track_gain {
        ff(ilst, "replaygain_track_gain", format!("{g:+.2} dB"));
    }
    if let Some(p) = v.replaygain_track_peak {
        ff(ilst, "replaygain_track_peak", format!("{p:.6}"));
    }
    touched.into_inner()
}

fn mp4_clear(ilst: &mut Ilst, fields: &[String]) -> Vec<String> {
    let mut removed = Vec::new();
    for f in fields {
        if f == "bpm" && ilst.remove(&AtomIdent::Fourcc(*b"tmpo")).count() > 0 {
            removed.push("tmpo".to_string());
        }
        if let Some(name) = mp4_freeform_name(f) {
            let mut hit = false;
            ilst.retain(|a| {
                let m = matches!(a.ident(), AtomIdent::Freeform { mean, name: n } if mean == FREEFORM_MEAN && n.eq_ignore_ascii_case(name));
                hit |= m;
                !m
            });
            if hit {
                removed.push(freeform_key(name));
            }
        }
    }
    removed
}

fn write_opts(id3_v23: bool) -> WriteOptions {
    let mut o = WriteOptions::new();
    if id3_v23 {
        o.use_id3v23(true);
    }
    o
}

/// What to do to a file's tag.
enum Edit<'a> {
    Apply(&'a DjFields),
    Clear(&'a [String]),
}

macro_rules! id3_file_edit {
    ($ty:ty, $path:expr, $edit:expr) => {{
        let mut r = BufReader::new(File::open($path)?);
        let mut f = <$ty>::read_from(&mut r, ParseOptions::new())?;
        drop(r);
        if f.id3v2().is_none() {
            if let Edit::Clear(_) = $edit {
                return Ok(Vec::new());
            }
            f.set_id3v2(Id3v2Tag::new());
        }
        let Some(tag) = f.id3v2_mut() else {
            return Ok(Vec::new());
        };
        // Keep the major version the file already uses: rewriting a v2.3 tag as v2.4 is
        // unreadable to some hardware players.
        let v23 = tag.original_version() == Id3v2Version::V3;
        let touched = match $edit {
            Edit::Apply(v) => id3_apply(tag, v),
            Edit::Clear(fields) => id3_clear(tag, fields),
        };
        if !touched.is_empty() {
            f.save_to_path($path, write_opts(v23))?;
        }
        touched
    }};
}

/// Edit the DJ fields of `path` *in place* on whatever file is given. Callers go through
/// [`atomic_edit`]; this is never used on the user's original.
fn edit_file(path: &Path, container: Container, edit: Edit<'_>) -> Result<Vec<String>> {
    let opts = ParseOptions::new();
    Ok(match container {
        Container::Mpeg => id3_file_edit!(MpegFile, path, edit),
        Container::Aiff => id3_file_edit!(AiffFile, path, edit),
        Container::Wav => id3_file_edit!(WavFile, path, edit),
        Container::Flac => {
            let mut r = BufReader::new(File::open(path)?);
            let mut f = FlacFile::read_from(&mut r, opts)?;
            drop(r);
            if f.vorbis_comments().is_none() {
                if let Edit::Clear(_) = edit {
                    return Ok(Vec::new());
                }
                f.set_vorbis_comments(VorbisComments::new());
            }
            let Some(vc) = f.vorbis_comments_mut() else {
                return Ok(Vec::new());
            };
            let touched = match edit {
                Edit::Apply(v) => vorbis_apply(vc, v),
                Edit::Clear(fs) => vorbis_clear(vc, fs),
            };
            if !touched.is_empty() {
                f.save_to_path(path, WriteOptions::new())?;
            }
            touched
        }
        Container::Vorbis => {
            let mut r = BufReader::new(File::open(path)?);
            let mut f = VorbisFile::read_from(&mut r, opts)?;
            drop(r);
            let touched = match edit {
                Edit::Apply(v) => vorbis_apply(f.vorbis_comments_mut(), v),
                Edit::Clear(fs) => vorbis_clear(f.vorbis_comments_mut(), fs),
            };
            if !touched.is_empty() {
                f.save_to_path(path, WriteOptions::new())?;
            }
            touched
        }
        Container::Opus => {
            let mut r = BufReader::new(File::open(path)?);
            let mut f = OpusFile::read_from(&mut r, opts)?;
            drop(r);
            let touched = match edit {
                Edit::Apply(v) => vorbis_apply(f.vorbis_comments_mut(), v),
                Edit::Clear(fs) => vorbis_clear(f.vorbis_comments_mut(), fs),
            };
            if !touched.is_empty() {
                f.save_to_path(path, WriteOptions::new())?;
            }
            touched
        }
        Container::Mp4 => {
            let mut r = BufReader::new(File::open(path)?);
            let mut f = Mp4File::read_from(&mut r, opts)?;
            drop(r);
            if f.ilst().is_none() {
                if let Edit::Clear(_) = edit {
                    return Ok(Vec::new());
                }
                f.set_ilst(Ilst::new());
            }
            let Some(ilst) = f.ilst_mut() else {
                return Ok(Vec::new());
            };
            let touched = match edit {
                Edit::Apply(v) => mp4_apply(ilst, v),
                Edit::Clear(fs) => mp4_clear(ilst, fs),
            };
            if !touched.is_empty() {
                f.save_to_path(path, WriteOptions::new())?;
            }
            touched
        }
        Container::Aac | Container::Other => {
            return Err(MediaError::Unsupported(format!(
                "no writer for {}",
                path.display()
            )));
        }
    })
}

fn mtime_ns(meta: &fs::Metadata) -> Option<i64> {
    let d = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(d.as_nanos()).ok()
}

fn finish(path: &Path, fields: Vec<String>, frames: Vec<String>) -> Result<WriteOutcome> {
    let meta = fs::metadata(path)?;
    Ok(WriteOutcome {
        status: WriteStatus::Written,
        mode: WriteKind::Copy,
        fields,
        frames,
        mtime_ns: mtime_ns(&meta),
        size_bytes: Some(meta.len()),
        tag_hash: Some(read_tags(path).tag_hash()),
    })
}

/// Apply a plan to its file, atomically.
///
/// Returns `Ok` with status `Skipped` (no gaps) or `Unsupported` (no writer for the container)
/// without touching the file. On success the outcome carries the new `(mtime_ns, size_bytes,
/// tag_hash)` for the caller to store in the same DB transaction. Any failure (read-only file,
/// parse/encode error, I/O) is an `Err`, the original is byte-identical and the temp file is
/// gone.
pub fn write_dj_fields(path: &Path, plan: &TagWritePlan, mode: WriteMode) -> Result<WriteOutcome> {
    let _ = mode; // both modes are atomic, see WriteMode
    if !plan.supported {
        return Ok(WriteOutcome::none(WriteStatus::Unsupported));
    }
    if plan.is_noop() {
        return Ok(WriteOutcome::none(WriteStatus::Skipped));
    }
    let gaps: Vec<String> = plan.gaps().into_iter().map(str::to_string).collect();
    let container = container_for(path);
    let frames = atomic_edit(path, |tmp| {
        edit_file(tmp, container, Edit::Apply(&plan.values))
    })?;
    finish(path, gaps, frames)
}

/// Convenience: plan against `groups` then write.
pub fn fill_dj_fields(
    path: &Path,
    values: &DjFields,
    groups: &[FieldGroup],
) -> Result<(TagWritePlan, WriteOutcome)> {
    let plan = plan_write(path, values, groups);
    let out = write_dj_fields(path, &plan, WriteMode::Auto)?;
    Ok((plan, out))
}

/// Read the current DJ fields of a file (undo journal / diff support).
pub fn read_dj_fields(path: &Path) -> DjFields {
    let t = read_tags(path);
    DjFields {
        bpm: t.bpm,
        initial_key: t.initial_key,
        camelot: t.camelot,
        energy: t.energy,
        replaygain_track_gain: t.replaygain_track_gain,
        replaygain_track_peak: t.replaygain_track_peak,
    }
}

/// Atomically remove the given logical fields (`bpm`, `initial_key`, ...) from the file.
///
/// Used by the undo journal. Returns `Skipped` when none of the fields were present.
pub fn remove_dj_fields(path: &Path, fields: &[String]) -> Result<WriteOutcome> {
    if !is_writable(path) {
        return Ok(WriteOutcome::none(WriteStatus::Unsupported));
    }
    // Probe first on the original (read-only) so a no-op does not rewrite the file.
    let mut present = false;
    {
        let cur = read_dj_fields(path);
        for f in fields {
            present |= cur.rendered(f).is_some();
        }
    }
    if !present {
        return Ok(WriteOutcome::none(WriteStatus::Skipped));
    }
    let container = container_for(path);
    let frames = atomic_edit(path, |tmp| edit_file(tmp, container, Edit::Clear(fields)))?;
    if frames.is_empty() {
        return Ok(WriteOutcome::none(WriteStatus::Skipped));
    }
    finish(path, fields.to_vec(), frames)
}

/// Result of [`undo_fields`].
#[derive(Debug, Clone)]
pub struct UndoResult {
    /// Fields that were removed.
    pub removed: Vec<String>,
    /// The write that removed them (None when nothing was removable).
    pub outcome: Option<WriteOutcome>,
}

/// Remove fields this app wrote, unless they have since been edited.
///
/// `written` maps field name to the rendered value recorded when we wrote it. A field whose
/// current value no longer matches was changed by the user or another tool after the fact, so it
/// is left alone and not reported as removed.
pub fn undo_fields(
    path: &Path,
    fields: &[String],
    written: &BTreeMap<String, String>,
) -> Result<UndoResult> {
    if !is_writable(path) {
        return Ok(UndoResult {
            removed: vec![],
            outcome: None,
        });
    }
    let tags = read_tags(path);
    let mut removable: Vec<String> = Vec::new();
    for name in fields {
        let Some(text) = written.get(name) else {
            continue;
        };
        let mut recorded = DjFields::default();
        if !recorded.set_from_rendered(name, text) {
            continue;
        }
        if !FIELD_NAMES.contains(&name.as_str()) || existing_rendered(&tags, name).is_none() {
            continue;
        }
        if agrees(&tags, &recorded, name) {
            removable.push(name.clone());
        }
    }
    if removable.is_empty() {
        return Ok(UndoResult {
            removed: vec![],
            outcome: None,
        });
    }
    let outcome = remove_dj_fields(path, &removable)?;
    let removed = if outcome.status == WriteStatus::Written {
        removable
    } else {
        vec![]
    };
    Ok(UndoResult {
        removed,
        outcome: Some(outcome),
    })
}

#[cfg(test)]
mod tests {
    //! Port of `test_dj_fields.py`: analysis values to tag values, no files involved.
    use super::*;

    #[test]
    fn energy_level_maps_to_deciles() {
        for (value, expected) in [
            (0.0, 1),
            (0.05, 1),
            (0.1, 2),
            (0.55, 6),
            (0.99, 10),
            (1.0, 10),
            (-0.2, 1),
            (1.4, 10),
        ] {
            assert_eq!(energy_level(Some(value)), Some(expected), "{value}");
        }
    }

    #[test]
    fn energy_level_is_never_zero_and_never_decreases() {
        let levels: Vec<i32> = (0..=100)
            .map(|i| energy_level(Some(f64::from(i) / 100.0)).unwrap())
            .collect();
        assert!(levels.iter().all(|l| (1..=10).contains(l)));
        assert!(levels.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn energy_level_passes_none_through() {
        assert_eq!(energy_level(None), None);
    }

    #[test]
    fn format_gain_matches_the_encoder_convention() {
        assert_eq!(format_gain(Some(-3.4239)).as_deref(), Some("-3.42 dB"));
        assert_eq!(format_gain(Some(2.0)).as_deref(), Some("+2.00 dB"));
        assert_eq!(format_gain(Some(0.0)).as_deref(), Some("+0.00 dB"));
        assert_eq!(format_gain(None), None);
    }

    #[test]
    fn format_gain_round_trips_through_the_reader() {
        for db in [-12.5, -3.42, 0.0, 2.0] {
            let rendered = format_gain(Some(db)).unwrap();
            let parsed = to_float(&rendered).unwrap();
            assert!(
                (parsed - db).abs() < 0.005,
                "{db} -> {rendered} -> {parsed}"
            );
        }
    }

    #[test]
    fn peak_linear_converts_dbtp() {
        for (dbtp, expected) in [
            (0.0, 1.0),
            (-0.5, 0.944061),
            (-6.0, 0.501187),
            (1.2, 1.148154),
        ] {
            let v = peak_linear(Some(dbtp)).unwrap();
            assert!((v - expected).abs() < 1e-6, "{dbtp}: {v}");
        }
    }

    #[test]
    fn peak_above_full_scale_is_not_clamped() {
        assert!(peak_linear(Some(1.2)).unwrap() > 1.0);
    }

    fn row() -> AnalysisInput {
        AnalysisInput::default()
    }

    #[test]
    fn from_analysis_derives_key_and_camelot_together() {
        let r = AnalysisInput {
            bpm: Some(128.004),
            key_root: Some(9),
            key_mode: Some("minor".into()),
            camelot: Some("8A".into()),
            true_peak_db: Some(-0.5),
            ..row()
        };
        let v = from_analysis(&r, &ALL_GROUPS, 0.0, 0.0);
        assert_eq!(v.bpm, Some(128.0));
        assert_eq!(v.initial_key.as_deref(), Some("Am"));
        assert_eq!(v.camelot.as_deref(), Some("8A"));
        assert!((v.replaygain_track_peak.unwrap() - 0.944061).abs() < 1e-6);
    }

    #[test]
    fn from_analysis_rederives_a_missing_camelot() {
        let r = AnalysisInput {
            key_root: Some(9),
            key_mode: Some("minor".into()),
            ..row()
        };
        assert_eq!(
            from_analysis(&r, &ALL_GROUPS, 0.0, 0.0).camelot.as_deref(),
            Some("8A")
        );
    }

    #[test]
    fn low_key_confidence_suppresses_key_but_not_bpm() {
        let r = AnalysisInput {
            bpm: Some(124.0),
            key_root: Some(9),
            key_mode: Some("minor".into()),
            camelot: Some("8A".into()),
            key_confidence: Some(0.2),
            ..row()
        };
        let v = from_analysis(&r, &ALL_GROUPS, 0.0, 0.5);
        assert_eq!(v.bpm, Some(124.0));
        assert_eq!(v.initial_key, None);
        assert_eq!(v.camelot, None);
    }

    #[test]
    fn a_failed_analysis_offers_nothing() {
        let r = AnalysisInput {
            failed: true,
            bpm: Some(128.0),
            key_root: Some(9),
            key_mode: Some("minor".into()),
            ..row()
        };
        assert!(from_analysis(&r, &ALL_GROUPS, 0.0, 0.0).is_empty());
    }

    #[test]
    fn groups_narrow_what_is_offered() {
        let r = AnalysisInput {
            bpm: Some(128.0),
            key_root: Some(9),
            key_mode: Some("minor".into()),
            camelot: Some("8A".into()),
            energy: Some(0.4),
            ..row()
        };
        let v = from_analysis(&r, &[FieldGroup::Bpm], 0.0, 0.0);
        assert_eq!(v.bpm, Some(128.0));
        assert_eq!(v.initial_key, None);
        assert_eq!(v.camelot, None);
        assert_eq!(v.energy, None);
    }

    #[test]
    fn energy_is_off_by_default() {
        let r = AnalysisInput {
            energy: Some(0.9),
            bpm: Some(120.0),
            ..row()
        };
        assert_eq!(from_analysis(&r, &DEFAULT_GROUPS, 0.0, 0.0).energy, None);
        assert!(!DEFAULT_GROUPS.contains(&FieldGroup::Energy));
        assert_eq!(
            from_analysis(&r, &[FieldGroup::Energy], 0.0, 0.0).energy,
            Some(10)
        );
    }

    #[test]
    fn only_keeps_just_the_named_fields() {
        let v = DjFields {
            bpm: Some(128.0),
            initial_key: Some("Am".into()),
            camelot: Some("8A".into()),
            ..Default::default()
        };
        let n = v.only(&["bpm"]);
        assert_eq!(n.bpm, Some(128.0));
        assert_eq!(n.initial_key, None);
        assert_eq!(n.camelot, None);
    }

    #[test]
    fn group_parsing_and_fields() {
        assert_eq!(
            FieldGroup::parse("replaygain"),
            Some(FieldGroup::ReplayGain)
        );
        assert_eq!(FieldGroup::parse("nope"), None);
        assert_eq!(
            fields_for(&DEFAULT_GROUPS),
            vec![
                "bpm",
                "initial_key",
                "camelot",
                "replaygain_track_gain",
                "replaygain_track_peak"
            ]
        );
    }

    #[test]
    fn tkey_validation() {
        for ok in ["Am", "C", "F#", "Bb", "Dbm", "o"] {
            assert!(is_tkey(ok), "{ok}");
        }
        for bad in ["", "H", "A major", "8A", "am", "A#bm"] {
            assert!(!is_tkey(bad), "{bad}");
        }
    }

    #[test]
    fn rendered_round_trip() {
        let mut d = DjFields::default();
        assert!(d.set_from_rendered("replaygain_track_gain", "-3.42 dB"));
        assert!(d.set_from_rendered("bpm", "128.02"));
        assert!(d.set_from_rendered("energy", "7"));
        assert!(!d.set_from_rendered("energy", "x"));
        assert!(!d.set_from_rendered("bogus", "1"));
        assert_eq!(
            d.rendered("replaygain_track_gain").as_deref(),
            Some("-3.42 dB")
        );
        assert_eq!(d.rendered("bpm").as_deref(), Some("128.02"));
    }
}
