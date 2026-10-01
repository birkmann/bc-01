//! Canonical tag model and per-container readers.
//!
//! Port of `services/metadata/{model,mappers,service}.py`. Dispatch is on file extension, one
//! reader per container family:
//!
//! | Field      | ID3v2.4                    | Vorbis       | MP4                     |
//! |------------|----------------------------|--------------|-------------------------|
//! | BPM        | TBPM (int) + TXXX:BPM      | BPM          | tmpo (int) + FF BPM     |
//! | Key        | TKEY, TXXX:INITIALKEY      | INITIALKEY   | FF initialkey           |
//! | Camelot    | TXXX:CAMELOT               | CAMELOT      | FF camelot              |
//! | Energy     | TXXX:EnergyLevel           | ENERGYLEVEL  | FF energylevel          |
//! | ReplayGain | TXXX:REPLAYGAIN_TRACK_GAIN | REPLAYGAIN_* | FF replaygain_*         |
//! | Label      | TPUB                       | LABEL        | FF LABEL                |
//!
//! `FF` is the MP4 freeform prefix `----:com.apple.iTunes:`.
//!
//! [`read_tags`] opens each file exactly once and never fails: an unreadable file yields a
//! record whose title is the file stem so it still shows up in the library.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::id3::v2::{Frame, FrameId, Id3v2Tag};
use lofty::iff::aiff::AiffFile;
use lofty::iff::wav::WavFile;
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::{OggPictureStorage, OpusFile, VorbisFile};
use lofty::picture::{Picture, PictureType};
use lofty::prelude::Accessor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::Result;

/// Extensions the scanner treats as audio (lower case, with leading dot).
pub const AUDIO_EXTENSIONS: [&str; 10] = [
    ".mp3", ".flac", ".m4a", ".mp4", ".aac", ".ogg", ".opus", ".wav", ".aiff", ".wma",
];

/// Sidecar cover file stems, in priority order.
pub const COVER_STEMS: [&str; 5] = ["cover", "folder", "front", "album", "artwork"];
/// Sidecar cover extensions, in priority order.
pub const COVER_EXTENSIONS: [&str; 5] = [".jpg", ".jpeg", ".png", ".gif", ".webp"];

/// True when `path` has a known audio extension (case-insensitive).
pub fn is_audio_path(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let dotted = format!(".{}", ext.to_ascii_lowercase());
            AUDIO_EXTENSIONS.contains(&dotted.as_str())
        }
        None => false,
    }
}

/// One track's tags, normalised across ID3 / Vorbis / MP4.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub date: Option<String>,
    pub genres: Vec<String>,
    pub track_no: Option<i32>,
    pub track_total: Option<i32>,
    pub disc_no: Option<i32>,
    pub label: Option<String>,
    pub comment: Option<String>,
    pub isrc: Option<String>,

    // DJ fields.
    pub bpm: Option<f64>,
    pub initial_key: Option<String>,
    pub camelot: Option<String>,
    /// Mixed In Key style energy, 1-10.
    pub energy: Option<i32>,
    /// ReplayGain track gain in dB.
    pub replaygain_track_gain: Option<f64>,
    /// ReplayGain track peak, linear amplitude.
    pub replaygain_track_peak: Option<f64>,

    // Read-only technical properties.
    pub duration_ms: Option<i64>,
    /// Lower-case file extension without dot (`mp3`, `flac` ...).
    pub codec: Option<String>,
    /// Overall bitrate in bits per second.
    pub bitrate: Option<i32>,
    pub sample_rate: Option<i32>,
    pub channels: Option<i32>,
    pub has_art: bool,
}

