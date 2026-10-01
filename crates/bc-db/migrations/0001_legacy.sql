-- Schema of the Python app (bcapp) at import time, dumped from the real DB.
-- Fresh installs apply this; imported DBs already have it (user_version 0 -> 1).
CREATE TABLE library_roots (
	id INTEGER NOT NULL, 
	path VARCHAR NOT NULL, 
	kind VARCHAR NOT NULL, 
	watch BOOLEAN NOT NULL, 
	enabled BOOLEAN NOT NULL, 
	last_scan_at DATETIME, 
	last_scan_ms INTEGER, 
	PRIMARY KEY (id), 
	CONSTRAINT ck_root_kind CHECK (kind IN ('library','downloads')), 
	UNIQUE (path)
);
CREATE TABLE artists (
	id INTEGER NOT NULL, 
	name VARCHAR NOT NULL, 
	name_key VARCHAR NOT NULL, 
	sort_name VARCHAR, 
	bandcamp_url VARCHAR, 
	bandcamp_id INTEGER, 
	location VARCHAR, 
	image_path VARCHAR, 
	created_at DATETIME NOT NULL, 
	PRIMARY KEY (id), 
	UNIQUE (bandcamp_url)
);
CREATE UNIQUE INDEX ix_artists_name_key ON artists (name_key);
CREATE TABLE labels (
	id INTEGER NOT NULL, 
	name VARCHAR NOT NULL, 
	name_key VARCHAR NOT NULL, 
	bandcamp_url VARCHAR, 
	PRIMARY KEY (id), 
	UNIQUE (bandcamp_url)
);
CREATE UNIQUE INDEX ix_labels_name_key ON labels (name_key);
CREATE TABLE tags (
	id INTEGER NOT NULL, 
	name VARCHAR NOT NULL, 
	name_key VARCHAR NOT NULL, 
	kind VARCHAR NOT NULL, 
	track_count INTEGER NOT NULL, 
	idf FLOAT, 
	PRIMARY KEY (id)
);
CREATE UNIQUE INDEX ix_tags_name_key ON tags (name_key);
CREATE INDEX ix_tags_track_count ON tags (track_count);
CREATE TABLE releases (
	id INTEGER NOT NULL, 
	title VARCHAR NOT NULL, 
	title_key VARCHAR NOT NULL, 
	artist_id INTEGER, 
	label_id INTEGER, 
	kind VARCHAR NOT NULL, 
	release_date VARCHAR, 
	year INTEGER, 
	bandcamp_url VARCHAR, 
	bandcamp_item_id INTEGER, 
	cover_path VARCHAR, 
	about TEXT, 
	credits TEXT, 
	folder_path VARCHAR, 
	added_at DATETIME NOT NULL, source_fan_id INTEGER REFERENCES fans(id) ON DELETE RESTRICT, expected_track_count INTEGER, snippet_only BOOLEAN NOT NULL DEFAULT 0, 
	PRIMARY KEY (id), 
	CONSTRAINT uq_release_identity UNIQUE (artist_id, title_key, year), 
	CONSTRAINT ck_release_kind CHECK (kind IN ('album','single','ep','compilation','unknown')), 
	FOREIGN KEY(artist_id) REFERENCES artists (id) ON DELETE SET NULL, 
	FOREIGN KEY(label_id) REFERENCES labels (id) ON DELETE SET NULL, 
	UNIQUE (bandcamp_url)
);
CREATE INDEX ix_releases_year ON releases (year);
CREATE INDEX ix_releases_title_key ON releases (title_key);
CREATE INDEX ix_releases_label_id ON releases (label_id);
CREATE INDEX ix_releases_artist_id ON releases (artist_id);
CREATE TABLE tracks (
	id INTEGER NOT NULL, 
	release_id INTEGER, 
	artist_id INTEGER, 
	title VARCHAR NOT NULL, 
	title_key VARCHAR NOT NULL, 
	track_no INTEGER, 
	disc_no INTEGER, 
	duration_ms INTEGER, 
	isrc VARCHAR, 
	bandcamp_url VARCHAR, 
	rating INTEGER, 
	loved BOOLEAN NOT NULL, 
	comment TEXT, 
	play_count INTEGER NOT NULL, 
	skip_count INTEGER NOT NULL, 
	last_played_at DATETIME, 
	added_at DATETIME NOT NULL, is_snippet BOOLEAN NOT NULL DEFAULT 0, 
	PRIMARY KEY (id), 
	CONSTRAINT ck_track_rating CHECK (rating IS NULL OR (rating BETWEEN 0 AND 5)), 
	FOREIGN KEY(release_id) REFERENCES releases (id) ON DELETE CASCADE, 
	FOREIGN KEY(artist_id) REFERENCES artists (id) ON DELETE SET NULL
);
CREATE INDEX ix_tracks_artist_id ON tracks (artist_id);
CREATE INDEX ix_tracks_title_key ON tracks (title_key);
CREATE INDEX ix_tracks_added_at ON tracks (added_at);
CREATE INDEX ix_tracks_release_order ON tracks (release_id, disc_no, track_no);
CREATE INDEX ix_tracks_release_id ON tracks (release_id);
CREATE INDEX ix_tracks_loved ON tracks (loved);
CREATE TABLE files (
	id INTEGER NOT NULL, 
	track_id INTEGER, 
	root_id INTEGER NOT NULL, 
	path VARCHAR NOT NULL, 
	rel_path VARCHAR NOT NULL, 
	ext VARCHAR NOT NULL, 
	codec VARCHAR, 
	bitrate INTEGER, 
	sample_rate INTEGER, 
	channels INTEGER, 
	size_bytes INTEGER NOT NULL, 
	mtime_ns INTEGER NOT NULL, 
	inode INTEGER, 
	tag_hash VARCHAR, 
	first_seen_at DATETIME NOT NULL, 
	last_seen_at DATETIME NOT NULL, 
	missing_since DATETIME, 
	PRIMARY KEY (id), 
	FOREIGN KEY(track_id) REFERENCES tracks (id) ON DELETE CASCADE, 
	FOREIGN KEY(root_id) REFERENCES library_roots (id) ON DELETE CASCADE, 
	UNIQUE (path)
);
CREATE INDEX ix_files_root_id ON files (root_id);
CREATE INDEX ix_files_missing_since ON files (missing_since);
CREATE INDEX ix_files_track_id ON files (track_id);
CREATE TABLE track_tags (
	track_id INTEGER NOT NULL, 
	tag_id INTEGER NOT NULL, 
	source VARCHAR NOT NULL, 
	weight FLOAT NOT NULL, 
	PRIMARY KEY (track_id, tag_id, source), 
	FOREIGN KEY(track_id) REFERENCES tracks (id) ON DELETE CASCADE, 
	FOREIGN KEY(tag_id) REFERENCES tags (id) ON DELETE CASCADE
);
CREATE INDEX ix_track_tags_tag_id ON track_tags (tag_id);
CREATE TABLE analysis (
	track_id INTEGER NOT NULL, 
	analyzer_version INTEGER NOT NULL, 
	backend VARCHAR NOT NULL, 
	status VARCHAR NOT NULL, 
	error TEXT, 
	analyzed_at DATETIME NOT NULL, 
	bpm FLOAT, 
	bpm_confidence FLOAT, 
	beat_offset_ms FLOAT, 
	key_root INTEGER, 
	key_mode VARCHAR, 
	camelot VARCHAR, 
	key_confidence FLOAT, 
	loudness_lufs FLOAT, 
	true_peak_db FLOAT, 
	replaygain_gain FLOAT, 
	energy FLOAT, 
	danceability FLOAT, 
	PRIMARY KEY (track_id), 
	FOREIGN KEY(track_id) REFERENCES tracks (id) ON DELETE CASCADE
);
CREATE INDEX ix_analysis_camelot ON analysis (camelot);
CREATE INDEX ix_analysis_analyzer_version ON analysis (analyzer_version);
CREATE INDEX ix_analysis_bpm ON analysis (bpm);
CREATE TABLE play_history (
	id INTEGER NOT NULL, 
	track_id INTEGER NOT NULL, 
	started_at DATETIME NOT NULL, 
	ms_played INTEGER NOT NULL, 
	completed BOOLEAN NOT NULL, 
	skipped BOOLEAN NOT NULL, 
	source VARCHAR NOT NULL, 
	PRIMARY KEY (id), 
	FOREIGN KEY(track_id) REFERENCES tracks (id) ON DELETE CASCADE
);
CREATE INDEX ix_play_history_started_at ON play_history (started_at);
CREATE INDEX ix_play_history_track_id ON play_history (track_id);
CREATE VIRTUAL TABLE search_index USING fts5(
  title, artist, album, label, tags,
  track_id UNINDEXED,
  tokenize = "unicode61 remove_diacritics 2",
  prefix = '2 3'
);
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
	CONSTRAINT ck_job_kind CHECK (kind IN ('download','analyze','scan','metadata_bulk','harvest_import')), 
	CONSTRAINT ck_job_status CHECK (status IN ('queued','running','paused','completed','failed','cancelled'))
);
CREATE INDEX ix_jobs_claim ON jobs (status, priority, created_at);
CREATE INDEX ix_jobs_created_at ON jobs (created_at);
CREATE INDEX ix_jobs_kind ON jobs (kind);
CREATE INDEX ix_jobs_status ON jobs (status);
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
CREATE INDEX ix_job_items_job_id ON job_items (job_id);
CREATE INDEX ix_job_items_claim ON job_items (status, next_attempt_at, job_id, seq);
CREATE INDEX ix_job_items_status ON job_items (status);
CREATE TABLE harvest_sources (
	id INTEGER NOT NULL, 
	kind VARCHAR NOT NULL, 
	identifier VARCHAR NOT NULL, 
	label VARCHAR, 
	url VARCHAR, 
	enabled BOOLEAN NOT NULL, 
	config TEXT NOT NULL, 
	cursor VARCHAR, 
	last_run_at DATETIME, 
	last_error TEXT, 
	items_seen INTEGER NOT NULL, 
	items_new INTEGER NOT NULL, 
	created_at DATETIME NOT NULL, 
	PRIMARY KEY (id), 
	CONSTRAINT uq_harvest_source UNIQUE (kind, identifier)
);
CREATE INDEX ix_harvest_sources_kind ON harvest_sources (kind);
CREATE TABLE harvest_items (
	id INTEGER NOT NULL, 
	url VARCHAR NOT NULL, 
	url_kind VARCHAR NOT NULL, 
	state VARCHAR NOT NULL, 
	title VARCHAR NOT NULL, 
	artist_name VARCHAR NOT NULL, 
	label_name VARCHAR, 
	art_url VARCHAR, 
	release_date VARCHAR, 
	track_count INTEGER, 
	tags TEXT NOT NULL, 
	bc_item_id INTEGER, 
	band_id INTEGER, 
	source_kind VARCHAR, 
	source_label VARCHAR, 
	in_collection BOOLEAN NOT NULL, 
	in_wishlist BOOLEAN NOT NULL, 
	is_free_download BOOLEAN NOT NULL, 
	is_purchasable BOOLEAN NOT NULL, 
	is_preorder BOOLEAN NOT NULL, 
	extract_tier VARCHAR, 
	release_id INTEGER, 
	discovered_at DATETIME NOT NULL, 
	resolved_at DATETIME, 
	PRIMARY KEY (id), 
	CONSTRAINT ck_harvest_state CHECK (state IN ('new','queued','downloaded','ignored','failed','in_library')), 
	FOREIGN KEY(release_id) REFERENCES releases (id) ON DELETE SET NULL
);
CREATE INDEX ix_harvest_items_discovered_at ON harvest_items (discovered_at);
CREATE INDEX ix_harvest_items_in_wishlist ON harvest_items (in_wishlist);
CREATE INDEX ix_harvest_items_in_collection ON harvest_items (in_collection);
CREATE INDEX ix_harvest_items_state ON harvest_items (state);
CREATE INDEX ix_harvest_items_source_kind ON harvest_items (source_kind);
CREATE UNIQUE INDEX ix_harvest_items_url ON harvest_items (url);
CREATE TABLE settings (
	"key" VARCHAR NOT NULL, 
	value TEXT NOT NULL, 
	updated_at DATETIME NOT NULL, 
	PRIMARY KEY ("key")
);
CREATE TABLE playlists (
	id INTEGER NOT NULL, 
	name VARCHAR NOT NULL, 
	description TEXT, 
	kind VARCHAR NOT NULL, 
	rules TEXT, 
	created_at DATETIME NOT NULL, 
	updated_at DATETIME NOT NULL, 
	PRIMARY KEY (id), 
	CONSTRAINT ck_playlist_kind CHECK (kind IN ('manual','smart'))
);
CREATE TABLE dj_sets (
	id INTEGER NOT NULL, 
	name VARCHAR NOT NULL, 
	venue VARCHAR, 
	event_date VARCHAR, 
	target_minutes INTEGER, 
	notes TEXT, 
	status VARCHAR NOT NULL, 
	created_at DATETIME NOT NULL, 
	updated_at DATETIME NOT NULL, pool_sources TEXT NOT NULL DEFAULT '[]', 
	PRIMARY KEY (id), 
	CONSTRAINT ck_set_status CHECK (status IN ('draft','ready','performed','archived'))
);
CREATE TABLE playlist_items (
	id INTEGER NOT NULL, 
	playlist_id INTEGER NOT NULL, 
	track_id INTEGER NOT NULL, 
	position FLOAT NOT NULL, 
	added_at DATETIME NOT NULL, 
	PRIMARY KEY (id), 
	FOREIGN KEY(playlist_id) REFERENCES playlists (id) ON DELETE CASCADE, 
	FOREIGN KEY(track_id) REFERENCES tracks (id) ON DELETE CASCADE
);
CREATE INDEX ix_playlist_items_playlist_id ON playlist_items (playlist_id);
CREATE INDEX ix_playlist_items_order ON playlist_items (playlist_id, position);
CREATE TABLE dj_set_items (
	id INTEGER NOT NULL, 
	set_id INTEGER NOT NULL, 
	track_id INTEGER, 
	position FLOAT NOT NULL, 
	cue_in_ms INTEGER, 
	cue_out_ms INTEGER, 
	tempo_adjust_pct FLOAT NOT NULL, 
	key_lock BOOLEAN NOT NULL, 
	transition_type VARCHAR, 
	transition_beats INTEGER, 
	transition_notes TEXT, 
	energy INTEGER, 
	snapshot TEXT NOT NULL, 
	PRIMARY KEY (id), 
	CONSTRAINT ck_set_energy CHECK (energy IS NULL OR (energy BETWEEN 1 AND 10)), 
	FOREIGN KEY(set_id) REFERENCES dj_sets (id) ON DELETE CASCADE, 
	FOREIGN KEY(track_id) REFERENCES tracks (id) ON DELETE SET NULL
);
CREATE INDEX ix_dj_set_items_set_id ON dj_set_items (set_id);
CREATE INDEX ix_dj_set_items_order ON dj_set_items (set_id, position);
CREATE TABLE blacklist (
	id INTEGER NOT NULL, 
	url_key VARCHAR, 
	artist_key VARCHAR, 
	title_key VARCHAR, 
	url VARCHAR, 
	artist_name VARCHAR NOT NULL, 
	title VARCHAR NOT NULL, 
	reason VARCHAR, 
	added_at DATETIME NOT NULL, 
	PRIMARY KEY (id)
);
CREATE UNIQUE INDEX ix_blacklist_url_key ON blacklist (url_key);
CREATE INDEX ix_blacklist_names ON blacklist (artist_key, title_key);
CREATE INDEX ix_blacklist_added_at ON blacklist (added_at);
CREATE TABLE loved_streams (
	id INTEGER NOT NULL, 
	page_url VARCHAR NOT NULL, 
	track_key VARCHAR NOT NULL, 
	bc_track_id INTEGER, 
	track_index INTEGER, 
	title VARCHAR NOT NULL, 
	artist_name VARCHAR NOT NULL, 
	release_title VARCHAR NOT NULL, 
	art_url VARCHAR, 
	duration_ms INTEGER, 
	added_at DATETIME NOT NULL, 
	PRIMARY KEY (id), 
	CONSTRAINT uq_loved_stream UNIQUE (page_url, track_key)
);
CREATE INDEX ix_loved_streams_page_url ON loved_streams (page_url);
CREATE INDEX ix_loved_streams_added_at ON loved_streams (added_at);
CREATE TABLE favorites (
	id INTEGER NOT NULL, 
	artist_id INTEGER, 
	label_id INTEGER, 
	tag_id INTEGER, 
	created_at DATETIME NOT NULL, 
	PRIMARY KEY (id), 
	CONSTRAINT ck_favorite_one_target CHECK ((artist_id IS NOT NULL) + (label_id IS NOT NULL) + (tag_id IS NOT NULL) = 1), 
	UNIQUE (artist_id), 
	FOREIGN KEY(artist_id) REFERENCES artists (id) ON DELETE CASCADE, 
	UNIQUE (label_id), 
	FOREIGN KEY(label_id) REFERENCES labels (id) ON DELETE CASCADE, 
	UNIQUE (tag_id), 
	FOREIGN KEY(tag_id) REFERENCES tags (id) ON DELETE CASCADE
);
CREATE INDEX ix_favorites_created_at ON favorites (created_at);
CREATE TABLE fans (
	id INTEGER NOT NULL, 
	bc_fan_id INTEGER, 
	username VARCHAR NOT NULL, 
	url VARCHAR NOT NULL, 
	display_name VARCHAR, 
	is_self BOOLEAN NOT NULL, 
	wishlist_count INTEGER, 
	collection_count INTEGER, 
	last_walk_at DATETIME, 
	last_walk TEXT, 
	last_error VARCHAR, 
	created_at DATETIME NOT NULL, 
	PRIMARY KEY (id), 
	UNIQUE (bc_fan_id), 
	UNIQUE (username)
);
CREATE INDEX ix_fans_is_self ON fans (is_self);
CREATE TABLE fan_items (
	fan_id INTEGER NOT NULL, 
	item_id INTEGER NOT NULL, 
	tab VARCHAR NOT NULL, 
	position INTEGER, 
	first_seen_at DATETIME NOT NULL, 
	last_seen_at DATETIME NOT NULL, 
	PRIMARY KEY (fan_id, item_id, tab), 
	FOREIGN KEY(fan_id) REFERENCES fans (id) ON DELETE CASCADE, 
	FOREIGN KEY(item_id) REFERENCES harvest_items (id) ON DELETE CASCADE
);
CREATE INDEX ix_fan_items_order ON fan_items (fan_id, tab, position);
CREATE INDEX ix_releases_source_fan_id ON releases (source_fan_id);
CREATE INDEX ix_harvest_items_release_id ON harvest_items (release_id);
CREATE INDEX ix_tracks_is_snippet ON tracks (is_snippet);
CREATE INDEX ix_releases_snippet_only ON releases (snippet_only);
