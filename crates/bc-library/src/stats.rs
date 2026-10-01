//! `/library/stats`, `/library/roots` listing helper and disk usage.

use std::path::Path;

use bc_db::rusqlite::Connection;
use bc_libcore::{ApiResult, Scope};
use bc_types::library::*;

pub fn root_out(c: &Connection, id: i64) -> ApiResult<Option<RootOut>> {
    Ok(roots(c)?.into_iter().find(|r| r.id == id))
}

pub fn roots(c: &Connection) -> ApiResult<Vec<RootOut>> {
    let mut counts = std::collections::HashMap::new();
    {
        let mut st = c.prepare("SELECT root_id, COUNT(*) FROM files GROUP BY root_id")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            counts.insert(r.get::<_, i64>(0)?, r.get::<_, i64>(1)?);
        }
    }
    let mut st = c.prepare("SELECT id, path, kind, enabled, watch, last_scan_at, last_scan_ms FROM library_roots ORDER BY id")?;
    let out = st
        .query_map([], |r| {
            let id: i64 = r.get(0)?;
            Ok(RootOut {
                id,
                path: r.get(1)?,
                kind: r.get(2)?,
                enabled: r.get(3)?,
                watch: r.get(4)?,
                last_scan_at: bc_db::util::iso_opt(r.get(5)?),
                last_scan_ms: r.get(6)?,
                track_count: counts.get(&id).copied().unwrap_or(0),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(out)
}

/// Capacity of the volume new downloads land on: the first enabled library root that answers.
pub fn disk_usage(roots: &[RootOut]) -> Option<DiskUsage> {
    let mut order: Vec<&RootOut> = roots.iter().collect();
    order.sort_by_key(|r| (!r.enabled, r.kind != "library"));
    for r in order {
        if let Some(u) = statvfs(Path::new(&r.path)) {
            return Some(u);
        }
    }
    None
}

#[cfg(unix)]
fn statvfs(p: &Path) -> Option<DiskUsage> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(p.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid NUL-terminated path and a properly sized out-struct.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut s) };
    if rc != 0 {
        return None;
    }
    let bs = s.f_frsize as i64;
    let total = s.f_blocks as i64 * bs;
    let free = s.f_bavail as i64 * bs;
    Some(DiskUsage { total_bytes: total, used_bytes: total - s.f_bfree as i64 * bs, free_bytes: free })
}

#[cfg(not(unix))]
fn statvfs(_p: &Path) -> Option<DiskUsage> {
    None
}

pub fn library_stats(c: &Connection, scope: &Scope) -> ApiResult<LibraryStats> {
    let roots = roots(c)?;
    let one = |sql: String| -> ApiResult<i64> { Ok(c.query_row(&sql, [], |r| r.get::<_, i64>(0))?) };
    let tracks = one(format!("SELECT COUNT(*) FROM tracks t WHERE 1=1{}", scope.and_track("t")))?;
    let releases = one(format!("SELECT COUNT(*) FROM releases r WHERE 1=1{}", scope.and_release("r")))?;
    let artists = one(format!("SELECT COUNT(*) FROM artists a WHERE 1=1{}", scope.and_artist("a")))?;
    let (tags, total_bytes, analyzed) = if scope.filtered() {
        (
            one(format!("SELECT COUNT(DISTINCT tt.tag_id) FROM track_tags tt JOIN tracks t ON t.id = tt.track_id WHERE 1=1{}", scope.and_track("t")))?,
            one(format!("SELECT COALESCE(SUM(f.size_bytes), 0) FROM files f JOIN tracks t ON t.id = f.track_id WHERE 1=1{}", scope.and_track("t")))?,
            one(format!("SELECT COUNT(*) FROM analysis an JOIN tracks t ON t.id = an.track_id WHERE 1=1{}", scope.and_track("t")))?,
        )
    } else {
        (
            one("SELECT COUNT(*) FROM tags WHERE track_count > 0".into())?,
            one("SELECT COALESCE(SUM(size_bytes), 0) FROM files".into())?,
            one("SELECT COUNT(*) FROM analysis".into())?,
        )
    };
    let total_duration_ms = one(format!("SELECT COALESCE(SUM(t.duration_ms), 0) FROM tracks t WHERE 1=1{}", scope.and_track("t")))?;
    let missing_files = one("SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL".into())?;
    let plays = one(format!("SELECT COALESCE(SUM(t.play_count), 0) FROM tracks t WHERE 1=1{}", scope.and_track("t")))?;
    let listened_ms = one("SELECT COALESCE(SUM(ms_played), 0) FROM play_history".into())?;
    let disk = disk_usage(&roots);
    Ok(LibraryStats { tracks, releases, artists, tags, total_bytes, total_duration_ms, missing_files, analyzed, plays, listened_ms, roots, disk })
}