#[derive(Serialize)]
struct HashView<'a> {
    title: &'a Option<String>,
    artist: &'a Option<String>,
    album_artist: &'a Option<String>,
    album: &'a Option<String>,
    date: &'a Option<String>,
    genres: &'a Vec<String>,
    track_no: &'a Option<i32>,
    track_total: &'a Option<i32>,
    disc_no: &'a Option<i32>,
    label: &'a Option<String>,
    comment: &'a Option<String>,
    isrc: &'a Option<String>,
    bpm: &'a Option<f64>,
    initial_key: &'a Option<String>,
    camelot: &'a Option<String>,
    energy: &'a Option<i32>,
    replaygain_track_gain: &'a Option<f64>,
    replaygain_track_peak: &'a Option<f64>,
}

impl TrackTags {
    /// First 4-digit year in `date`, which may be a bare year or ISO.
    pub fn year(&self) -> Option<i32> {
        let date = self.date.as_deref()?.trim();
        let head: String = date.chars().take(4).collect();
        if head.len() == 4 && head.bytes().all(|b| b.is_ascii_digit()) {
            head.parse().ok()
        } else {
            None
        }
    }

    /// Stable sha256 (hex) of the *editable* tags. Technical properties are excluded: they
    /// follow the audio, not the tags. The format intentionally differs from the Python one.
    pub fn tag_hash(&self) -> String {
        let view = HashView {
            title: &self.title,
            artist: &self.artist,
            album_artist: &self.album_artist,
            album: &self.album,
            date: &self.date,
            genres: &self.genres,
            track_no: &self.track_no,
            track_total: &self.track_total,
            disc_no: &self.disc_no,
            label: &self.label,
            comment: &self.comment,
            isrc: &self.isrc,
            bpm: &self.bpm,
            initial_key: &self.initial_key,
            camelot: &self.camelot,
            energy: &self.energy,
            replaygain_track_gain: &self.replaygain_track_gain,
            replaygain_track_peak: &self.replaygain_track_peak,
        };
        let blob = serde_json::to_vec(&view).unwrap_or_default();
        hex(&Sha256::digest(&blob))
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Embedded or sidecar cover image bytes plus MIME type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artwork {
    pub data: Vec<u8>,
    pub mime: String,
}

impl Artwork {
    /// File extension (with dot) for the MIME type; `.jpg` when unknown.
    pub fn ext(&self) -> &'static str {
        match self.mime.as_str() {
            "image/png" => ".png",
            "image/gif" => ".gif",
            "image/webp" => ".webp",
            _ => ".jpg",
        }
    }
}

// ---------------------------------------------------------------------------
// small value helpers (ports of _first / _to_int / _to_float)
// ---------------------------------------------------------------------------

/// `Some(s)` unless `s` is empty or blank.
fn non_empty(s: &str) -> Option<String> {
    if s.trim().is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// First value of an ID3v2.4 multi-value (NUL separated) string.
fn first_value(s: &str) -> &str {
    s.split('\0').next().unwrap_or("")
}

/// `"3/12"` -> 3. `None` if the head is not an integer.
pub(crate) fn to_int(text: &str) -> Option<i32> {
    let head = text.split('/').next()?.trim();
    head.parse::<i32>().ok()
}

/// Parse a float, tolerating a `dB` suffix (`"-3.42 dB"`). Non-finite values are rejected.
pub(crate) fn to_float(text: &str) -> Option<f64> {
    let cleaned = text.replace("dB", "").replace("db", "");
    let v: f64 = cleaned.trim().parse().ok()?;
    v.is_finite().then_some(v)
}

/// The "or" fallback of the Python reader: a parsed 0.0 counts as missing.
fn truthy(v: Option<f64>) -> Option<f64> {
    v.filter(|x| *x != 0.0)
}

// ---------------------------------------------------------------------------
// container dispatch
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Container {
    Mpeg,
    Aiff,
    Wav,
    Flac,
    Vorbis,
    Opus,
    Mp4,
    Aac,
    Other,
}

pub(crate) fn extension_lower(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default()
}

pub(crate) fn container_for(path: &Path) -> Container {
    match extension_lower(path).as_str() {
        "mp3" => Container::Mpeg,
        "aiff" | "aif" => Container::Aiff,
        "wav" => Container::Wav,
        "flac" => Container::Flac,
        "ogg" => Container::Vorbis,
        "opus" => Container::Opus,
        "m4a" | "mp4" => Container::Mp4,
        "aac" => Container::Aac,
        _ => Container::Other,
    }
}

fn open_reader(path: &Path) -> Result<BufReader<File>> {
    Ok(BufReader::with_capacity(16 * 1024, File::open(path)?))
}

fn set_props(
    t: &mut TrackTags,
    duration: std::time::Duration,
    kbps: Option<u32>,
    rate: Option<u32>,
    ch: Option<u32>,
) {
    let ms = duration.as_millis();
    t.duration_ms = if ms > 0 { i64::try_from(ms).ok() } else { None };
    t.bitrate = kbps
        .filter(|b| *b > 0)
        .map(|b| (b as i32).saturating_mul(1000));
    t.sample_rate = rate.filter(|r| *r > 0).map(|r| r as i32);
    t.channels = ch.filter(|c| *c > 0).map(|c| c as i32);
}

/// Read one file's tags. Never fails: an unreadable file yields a title-only record
/// (title = file stem, codec from the extension) so it still appears in the library.
pub fn read_tags(path: &Path) -> TrackTags {
    let mut tags = match read_inner(path) {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "tag read failed");
            TrackTags::default()
        }
    };
    let ext = extension_lower(path);
    if tags.codec.is_none() && !ext.is_empty() {
        tags.codec = Some(ext);
    }
    if tags.title.as_deref().is_none_or(|t| t.trim().is_empty()) {
        tags.title = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string);
    }
    tags
}

