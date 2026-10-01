//! Stop downloading before the disk is full.
//!
//! A drive that fills up mid-download leaves a half-written album, a database
//! that can no longer write, and often an unbootable machine. So the worker
//! checks the free space on the downloads drive before every claim and holds --
//! nothing new is claimed, and whatever bandcamp-dl has in flight is handed back
//! to the queue -- once free space drops below a limit the user sets. It lets go
//! again on its own once space is freed, with a little margin so it does not flap
//! around the line.
//!
//! The limit is a setting (bytes) rather than a config flag: it is the kind of
//! thing that changes when a drive does.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use bc_db::rusqlite::{Connection, Transaction};
use bc_db::{Db, settings};
use serde::{Deserialize, Serialize};

pub const MIN_FREE_KEY: &str = "downloads.min_free_bytes";

/// Free space, in bytes, below which downloads hold. Five gigabytes: an album is
/// under a gigabyte, and the OS wants a few for itself.
pub const DEFAULT_MIN_FREE_BYTES: i64 = 5 * 1024 * 1024 * 1024;

/// How far above the limit free space must climb before downloads resume, so a
/// limit sitting right at the current free space does not stop and start every
/// few seconds.
pub const RESUME_MARGIN_BYTES: i64 = 512 * 1024 * 1024;

/// The limit in force: the stored setting, or the default when it is absent or
/// unparsable. Negative stored values read as 0.
pub fn read_min_free(c: &Connection) -> bc_db::Result<i64> {
    Ok(match settings::get(c, MIN_FREE_KEY)? {
        None => DEFAULT_MIN_FREE_BYTES,
        Some(raw) => match raw.trim().parse::<i64>() {
            Ok(v) => v.max(0),
            Err(_) => DEFAULT_MIN_FREE_BYTES,
        },
    })
}

/// Store the limit (negative values are clamped to 0) and return what was stored.
pub fn write_min_free(t: &Transaction<'_>, value: i64) -> bc_db::Result<i64> {
    let value = value.max(0);
    settings::set(t, MIN_FREE_KEY, &value.to_string())?;
    Ok(value)
}

pub fn read_min_free_db(db: &Db) -> bc_db::Result<i64> {
    db.read(read_min_free)
}

pub fn write_min_free_db(db: &Db, value: i64) -> bc_db::Result<i64> {
    db.write(move |t| write_min_free(t, value))
}

pub async fn read_min_free_async(db: &Db) -> bc_db::Result<i64> {
    db.read_async(read_min_free).await
}

pub async fn write_min_free_async(db: &Db, value: i64) -> bc_db::Result<i64> {
    db.write_async(move |t| write_min_free(t, value)).await
}

/// Free space on the volume `path` lives on (what an unprivileged process may
/// use, like `shutil.disk_usage(...).free`); `None` when nothing of it exists or
/// it cannot be read.
///
/// The downloads folder may not exist yet on a fresh install: walk up to the
/// nearest ancestor that does, which is on the same volume.
pub fn free_bytes(path: &Path) -> Option<i64> {
    let mut probe = path;
    while !probe.exists() {
        probe = match probe.parent() {
            Some(p) if p.as_os_str().is_empty() => Path::new("."),
            Some(p) => p,
            None => return None,
        };
    }
    let c_path = CString::new(probe.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c_path` is a valid NUL-terminated string and `st` is a properly
    // sized, writable `statvfs` the call fills in.
    let st = unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut st) != 0 {
            return None;
        }
        st
    };
    let free = (st.f_bavail as u128).saturating_mul(st.f_frsize as u128);
    Some(i64::try_from(free).unwrap_or(i64::MAX))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskState {
    pub path: String,
    pub free_bytes: Option<i64>,
    pub min_free_bytes: i64,
    pub held: bool,
}

/// Whether downloads should be held given the free space now.
///
/// Hysteresis: once held, they stay held until free space is comfortably above
/// the limit. Unknown free space (the drive is gone) holds too -- writing blind
/// is the one thing this exists to prevent.
pub fn should_hold(free: Option<i64>, min_free: i64, held: bool) -> bool {
    let Some(free) = free else { return true };
    if held {
        return free < min_free + RESUME_MARGIN_BYTES;
    }
    free < min_free
}
