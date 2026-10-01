-- v2: PLAN §4.1 additions + denormalised sort keys + FTS for entities + hot-query indexes.
-- Everything is additive. Applied on top of the legacy schema (v1).

-- Analysis extras, beat_grids, cue_points and waveform_meta are defined by workstream 3
-- (req_ws3_analysis.sql / req_ws3_tables.sql, applied after this migration).

-- ---------------------------------------------------------------- new tables
CREATE TABLE IF NOT EXISTS artwork (
    release_id  INTEGER PRIMARY KEY REFERENCES releases(id) ON DELETE CASCADE,
    hash        TEXT,                                      -- content hash of the source image
    version     TEXT NOT NULL,                             -- the immutable ?v= value
    blurhash    TEXT,
    color       TEXT,                                      -- dominant colour #rrggbb
    width       INTEGER,
    height      INTEGER,
    sizes       INTEGER NOT NULL DEFAULT 0,                -- bitmask of webp sizes on disk: 1=thumb 2=medium 4=full
    source      TEXT,                                      -- legacy | embedded | sidecar
    updated_at  DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS ui_state (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  TEXT
);

CREATE TABLE IF NOT EXISTS harvest_item_tags (
    item_id  INTEGER NOT NULL REFERENCES harvest_items(id) ON DELETE CASCADE,
    tag_key  TEXT NOT NULL,
    tag      TEXT NOT NULL,
    PRIMARY KEY (item_id, tag_key)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS ix_harvest_item_tags_key ON harvest_item_tags (tag_key, item_id);

-- ---------------------------------------------------------------- denormalised track columns
-- `available` = has at least one non-missing file (kept by triggers on `files`).
-- artist_key/album_key/year = sort keys copied from artists/releases (kept by triggers).
-- bpm = analysis.bpm (kept by triggers). They exist so every sortable column has an
-- index that also covers the hot filters (is_snippet, available): deep offsets never
-- touch the table.
ALTER TABLE tracks ADD COLUMN available INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tracks ADD COLUMN artist_key TEXT;
ALTER TABLE tracks ADD COLUMN album_key TEXT;
ALTER TABLE tracks ADD COLUMN year INTEGER;
ALTER TABLE tracks ADD COLUMN bpm REAL;

UPDATE tracks SET available = EXISTS (SELECT 1 FROM files f WHERE f.track_id = tracks.id AND f.missing_since IS NULL);
UPDATE tracks SET artist_key = (SELECT a.name_key FROM artists a
                                 WHERE a.id = COALESCE(tracks.artist_id, (SELECT r.artist_id FROM releases r WHERE r.id = tracks.release_id)));
UPDATE tracks SET album_key = (SELECT r.title_key FROM releases r WHERE r.id = tracks.release_id),
                  year      = (SELECT r.year      FROM releases r WHERE r.id = tracks.release_id);
UPDATE tracks SET bpm = (SELECT an.bpm FROM analysis an WHERE an.track_id = tracks.id);

CREATE TRIGGER IF NOT EXISTS trg_files_ai AFTER INSERT ON files WHEN NEW.track_id IS NOT NULL BEGIN
    UPDATE tracks SET available = 1 - available
     WHERE id = NEW.track_id
       AND available <> (EXISTS (SELECT 1 FROM files f WHERE f.track_id = NEW.track_id AND f.missing_since IS NULL));
END;
CREATE TRIGGER IF NOT EXISTS trg_files_au AFTER UPDATE OF missing_since, track_id ON files BEGIN
    UPDATE tracks SET available = 1 - available
     WHERE id = NEW.track_id
       AND available <> (EXISTS (SELECT 1 FROM files f WHERE f.track_id = NEW.track_id AND f.missing_since IS NULL));
    UPDATE tracks SET available = 1 - available
     WHERE id = OLD.track_id AND OLD.track_id IS NOT NEW.track_id
       AND available <> (EXISTS (SELECT 1 FROM files f WHERE f.track_id = OLD.track_id AND f.missing_since IS NULL));
END;
CREATE TRIGGER IF NOT EXISTS trg_files_ad AFTER DELETE ON files WHEN OLD.track_id IS NOT NULL BEGIN
    UPDATE tracks SET available = 1 - available
     WHERE id = OLD.track_id
       AND available <> (EXISTS (SELECT 1 FROM files f WHERE f.track_id = OLD.track_id AND f.missing_since IS NULL));
END;

CREATE TRIGGER IF NOT EXISTS trg_tracks_ai AFTER INSERT ON tracks BEGIN
    UPDATE tracks SET
        artist_key = (SELECT a.name_key FROM artists a WHERE a.id = COALESCE(NEW.artist_id, (SELECT r.artist_id FROM releases r WHERE r.id = NEW.release_id))),
        album_key  = (SELECT r.title_key FROM releases r WHERE r.id = NEW.release_id),
        year       = (SELECT r.year      FROM releases r WHERE r.id = NEW.release_id)
     WHERE id = NEW.id;
END;
CREATE TRIGGER IF NOT EXISTS trg_tracks_au AFTER UPDATE OF artist_id, release_id ON tracks BEGIN
    UPDATE tracks SET
        artist_key = (SELECT a.name_key FROM artists a WHERE a.id = COALESCE(NEW.artist_id, (SELECT r.artist_id FROM releases r WHERE r.id = NEW.release_id))),
        album_key  = (SELECT r.title_key FROM releases r WHERE r.id = NEW.release_id),
        year       = (SELECT r.year      FROM releases r WHERE r.id = NEW.release_id)
     WHERE id = NEW.id;
END;
CREATE TRIGGER IF NOT EXISTS trg_artists_key_au AFTER UPDATE OF name_key ON artists BEGIN
    UPDATE tracks SET artist_key = NEW.name_key WHERE artist_id = NEW.id;
    UPDATE tracks SET artist_key = NEW.name_key
     WHERE artist_id IS NULL AND release_id IN (SELECT id FROM releases WHERE artist_id = NEW.id);
END;
CREATE TRIGGER IF NOT EXISTS trg_releases_key_au AFTER UPDATE OF title_key, year, artist_id ON releases BEGIN
    UPDATE tracks SET album_key = NEW.title_key, year = NEW.year WHERE release_id = NEW.id;
    UPDATE tracks SET artist_key = (SELECT a.name_key FROM artists a WHERE a.id = NEW.artist_id)
     WHERE release_id = NEW.id AND artist_id IS NULL;
END;
CREATE TRIGGER IF NOT EXISTS trg_analysis_ai AFTER INSERT ON analysis BEGIN
    UPDATE tracks SET bpm = NEW.bpm WHERE id = NEW.track_id;
END;
CREATE TRIGGER IF NOT EXISTS trg_analysis_au AFTER UPDATE OF bpm ON analysis BEGIN
    UPDATE tracks SET bpm = NEW.bpm WHERE id = NEW.track_id;
END;
CREATE TRIGGER IF NOT EXISTS trg_analysis_ad AFTER DELETE ON analysis BEGIN
    UPDATE tracks SET bpm = NULL WHERE id = OLD.track_id;
END;

-- ---------------------------------------------------------------- FTS5 for entities (substring semantics, like the old LIKE '%q%')
CREATE VIRTUAL TABLE IF NOT EXISTS artist_search  USING fts5(name, tokenize = 'trigram');
CREATE VIRTUAL TABLE IF NOT EXISTS label_search   USING fts5(name, tokenize = 'trigram');
CREATE VIRTUAL TABLE IF NOT EXISTS release_search USING fts5(title, artist, label, tokenize = 'trigram');
-- Opt-in typo-tolerant substring search over tracks; filled by `bc doctor --build-trigram`.
CREATE VIRTUAL TABLE IF NOT EXISTS search_trigram USING fts5(title, artist, album, label, tokenize = 'trigram');

INSERT INTO artist_search(rowid, name) SELECT id, name FROM artists;
INSERT INTO label_search(rowid, name)  SELECT id, name FROM labels;
INSERT INTO release_search(rowid, title, artist, label)
    SELECT r.id, r.title, COALESCE(a.name, ''), COALESCE(l.name, '')
      FROM releases r LEFT JOIN artists a ON a.id = r.artist_id LEFT JOIN labels l ON l.id = r.label_id;

CREATE TRIGGER IF NOT EXISTS trg_artist_search_ai AFTER INSERT ON artists BEGIN
    INSERT INTO artist_search(rowid, name) VALUES (NEW.id, NEW.name);
END;
CREATE TRIGGER IF NOT EXISTS trg_artist_search_au AFTER UPDATE OF name ON artists BEGIN
    DELETE FROM artist_search WHERE rowid = OLD.id;
    INSERT INTO artist_search(rowid, name) VALUES (NEW.id, NEW.name);
    DELETE FROM release_search WHERE rowid IN (SELECT id FROM releases WHERE artist_id = NEW.id);
    INSERT INTO release_search(rowid, title, artist, label)
        SELECT r.id, r.title, NEW.name, COALESCE((SELECT l.name FROM labels l WHERE l.id = r.label_id), '')
          FROM releases r WHERE r.artist_id = NEW.id;
END;
CREATE TRIGGER IF NOT EXISTS trg_artist_search_ad AFTER DELETE ON artists BEGIN
    DELETE FROM artist_search WHERE rowid = OLD.id;
END;
CREATE TRIGGER IF NOT EXISTS trg_label_search_ai AFTER INSERT ON labels BEGIN
    INSERT INTO label_search(rowid, name) VALUES (NEW.id, NEW.name);
END;
CREATE TRIGGER IF NOT EXISTS trg_label_search_au AFTER UPDATE OF name ON labels BEGIN
    DELETE FROM label_search WHERE rowid = OLD.id;
    INSERT INTO label_search(rowid, name) VALUES (NEW.id, NEW.name);
    DELETE FROM release_search WHERE rowid IN (SELECT id FROM releases WHERE label_id = NEW.id);
    INSERT INTO release_search(rowid, title, artist, label)
        SELECT r.id, r.title, COALESCE((SELECT a.name FROM artists a WHERE a.id = r.artist_id), ''), NEW.name
          FROM releases r WHERE r.label_id = NEW.id;
END;
CREATE TRIGGER IF NOT EXISTS trg_label_search_ad AFTER DELETE ON labels BEGIN
    DELETE FROM label_search WHERE rowid = OLD.id;
END;
CREATE TRIGGER IF NOT EXISTS trg_release_search_ai AFTER INSERT ON releases BEGIN
    INSERT INTO release_search(rowid, title, artist, label)
    VALUES (NEW.id, NEW.title,
            COALESCE((SELECT a.name FROM artists a WHERE a.id = NEW.artist_id), ''),
            COALESCE((SELECT l.name FROM labels l WHERE l.id = NEW.label_id), ''));
END;
CREATE TRIGGER IF NOT EXISTS trg_release_search_au AFTER UPDATE OF title, artist_id, label_id ON releases BEGIN
    DELETE FROM release_search WHERE rowid = OLD.id;
    INSERT INTO release_search(rowid, title, artist, label)
    VALUES (NEW.id, NEW.title,
            COALESCE((SELECT a.name FROM artists a WHERE a.id = NEW.artist_id), ''),
            COALESCE((SELECT l.name FROM labels l WHERE l.id = NEW.label_id), ''));
END;
CREATE TRIGGER IF NOT EXISTS trg_release_search_ad AFTER DELETE ON releases BEGIN
    DELETE FROM release_search WHERE rowid = OLD.id;
END;

-- ---------------------------------------------------------------- hot-query indexes
-- Sort indexes end in (id, is_snippet, available): a deep OFFSET walks the index only,
-- the table row is fetched (deferred seek) just for the page that is returned.
DROP INDEX IF EXISTS ix_tracks_added_at;
DROP INDEX IF EXISTS ix_tracks_title_key;
CREATE INDEX IF NOT EXISTS ix_tracks_s_added      ON tracks (added_at, id, is_snippet, available);
CREATE INDEX IF NOT EXISTS ix_tracks_s_title      ON tracks (title_key, id, is_snippet, available);
CREATE INDEX IF NOT EXISTS ix_tracks_s_artist     ON tracks (artist_key, id, is_snippet, available);
CREATE INDEX IF NOT EXISTS ix_tracks_s_album      ON tracks (album_key, disc_no, track_no, id, is_snippet, available);
CREATE INDEX IF NOT EXISTS ix_tracks_s_duration   ON tracks (duration_ms, id, is_snippet, available);
CREATE INDEX IF NOT EXISTS ix_tracks_s_bpm        ON tracks (bpm, id, is_snippet, available);
CREATE INDEX IF NOT EXISTS ix_tracks_s_plays      ON tracks (play_count, id, is_snippet, available);
CREATE INDEX IF NOT EXISTS ix_tracks_s_lastplayed ON tracks (last_played_at, id, is_snippet, available);
CREATE INDEX IF NOT EXISTS ix_tracks_s_year       ON tracks (year, id, is_snippet, available);
CREATE INDEX IF NOT EXISTS ix_tracks_s_rating     ON tracks (rating, id, is_snippet, available);
CREATE INDEX IF NOT EXISTS ix_track_tags_tag_track ON track_tags (tag_id, track_id);
CREATE INDEX IF NOT EXISTS ix_files_track_missing ON files (track_id, missing_since);
CREATE INDEX IF NOT EXISTS ix_releases_s_added    ON releases (added_at, id, snippet_only, source_fan_id);
CREATE INDEX IF NOT EXISTS ix_releases_label_added ON releases (label_id, added_at);
CREATE INDEX IF NOT EXISTS ix_releases_artist_year ON releases (artist_id, year);
CREATE INDEX IF NOT EXISTS ix_play_history_completed ON play_history (completed, started_at, track_id);
CREATE INDEX IF NOT EXISTS ix_analysis_camelot_bpm ON analysis (camelot, bpm);
CREATE INDEX IF NOT EXISTS ix_tracks_count ON tracks (is_snippet, available, duration_ms);
CREATE INDEX IF NOT EXISTS ix_tracks_artist_plays ON tracks (artist_id, play_count);