fn read_inner(path: &Path) -> Result<TrackTags> {
    let opts = ParseOptions::new();
    let container = container_for(path);
    let mut t = TrackTags {
        codec: Some(extension_lower(path)).filter(|e| !e.is_empty()),
        ..TrackTags::default()
    };
    match container {
        Container::Mpeg => {
            let mut r = open_reader(path)?;
            let f = MpegFile::read_from(&mut r, opts)?;
            let p = f.properties();
            set_props(
                &mut t,
                p.duration(),
                Some(p.overall_bitrate()),
                Some(p.sample_rate()),
                Some(u32::from(p.channels())),
            );
            if let Some(tag) = f.id3v2() {
                from_id3(&mut t, tag);
            }
        }
        Container::Aiff => {
            let mut r = open_reader(path)?;
            let f = AiffFile::read_from(&mut r, opts)?;
            let p = f.properties();
            set_props(
                &mut t,
                p.duration(),
                Some(p.overall_bitrate()),
                Some(p.sample_rate()),
                Some(u32::from(p.channels())),
            );
            if let Some(tag) = f.id3v2() {
                from_id3(&mut t, tag);
            }
        }
        Container::Wav => {
            let mut r = open_reader(path)?;
            let f = WavFile::read_from(&mut r, opts)?;
            let p = f.properties();
            set_props(
                &mut t,
                p.duration(),
                Some(p.overall_bitrate()),
                Some(p.sample_rate()),
                Some(u32::from(p.channels())),
            );
            if let Some(tag) = f.id3v2() {
                from_id3(&mut t, tag);
            }
        }
        Container::Aac => {
            let mut r = open_reader(path)?;
            let f = lofty::aac::AacFile::read_from(&mut r, opts)?;
            let p = f.properties();
            set_props(
                &mut t,
                p.duration(),
                Some(p.overall_bitrate()),
                Some(p.sample_rate()),
                Some(u32::from(p.channels())),
            );
            if let Some(tag) = f.id3v2() {
                from_id3(&mut t, tag);
            }
        }
        Container::Flac => {
            let mut r = open_reader(path)?;
            let f = FlacFile::read_from(&mut r, opts)?;
            let p = f.properties();
            set_props(
                &mut t,
                p.duration(),
                Some(p.overall_bitrate()),
                Some(p.sample_rate()),
                Some(u32::from(p.channels())),
            );
            if let Some(vc) = f.vorbis_comments() {
                from_vorbis(&mut t, vc);
            }
            t.has_art = !f.pictures().is_empty();
        }
        Container::Vorbis => {
            let mut r = open_reader(path)?;
            let f = VorbisFile::read_from(&mut r, opts)?;
            let p = f.properties();
            set_props(
                &mut t,
                p.duration(),
                Some(p.overall_bitrate()),
                Some(p.sample_rate()),
                Some(u32::from(p.channels())),
            );
            from_vorbis(&mut t, f.vorbis_comments());
            t.has_art = !f.vorbis_comments().pictures().is_empty();
        }
        Container::Opus => {
            let mut r = open_reader(path)?;
            let f = OpusFile::read_from(&mut r, opts)?;
            let p = f.properties();
            set_props(
                &mut t,
                p.duration(),
                Some(p.overall_bitrate()),
                Some(48_000),
                Some(u32::from(p.channels())),
            );
            from_vorbis(&mut t, f.vorbis_comments());
            t.has_art = !f.vorbis_comments().pictures().is_empty();
        }
        Container::Mp4 => {
            let mut r = open_reader(path)?;
            let f = Mp4File::read_from(&mut r, opts)?;
            let p = f.properties();
            set_props(
                &mut t,
                p.duration(),
                p.overall_bitrate(),
                p.sample_rate(),
                p.channels().map(u32::from),
            );
            if let Some(ilst) = f.ilst() {
                from_mp4(&mut t, ilst);
            }
        }
        Container::Other => generic_read(path, &mut t)?,
    }
    Ok(t)
}

