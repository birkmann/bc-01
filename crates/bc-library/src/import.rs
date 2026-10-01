//! The one-shot legacy importer (`bc import`): SQLite-backup the old Python app's DB into
//! `$BC_DATA_DIR/library.db`, migrate it, run the old startup repair passes ONCE as versioned data
//! migrations, move the Bandcamp cookie into the keyring, queue the lazy art re-encode, and write a
//! verification report (row counts, FTS parity, playlist/set checksums). PLAN §4.2.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bc_core::{Config, EventBus};
use bc_db::rusqlite::{Connection, OpenFlags, backup};
use bc_db::{Db, migrate};
use bc_libcore::{ApiError, Ctx};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// The legacy `library.db`, or the legacy data dir containing it (and `backups/`, `cache/art/`).
    pub from: Option<PathBuf>,
    /// Replace an existing `library.db` (it is moved to `library.db.pre-import-<ts>`).
    pub force: bool,
    /// Skip the (slow) repair passes (tests).
    pub skip_repairs: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TableCount {
    pub table: String,
    pub legacy: i64,
    pub new: i64,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StepTiming {
    pub step: String,
    pub ms: u64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FtsParity {
    pub queries: usize,
    pub identical: usize,
    pub mismatches: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ImportReport {
    pub source: String,
    pub target: String,
    pub counts: Vec<TableCount>,
    pub fts: FtsParity,
    pub playlists_checksum_legacy: String,
    pub playlists_checksum_new: String,
    pub cookie_moved: Option<String>,
    pub backups_copied: usize,
    pub art_rows: i64,
    pub timings: Vec<StepTiming>,
    pub total_ms: u64,
    pub ok: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("{0}")]
    Msg(String),
    #[error(transparent)]
    Db(#[from] bc_db::DbError),
    #[error(transparent)]
    Sqlite(#[from] bc_db::rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Api(#[from] ApiError),
}

type Result<T> = std::result::Result<T, ImportError>;

/// Tables compared legacy vs new (every legacy table that carries user data).
pub const COMPARED_TABLES: &[&str] = &[
    "library_roots", "artists", "labels", "tags", "releases", "tracks", "files", "track_tags", "analysis", "play_history", "search_index",
    "jobs", "job_items", "harvest_sources", "harvest_items", "settings", "playlists", "dj_sets", "playlist_items", "dj_set_items", "blacklist",
    "loved_streams", "favorites", "fans", "fan_items",
];

/// Resolve `--from`/`BC_LEGACY_DB` to (db file, data dir).
pub fn resolve_source(from: &Path) -> Result<(PathBuf, PathBuf)> {
    if from.is_dir() {
        let db = from.join("library.db");
        if !db.is_file() {
            return Err(ImportError::Msg(format!("no library.db in {}", from.display())));
        }
        return Ok((db, from.to_path_buf()));
    }
    if !from.is_file() {
        return Err(ImportError::Msg(format!("legacy database not found: {}", from.display())));
    }
    Ok((from.to_path_buf(), from.parent().map(Path::to_path_buf).unwrap_or_default()))
}

fn timed<T>(timings: &mut Vec<StepTiming>, step: &str, f: impl FnOnce() -> Result<(T, String)>) -> Result<T> {
    let t = Instant::now();
    tracing::info!(step, "import step");
    let (v, detail) = f()?;
    let ms = t.elapsed().as_millis() as u64;
    tracing::info!(step, ms, %detail, "import step done");
    timings.push(StepTiming { step: step.into(), ms, detail });
    Ok(v)
}

/// Run the import. Blocking (call from `spawn_blocking` / a CLI thread). Progress lines go to `log`.
pub fn run_import(config: &Config, opts: &ImportOptions, log: &dyn Fn(&str)) -> Result<ImportReport> {
    let total_t = Instant::now();
    let from = opts
        .from
        .clone()
        .or_else(|| config.legacy_db.clone())
        .ok_or_else(|| ImportError::Msg("no source: pass --from <legacy dir or library.db> or set BC_LEGACY_DB".into()))?;
    let (src_db, src_dir) = resolve_source(&from)?;
    config.ensure_dirs()?;
    let target = config.db_path();
    let staging = target.with_file_name("library.db.importing");
    if target.exists() {
        if !opts.force {
            return Err(ImportError::Msg(format!("{} already exists (use --force to replace it)", target.display())));
        }
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        for ext in ["", "-wal", "-shm"] {
            let p = PathBuf::from(format!("{}{ext}", target.display()));
            if p.exists() {
                std::fs::rename(&p, PathBuf::from(format!("{}.pre-import-{ts}{ext}", target.display())))?;
            }
        }
    }
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", staging.display()));
    }
    let mut report = ImportReport { source: src_db.display().to_string(), target: target.display().to_string(), ..Default::default() };
    let mut timings: Vec<StepTiming> = vec![];

    // 1. Online backup of the legacy DB (consistent even if the old app is still running; opened read-only).
    timed(&mut timings, "backup", || {
        log("backing up the legacy database ...");
        let src = Connection::open_with_flags(&src_db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut dst = Connection::open(&staging)?;
        {
            let b = backup::Backup::new(&src, &mut dst)?;
            b.run_to_completion(4096, Duration::from_millis(0), None)?;
        }
        dst.execute_batch("PRAGMA journal_mode=WAL")?;
        Ok(((), format!("{} -> {}", src_db.display(), staging.display())))
    })?;

    // 2. Open the copy: this applies the schema migrations (v1 stamp, v2 additions, req_*.sql).
    let db = timed(&mut timings, "migrate", || {
        let db = Db::open(&staging)?;
        let v: i64 = db.read(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?))?;
        Ok((db, format!("schema version {v}")))
    })?;
    let ctx = Ctx::new(db.clone(), Arc::new(EventBus::new()), config.clone());
    post_steps(&ctx, config, opts, &src_dir, log, &mut timings, &mut report)?;

    // 7. Verification.
    let verify_t = Instant::now();
    verify(&db, &src_db, &mut report)?;
    report.art_rows = db.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM artwork WHERE sizes = 0", [], |r| r.get(0))?))?;
    timings.push(StepTiming { step: "verify".into(), ms: verify_t.elapsed().as_millis() as u64, detail: String::new() });

    // 8. Seal: checkpoint, close, rename into place.
    db.checkpoint()?;
    drop(ctx);
    drop(db);
    std::thread::sleep(Duration::from_millis(200));
    for ext in ["", "-wal", "-shm"] {
        let from_p = PathBuf::from(format!("{}{ext}", staging.display()));
        if from_p.exists() {
            std::fs::rename(&from_p, PathBuf::from(format!("{}{ext}", target.display())))?;
        }
    }
    report.timings = timings;
    report.total_ms = total_t.elapsed().as_millis() as u64;
    report.ok = report.counts.iter().all(|c| c.ok) && report.fts.identical == report.fts.queries && report.playlists_checksum_legacy == report.playlists_checksum_new;
    Ok(report)
}


/// Whether the live database holds any library content (the import route refuses to overwrite it without `force`).
pub fn is_empty(c: &Connection) -> bool {
    let n = |t: &str| -> i64 { c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0)).unwrap_or(0) };
    n("tracks") == 0 && n("releases") == 0 && n("playlists") == 0 && n("dj_sets") == 0
}

/// Import into the RUNNING database (what `POST /library/import` does): the legacy DB is restored over the live
/// writer connection with the SQLite backup API, then migrated and post-processed like the offline import. With
/// `force` over a non-empty database the previous content is first copied to `library.db.pre-import-<ts>`.
/// Blocking; progress lines go to `log`.
pub fn run_import_live(ctx: &Ctx, opts: &ImportOptions, log: &dyn Fn(&str)) -> Result<ImportReport> {
    let total_t = Instant::now();
    let config = (*ctx.config).clone();
    let from = opts
        .from
        .clone()
        .or_else(|| config.legacy_db.clone())
        .ok_or_else(|| ImportError::Msg("no source: pass `from` (legacy data dir or library.db) or set BC_LEGACY_DB".into()))?;
    let (src_db, src_dir) = resolve_source(&from)?;
    let empty = ctx.read(|c| Ok(is_empty(c)))?;
    if !empty && !opts.force {
        return Err(ImportError::Msg("the library is not empty; pass force=true to replace it".into()));
    }
    config.ensure_dirs()?;
    let mut report = ImportReport { source: src_db.display().to_string(), target: config.db_path().display().to_string(), ..Default::default() };
    let mut timings: Vec<StepTiming> = vec![];
    if !empty {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let safety = config.db_path().with_file_name(format!("library.db.pre-import-{ts}"));
        timed(&mut timings, "safety-copy", || {
            log("copying the current library aside ...");
            let s2 = safety.clone();
            ctx.db.with_writer(move |conn| {
                let mut dst = Connection::open(&s2)?;
                let b = backup::Backup::new(conn, &mut dst)?;
                b.run_to_completion(4096, Duration::from_millis(0), None)?;
                Ok(())
            })?;
            Ok(((), safety.display().to_string()))
        })?;
    }
    timed(&mut timings, "restore", || {
        log("restoring the legacy database ...");
        let s2 = src_db.clone();
        ctx.db.with_writer(move |conn| {
            let src = Connection::open_with_flags(&s2, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            {
                let b = backup::Backup::new(&src, conn)?;
                b.run_to_completion(4096, Duration::from_millis(0), None)?;
            }
            let _ = conn.execute_batch("PRAGMA journal_mode=WAL");
            migrate::migrate(conn)?;
            Ok(())
        })?;
        Ok(((), format!("{} -> live database", src_db.display())))
    })?;
    post_steps(ctx, &config, opts, &src_dir, log, &mut timings, &mut report)?;
    verify(&ctx.db, &src_db, &mut report)?;
    report.art_rows = ctx.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM artwork WHERE sizes = 0", [], |r| r.get(0))?))?;
    ctx.bus.invalidate("track", vec![]);
    ctx.bus.invalidate("release", vec![]);
    ctx.bus.invalidate("artist", vec![]);
    ctx.bus.invalidate("label", vec![]);
    ctx.bus.publish(bc_types::library::TOPIC_LIBRARY_CHANGED, &bc_types::library::LibraryChanged { scope: Some("import".into()), ..Default::default() });
    report.timings = timings;
    report.total_ms = total_t.elapsed().as_millis() as u64;
    report.ok = report.counts.iter().all(|c| c.ok) && report.fts.identical == report.fts.queries && report.playlists_checksum_legacy == report.playlists_checksum_new;
    Ok(report)
}

/// Steps 3-6 shared by the offline and the live import: data migrations, cookie, undo journals, ANALYZE.
fn post_steps(ctx: &Ctx, config: &Config, opts: &ImportOptions, src_dir: &Path, log: &dyn Fn(&str), timings: &mut Vec<StepTiming>, report: &mut ImportReport) -> Result<()> {
    let db = ctx.db.clone();
    // 3. Versioned data migrations (each recorded in `settings`, never re-run).
    for dm in data_migrations(opts) {
        let done = db.read(|c| Ok(migrate::data_migration_done(c, dm.name)?))?;
        if done {
            continue;
        }
        timed(timings, dm.name, || {
            log(&format!("data migration {} ...", dm.name));
            let n = (dm.run)(ctx, src_dir, &mut |m| log(m))?;
            let name = dm.name;
            db.write(move |t| {
                migrate::mark_data_migration(t, name)?;
                Ok(())
            })?;
            Ok(((), format!("{n} rows")))
        })?;
    }

    // 4. Cookie -> keyring.
    report.cookie_moved = timed(timings, "cookie", || {
        let r = move_cookie(ctx, config)?;
        Ok((r.clone(), r.unwrap_or_else(|| "none stored".into())))
    })?;

    // 5. Undo journals.
    report.backups_copied = timed(timings, "backups", || {
        let n = copy_backups(&src_dir.join("backups"), &config.backups_dir())?;
        Ok((n, format!("{n} journal(s)")))
    })?;

    // 6. Planner statistics (the sort/filter plans depend on them).
    timed(timings, "analyze", || {
        db.write(|t| {
            t.execute_batch("PRAGMA analysis_limit=0; ANALYZE; PRAGMA analysis_limit=400;")?;
            Ok(())
        })?;
        Ok(((), String::new()))
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------------------
// data migrations
// ---------------------------------------------------------------------------------------

struct DataMigration {
    name: &'static str,
    run: fn(&Ctx, &Path, &mut dyn FnMut(&str)) -> Result<u64>,
}

fn data_migrations(opts: &ImportOptions) -> Vec<DataMigration> {
    let mut v = vec![
        DataMigration { name: "analysis_analyzer", run: dm_analysis_analyzer },
        DataMigration { name: "artwork_legacy_rows", run: dm_artwork_rows },
        DataMigration { name: "harvest_item_tags", run: dm_harvest_item_tags },
    ];
    if !opts.skip_repairs {
        v.push(DataMigration { name: "legacy_repairs", run: dm_repairs });
    }
    v
}

/// WS3: imported rows are marked `essentia-import`; `analyzer_version` stays 1.
fn dm_analysis_analyzer(ctx: &Ctx, _dir: &Path, _log: &mut dyn FnMut(&str)) -> Result<u64> {
    let has_col: bool = ctx.read(|c| Ok(c.prepare("SELECT analyzer FROM analysis LIMIT 0").is_ok()))?;
    if !has_col {
        return Ok(0);
    }
    Ok(ctx.write(|t| Ok(t.execute("UPDATE analysis SET analyzer = 'essentia-import' WHERE analyzer IS NULL", [])? as u64))?)
}

/// One `artwork` row per release with a legacy cover; `sizes = 0` marks "not yet re-encoded".
/// The version is a short hash of the legacy file's identity so `?v=` URLs are stable and cacheable
/// from the first request, before any conversion has happened.
fn dm_artwork_rows(ctx: &Ctx, _dir: &Path, log: &mut dyn FnMut(&str)) -> Result<u64> {
    use rayon::prelude::*;
    let rows: Vec<(i64, String)> = ctx.read(|c| {
        let mut st = c.prepare("SELECT id, cover_path FROM releases WHERE cover_path IS NOT NULL AND id NOT IN (SELECT release_id FROM artwork)")?;
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<Vec<_>, _>>()?)
    })?;
    log(&format!("  stat {} legacy covers", rows.len()));
    let stamped: Vec<(i64, String, bool)> = rows
        .par_iter()
        .map(|(id, p)| {
            let meta = std::fs::metadata(p).ok();
            let seed = match &meta {
                Some(m) => {
                    format!("legacy:{id}:{}:{}", m.len(), m.mtime_nanos_total())
                }
                None => format!("legacy:{id}:missing"),
            };
            let mut h = Sha256::new();
            h.update(seed.as_bytes());
            let hex: String = h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect();
            (*id, hex, meta.is_some())
        })
        .collect();
    let n = stamped.len() as u64;
    ctx.db.write_chunks(stamped, 500, |t, chunk| {
        let mut st = t.prepare_cached("INSERT OR IGNORE INTO artwork (release_id, version, source, sizes) VALUES (?1, ?2, ?3, 0)")?;
        for (id, ver, present) in chunk {
            st.execute(bc_db::rusqlite::params![id, ver, if *present { "legacy" } else { "legacy-missing" }])?;
        }
        Ok(())
    })?;
    Ok(n)
}

trait MtimeExt {
    fn mtime_nanos_total(&self) -> i128;
}
impl MtimeExt for std::fs::Metadata {
    fn mtime_nanos_total(&self) -> i128 {
        use std::os::unix::fs::MetadataExt;
        self.mtime() as i128 * 1_000_000_000 + self.mtime_nsec() as i128
    }
}

/// `harvest_items.tags` (JSON array in a TEXT column) -> normalised `harvest_item_tags`.
fn dm_harvest_item_tags(ctx: &Ctx, _dir: &Path, _log: &mut dyn FnMut(&str)) -> Result<u64> {
    let rows: Vec<(i64, String)> = ctx.read(|c| {
        let mut st = c.prepare("SELECT id, tags FROM harvest_items WHERE tags IS NOT NULL AND tags != '' AND tags != '[]'")?;
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<Vec<_>, _>>()?)
    })?;
    let mut flat: Vec<(i64, String, String)> = Vec::new();
    for (id, json) in rows {
        let Ok(tags) = serde_json::from_str::<Vec<serde_json::Value>>(&json) else { continue };
        for t in tags {
            let name = match t {
                serde_json::Value::String(s) => s,
                serde_json::Value::Object(o) => o.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                _ => continue,
            };
            let key = bc_db::util::name_key(&name);
            if !key.is_empty() {
                flat.push((id, key, name));
            }
        }
    }
    let n = flat.len() as u64;
    ctx.db.write_chunks(flat, 2000, |t, chunk| {
        let mut st = t.prepare_cached("INSERT OR IGNORE INTO harvest_item_tags (item_id, tag_key, tag) VALUES (?1, ?2, ?3)")?;
        for (id, k, name) in chunk {
            st.execute(bc_db::rusqlite::params![id, k, name])?;
        }
        Ok(())
    })?;
    Ok(n)
}

/// The old startup repair passes (`main.py` lifespan), run once via `bc_maint::repairs::run_all`.
fn dm_repairs(ctx: &Ctx, _dir: &Path, log: &mut dyn FnMut(&str)) -> Result<u64> {
    let r = bc_maint::repairs::run_all(ctx)?;
    for (pass, ms) in &r.timings_ms {
        log(&format!("  {pass}: {ms} ms"));
    }
    Ok((r.release_urls_backfilled + r.release_labels_changed + r.release_urls_corrected + r.release_urls_cleared + r.harvest_links_reset + r.folder_twins_merged + r.queued_resolved + r.queued_reopened + r.expected_counts_learned + r.snippet_tracks_changed + r.snippet_releases_changed) as u64)
}

// ---------------------------------------------------------------------------------------
// cookie, journals
// ---------------------------------------------------------------------------------------

/// `settings.bandcamp.identity_cookie` -> keyring (0600 file fallback); the row is deleted from the new DB.
fn move_cookie(ctx: &Ctx, config: &Config) -> Result<Option<String>> {
    let value: Option<String> = ctx.read(|c| bc_db::settings::get(c, "bandcamp.identity_cookie").map_err(ApiError::from))?;
    let Some(v) = value.filter(|v| !v.trim().is_empty()) else { return Ok(None) };
    let secrets = bc_libcore::secrets::Secrets::new(&config.data_dir);
    let backend = secrets.set(bc_libcore::secrets::BANDCAMP_COOKIE, &v).map_err(|e| ImportError::Msg(e.to_string()))?;
    ctx.write(|t| {
        t.execute("DELETE FROM settings WHERE key = 'bandcamp.identity_cookie'", [])?;
        Ok(())
    })?;
    Ok(Some(format!("{backend:?}")))
}

fn copy_backups(from: &Path, to: &Path) -> Result<usize> {
    if !from.is_dir() {
        return Ok(0);
    }
    std::fs::create_dir_all(to)?;
    let mut n = 0;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let p = e.path();
        if p.extension().is_some_and(|x| x == "jsonl") {
            let dest = to.join(e.file_name());
            if !dest.exists() {
                std::fs::copy(&p, &dest)?;
                n += 1;
            }
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------------------
// verification
// ---------------------------------------------------------------------------------------

fn count(c: &Connection, table: &str) -> i64 {
    // the importer's own data-migration markers live in `settings` too
    let filter = if table == "settings" { " WHERE key NOT LIKE 'data_migration.%'" } else { "" };
    c.query_row(&format!("SELECT COUNT(*) FROM {table}{filter}"), [], |r| r.get(0)).unwrap_or(-1)
}

fn playlists_checksum(c: &Connection) -> String {
    let mut h = Sha256::new();
    for sql in [
        "SELECT id, name, kind, COALESCE(rules,'') FROM playlists ORDER BY id",
        "SELECT playlist_id, track_id, position FROM playlist_items ORDER BY playlist_id, position, id",
        "SELECT id, name, status, pool_sources FROM dj_sets ORDER BY id",
        "SELECT set_id, track_id, position, cue_in_ms, cue_out_ms, tempo_adjust_pct, key_lock, COALESCE(transition_type,''), snapshot FROM dj_set_items ORDER BY set_id, position, id",
    ] {
        if let Ok(mut st) = c.prepare(sql)
            && let Ok(mut rows) = st.query([])
        {
            while let Ok(Some(r)) = rows.next() {
                let n = r.as_ref().column_count();
                for i in 0..n {
                    let v: bc_db::rusqlite::types::Value = r.get(i).unwrap_or(bc_db::rusqlite::types::Value::Null);
                    h.update(format!("{v:?}|").as_bytes());
                }
                h.update(b"\n");
            }
        }
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// 50 sample queries drawn from the data itself (title words, artist names, prefixes).
fn sample_queries(legacy: &Connection) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for sql in [
        "SELECT name FROM artists WHERE id % 997 = 3 LIMIT 15",
        "SELECT title FROM releases WHERE id % 701 = 5 LIMIT 15",
        "SELECT name FROM labels WHERE id % 61 = 1 LIMIT 10",
        "SELECT name FROM tags WHERE id % 211 = 2 LIMIT 10",
    ] {
        if let Ok(mut st) = legacy.prepare(sql)
            && let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0))
        {
            out.extend(rows.flatten());
        }
    }
    out.truncate(50);
    out
}

fn fts_hits(c: &Connection, q: &str) -> Option<i64> {
    let expr = bc_db::fts::fts_escape(q);
    if expr.is_empty() {
        return None;
    }
    c.query_row("SELECT COUNT(*) FROM search_index WHERE search_index MATCH ?1", [expr], |r| r.get(0)).ok()
}

fn verify(db: &Db, legacy_path: &Path, report: &mut ImportReport) -> Result<()> {
    let legacy = Connection::open_with_flags(legacy_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let queries = sample_queries(&legacy);
    db.read(|c| {
        for t in COMPARED_TABLES {
            let (l, n) = (count(&legacy, t), count(c, t));
            // repair passes may add bookkeeping settings (e.g. the snippet rules version)
            let ok = if *t == "settings" { n >= l } else { l == n };
            report.counts.push(TableCount { table: (*t).into(), legacy: l, new: n, ok });
        }
        let mut fts = FtsParity { queries: queries.len(), ..Default::default() };
        for q in &queries {
            let (a, b) = (fts_hits(&legacy, q), fts_hits(c, q));
            if a == b {
                fts.identical += 1;
            } else {
                fts.mismatches.push(format!("{q:?}: legacy {a:?} new {b:?}"));
            }
        }
        report.fts = fts;
        report.playlists_checksum_legacy = playlists_checksum(&legacy);
        report.playlists_checksum_new = playlists_checksum(c);
        Ok(())
    })?;
    Ok(())
}
