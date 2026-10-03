-- Files removed from the library but left on disk. The scanner, the watcher and every ingest skip
-- these paths, so a removal survives the next scan; deleting the row lets the file back in.
CREATE TABLE IF NOT EXISTS excluded_files (
    path        TEXT PRIMARY KEY,               -- as stored in files.path when it was removed
    artist_name TEXT NOT NULL DEFAULT '',       -- for the list in Cleanup (the track row is gone)
    title       TEXT NOT NULL DEFAULT '',
    added_at    TEXT NOT NULL                   -- UTC, "YYYY-MM-DD HH:MM:SS"
);
