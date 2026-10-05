-- Releases already in step with the saved Bandcamp-links file (`links.db`, see bc-library
-- ledger.rs). A release without a row is new to that file: its saved links are restored first.
CREATE TABLE IF NOT EXISTS release_ledger (
    release_id INTEGER PRIMARY KEY REFERENCES releases(id) ON DELETE CASCADE,
    ledger_key TEXT NOT NULL,              -- its row in links.db
    fp         INTEGER NOT NULL            -- fingerprint of what was last saved
);
