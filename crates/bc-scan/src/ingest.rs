//! Turning files into library rows (port of `services/library/ingest.py`) plus the public
//! ingest API used by the download worker.
//!
//! Layering:
//! * [`ingest_batch`] is the DB side: runs inside ONE write transaction over already-read
//!   [`ReadFile`]s (no file or tag I/O in there). Identity rules, upserts, tag counts,
//!   FTS reindex and the `snippet_only` refresh all happen in that transaction.
//! * [`run_pipeline`] is the three-stage engine (stat'd work list -> parallel tag/cover read
//!   -> batched writes) shared by the scanner, the watcher and [`ingest_paths`].
//! * [`ingest_paths`] / [`ingest_dir`] / [`root_for_path`] are the public entry points.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bc_db::rusqlite::{Connection, OptionalExtension, Transaction, params};
use bc_db::util::{name_key, now_db};
use bc_libcore::{ApiError, ApiResult, Ctx};
use bc_types::library::{IngestReport, LibraryChanged, TOPIC_LIBRARY_CHANGED};
use rayon::prelude::*;

use crate::media::{CoverArt, FileTags};

pub const UNKNOWN_ALBUM: &str = "Unknown Album";
/// Rows per write transaction (PLAN §3.3).
pub const COMMIT_BATCH: usize = 200;

/// Whether a title names a Bandcamp teaser clip (the classifier lives in `bc-maint`).
pub fn classify_snippet(title: &str) -> bool {
    bc_maint::snippets::is_snippet_title(title)
}

/// Options for an ingest of explicit paths.
#[derive(Debug, Clone, Default)]
pub struct IngestOptions {
    /// A fan-shelf download: NEW releases get `releases.source_fan_id` (existing ones are
    /// never re-filed by a later arrival).
    pub source_fan_id: Option<i64>,
    /// `Some(true)` forces `tracks.is_snippet` for every ingested track (the download came
    /// from a snippet-only page); `None` classifies by title.
    pub snippet: Option<bool>,
    /// Extract cover art for releases that have none (default true via [`IngestOptions::new`]).
    pub skip_art: bool,
}

impl IngestOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn fan(mut self, fan_id: i64) -> Self {
        self.source_fan_id = Some(fan_id);
        self
    }
}

/// A file whose stat is known (stage a output).
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub path: PathBuf,
    pub size: i64,
    pub mtime_ns: i64,
    pub inode: Option<i64>,
}

/// A file whose tags (and maybe cover) have been read (stage b output).
#[derive(Debug)]
pub struct ReadFile {
    pub item: WorkItem,
    pub tags: FileTags,
    pub cover: Option<Arc<CoverArt>>,
}

/// What the write stage needs to know about the root.
#[derive(Debug, Clone)]
pub struct RootInfo {
    pub id: i64,
    pub path: PathBuf,
    pub kind: String,
}

/// Counters accumulated by ingest.
#[derive(Debug, Default, Clone)]
pub struct IngestTally {
    pub files_added: i64,
    pub files_updated: i64,
    pub tracks_added: i64,
    pub tracks_updated: i64,
    pub track_ids: Vec<i64>,
    pub releases_touched: Vec<i64>,
    pub releases_created: Vec<i64>,
    pub errors: Vec<String>,
}

impl IngestTally {
    fn merge(&mut self, o: IngestTally) {
        self.files_added += o.files_added;
        self.files_updated += o.files_updated;
        self.tracks_added += o.tracks_added;
        self.tracks_updated += o.tracks_updated;
        self.track_ids.extend(o.track_ids);
        self.releases_touched.extend(o.releases_touched);
        self.releases_created.extend(o.releases_created);
        self.errors.extend(o.errors);
    }
}

// ------------------------------------------------------------------ in-transaction caches

#[derive(Clone)]
struct ReleaseRow {
    id: i64,
    label_id: Option<i64>,
    expected: Option<i64>,
    cover_path: Option<String>,
}

#[derive(Default)]
struct Caches {
    artists: HashMap<String, i64>,
    labels: HashMap<String, i64>,
    tags: HashMap<String, i64>,
    releases: HashMap<(String, Option<i64>, Option<i64>), ReleaseRow>,
}

impl Caches {
    fn clear(&mut self) {
        *self = Caches::default();
    }
}

