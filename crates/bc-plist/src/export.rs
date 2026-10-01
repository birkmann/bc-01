//! Track exports: m3u8, csv and a streamed zip (port of `services/library/zipexport.py` and the
//! export helpers of `api/routes/library.py`).
//!
//! Playlists, DJ sets and the library's `GET /tracks/export` all go through here:
//!
//! 1. resolve the ids of the tracks to export (in the order the file should have),
//! 2. [`load_export_tracks`] -> one query for the lot,
//! 3. [`export_response`] (or [`m3u8`] / [`csv`] / [`zip_response`] directly).
//!
//! m3u8 and zip leave out tracks without a file on disk (a playlist entry pointing nowhere just
//! errors in the target player); csv is an inventory and keeps them. m3u8 and csv contain file
//! paths by design (it is a file export); no other response of this crate does.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bc_db::rusqlite::Connection;
use bc_libcore::{ApiError, ApiResult};
use bc_types::library::{ExportFormat, TrackQuery};
use bytes::Bytes;

/// Read chunk of the zip writer (also the flush granularity is a quarter of it).
pub const CHUNK: usize = 1 << 20;
/// Bytes buffered before a chunk is handed to the HTTP body.
const FLUSH_AT: usize = 256 * 1024;
/// Chunks in flight between the writer thread and the response body (back-pressure).
const CHANNEL_DEPTH: usize = 4;

/// Everything an export needs about one track.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExportTrack {
    pub id: i64,
    pub title: String,
    /// The track's artist, else its release's.
    pub artist: String,
    /// Release title.
    pub album: String,
    pub year: Option<i64>,
    pub duration_ms: Option<i64>,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub energy: Option<f64>,
    pub rating: Option<i64>,
    pub loved: bool,
    pub play_count: i64,
    pub tags: Vec<String>,
    /// `datetime.isoformat()` of the naive UTC timestamp (`2020-01-02T03:04:05.123456`).
    pub added_at: Option<String>,
    /// Primary file path: the first non-missing file, else any file, else empty.
    pub path: String,
    /// No file of the track is present on disk.
    pub missing: bool,
}

/// Export rows for `ids`, in the order given (duplicates kept: a playlist may hold a track
/// twice; unknown ids are skipped). Two queries however many ids.
pub fn load_export_tracks(c: &Connection, ids: &[i64]) -> ApiResult<Vec<ExportTrack>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let json = serde_json::to_string(ids).map_err(ApiError::internal)?;
    let mut by_id: HashMap<i64, ExportTrack> = HashMap::new();
    {
        let mut st = c.prepare_cached(
            "SELECT t.id, t.title, COALESCE(ta.name, ra.name, ''), COALESCE(r.title, ''), r.year,
                    t.duration_ms, an.bpm, an.camelot, an.energy, t.rating, t.loved, t.play_count,
                    t.added_at, f.path, f.missing_since IS NOT NULL
               FROM tracks t
               LEFT JOIN artists ta ON ta.id = t.artist_id
               LEFT JOIN releases r ON r.id = t.release_id
               LEFT JOIN artists ra ON ra.id = r.artist_id
               LEFT JOIN analysis an ON an.track_id = t.id
               LEFT JOIN files f ON f.id = (SELECT id FROM files WHERE track_id = t.id
                                             ORDER BY (missing_since IS NOT NULL), id LIMIT 1)
              WHERE t.id IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = st.query_map([&json], |r| {
            let path: Option<String> = r.get(13)?;
            let file_missing: Option<bool> = r.get(14)?;
            Ok(ExportTrack {
                id: r.get(0)?,
                title: r.get(1)?,
                artist: r.get(2)?,
                album: r.get(3)?,
                year: r.get(4)?,
                duration_ms: r.get(5)?,
                bpm: r.get(6)?,
                camelot: r.get(7)?,
                energy: r.get(8)?,
                rating: r.get(9)?,
                loved: r.get(10)?,
                play_count: r.get(11)?,
                tags: vec![],
                added_at: r.get::<_, Option<String>>(12)?.map(|s| py_isoformat(&s)),
                path: path.unwrap_or_default(),
                missing: file_missing.unwrap_or(true),
            })
        })?;
        for t in rows {
            let t = t?;
            by_id.insert(t.id, t);
        }
    }
    {
        let mut st = c.prepare_cached(
            "SELECT tt.track_id, g.name FROM track_tags tt JOIN tags g ON g.id = tt.tag_id
              WHERE tt.track_id IN (SELECT value FROM json_each(?1)) ORDER BY tt.track_id, g.id",
        )?;
        let mut rows = st.query([&json])?;
        while let Some(r) = rows.next()? {
            let (tid, name): (i64, String) = (r.get(0)?, r.get(1)?);
            if let Some(t) = by_id.get_mut(&tid) {
                t.tags.push(name);
            }
        }
    }
    Ok(ids.iter().filter_map(|id| by_id.get(id).cloned()).collect())
}