/// Best-effort for containers without a dedicated reader: probe by content.
fn generic_read(path: &Path, t: &mut TrackTags) -> Result<()> {
    use lofty::file::TaggedFileExt;
    let probe = lofty::probe::Probe::open(path)?.guess_file_type()?;
    let tagged = probe.read()?;
    let p = tagged.properties();
    set_props(
        t,
        p.duration(),
        p.overall_bitrate(),
        p.sample_rate(),
        p.channels().map(u32::from),
    );
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        t.title = tag.title().and_then(|s| non_empty(&s));
        t.artist = tag.artist().and_then(|s| non_empty(&s));
        t.album = tag.album().and_then(|s| non_empty(&s));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ID3v2
// ---------------------------------------------------------------------------

fn fid(id: &'static str) -> FrameId<'static> {
    FrameId::Valid(Cow::Borrowed(id))
}

fn id3_text(tag: &Id3v2Tag, id: &'static str) -> Option<String> {
    tag.get_text(&fid(id))
        .and_then(|s| non_empty(first_value(s)))
}

fn from_id3(t: &mut TrackTags, tag: &Id3v2Tag) {
    t.title = id3_text(tag, "TIT2");
    t.artist = id3_text(tag, "TPE1");
    t.album_artist = id3_text(tag, "TPE2");
    t.album = id3_text(tag, "TALB");
    t.date = match tag.get(&fid("TDRC")) {
        Some(Frame::Timestamp(ts)) => Some(ts.timestamp.to_string()),
        Some(Frame::Text(tf)) => non_empty(first_value(&tf.value)),
        _ => None,
    }
    .or_else(|| id3_text(tag, "TYER"));
    t.label = id3_text(tag, "TPUB");
    t.isrc = id3_text(tag, "TSRC");

    if let Some(genres) = tag.genres() {
        t.genres = genres
            .filter(|g| !g.trim().is_empty())
            .map(str::to_string)
            .collect();
    }

    if let Some(trck) = id3_text(tag, "TRCK") {
        t.track_no = to_int(&trck);
        if let Some((_, total)) = trck.split_once('/') {
            t.track_total = to_int(total);
        }
    }
    t.disc_no = id3_text(tag, "TPOS").as_deref().and_then(to_int);
    t.bpm = id3_text(tag, "TBPM").as_deref().and_then(to_float);
    t.initial_key = id3_text(tag, "TKEY");

    let mut comment: Option<String> = None;
    for frame in tag.iter() {
        match frame {
            Frame::UserText(f) => {
                let desc = f.description.to_ascii_uppercase();
                let value = first_value(&f.content);
                match desc.as_str() {
                    // Preferred over TBPM, which the spec limits to an integer.
                    "BPM" | "TEMPO" => t.bpm = truthy(to_float(value)).or(t.bpm),
                    "INITIALKEY" if t.initial_key.is_none() => t.initial_key = non_empty(value),
                    "CAMELOT" => t.camelot = non_empty(value),
                    "ENERGYLEVEL" => t.energy = to_int(value),
                    "REPLAYGAIN_TRACK_GAIN" => t.replaygain_track_gain = to_float(value),
                    "REPLAYGAIN_TRACK_PEAK" => t.replaygain_track_peak = to_float(value),
                    _ => {}
                }
            }
            Frame::Comment(c) if comment.is_none() => {
                comment = non_empty(first_value(&c.content));
            }
            Frame::Picture(_) => t.has_art = true,
            _ => {}
        }
    }
    t.comment = comment;
}

// ---------------------------------------------------------------------------
// Vorbis comments
// ---------------------------------------------------------------------------

fn vget(vc: &VorbisComments, key: &str) -> Option<String> {
    vc.get_all(key).find_map(non_empty)
}

fn from_vorbis(t: &mut TrackTags, vc: &VorbisComments) {
    t.title = vget(vc, "title");
    t.artist = vget(vc, "artist");
    t.album_artist = vget(vc, "albumartist");
    t.album = vget(vc, "album");
    t.date = vget(vc, "date");
    t.label = vget(vc, "label").or_else(|| vget(vc, "organization"));
    t.isrc = vget(vc, "isrc");
    t.comment = vget(vc, "comment").or_else(|| vget(vc, "description"));
    t.genres = vc
        .get_all("genre")
        .filter(|g| !g.trim().is_empty())
        .map(str::to_string)
        .collect();

    let trk = vget(vc, "tracknumber");
    t.track_no = trk.as_deref().and_then(to_int);
    t.track_total = vget(vc, "tracktotal")
        .or_else(|| vget(vc, "totaltracks"))
        .as_deref()
        .and_then(to_int)
        // "3/12" in TRACKNUMBER is common; the Python reader ignored it, we use it as a fallback.
        .or_else(|| {
            trk.as_deref()
                .and_then(|s| s.split_once('/'))
                .and_then(|(_, n)| to_int(n))
        });
    t.disc_no = vget(vc, "discnumber").as_deref().and_then(to_int);
    t.bpm = vget(vc, "bpm").as_deref().and_then(to_float);
    t.initial_key = vget(vc, "initialkey").or_else(|| vget(vc, "key"));
    t.camelot = vget(vc, "camelot");
    t.energy = vget(vc, "energylevel").as_deref().and_then(to_int);
    t.replaygain_track_gain = vget(vc, "replaygain_track_gain")
        .as_deref()
        .and_then(to_float);
    t.replaygain_track_peak = vget(vc, "replaygain_track_peak")
        .as_deref()
        .and_then(to_float);
}

// ---------------------------------------------------------------------------
// MP4
// ---------------------------------------------------------------------------

pub(crate) const FREEFORM_MEAN: &str = "com.apple.iTunes";

fn fourcc(code: &[u8; 4]) -> AtomIdent<'static> {
    AtomIdent::Fourcc(*code)
}

fn atom_string(atom: &Atom<'_>) -> Option<String> {
    atom.data().find_map(|d| match d {
        AtomData::UTF8(s) | AtomData::UTF16(s) => non_empty(s),
        AtomData::Unknown { data, .. } => non_empty(&String::from_utf8_lossy(data)),
        _ => None,
    })
}

fn ilst_text(ilst: &Ilst, code: &[u8; 4]) -> Option<String> {
    ilst.get(&fourcc(code)).and_then(atom_string)
}

/// Freeform atom value, with the name compared case-insensitively.
fn ilst_freeform(ilst: &Ilst, name: &str) -> Option<String> {
    ilst.into_iter().find_map(|atom| match atom.ident() {
        AtomIdent::Freeform { mean, name: n }
            if mean == FREEFORM_MEAN && n.eq_ignore_ascii_case(name) =>
        {
            atom_string(atom)
        }
        _ => None,
    })
}

fn from_mp4(t: &mut TrackTags, ilst: &Ilst) {
    t.title = ilst_text(ilst, b"\xa9nam");
    t.artist = ilst_text(ilst, b"\xa9ART");
    t.album_artist = ilst_text(ilst, b"aART");
    t.album = ilst_text(ilst, b"\xa9alb");
    t.date = ilst_text(ilst, b"\xa9day");
    t.comment = ilst_text(ilst, b"\xa9cmt");
    if let Some(atom) = ilst.get(&fourcc(b"\xa9gen")) {
        t.genres = atom
            .data()
            .filter_map(|d| match d {
                AtomData::UTF8(s) | AtomData::UTF16(s) => non_empty(s),
                _ => None,
            })
            .collect();
    }
    t.track_no = ilst.track().filter(|n| *n > 0).map(|n| n as i32);
    t.track_total = ilst.track_total().filter(|n| *n > 0).map(|n| n as i32);
    t.disc_no = ilst.disk().filter(|n| *n > 0).map(|n| n as i32);

    if let Some(atom) = ilst.get(&fourcc(b"tmpo")) {
        t.bpm = atom.data().find_map(|d| match d {
            AtomData::SignedInteger(v) => Some(f64::from(*v)),
            AtomData::UnsignedInteger(v) => Some(f64::from(*v)),
            AtomData::Unknown { data, .. } if !data.is_empty() && data.len() <= 4 => {
                Some(data.iter().fold(0u32, |a, b| (a << 8) | u32::from(*b)) as f64)
            }
            _ => None,
        });
    }
    if let Some(exact) = ilst_freeform(ilst, "BPM") {
        t.bpm = truthy(to_float(&exact)).or(t.bpm);
    }
    t.initial_key = ilst_freeform(ilst, "initialkey");
    t.camelot = ilst_freeform(ilst, "camelot");
    t.energy = ilst_freeform(ilst, "energylevel")
        .as_deref()
        .and_then(to_int);
    t.label = ilst_freeform(ilst, "LABEL");
    t.replaygain_track_gain = ilst_freeform(ilst, "replaygain_track_gain")
        .as_deref()
        .and_then(to_float);
    t.replaygain_track_peak = ilst_freeform(ilst, "replaygain_track_peak")
        .as_deref()
        .and_then(to_float);
    t.has_art = ilst.get(&fourcc(b"covr")).is_some();
}

// ---------------------------------------------------------------------------
// Cover art
// ---------------------------------------------------------------------------

fn picture_to_art(p: &Picture) -> Artwork {
    let mime = p
        .mime_type()
        .map(|m| m.as_str().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| sniff_mime(p.data()).to_string());
    Artwork {
        data: p.data().to_vec(),
        mime,
    }
}

pub(crate) fn sniff_mime(data: &[u8]) -> &'static str {
    if data.starts_with(b"\x89PNG") {
        "image/png"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else if data.len() > 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

/// Prefer the front cover (type 3) when several pictures are present.
fn best_picture<'a>(pics: impl Iterator<Item = &'a Picture> + Clone) -> Option<&'a Picture> {
    pics.clone()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pics.clone().next())
}

