-- Pre-orders: what Bandcamp says is out *now* for a release. One row per release, written by the
-- download worker (from the tralbum it already fetched) or by GET /releases/{id}/availability.
CREATE TABLE IF NOT EXISTS release_availability (
    release_id       INTEGER PRIMARY KEY REFERENCES releases(id) ON DELETE CASCADE,
    checked_at       TEXT NOT NULL,                 -- UTC, "YYYY-MM-DD HH:MM:SS"
    is_preorder      INTEGER NOT NULL DEFAULT 0,
    release_date     TEXT,                          -- "YYYY-MM-DD"
    tracks           TEXT NOT NULL DEFAULT '[]',    -- JSON [{track_num,title,duration_sec,available}]
    unreleased_count INTEGER NOT NULL DEFAULT 0     -- tracks with available=false (denormalised for the missing filter)
);