fn get_or_create_named(
    tx: &Transaction<'_>,
    cache: &mut HashMap<String, i64>,
    table: &str,
    name: Option<&str>,
) -> ApiResult<Option<i64>> {
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else { return Ok(None) };
    let key = name_key(name);
    if key.is_empty() {
        return Ok(None);
    }
    if let Some(id) = cache.get(&key) {
        return Ok(Some(*id));
    }
    let existing: Option<i64> = tx
        .prepare_cached(&format!("SELECT id FROM {table} WHERE name_key = ?1"))?
        .query_row([&key], |r| r.get(0))
        .optional()?;
    let id = match existing {
        Some(id) => id,
        None => {
            if table == "artists" {
                tx.prepare_cached("INSERT INTO artists(name, name_key, created_at) VALUES (?1, ?2, ?3)")?
                    .execute(params![name, key, now_db()])?;
            } else {
                tx.prepare_cached("INSERT INTO labels(name, name_key) VALUES (?1, ?2)")?.execute(params![name, key])?;
            }
            tx.last_insert_rowid()
        }
    };
    cache.insert(key, id);
    Ok(Some(id))
}

fn get_or_create_tag(tx: &Transaction<'_>, cache: &mut HashMap<String, i64>, name: &str) -> ApiResult<Option<i64>> {
    let cleaned = name.trim();
    if cleaned.is_empty() {
        return Ok(None);
    }
    let key = name_key(cleaned);
    if key.is_empty() {
        return Ok(None);
    }
    if let Some(id) = cache.get(&key) {
        return Ok(Some(*id));
    }
    let existing: Option<i64> =
        tx.prepare_cached("SELECT id FROM tags WHERE name_key = ?1")?.query_row([&key], |r| r.get(0)).optional()?;
    let id = match existing {
        Some(id) => id,
        None => {
            tx.prepare_cached("INSERT INTO tags(name, name_key, kind, track_count) VALUES (?1, ?2, 'bandcamp', 0)")?
                .execute(params![cleaned, key])?;
            tx.last_insert_rowid()
        }
    };
    cache.insert(key, id);
    Ok(Some(id))
}

/// Identify a release by (album artist, album title, year), then by folder.
///
/// The album artist is preferred over the track artist so a compilation does not fragment
/// into one album per track. The year is the *weakest* part of the identity (Bandcamp edits
/// release dates; the same record re-downloaded later arrives with another year), so a
/// release already holding this folder wins over the year -- otherwise a twin row is
/// created and the old one stays short forever.
/// Returns `(row, created)`.
fn get_or_create_release(
    tx: &Transaction<'_>,
    cache: &mut Caches,
    tags: &FileTags,
    artist_id: Option<i64>,
    folder: &str,
    fan: Option<i64>,
) -> ApiResult<(ReleaseRow, bool)> {
    let title = tags.album.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or(UNKNOWN_ALBUM);
    let key = name_key(title);
    let year = tags.year();
    let ck = (key.clone(), artist_id, year);
    if let Some(r) = cache.releases.get(&ck) {
        return Ok((r.clone(), false));
    }
    let map = |r: &bc_db::rusqlite::Row<'_>| {
        Ok(ReleaseRow { id: r.get(0)?, label_id: r.get(1)?, expected: r.get(2)?, cover_path: r.get(3)? })
    };
    let found = tx
        .prepare_cached(
            "SELECT id, label_id, expected_track_count, cover_path FROM releases
              WHERE title_key = ?1 AND artist_id IS ?2 AND year IS ?3 ORDER BY id LIMIT 1",
        )?
        .query_row(params![key, artist_id, year], map)
        .optional()?;
    if let Some(r) = found {
        cache.releases.insert(ck, r.clone());
        return Ok((r, false));
    }
    let same_folder = tx
        .prepare_cached(
            "SELECT id, label_id, expected_track_count, cover_path FROM releases
              WHERE title_key = ?1 AND artist_id IS ?2 AND folder_path = ?3 ORDER BY id LIMIT 1",
        )?
        .query_row(params![key, artist_id, folder], map)
        .optional()?;
    if let Some(r) = same_folder {
        cache.releases.insert(ck, r.clone());
        return Ok((r, false));
    }
    tx.prepare_cached(
        "INSERT INTO releases(title, title_key, artist_id, kind, release_date, year, folder_path, added_at, source_fan_id, snippet_only)
         VALUES (?1, ?2, ?3, 'album', ?4, ?5, ?6, ?7, ?8, 0)",
    )?
    .execute(params![title, key, artist_id, tags.date, year, folder, now_db(), fan])?;
    let row = ReleaseRow { id: tx.last_insert_rowid(), label_id: None, expected: None, cover_path: None };
    cache.releases.insert(ck, row.clone());
    Ok((row, true))
}

