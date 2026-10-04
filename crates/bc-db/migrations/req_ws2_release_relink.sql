-- Requested by WS2 (harvest relink). Releases the relink job already searched Bandcamp for, so a
-- stopped or restarted run resumes instead of spending the same searches again. A row is written
-- whether or not the search found the page; deleting the release forgets it.
CREATE TABLE IF NOT EXISTS release_relink (
    release_id INTEGER PRIMARY KEY REFERENCES releases(id) ON DELETE CASCADE,
    tried_at   TEXT NOT NULL,              -- UTC, "YYYY-MM-DD HH:MM:SS"
    url        TEXT                        -- the page it was linked to; NULL = no confident match
);
