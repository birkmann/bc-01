//! Files removed from the library but kept on disk.
//!
//! Removing only the rows is undone by the next scan, which finds the file and imports it again.
//! So a removal records each file's path here, and the scanner, the watcher and every ingest drop
//! excluded paths before they read a single tag. Lifting an exclusion lets the file back in.

use std::collections::HashSet;

use bc_db::rusqlite::{Connection, Transaction};
use bc_db::util::{iso, now_db};
use bc_libcore::ApiResult;
use bc_types::library::maint::ExcludedOut;

use crate::util::{ids_json, strs_json};

/// Exclude every file of these tracks (call before the track rows go: the paths, artist and title
/// are read from them). Returns the number of paths recorded.
pub fn add_for_tracks(t: &Transaction<'_>, track_ids: &[i64]) -> ApiResult<i64> {
    let n = t.execute(
        "INSERT INTO excluded_files(path, artist_name, title, added_at)
         SELECT f.path, COALESCE(a.name, ''), tr.title, ?2
           FROM files f JOIN tracks tr ON tr.id = f.track_id LEFT JOIN artists a ON a.id = tr.artist_id
          WHERE f.track_id IN (SELECT value FROM json_each(?1))
         ON CONFLICT(path) DO UPDATE SET artist_name = excluded.artist_name, title = excluded.title, added_at = excluded.added_at",
        (ids_json(track_ids), now_db()),
    )?;
    Ok(n as i64)
}

/// Every excluded path, for filtering a walk.
pub fn paths(c: &Connection) -> ApiResult<HashSet<String>> {
    let mut st = c.prepare("SELECT path FROM excluded_files")?;
    Ok(st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
}

/// The list, newest first.
pub fn list(c: &Connection) -> ApiResult<Vec<ExcludedOut>> {
    let mut st = c.prepare("SELECT path, artist_name, title, added_at FROM excluded_files ORDER BY added_at DESC, path")?;
    Ok(st
        .query_map([], |r| {
            Ok(ExcludedOut {
                path: r.get(0)?,
                artist_name: r.get(1)?,
                title: r.get(2)?,
                added_at: Some(iso(&r.get::<_, String>(3)?)),
            })
        })?
        .collect::<Result<_, _>>()?)
}

/// Lift the exclusion of these paths; returns how many were excluded.
pub fn remove(t: &Transaction<'_>, paths: &[String]) -> ApiResult<i64> {
    Ok(t.execute("DELETE FROM excluded_files WHERE path IN (SELECT value FROM json_each(?1))", [strs_json(paths)])? as i64)
}