/// Replace a track's file-sourced tags (only `source='file'` rows are touched, so user-added
/// and Bandcamp-sourced tags survive a rescan). `tags.track_count` follows the links exactly,
/// which makes re-ingest idempotent.
fn apply_tags(tx: &Transaction<'_>, cache: &mut HashMap<String, i64>, track_id: i64, genres: &[String], fresh: bool) -> ApiResult<()> {
    let mut wanted: Vec<i64> = Vec::new();
    for raw in genres {
        // Bandcamp genre strings often arrive as "techno; dub techno".
        for part in raw.replace(';', ",").split(',') {
            if let Some(id) = get_or_create_tag(tx, cache, part)?
                && !wanted.contains(&id)
            {
                wanted.push(id);
            }
        }
    }
    let current: Vec<i64> = if fresh {
        Vec::new()
    } else {
        let mut st = tx.prepare_cached("SELECT tag_id FROM track_tags WHERE track_id = ?1 AND source = 'file'")?;
        st.query_map([track_id], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    for id in &current {
        if !wanted.contains(id) {
            tx.prepare_cached("DELETE FROM track_tags WHERE track_id = ?1 AND tag_id = ?2 AND source = 'file'")?
                .execute(params![track_id, id])?;
            tx.prepare_cached("UPDATE tags SET track_count = track_count - 1 WHERE id = ?1 AND track_count > 0")?
                .execute([id])?;
        }
    }
    for id in &wanted {
        if !current.contains(id) {
            tx.prepare_cached("INSERT INTO track_tags(track_id, tag_id, source, weight) VALUES (?1, ?2, 'file', 1.0)")?
                .execute(params![track_id, id])?;
            tx.prepare_cached("UPDATE tags SET track_count = track_count + 1 WHERE id = ?1")?.execute([id])?;
        }
    }
    Ok(())
}

struct ExistingFile {
    id: i64,
    track_id: Option<i64>,
    tag_hash: Option<String>,
}

struct One {
    track_id: i64,
    release_id: i64,
    cover: Option<Arc<CoverArt>>,
    cover_missing: bool,
}

#[allow(clippy::too_many_arguments)]
fn ingest_one(
    tx: &Transaction<'_>,
    cache: &mut Caches,
    root: &RootInfo,
    rf: &ReadFile,
    opts: &IngestOptions,
    tally: &mut IngestTally,
    created: &mut HashSet<i64>,
) -> ApiResult<One> {
    let path = rf.item.path.to_str().ok_or_else(|| ApiError::bad("non-UTF-8 path"))?;
    let tags = &rf.tags;
    // Resolve the existing row by path: the scanner could pass it in, but other callers (the
    // download worker re-indexing a path) cannot, and without this lookup both the file
    // INSERT and the track would be duplicated.
    let existing: Option<ExistingFile> = tx
        .prepare_cached("SELECT id, track_id, tag_hash FROM files WHERE path = ?1")?
        .query_row([path], |r| Ok(ExistingFile { id: r.get(0)?, track_id: r.get(1)?, tag_hash: r.get(2)? }))
        .optional()?;

    let now = now_db();
    // Unchanged tags (only the stat moved, or the file re-appeared): refresh the file row only.
    if let Some(ex) = &existing
        && let Some(track_id) = ex.track_id
        && ex.tag_hash.as_deref() == Some(tags.tag_hash.as_str())
    {
        let release_id: Option<i64> =
            tx.prepare_cached("SELECT release_id FROM tracks WHERE id = ?1")?.query_row([track_id], |r| r.get(0)).optional()?.flatten();
        if let Some(release_id) = release_id {
            update_file_row(tx, ex.id, track_id, rf, &now)?;
            tally.files_updated += 1;
            let (cover_missing, cover) = cover_state(tx, release_id, rf)?;
            return Ok(One { track_id, release_id, cover, cover_missing });
        }
    }

    let artist_id = get_or_create_named(tx, &mut cache.artists, "artists", tags.artist.as_deref())?;
    let album_artist_id =
        get_or_create_named(tx, &mut cache.artists, "artists", tags.album_artist.as_deref())?.or(artist_id);
    let label_id = get_or_create_named(tx, &mut cache.labels, "labels", tags.label.as_deref())?;
    let folder = rf.item.path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let (mut release, release_created) =
        get_or_create_release(tx, cache, tags, album_artist_id, &folder, opts.source_fan_id)?;
    if release_created {
        created.insert(release.id);
    }
    let ck = (
        name_key(tags.album.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or(UNKNOWN_ALBUM)),
        album_artist_id,
        tags.year(),
    );
    if let Some(l) = label_id
        && release.label_id.is_none()
    {
        tx.prepare_cached("UPDATE releases SET label_id = ?1 WHERE id = ?2")?.execute(params![l, release.id])?;
        release.label_id = Some(l);
    }
    // The files' own "3/12" numbering says how long the record should be. Raised, never
    // lowered: on a multi-disc release each disc states its own total.
    if let Some(total) = tags.track_total
        && total > release.expected.unwrap_or(0)
    {
        tx.prepare_cached("UPDATE releases SET expected_track_count = ?1 WHERE id = ?2")?.execute(params![total, release.id])?;
        release.expected = Some(total);
    }
    if let Some(c) = cache.releases.get_mut(&ck) {
        *c = release.clone();
    }

    let stem = rf.item.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    // A file row whose track vanished (should not happen with FKs on) is treated as new.
    let existing_track: Option<(i64, String)> = match existing.as_ref().and_then(|e| e.track_id) {
        Some(id) => tx
            .prepare_cached("SELECT title FROM tracks WHERE id = ?1")?
            .query_row([id], |r| r.get::<_, String>(0))
            .optional()?
            .map(|t| (id, t)),
        None => None,
    };
    let track_id = match existing_track {
        None => {
            let title = tags.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or(stem);
            // The title is the only place a Bandcamp teaser clip says what it is, and it is
            // right here -- so read it now instead of re-reading every title later.
            let snippet = opts.snippet.unwrap_or_else(|| classify_snippet(&title));
            tx.prepare_cached(
                "INSERT INTO tracks(release_id, artist_id, title, title_key, track_no, disc_no, duration_ms, isrc,
                                    loved, play_count, skip_count, added_at, is_snippet)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 0, 0, ?9, ?10)",
            )?
            .execute(params![
                release.id,
                artist_id,
                title,
                name_key(&title),
                tags.track_no,
                tags.disc_no.unwrap_or(1),
                tags.duration_ms,
                tags.isrc,
                now,
                snippet as i64
            ])?;
            tally.tracks_added += 1;
            let id = tx.last_insert_rowid();
            apply_tags(tx, &mut cache.tags, id, &tags.genres, true)?;
            id
        }
        Some((id, cur_title)) => {
            let title = tags.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or(cur_title);
            let snippet = opts.snippet.unwrap_or_else(|| classify_snippet(&title));
            tx.prepare_cached(
                "UPDATE tracks SET title = ?1, title_key = ?2, is_snippet = ?3, track_no = ?4, disc_no = ?5,
                                   duration_ms = ?6, release_id = ?7, artist_id = ?8
                  WHERE id = ?9",
            )?
            .execute(params![
                title,
                name_key(&title),
                snippet as i64,
                tags.track_no,
                tags.disc_no.unwrap_or(1),
                tags.duration_ms,
                release.id,
                artist_id,
                id
            ])?;
            tally.tracks_updated += 1;
            apply_tags(tx, &mut cache.tags, id, &tags.genres, false)?;
            id
        }
    };

    match &existing {
        Some(ex) => {
            update_file_row(tx, ex.id, track_id, rf, &now)?;
            tally.files_updated += 1;
            conn_set_tag_hash(tx, ex.id, tags)?;
        }
        None => {
            let rel = rf.item.path.strip_prefix(&root.path).unwrap_or(&rf.item.path).to_string_lossy().into_owned();
            let ext = rf.item.path.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default();
            tx.prepare_cached(
                "INSERT INTO files(track_id, root_id, path, rel_path, ext, codec, bitrate, sample_rate, channels,
                                   size_bytes, mtime_ns, inode, tag_hash, first_seen_at, last_seen_at, missing_since)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14, NULL)",
            )?
            .execute(params![
                track_id,
                root.id,
                path,
                rel,
                ext,
                tags.codec,
                tags.bitrate,
                tags.sample_rate,
                tags.channels,
                rf.item.size,
                rf.item.mtime_ns,
                rf.item.inode,
                tags.tag_hash,
                now
            ])?;
            tally.files_added += 1;
        }
    }

    let cover_missing = release.cover_path.is_none();
    let cover = if cover_missing { rf.cover.clone() } else { None };
    Ok(One { track_id, release_id: release.id, cover, cover_missing })
}