fn id3_art(tag: &Id3v2Tag) -> Option<Artwork> {
    let pics = tag.iter().filter_map(|f| match f {
        Frame::Picture(p) => Some(p.picture.as_ref()),
        _ => None,
    });
    best_picture(pics).map(picture_to_art)
}

/// Embedded cover art only (no sidecar lookup). Never fails.
pub fn read_embedded_art(path: &Path) -> Option<Artwork> {
    match read_embedded_inner(path) {
        Ok(a) => a,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "art read failed");
            None
        }
    }
}

fn read_embedded_inner(path: &Path) -> Result<Option<Artwork>> {
    let opts = ParseOptions::new().read_properties(false);
    let mut r = open_reader(path)?;
    Ok(match container_for(path) {
        Container::Mpeg => MpegFile::read_from(&mut r, opts)?.id3v2().and_then(id3_art),
        Container::Aiff => AiffFile::read_from(&mut r, opts)?.id3v2().and_then(id3_art),
        Container::Wav => WavFile::read_from(&mut r, opts)?.id3v2().and_then(id3_art),
        Container::Aac => lofty::aac::AacFile::read_from(&mut r, opts)?
            .id3v2()
            .and_then(id3_art),
        Container::Flac => {
            let f = FlacFile::read_from(&mut r, opts)?;
            best_picture(f.pictures().iter().map(|(p, _)| p)).map(picture_to_art)
        }
        Container::Vorbis => {
            let f = VorbisFile::read_from(&mut r, opts)?;
            best_picture(f.vorbis_comments().pictures().iter().map(|(p, _)| p)).map(picture_to_art)
        }
        Container::Opus => {
            let f = OpusFile::read_from(&mut r, opts)?;
            best_picture(f.vorbis_comments().pictures().iter().map(|(p, _)| p)).map(picture_to_art)
        }
        Container::Mp4 => {
            let f = Mp4File::read_from(&mut r, opts)?;
            let atom = f.ilst().and_then(|i| i.get(&fourcc(b"covr")));
            atom.and_then(|a| {
                a.data().find_map(|d| match d {
                    AtomData::Picture(p) => Some(Artwork {
                        data: p.data().to_vec(),
                        mime: sniff_mime(p.data()).to_string(),
                    }),
                    AtomData::Unknown { data, .. } if !data.is_empty() => Some(Artwork {
                        data: data.clone(),
                        mime: sniff_mime(data).to_string(),
                    }),
                    _ => None,
                })
            })
        }
        Container::Other => None,
    })
}

