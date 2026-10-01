//! Pre-order availability cache (`release_availability`): which tracks of a record Bandcamp has
//! released, and when the rest is due. Written by the download worker and by
//! `GET /releases/{id}/availability`; read (cache only, never the network) by the release listings,
//! the missing filter and the fill guard.

use bc_db::rusqlite::{Connection, OptionalExtension, params};
use bc_types::library::{ReleaseAvailability, TrackAvailability};

use crate::{ApiError, ApiResult};

/// A pre-order's cached row is trusted for this long (hours) while its date is still ahead.
pub const FRESH_HOURS: i64 = 12;
/// A row that says "not a pre-order" cannot go stale by the calendar; recheck rarely.
pub const SETTLED_DAYS: i64 = 30;

/// Insert or replace the row for `a.release_id`, stamping `checked_at` now (UTC).
pub fn store(c: &Connection, a: &ReleaseAvailability) -> ApiResult<ReleaseAvailability> {
    let json = serde_json::to_string(&a.tracks).map_err(ApiError::internal)?;
    let unreleased = a.unreleased().count() as i64;
    c.execute(
        "INSERT INTO release_availability(release_id, checked_at, is_preorder, release_date, tracks, unreleased_count)
         VALUES (?1, datetime('now'), ?2, ?3, ?4, ?5)
         ON CONFLICT(release_id) DO UPDATE SET checked_at = excluded.checked_at, is_preorder = excluded.is_preorder,
             release_date = excluded.release_date, tracks = excluded.tracks, unreleased_count = excluded.unreleased_count",
        params![a.release_id, a.is_preorder, a.release_date, json, unreleased],
    )?;
    Ok(load(c, a.release_id)?.unwrap_or_else(|| a.clone()))
}

/// The cached row, whatever its age.
pub fn load(c: &Connection, release_id: i64) -> ApiResult<Option<ReleaseAvailability>> {
    let row = c
        .query_row(
            "SELECT checked_at, is_preorder, release_date, tracks FROM release_availability WHERE release_id = ?1",
            [release_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?)),
        )
        .optional()?;
    Ok(row.map(|(checked_at, is_preorder, release_date, tracks)| ReleaseAvailability {
        release_id,
        checked_at,
        is_preorder,
        release_date,
        tracks: serde_json::from_str::<Vec<TrackAvailability>>(&tracks).unwrap_or_default(),
        fetched: false,
    }))
}

/// Is the cached row still good? A pre-order is fresh for 12 h and only until its release date
/// (after which the page must be read again); a settled record for 30 days. Anything checked in
/// the last hour is fresh, so a date that passed without the page changing is not re-read per view.
pub fn is_fresh(c: &Connection, release_id: i64) -> ApiResult<bool> {
    let fresh: Option<bool> = c
        .query_row(
            "SELECT checked_at > datetime('now', '-1 hours')
                 OR CASE WHEN is_preorder = 1
                         THEN checked_at > datetime('now', '-' || ?2 || ' hours') AND (release_date IS NULL OR release_date > date('now'))
                         ELSE checked_at > datetime('now', '-' || ?3 || ' days') END
               FROM release_availability WHERE release_id = ?1",
            params![release_id, FRESH_HOURS, SETTLED_DAYS],
            |r| r.get(0),
        )
        .optional()?;
    Ok(fresh.unwrap_or(false))
}

/// A record is *waiting* when it is a pre-order whose date is ahead (or unknown). SQL twin of the
/// predicate used by `MISSING_COND`.
pub fn is_waiting(c: &Connection, release_id: i64) -> ApiResult<bool> {
    Ok(c
        .query_row(
            "SELECT 1 FROM release_availability WHERE release_id = ?1 AND is_preorder = 1 AND (release_date IS NULL OR release_date > date('now'))",
            [release_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}