fn cover_state(tx: &Transaction<'_>, release_id: i64, rf: &ReadFile) -> ApiResult<(bool, Option<Arc<CoverArt>>)> {
    let cp: Option<String> = tx
        .prepare_cached("SELECT cover_path FROM releases WHERE id = ?1")?
        .query_row([release_id], |r| r.get(0))
        .optional()?
        .flatten();
    let missing = cp.is_none();
    Ok((missing, if missing { rf.cover.clone() } else { None }))
}

fn conn_set_tag_hash(tx: &Transaction<'_>, file_id: i64, tags: &FileTags) -> ApiResult<()> {
    tx.prepare_cached("UPDATE files SET tag_hash = ?1 WHERE id = ?2")?.execute(params![tags.tag_hash, file_id])?;
    Ok(())
}

fn update_file_row(tx: &Transaction<'_>, file_id: i64, track_id: i64, rf: &ReadFile, now: &str) -> ApiResult<()> {
    let t = &rf.tags;
    tx.prepare_cached(
        "UPDATE files SET track_id = ?1, codec = ?2, bitrate = ?3, sample_rate = ?4, channels = ?5,
                size_bytes = ?6, mtime_ns = ?7, inode = ?8, last_seen_at = ?9, missing_since = NULL
          WHERE id = ?10",
    )?
    .execute(params![track_id, t.codec, t.bitrate, t.sample_rate, t.channels, rf.item.size, rf.item.mtime_ns, rf.item.inode, now, file_id])?;
    Ok(())
}