/// Look for cover.jpg / folder.png / front.jpeg ... beside the audio.
///
/// Needed because `bandcamp-dl` deletes its own `cover.jpg` after embedding, but files imported
/// from elsewhere often only have the sidecar. Matching is case-insensitive; the first stem x
/// extension combination in priority order wins.
pub fn find_sidecar_cover(directory: &Path) -> Option<PathBuf> {
    let mut existing: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    for entry in std::fs::read_dir(directory).ok()?.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if (ft.is_file() || (ft.is_symlink() && entry.path().is_file()))
            && let Some(name) = entry.file_name().to_str()
        {
            existing.insert(name.to_ascii_lowercase(), entry.path());
        }
    }
    for stem in COVER_STEMS {
        for ext in COVER_EXTENSIONS {
            if let Some(hit) = existing.remove(&format!("{stem}{ext}")) {
                return Some(hit);
            }
        }
    }
    None
}

fn sidecar_mime(path: &Path) -> &'static str {
    match extension_lower(path).as_str() {
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/jpeg",
    }
}

/// Embedded art first, then a sidecar image in the same directory.
pub fn read_cover(path: &Path) -> Option<Artwork> {
    if let Some(art) = read_embedded_art(path) {
        return Some(art);
    }
    let sidecar = find_sidecar_cover(path.parent()?)?;
    let mut data = Vec::new();
    File::open(&sidecar).ok()?.read_to_end(&mut data).ok()?;
    Some(Artwork {
        data,
        mime: sidecar_mime(&sidecar).to_string(),
    })
}
