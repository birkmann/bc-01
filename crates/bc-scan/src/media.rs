//! Thin bridge to `bc-media`: the scanner talks to plain structs of its own so the DB side
//! (ingest) stays testable with hand-made values.

use std::path::Path;

use sha2::{Digest, Sha256};

use bc_media::{artwork as martwork, tags as mtags};

/// The tag fields ingest consumes (a subset of `mtags::TrackTags`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub date: Option<String>,
    pub genres: Vec<String>,
    pub track_no: Option<i64>,
    pub track_total: Option<i64>,
    pub disc_no: Option<i64>,
    pub label: Option<String>,
    pub isrc: Option<String>,
    pub duration_ms: Option<i64>,
    pub codec: Option<String>,
    pub bitrate: Option<i64>,
    pub sample_rate: Option<i64>,
    pub channels: Option<i64>,
    /// Stable hash of the editable tags (what `files.tag_hash` stores).
    pub tag_hash: String,
}

impl FileTags {
    /// First 4-digit year of `date` (a bare year or ISO date).
    pub fn year(&self) -> Option<i64> {
        let head: String = self.date.as_deref()?.trim().chars().take(4).collect();
        if head.len() == 4 && head.chars().all(|c| c.is_ascii_digit()) { head.parse().ok() } else { None }
    }

    /// A tag hash computed from the fields ingest knows; used when the media layer gives none.
    pub fn compute_hash(&self) -> String {
        let mut h = Sha256::new();
        for part in [
            self.title.as_deref(),
            self.artist.as_deref(),
            self.album_artist.as_deref(),
            self.album.as_deref(),
            self.date.as_deref(),
            self.label.as_deref(),
            self.isrc.as_deref(),
        ] {
            h.update(part.unwrap_or("").as_bytes());
            h.update([0u8]);
        }
        for g in &self.genres {
            h.update(g.as_bytes());
            h.update([1u8]);
        }
        for n in [self.track_no, self.track_total, self.disc_no] {
            h.update(n.unwrap_or(-1).to_le_bytes());
        }
        format!("{:x}", h.finalize())
    }
}

/// A decoded-and-resized cover ready to be written (hash/blurhash/colour + three WebPs).
#[derive(Debug, Clone)]
pub struct CoverArt {
    /// `embedded` | `sidecar`
    pub source: &'static str,
    pub processed: ProcessedArt,
}

/// True for the audio extensions the scanner indexes.
pub fn is_audio_path(path: &Path) -> bool {
    mtags::is_audio_path(path)
}

/// Read one file's tags. Never fails: an unreadable file yields a title-only record.
pub fn read_file_tags(path: &Path) -> FileTags {
    let t = mtags::read_tags(path);
    let mut out = FileTags {
        title: t.title.clone(),
        artist: t.artist.clone(),
        album_artist: t.album_artist.clone(),
        album: t.album.clone(),
        date: t.date.clone(),
        genres: t.genres.clone(),
        track_no: t.track_no.map(i64::from),
        track_total: t.track_total.map(i64::from),
        disc_no: t.disc_no.map(i64::from),
        label: t.label.clone(),
        isrc: t.isrc.clone(),
        duration_ms: t.duration_ms,
        codec: t.codec.clone(),
        bitrate: t.bitrate.map(i64::from),
        sample_rate: t.sample_rate.map(i64::from),
        channels: t.channels.map(i64::from),
        tag_hash: t.tag_hash(),
    };
    if out.title.as_deref().map(str::trim).unwrap_or("").is_empty() {
        out.title = path.file_stem().map(|s| s.to_string_lossy().into_owned());
    }
    out
}

/// Cover for a folder: embedded art of the first few files that have one, then a sidecar image.
/// Decoding/resizing/encoding happens here (stage b, no DB access).
pub fn find_cover(folder: &Path, files: &[&Path]) -> Option<CoverArt> {
    for f in files.iter().take(3) {
        if let Some(a) = mtags::read_embedded_art(f)
            && let Ok(p) = martwork::process_cover(&a.data)
        {
            return Some(CoverArt { source: "embedded", processed: p });
        }
    }
    let side = mtags::find_sidecar_cover(folder)?;
    let data = std::fs::read(side).ok()?;
    martwork::process_cover(&data).ok().map(|p| CoverArt { source: "sidecar", processed: p })
}

/// Write the WebP files for a release; returns the size bitmask (1 thumb, 2 medium, 4 full).
pub fn write_cover_files(art_dir: &Path, release_id: i64, c: &CoverArt) -> Result<u8, String> {
    martwork::write_art_files(art_dir, release_id, &c.processed).map_err(|e| e.to_string())
}

/// Path of the full-size WebP (`releases.cover_path`).
pub fn full_art_path(art_dir: &Path, release_id: i64) -> std::path::PathBuf {
    martwork::art_file_path(art_dir, release_id, martwork::ArtSize::Full)
}

pub fn version_string(p: &martwork::ProcessedArt, mask: u8) -> String {
    martwork::version_string(&p.hash, mask)
}

pub use bc_media::artwork::{ProcessedArt, process_cover};

/// Full-size legacy JPEG path (`<art_dir>/<shard>/<id>.jpg`).
pub fn legacy_jpeg_path(art_dir: &Path, release_id: i64) -> std::path::PathBuf {
    bc_media::artwork::legacy_jpeg_path(art_dir, release_id, false)
}