/// Recompute `releases.snippet_only` for `ids`: a release is snippet-only when it holds tracks
/// and every one is a snippet. Mixed releases are left alone on purpose.
pub fn refresh_snippet_only(tx: &Transaction<'_>, ids: &[i64]) -> ApiResult<()> {
    for chunk in ids.chunks(400) {
        let ph = vec!["?"; chunk.len()].join(",");
        tx.execute(
            &format!(
                "UPDATE releases SET snippet_only = COALESCE((SELECT MIN(t.is_snippet) = 1 AND COUNT(*) > 0 FROM tracks t WHERE t.release_id = releases.id), 0)
                  WHERE id IN ({ph}) AND snippet_only IS NOT COALESCE((SELECT MIN(t.is_snippet) = 1 AND COUNT(*) > 0 FROM tracks t WHERE t.release_id = releases.id), 0)"
            ),
            bc_db::rusqlite::params_from_iter(chunk.iter()),
        )?;
    }
    Ok(())
}

/// Output of one write batch.
#[derive(Debug, Default)]
pub struct BatchOut {
    pub tally: IngestTally,
    /// Releases with no cover yet that this batch produced art for.
    pub needs_art: Vec<(i64, Arc<CoverArt>)>,
}

/// Ingest a batch of read files inside the caller's write transaction. One bad file does not
/// poison the rest: each runs in a savepoint and its error is reported in the tally.
pub fn ingest_batch(tx: &Transaction<'_>, root: &RootInfo, files: &[ReadFile], opts: &IngestOptions) -> ApiResult<BatchOut> {
    let mut cache = Caches::default();
    let mut out = BatchOut::default();
    let mut created: HashSet<i64> = HashSet::new();
    let mut touched: Vec<i64> = Vec::new();
    let mut touched_set: HashSet<i64> = HashSet::new();
    let mut art_seen: HashSet<i64> = HashSet::new();
    for rf in files {
        tx.execute_batch("SAVEPOINT ingest_one")?;
        let mut sub = IngestTally::default();
        match ingest_one(tx, &mut cache, root, rf, opts, &mut sub, &mut created) {
            Ok(one) => {
                tx.execute_batch("RELEASE ingest_one")?;
                out.tally.merge(sub);
                out.tally.track_ids.push(one.track_id);
                if touched_set.insert(one.release_id) {
                    touched.push(one.release_id);
                }
                if one.cover_missing
                    && let Some(c) = one.cover
                    && art_seen.insert(one.release_id)
                {
                    out.needs_art.push((one.release_id, c));
                }
            }
            Err(e) => {
                tx.execute_batch("ROLLBACK TO ingest_one; RELEASE ingest_one")?;
                cache.clear();
                tracing::warn!(path = %rf.item.path.display(), error = %e, "ingest failed");
                out.tally.errors.push(format!("{}: {e}", rf.item.path.display()));
            }
        }
    }
    bc_db::fts::reindex_tracks(tx, &out.tally.track_ids).map_err(ApiError::from)?;
    refresh_snippet_only(tx, &touched)?;
    out.tally.releases_created = created.into_iter().collect();
    out.tally.releases_created.sort_unstable();
    out.tally.releases_touched = touched;
    Ok(out)
}

