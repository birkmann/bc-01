-- A root move that has started and not yet finished (bc-maint relocate.rs). The row is written
-- before the first file moves and removed when the move completes or the user stops it, so a
-- move cut short by a quit or crash is picked up again at the next start.
CREATE TABLE IF NOT EXISTS root_moves (
    root_id    INTEGER PRIMARY KEY REFERENCES library_roots(id) ON DELETE CASCADE,
    target     TEXT NOT NULL,          -- the path the user asked for, as sent
    started_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);
