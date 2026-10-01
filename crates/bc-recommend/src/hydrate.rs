//! Hydration of `TrackOut` for winners only (two-phase: the pool is scored from light columns;
//! full display rows are read for the handful of tracks that made the page). Delegates to WS1's
//! `bc_libcore::hydrate::tracks_out`; every recommender goes through these two functions.

use std::collections::HashMap;

use bc_db::rusqlite::Connection;
use bc_types::library::TrackOut;

use crate::error::Result;

/// Display rows for `ids`, keyed by id. Unknown ids are absent.
pub fn briefs(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, TrackOut>> {
    Ok(bc_libcore::hydrate::tracks_out(conn, ids)?.into_iter().map(|t| (t.id, t)).collect())
}

/// Display rows in the order of `ids` (missing ids dropped).
pub fn briefs_ordered(conn: &Connection, ids: &[i64]) -> Result<Vec<TrackOut>> {
    Ok(bc_libcore::hydrate::tracks_out(conn, ids)?)
}