// ------------------------------------------------------------------ stage b: read

/// Read tags for `items` in parallel and, for folders whose release may lack art, one cover per
/// folder. `art_folders` holds folders that already have art (skipped). No DB access here.
pub fn read_items(items: Vec<WorkItem>, art_folders: &HashSet<String>, want_art: bool) -> Vec<ReadFile> {
    let mut read: Vec<ReadFile> = items
        .into_par_iter()
        .map(|item| {
            let tags = crate::media::read_file_tags(&item.path);
            ReadFile { item, tags, cover: None }
        })
        .collect();
    if !want_art {
        return read;
    }
    // folder -> indexes of its files
    let mut by_folder: HashMap<PathBuf, Vec<usize>> = HashMap::new();
    for (i, rf) in read.iter().enumerate() {
        if let Some(p) = rf.item.path.parent() {
            by_folder.entry(p.to_path_buf()).or_default().push(i);
        }
    }
    let todo: Vec<(PathBuf, Vec<usize>)> = by_folder
        .into_iter()
        .filter(|(f, _)| !art_folders.contains(f.to_string_lossy().as_ref()))
        .collect();
    let found: Vec<(Vec<usize>, Arc<CoverArt>)> = todo
        .into_par_iter()
        .filter_map(|(folder, idxs)| {
            let paths: Vec<&Path> = idxs.iter().map(|&i| read[i].item.path.as_path()).collect();
            crate::media::find_cover(&folder, &paths).map(|c| (idxs, Arc::new(c)))
        })
        .collect();
    for (idxs, cover) in found {
        for i in idxs {
            read[i].cover = Some(cover.clone());
        }
    }
    read
}

// ------------------------------------------------------------------ art writes

/// Write the WebP files for `(release_id, cover)` pairs (outside any tx, in parallel) and insert
/// the `artwork` rows + `releases.cover_path` in one small batch. Returns releases that got art.
pub fn store_art(ctx: &Ctx, jobs: Vec<(i64, Arc<CoverArt>)>) -> ApiResult<usize> {
    if jobs.is_empty() {
        return Ok(0);
    }
    let art_dir = ctx.config.art_dir();
    let written: Vec<(i64, Arc<CoverArt>, u8)> = jobs
        .into_par_iter()
        .filter_map(|(rid, c)| match crate::media::write_cover_files(&art_dir, rid, &c) {
            Ok(mask) => Some((rid, c, mask)),
            Err(e) => {
                tracing::warn!(release_id = rid, error = %e, "writing cover art failed");
                None
            }
        })
        .collect();
    let n = written.len();
    let art_dir2 = art_dir.clone();
    ctx.db
        .write_chunks(written, COMMIT_BATCH, move |tx, chunk| {
            for (rid, c, mask) in chunk {
                let full = crate::media::full_art_path(&art_dir2, *rid);
                let p = &c.processed;
                tx.prepare_cached(
                    "INSERT INTO artwork(release_id, hash, version, blurhash, color, width, height, sizes, source, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, CURRENT_TIMESTAMP)
                     ON CONFLICT(release_id) DO UPDATE SET hash=excluded.hash, version=excluded.version, blurhash=excluded.blurhash,
                       color=excluded.color, width=excluded.width, height=excluded.height, sizes=excluded.sizes,
                       source=excluded.source, updated_at=excluded.updated_at",
                )?
                .execute(params![
                    rid,
                    p.hash,
                    crate::media::version_string(&c.processed, *mask),
                    p.blurhash,
                    p.color,
                    p.width,
                    p.height,
                    *mask as i64,
                    c.source
                ])?;
                tx.prepare_cached("UPDATE releases SET cover_path = ?1 WHERE id = ?2 AND cover_path IS NULL")?
                    .execute(params![full.to_string_lossy(), rid])?;
            }
            Ok(())
        })
        .map_err(ApiError::from)?;
    Ok(n)
}

// ------------------------------------------------------------------ pipeline

/// Totals of a pipeline run.
#[derive(Debug, Default)]
pub struct PipelineOut {
    pub tally: IngestTally,
    pub cancelled: bool,
}

