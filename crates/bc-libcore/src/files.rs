//! Track id -> playable file (for the streaming route and the audio engine). The path is
//! validated to live under an enabled library root before it is handed out.

use std::path::PathBuf;

use bc_db::rusqlite::{Connection, OptionalExtension};

use crate::error::{ApiError, ApiResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFile {
    pub file_id: i64,
    pub path: PathBuf,
    pub ext: String,
    pub codec: Option<String>,
    pub size_bytes: i64,
    pub mtime_ns: i64,
}

/// Best playable file of a track: `files WHERE missing_since IS NULL`, lowest id first.
pub fn resolve_track_file(c: &Connection, track_id: i64) -> ApiResult<ResolvedFile> {
    let row = c
        .query_row(
            "SELECT f.id, f.path, f.ext, f.codec, f.size_bytes, f.mtime_ns FROM files f
              WHERE f.track_id = ?1 AND f.missing_since IS NULL ORDER BY f.id LIMIT 1",
            [track_id],
            |r| Ok(ResolvedFile { file_id: r.get(0)?, path: PathBuf::from(r.get::<_, String>(1)?), ext: r.get(2)?, codec: r.get(3)?, size_bytes: r.get(4)?, mtime_ns: r.get(5)? }),
        )
        .optional()?;
    let Some(f) = row else {
        let exists = c.query_row("SELECT 1 FROM tracks WHERE id = ?1", [track_id], |_| Ok(())).optional()?.is_some();
        return Err(ApiError::not_found(if exists { format!("no available file for track {track_id}") } else { format!("track {track_id} not found") }));
    };
    if !f.path.is_file() {
        return Err(ApiError::not_found(format!("file missing on disk for track {track_id}")));
    }
    // Defence in depth: the path came from our own DB, but must still lie under an enabled root.
    let roots: Vec<PathBuf> = {
        let mut st = c.prepare("SELECT path FROM library_roots WHERE enabled = 1")?;
        st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?.into_iter().map(PathBuf::from).collect()
    };
    if !roots.is_empty() && !roots.iter().any(|r| bc_core::paths::is_within(r, &f.path).is_ok()) {
        return Err(ApiError::not_found(format!("file outside library roots for track {track_id}")));
    }
    Ok(f)
}
