-- Requested by WS2 (bc-jobs). Drops the legacy `CHECK (kind IN (...))` on `jobs`
-- so new job kinds (move, harvest, walk, sweep, enrich, ...) can be stored.
-- SQLite cannot ALTER a CHECK, so `jobs` and `job_items` are rebuilt, preserving
-- every row, column and index. The status CHECKs are kept.
--
-- Safe inside the migration transaction with foreign_keys=ON: the children
-- (`job_items`) are rebuilt first-class and dropped *before* their parent, so no
-- ON DELETE CASCADE ever fires. Nothing else references `jobs`.
--
-- Idempotent guard is the caller's user_version; do not run twice.

ALTER TABLE jobs RENAME TO jobs_old;
ALTER TABLE job_items RENAME TO job_items_old;

CREATE TABLE jobs (
	id VARCHAR NOT NULL,
	kind VARCHAR NOT NULL,
	status VARCHAR NOT NULL,
	label VARCHAR,
	priority INTEGER NOT NULL,
	params TEXT NOT NULL,
	total INTEGER NOT NULL,
	completed INTEGER NOT NULL,
	failed INTEGER NOT NULL,
	skipped INTEGER NOT NULL,
	cancel_requested BOOLEAN NOT NULL,
	error TEXT,
	created_at DATETIME NOT NULL,
	started_at DATETIME,
	finished_at DATETIME,
	PRIMARY KEY (id),
	CONSTRAINT ck_job_status CHECK (status IN ('queued','running','paused','completed','failed','cancelled'))
);

INSERT INTO jobs (id, kind, status, label, priority, params, total, completed, failed, skipped,
                  cancel_requested, error, created_at, started_at, finished_at)
SELECT id, kind, status, label, priority, params, total, completed, failed, skipped,
       cancel_requested, error, created_at, started_at, finished_at
  FROM jobs_old;

CREATE TABLE job_items (
	id INTEGER NOT NULL,
	job_id VARCHAR NOT NULL,
	seq INTEGER NOT NULL,
	status VARCHAR NOT NULL,
	url VARCHAR,
	url_kind VARCHAR,
	source VARCHAR,
	target_dir VARCHAR,
	track_id INTEGER,
	attempts INTEGER NOT NULL,
	max_attempts INTEGER NOT NULL,
	next_attempt_at DATETIME,
	lease_expires_at DATETIME,
	worker_pid INTEGER,
	progress FLOAT NOT NULL,
	message VARCHAR,
	last_error TEXT,
	error_class VARCHAR,
	result TEXT,
	release_id INTEGER,
	started_at DATETIME,
	finished_at DATETIME,
	PRIMARY KEY (id),
	CONSTRAINT uq_job_item_seq UNIQUE (job_id, seq),
	CONSTRAINT ck_job_item_status CHECK (status IN ('pending','running','done','failed','skipped','cancelled')),
	FOREIGN KEY(job_id) REFERENCES jobs (id) ON DELETE CASCADE,
	FOREIGN KEY(track_id) REFERENCES tracks (id) ON DELETE CASCADE,
	FOREIGN KEY(release_id) REFERENCES releases (id) ON DELETE SET NULL
);

INSERT INTO job_items (id, job_id, seq, status, url, url_kind, source, target_dir, track_id,
                       attempts, max_attempts, next_attempt_at, lease_expires_at, worker_pid,
                       progress, message, last_error, error_class, result, release_id,
                       started_at, finished_at)
SELECT id, job_id, seq, status, url, url_kind, source, target_dir, track_id,
       attempts, max_attempts, next_attempt_at, lease_expires_at, worker_pid,
       progress, message, last_error, error_class, result, release_id,
       started_at, finished_at
  FROM job_items_old;

DROP TABLE job_items_old;
DROP TABLE jobs_old;

CREATE INDEX ix_jobs_claim ON jobs (status, priority, created_at);
CREATE INDEX ix_jobs_created_at ON jobs (created_at);
CREATE INDEX ix_jobs_kind ON jobs (kind);
CREATE INDEX ix_jobs_status ON jobs (status);
CREATE INDEX ix_job_items_job_id ON job_items (job_id);
CREATE INDEX ix_job_items_claim ON job_items (status, next_attempt_at, job_id, seq);
CREATE INDEX ix_job_items_status ON job_items (status);