/// Progress callback: `(phase, done_files, total_files)`.
pub type Tick<'a> = &'a mut dyn FnMut(&'static str, i64, i64);

/// Stages b+c over a stat'd work list. Reading chunk N+1 overlaps writing chunk N; chunks are
/// aligned to folder boundaries so a folder's cover is read at most once.
pub fn run_pipeline(
    ctx: &Ctx,
    root: &RootInfo,
    mut work: Vec<WorkItem>,
    opts: &IngestOptions,
    cancel: &AtomicBool,
    tick: Tick<'_>,
) -> ApiResult<PipelineOut> {
    let mut out = PipelineOut::default();
    if work.is_empty() {
        return Ok(out);
    }
    work.sort_by(|a, b| a.path.cmp(&b.path));
    let total = work.len() as i64;
    let art_folders: HashSet<String> = if opts.skip_art {
        HashSet::new()
    } else {
        ctx.db
            .read_with::<_, ApiError>(|c| {
                let mut st = c.prepare("SELECT DISTINCT folder_path FROM releases WHERE cover_path IS NOT NULL AND folder_path IS NOT NULL")?;
                Ok(st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<HashSet<_>, _>>()?)
            })?
    };
    // Chunk boundaries on folder changes.
    let mut chunks: Vec<Vec<WorkItem>> = Vec::new();
    let mut cur: Vec<WorkItem> = Vec::new();
    for it in work {
        if cur.len() >= COMMIT_BATCH && cur.last().map(|l| l.path.parent()) != Some(it.path.parent()) {
            chunks.push(std::mem::take(&mut cur));
        }
        cur.push(it);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }

    let (tx, rx) = crossbeam_channel::bounded::<Vec<ReadFile>>(1);
    let want_art = !opts.skip_art;
    let mut done = 0i64;
    let mut fatal: Option<ApiError> = None;
    std::thread::scope(|s| {
        let art_folders = &art_folders;
        s.spawn(move || {
            for chunk in chunks {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let read = read_items(chunk, art_folders, want_art);
                if tx.send(read).is_err() {
                    break;
                }
            }
        });
        while let Ok(files) = rx.recv() {
            tick("write", done, total);
            let n = files.len() as i64;
            let (r, o) = (root.clone(), opts.clone());
            let res = ctx.db.write_with::<_, ApiError>(move |t| {
                let b = ingest_batch(t, &r, &files, &o)?;
                Ok(b)
            });
            match res {
                Ok(b) => {
                    out.tally.merge(b.tally);
                    if let Err(e) = store_art(ctx, b.needs_art) {
                        tracing::warn!(error = %e, "storing cover art failed");
                    }
                }
                Err(e) => {
                    fatal = Some(e);
                    cancel.store(true, Ordering::Relaxed);
                    break;
                }
            }
            done += n;
            tick("read", done, total);
            if cancel.load(Ordering::Relaxed) {
                out.cancelled = true;
                break;
            }
        }
        drop(rx);
    });
    if let Some(e) = fatal {
        return Err(e);
    }
    if cancel.load(Ordering::Relaxed) && done < total {
        out.cancelled = true;
    }
    Ok(out)
}

// ------------------------------------------------------------------ roots lookups

pub(crate) fn load_roots(c: &Connection) -> ApiResult<Vec<RootInfo>> {
    let mut st = c.prepare("SELECT id, path, kind FROM library_roots ORDER BY id")?;
    Ok(st
        .query_map([], |r| Ok(RootInfo { id: r.get(0)?, path: PathBuf::from(r.get::<_, String>(1)?), kind: r.get(2)? }))?
        .collect::<Result<_, _>>()?)
}

/// The root whose path is the longest ancestor of `path` (roots are compared on canonical-ish
/// paths as stored).
pub fn root_for_path(ctx: &Ctx, path: &Path) -> ApiResult<Option<i64>> {
    let roots = ctx.read(load_roots)?;
    Ok(best_root(&roots, path).map(|r| r.id))
}

pub(crate) fn best_root<'a>(roots: &'a [RootInfo], path: &Path) -> Option<&'a RootInfo> {
    let canon = std::fs::canonicalize(path).ok();
    roots
        .iter()
        .filter(|r| path.starts_with(&r.path) || canon.as_ref().is_some_and(|c| c.starts_with(&r.path)))
        .max_by_key(|r| r.path.components().count())
}

