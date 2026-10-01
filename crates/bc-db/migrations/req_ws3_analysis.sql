-- Requested by WS3 (analysis). WS1: append to bc-db/src/migrate.rs as the next version.
-- bc_analysis::schema::ensure() applies the same DDL idempotently, so wiring is optional but
-- preferred (it makes the columns exist for readers that run before the analysis service).

ALTER TABLE analysis ADD COLUMN bpm_candidates TEXT;          -- JSON array of BPM (octave alternatives)
ALTER TABLE analysis ADD COLUMN grid_kind TEXT;               -- constant | variable
ALTER TABLE analysis ADD COLUMN downbeat_offset_ms REAL;      -- time of the first downbeat
ALTER TABLE analysis ADD COLUMN lra REAL;                     -- loudness range (LU)
ALTER TABLE analysis ADD COLUMN true_peak_dbtp REAL;          -- true peak, dBTP (4x oversampled)
ALTER TABLE analysis ADD COLUMN energy_v2 REAL;               -- calibrated 1..10
ALTER TABLE analysis ADD COLUMN analyzer TEXT;                -- essentia-import | bc-rs-1 | essentia-sidecar

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