/// `2020-01-02 03:04:05.000000` -> `2020-01-02T03:04:05` (Python drops zero microseconds).
fn py_isoformat(db: &str) -> String {
    let s = db.replacen(' ', "T", 1);
    match s.strip_suffix(".000000") {
        Some(base) => base.to_string(),
        None => s,
    }
}

/// `#EXTM3U` playlist. Tracks without a file on disk are left out.
pub fn m3u8(tracks: &[ExportTrack]) -> String {
    let mut out = String::from("#EXTM3U\n");
    for t in tracks.iter().filter(|t| !t.missing) {
        let seconds = t.duration_ms.unwrap_or(0) / 1000;
        out.push_str(&format!("#EXTINF:{seconds},{} - {}\n{}\n", t.artist, t.title, t.path));
    }
    out
}

/// Python's `str(float)`: integral values keep their `.0`.
fn py_float(v: f64) -> String {
    if v.is_finite() && v == v.trunc() && v.abs() < 1e16 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

/// The csv columns, in the legacy order.
pub const CSV_HEADER: [&str; 14] = [
    "title",
    "artist",
    "album",
    "year",
    "duration_seconds",
    "bpm",
    "camelot",
    "energy",
    "rating",
    "loved",
    "play_count",
    "tags",
    "added_at",
    "path",
];

/// csv inventory (CRLF rows, legacy columns). Keeps tracks whose file is missing.
pub fn csv(tracks: &[ExportTrack]) -> String {
    let mut w = csv::WriterBuilder::new().terminator(csv::Terminator::CRLF).from_writer(Vec::new());
    let _ = w.write_record(CSV_HEADER);
    for t in tracks {
        let record = [
            t.title.clone(),
            t.artist.clone(),
            t.album.clone(),
            t.year.map(|y| y.to_string()).unwrap_or_default(),
            // Python's round() is banker's rounding.
            ((t.duration_ms.unwrap_or(0) as f64 / 1000.0).round_ties_even() as i64).to_string(),
            t.bpm.map(py_float).unwrap_or_default(),
            t.camelot.clone().unwrap_or_default(),
            t.energy.map(py_float).unwrap_or_default(),
            t.rating.filter(|r| *r != 0).map(|r| r.to_string()).unwrap_or_default(),
            if t.loved { "yes" } else { "no" }.to_string(),
            t.play_count.to_string(),
            t.tags.join("; "),
            t.added_at.clone().unwrap_or_default(),
            t.path.clone(),
        ];
        let _ = w.write_record(&record);
    }
    let bytes = w.into_inner().unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// What an export of the `/tracks` filter is called: the thing exported, not "tracks".
///
/// The narrowest filter wins, since that is what the page the download was started from is
/// showing: one record, then its maker, then the label, then whatever collection stood in for one.
pub fn export_name(c: &Connection, q: &TrackQuery) -> ApiResult<String> {
    let only = q.release_id.or(if q.release_ids.len() == 1 { Some(q.release_ids[0]) } else { None });
    if let Some(rid) = only {
        let row = c
            .query_row(
                "SELECT r.title, a.name FROM releases r LEFT JOIN artists a ON a.id = r.artist_id WHERE r.id = ?1",
                [rid],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .ok();
        if let Some((title, artist)) = row {
            return Ok(match artist.filter(|a| !a.is_empty()) {
                Some(a) => format!("{a} - {title}"),
                None => title,
            });
        }
    }
    if !q.release_ids.is_empty() {
        return Ok(format!("{} albums", q.release_ids.len()));
    }
    if let Some(aid) = q.artist_id
        && let Ok(name) = c.query_row("SELECT name FROM artists WHERE id = ?1", [aid], |r| r.get::<_, String>(0))
    {
        return Ok(name);
    }
    if let Some(lid) = q.label_id
        && let Ok(name) = c.query_row("SELECT name FROM labels WHERE id = ?1", [lid], |r| r.get::<_, String>(0))
    {
        return Ok(name);
    }
    if q.loved == Some(true) {
        return Ok("Loved".into());
    }
    if q.favorites == Some(true) {
        return Ok("Favourites".into());
    }
    if !q.tags.is_empty() {
        return Ok(q.tags.join(", "));
    }
    if let Some(s) = q.q.as_deref().filter(|s| !s.is_empty()) {
        return Ok(format!("Search - {s}"));
    }
    Ok("Tracks".into())
}

/// A file or folder name that is safe on Windows/exFAT, where most exported zips end up:
/// path syntax and control characters become `_`, leading/trailing spaces and dots go, 150 chars.
pub fn safe_component(name: &str, fallback: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 0x20 { '_' } else { c })
        .collect();
    let trimmed = replaced.trim_matches(|c| c == ' ' || c == '.');
    let cut: String = trimmed.chars().take(150).collect();
    if cut.is_empty() { fallback.to_string() } else { cut }
}

/// [`safe_component`] with the legacy `untitled` fallback.
pub fn safe_name(name: &str) -> String {
    safe_component(name, "untitled")
}

/// Python's `urllib.parse.quote` (safe = `/`).
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `Content-Disposition` value that survives quotes and non-ASCII list names.
pub fn attachment_disposition(filename: &str) -> String {
    let fallback: String = filename
        .chars()
        .map(|c| if !c.is_ascii() { '?' } else if c == '"' { '\'' } else if c.is_ascii_control() { '_' } else { c })
        .collect();
    format!("attachment; filename=\"{fallback}\"; filename*=UTF-8''{}", quote(filename))
}

/// Headers for a file download (`Content-Disposition`).
pub fn attachment_headers(filename: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&attachment_disposition(filename)) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h
}

/// `(arcname, path)` for each `(title, artist, path)` that exists on disk.
///
/// Everything lands flat inside one named folder (no per-album nesting) so the export is one
/// directory to drag somewhere. Names carry a position prefix so a name sort replays the list's
/// order, the only ordering a zip reliably keeps. Entries are numbered after the missing files
/// are dropped, and the same name twice cannot occur because the number differs.
pub fn track_entries(items: &[(String, String, String)], folder: Option<&str>) -> Vec<(String, PathBuf)> {
    let present: Vec<(&str, &str, PathBuf)> = items
        .iter()
        .filter(|(_, _, raw)| !raw.is_empty() && Path::new(raw).is_file())
        .map(|(t, a, raw)| (t.as_str(), a.as_str(), PathBuf::from(raw)))
        .collect();
    let prefix = folder.map(|f| format!("{}/", safe_name(f))).unwrap_or_default();
    let width = present.len().to_string().len().max(2);
    present
        .into_iter()
        .enumerate()
        .map(|(i, (title, artist, path))| {
            let stem = if artist.is_empty() { title.to_string() } else { format!("{artist} - {title}") };
            let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default();
            (format!("{prefix}{:0width$} {}{ext}", i + 1, safe_name(&stem), width = width), path)
        })
        .collect()
}

/// [`track_entries`] over export rows (missing files skipped).
pub fn entries_for(tracks: &[ExportTrack], folder: Option<&str>) -> Vec<(String, PathBuf)> {
    let items: Vec<(String, String, String)> =
        tracks.iter().filter(|t| !t.missing).map(|t| (t.title.clone(), t.artist.clone(), t.path.clone())).collect();
    track_entries(&items, folder)
}

/// `Write` sink that ships its buffer to the response body in chunks. Unseekable on purpose:
/// the zip writer then emits data descriptors instead of rewinding to patch headers, which is
/// what makes a byte-stream archive possible. A dropped receiver (client went away) surfaces as
/// `BrokenPipe` and ends the writer thread.
struct ChannelWriter {
    tx: tokio::sync::mpsc::Sender<io::Result<Bytes>>,
    buf: Vec<u8>,
}

impl ChannelWriter {
    fn ship(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = Bytes::from(std::mem::take(&mut self.buf));
        self.tx.blocking_send(Ok(chunk)).map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= FLUSH_AT {
            self.ship()?;
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.ship()
    }
}

/// DOS date/time of a file's mtime (zip's native stamp); the epoch of 1980 when unknown.
fn dos_mtime(path: &Path) -> (u16, u16) {
    use chrono::{Datelike, Timelike};
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .map(chrono::DateTime::<chrono::Utc>::from)
        .filter(|d| (1980..=2107).contains(&d.year()))
        .map(|d| {
            let date = (((d.year() - 1980) as u16) << 9) | ((d.month() as u16) << 5) | d.day() as u16;
            let time = ((d.hour() as u16) << 11) | ((d.minute() as u16) << 5) | (d.second() as u16 / 2);
            (time, date)
        })
        .unwrap_or((0, (1 << 5) | 1))
}

/// Bytes written so far through a wrapped writer (offsets in the central directory).
struct Counting<W: Write> {
    inner: W,
    pos: u64,
}

impl<W: Write> Write for Counting<W> {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(b)?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct CentralEntry {
    name: String,
    crc: u32,
    size: u64,
    offset: u64,
    zip64: bool,
    time: u16,
    date: u16,
}

const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_DESCRIPTOR: u32 = 0x0807_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;
const SIG_EOCD64: u32 = 0x0606_4b50;
const SIG_LOCATOR64: u32 = 0x0706_4b50;
/// bit 3: sizes and crc follow the data; bit 11: UTF-8 names.
const FLAGS: u16 = 0x0008 | 0x0800;
/// Entries at least this big are written ZIP64 (decided up front from the file's length).
const ZIP64_AT: u64 = 0xFFFF_0000;

/// A forward-only ZIP writer for STORED entries: no seeking, no buffering of file data.
///
/// Sizes and CRC follow each entry in a data descriptor (general-purpose bit 3), which is what
/// lets the archive be emitted as a byte stream while the files are read once, at disk speed.
/// ZIP64 is used per entry for files of 4 GiB and over and for the directory when it needs it.
pub struct ZipStream<W: Write> {
    w: Counting<W>,
    entries: Vec<CentralEntry>,
    buf: Vec<u8>,
}

impl<W: Write> ZipStream<W> {
    pub fn new(inner: W) -> Self {
        Self { w: Counting { inner, pos: 0 }, entries: Vec::new(), buf: vec![0u8; CHUNK] }
    }

    /// Append `src` (`len_hint` bytes long, from the file's metadata) as a stored entry.
    pub fn add_stored(&mut self, name: &str, src: &mut impl Read, len_hint: u64, mtime: (u16, u16)) -> io::Result<()> {
        let zip64 = len_hint >= ZIP64_AT;
        let offset = self.w.pos;
        let (time, date) = mtime;
        let w = &mut self.w;
        w.write_all(&SIG_LOCAL.to_le_bytes())?;
        w.write_all(&(if zip64 { 45u16 } else { 20 }).to_le_bytes())?;
        w.write_all(&FLAGS.to_le_bytes())?;
        w.write_all(&0u16.to_le_bytes())?; // stored
        w.write_all(&time.to_le_bytes())?;
        w.write_all(&date.to_le_bytes())?;
        w.write_all(&0u32.to_le_bytes())?; // crc: in the descriptor
        let sizes = if zip64 { u32::MAX } else { 0 };
        w.write_all(&sizes.to_le_bytes())?;
        w.write_all(&sizes.to_le_bytes())?;
        w.write_all(&u16::try_from(name.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?.to_le_bytes())?;
        w.write_all(&(if zip64 { 20u16 } else { 0 }).to_le_bytes())?;
        w.write_all(name.as_bytes())?;
        if zip64 {
            w.write_all(&1u16.to_le_bytes())?;
            w.write_all(&16u16.to_le_bytes())?;
            w.write_all(&[0u8; 16])?;
        }
        let mut hasher = crc32fast::Hasher::new();
        let mut size = 0u64;
        loop {
            let n = src.read(&mut self.buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&self.buf[..n]);
            self.w.write_all(&self.buf[..n])?;
            size += n as u64;
        }
        if size >= ZIP64_AT && !zip64 {
            // the file grew past 4 GiB while being read: the header promised 32-bit sizes
            return Err(io::Error::new(io::ErrorKind::InvalidData, "file grew while being zipped"));
        }
        let crc = hasher.finalize();
        let w = &mut self.w;
        w.write_all(&SIG_DESCRIPTOR.to_le_bytes())?;
        w.write_all(&crc.to_le_bytes())?;
        if zip64 {
            w.write_all(&size.to_le_bytes())?;
            w.write_all(&size.to_le_bytes())?;
        } else {
            w.write_all(&(size as u32).to_le_bytes())?;
            w.write_all(&(size as u32).to_le_bytes())?;
        }
        self.entries.push(CentralEntry { name: name.to_string(), crc, size, offset, zip64, time, date });
        Ok(())
    }

    /// Write the central directory and return the sink.
    pub fn finish(mut self) -> io::Result<W> {
        let cd_start = self.w.pos;
        let w = &mut self.w;
        for e in &self.entries {
            let big_offset = e.offset >= 0xFFFF_FFFF;
            let mut extra: Vec<u8> = Vec::new();
            if e.zip64 {
                extra.extend_from_slice(&e.size.to_le_bytes());
                extra.extend_from_slice(&e.size.to_le_bytes());
            }
            if big_offset {
                extra.extend_from_slice(&e.offset.to_le_bytes());
            }
            let need64 = e.zip64 || big_offset;
            w.write_all(&SIG_CENTRAL.to_le_bytes())?;
            w.write_all(&((3u16 << 8) | 45).to_le_bytes())?; // made by: unix, 4.5
            w.write_all(&(if need64 { 45u16 } else { 20 }).to_le_bytes())?;
            w.write_all(&FLAGS.to_le_bytes())?;
            w.write_all(&0u16.to_le_bytes())?;
            w.write_all(&e.time.to_le_bytes())?;
            w.write_all(&e.date.to_le_bytes())?;
            w.write_all(&e.crc.to_le_bytes())?;
            let size32 = if e.zip64 { u32::MAX } else { e.size as u32 };
            w.write_all(&size32.to_le_bytes())?;
            w.write_all(&size32.to_le_bytes())?;
            w.write_all(&(e.name.len() as u16).to_le_bytes())?;
            let elen = if extra.is_empty() { 0 } else { extra.len() as u16 + 4 };
            w.write_all(&elen.to_le_bytes())?;
            w.write_all(&0u16.to_le_bytes())?; // comment
            w.write_all(&0u16.to_le_bytes())?; // disk
            w.write_all(&0u16.to_le_bytes())?; // internal attrs
            w.write_all(&((0o100_644u32) << 16).to_le_bytes())?;
            w.write_all(&(if big_offset { u32::MAX } else { e.offset as u32 }).to_le_bytes())?;
            w.write_all(e.name.as_bytes())?;
            if !extra.is_empty() {
                w.write_all(&1u16.to_le_bytes())?;
                w.write_all(&(extra.len() as u16).to_le_bytes())?;
                w.write_all(&extra)?;
            }
        }
        let cd_size = w.pos - cd_start;
        let n = self.entries.len() as u64;
        let zip64_end = n >= 0xFFFF || cd_start >= 0xFFFF_FFFF || cd_size >= 0xFFFF_FFFF;
        if zip64_end {
            let eocd64_at = w.pos;
            w.write_all(&SIG_EOCD64.to_le_bytes())?;
            w.write_all(&44u64.to_le_bytes())?;
            w.write_all(&45u16.to_le_bytes())?;
            w.write_all(&45u16.to_le_bytes())?;
            w.write_all(&0u32.to_le_bytes())?;
            w.write_all(&0u32.to_le_bytes())?;
            w.write_all(&n.to_le_bytes())?;
            w.write_all(&n.to_le_bytes())?;
            w.write_all(&cd_size.to_le_bytes())?;
            w.write_all(&cd_start.to_le_bytes())?;
            w.write_all(&SIG_LOCATOR64.to_le_bytes())?;
            w.write_all(&0u32.to_le_bytes())?;
            w.write_all(&eocd64_at.to_le_bytes())?;
            w.write_all(&1u32.to_le_bytes())?;
        }
        w.write_all(&SIG_EOCD.to_le_bytes())?;
        w.write_all(&0u16.to_le_bytes())?;
        w.write_all(&0u16.to_le_bytes())?;
        let n16 = if zip64_end { u16::MAX } else { n as u16 };
        w.write_all(&n16.to_le_bytes())?;
        w.write_all(&n16.to_le_bytes())?;
        w.write_all(&(if zip64_end { u32::MAX } else { cd_size as u32 }).to_le_bytes())?;
        w.write_all(&(if zip64_end { u32::MAX } else { cd_start as u32 }).to_le_bytes())?;
        w.write_all(&0u16.to_le_bytes())?;
        w.flush()?;
        Ok(self.w.inner)
    }
}

/// Write the archive (entries STORED: audio is already compressed, so the download runs at disk
/// speed instead of burning CPU re-deflating FLAC) to any sink. Files that vanished since the
/// listing are skipped: a shorter zip beats a corrupt one. Memory use is one read chunk.
pub fn write_zip<W: Write>(sink: W, entries: &[(String, PathBuf)]) -> io::Result<()> {
    let mut zw = ZipStream::new(sink);
    for (arcname, path) in entries {
        let Ok(mut src) = std::fs::File::open(path) else { continue };
        let len = src.metadata().map(|m| m.len()).unwrap_or(0);
        zw.add_stored(arcname, &mut src, len, dos_mtime(path))?;
    }
    zw.finish().map(|_| ())
}

/// Stream `entries` as `{folder}.zip`. A blocking writer thread feeds a bounded channel that is
/// the response body, so a multi-gigabyte list never sits in memory and a slow client slows the
/// disk reads (back-pressure). `folder` names the download only; arc names carry the folder
/// prefix already (see [`track_entries`]).
pub fn zip_response(entries: Vec<(String, PathBuf)>, folder: &str) -> Response {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<io::Result<Bytes>>(CHANNEL_DEPTH);
    std::thread::spawn(move || {
        let sink = ChannelWriter { tx: tx.clone(), buf: Vec::with_capacity(FLUSH_AT + 4096) };
        if let Err(e) = write_zip(sink, &entries)
            && e.kind() != io::ErrorKind::BrokenPipe
        {
            tracing::warn!(error = %e, "zip export failed mid-stream");
            let _ = tx.blocking_send(Err(e));
        }
    });
    let stream = futures::stream::poll_fn(move |cx| rx.poll_recv(cx));
    let mut resp = Response::new(Body::from_stream(stream));
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zip"));
    resp.headers_mut().extend(attachment_headers(&format!("{}.zip", safe_name(folder))));
    resp
}

/// The tracks' audio files as a zip named after the list (archive name == the one folder in it).
/// 400 when none of the files is on disk.
pub fn export_zip(tracks: &[ExportTrack], name: &str) -> ApiResult<Response> {
    let folder = safe_name(name);
    let entries = entries_for(tracks, Some(&folder));
    if entries.is_empty() {
        return Err(ApiError::bad("none of these tracks have a file on disk"));
    }
    Ok(zip_response(entries, &folder))
}

/// A text download (`audio/x-mpegurl` / `text/csv`) named `{name}.{ext}`.
fn text_download(body: String, mime: &'static str, filename: &str) -> Response {
    let mut resp = (StatusCode::OK, body).into_response();
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    resp.headers_mut().extend(attachment_headers(filename));
    resp
}

/// The finished export response in the requested format, named `name` (sanitised for the file
/// name). `tracks` are in file order.
pub fn export_response(format: ExportFormat, tracks: &[ExportTrack], name: &str) -> ApiResult<Response> {
    let stem = safe_name(name);
    match format {
        ExportFormat::Zip => export_zip(tracks, &stem),
        ExportFormat::Csv => Ok(text_download(csv(tracks), "text/csv; charset=utf-8", &format!("{stem}.csv"))),
        ExportFormat::M3u8 => {
            Ok(text_download(m3u8(tracks), "audio/x-mpegurl; charset=utf-8", &format!("{stem}.m3u8")))
        }
    }
}
