-- tables of bc-db/migrations/req_ws3_analysis.sql (idempotent twin used by schema::ensure)
CREATE TABLE IF NOT EXISTS beat_grids (
    track_id       INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL,                              -- constant | variable
    grid           TEXT NOT NULL,                              -- JSON bc_types::analysis::BeatGrid
    downbeat_phase INTEGER,
    confidence     REAL,
    source         TEXT NOT NULL,
    updated_at     DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS cue_points (
    id         INTEGER PRIMARY KEY,
    track_id   INTEGER NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    kind       TEXT NOT NULL,   -- hot | memory | mix_in | mix_out | drop | intro | outro | loop
    pos_ms     REAL NOT NULL,
    end_ms     REAL,
    label      TEXT,
    color      TEXT,
    slot       INTEGER,
    auto       INTEGER NOT NULL DEFAULT 0,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS ix_cue_points_track ON cue_points (track_id);

CREATE TABLE IF NOT EXISTS waveform_meta (
    track_id        INTEGER PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    format_version  INTEGER NOT NULL,
    source_hash     TEXT NOT NULL,
    sample_rate     INTEGER NOT NULL,
    duration_ms     INTEGER NOT NULL,
    overview_points INTEGER NOT NULL,
    detail_points   INTEGER NOT NULL,
    bytes           INTEGER NOT NULL,
    updated_at      DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);