/// Drop the files removed from the library (see [`bc_maint::excluded`]) from a work list; returns
/// how many went. Every way into the library calls this before reading a tag.
pub(crate) fn drop_excluded(ctx: &Ctx, items: &mut Vec<WorkItem>) -> ApiResult<usize> {
    let excluded = ctx.read(bc_maint::excluded::paths)?;
    if excluded.is_empty() {
        return Ok(0);
    }
    let before = items.len();
    items.retain(|it| it.path.to_str().is_none_or(|p| !excluded.contains(p)));
    Ok(before - items.len())
}

pub(crate) fn stat_item(path: &Path) -> std::io::Result<WorkItem> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path)?;
    Ok(WorkItem {
        path: path.to_path_buf(),
        size: m.len() as i64,
        mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
        inode: Some(m.ino() as i64),
    })
}

fn publish_changed(ctx: &Ctx, rep: &IngestReport) {
    if rep.track_ids.is_empty() {
        return;
    }
    ctx.bus.invalidate("track", rep.track_ids.clone());
    ctx.bus.invalidate("release", rep.release_ids.clone());
    let changed = LibraryChanged {
        added_tracks: Vec::new(),
        changed_tracks: rep.track_ids.clone(),
        ..Default::default()
    };
    let mut changed = changed;
    changed.added_tracks = if rep.tracks_added > 0 { rep.track_ids.clone() } else { Vec::new() };
    ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &changed);
}

/// Index explicit files under `root_id`: stat + read + write, idempotent (re-ingesting a path
/// updates in place). Files not under an audio extension, or unreadable, are reported in
/// `errors`. Publishes `invalidate` + `library.changed`. **Blocking**: call from
/// `spawn_blocking` / a worker thread (or use [`ingest_paths_async`]).
pub fn ingest_paths(ctx: &Ctx, root_id: i64, paths: &[PathBuf], opts: &IngestOptions) -> ApiResult<IngestReport> {
    let root = ctx
        .read(|c| Ok(load_roots(c)?.into_iter().find(|r| r.id == root_id)))?
        .ok_or_else(|| ApiError::not_found(format!("root {root_id} not found")))?;
    let mut report = IngestReport::default();
    let mut work = Vec::new();
    let mut seen = HashSet::new();
    for p in paths {
        if !seen.insert(p.clone()) {
            continue;
        }
        if !crate::media::is_audio_path(p) {
            continue;
        }
        match stat_item(p) {
            Ok(it) => work.push(it),
            Err(e) => report.errors.push(format!("{}: {e}", p.display())),
        }
    }
    drop_excluded(ctx, &mut work)?;
    report.files_seen = work.len() as i64;
    let cancel = AtomicBool::new(false);
    let mut tick = |_: &'static str, _: i64, _: i64| {};
    let out = run_pipeline(ctx, &root, work, opts, &cancel, &mut tick)?;
    let t = out.tally;
    report.files_added = t.files_added;
    report.files_updated = t.files_updated;
    report.tracks_added = t.tracks_added;
    report.tracks_updated = t.tracks_updated;
    report.track_ids = t.track_ids;
    report.release_ids = t.releases_touched;
    report.releases_created = t.releases_created;
    report.errors.extend(t.errors);
    publish_changed(ctx, &report);
    Ok(report)
}

/// Async wrapper around [`ingest_paths`].
pub async fn ingest_paths_async(ctx: &Ctx, root_id: i64, paths: Vec<PathBuf>, opts: IngestOptions) -> ApiResult<IngestReport> {
    let ctx = ctx.clone();
    tokio::task::spawn_blocking(move || ingest_paths(&ctx, root_id, &paths, &opts)).await.map_err(ApiError::internal)?
}

/// Ingest every audio file under `dir` (found by walking it) into the root that contains it by
/// path ancestry. Errors with `BadRequest` when no root contains `dir`.
pub fn ingest_dir(ctx: &Ctx, dir: &Path, opts: &IngestOptions) -> ApiResult<IngestReport> {
    let root_id = root_for_path(ctx, dir)?.ok_or_else(|| ApiError::bad(format!("no library root contains {}", dir.display())))?;
    let (items, _failed) = crate::scanner::walk_audio(dir);
    let paths: Vec<PathBuf> = items.into_iter().map(|i| i.path).collect();
    ingest_paths(ctx, root_id, &paths, opts)
}

/// Async wrapper around [`ingest_dir`].
pub async fn ingest_dir_async(ctx: &Ctx, dir: PathBuf, opts: IngestOptions) -> ApiResult<IngestReport> {
    let ctx = ctx.clone();
    tokio::task::spawn_blocking(move || ingest_dir(&ctx, &dir, &opts)).await.map_err(ApiError::internal)?
}
