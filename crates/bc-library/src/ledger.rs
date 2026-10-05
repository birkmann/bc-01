//! The Bandcamp-links ledger: every release's Bandcamp page, label, artist and relink result, saved
//! outside `library.db` so a rebuilt library gets them back without asking Bandcamp again.
//!
//! The file is `links.db` in the data dir, copied to `.bc-links.db` at the top of every library
//! root, so it also survives losing the data dir. Rows are keyed by the album folder (relative to
//! its root) and title, never by row id; artist + title + year is the fallback when folders moved.
//!
//! [`sync`] runs at startup, after every full scan and every few minutes:
//! - a release `library.db` has not yet matched against the file (no `release_ledger` row) gets the
//!   saved links first. The file wins there: it holds the last known state, a fresh scan only tags.
//! - then every release whose links changed is written back. From then on the library wins, so an
//!   edit made in the app is kept.
//!
//! Releases that are merely missing (deleted files, a scan still running) keep their rows; only a
//! known release whose links were cleared clears its row.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bc_db::rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use bc_db::util::{name_key, now_db};
use bc_libcore::{ApiResult, Ctx};
use sha2::{Digest, Sha256};

pub const FILE_NAME: &str = "links.db";
pub const MIRROR_NAME: &str = ".bc-links.db";
const SYNC_EVERY: Duration = Duration::from_secs(5 * 60);

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS releases (
    key          TEXT PRIMARY KEY,
    folder       TEXT,
    artist       TEXT,
    artist_key   TEXT,
    title        TEXT NOT NULL,
    title_key    TEXT NOT NULL,
    year         INTEGER,
    bandcamp_url TEXT,
    item_id      INTEGER,
    label        TEXT,
    label_url    TEXT,
    relink_at    TEXT,              -- when relink searched for it; NULL = never
    relink_url   TEXT,              -- what it found; NULL with relink_at set = no match
    saved_at     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS ix_releases_folder ON releases (folder);
CREATE TABLE IF NOT EXISTS artists (
    name_key     TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    bandcamp_url TEXT NOT NULL,
    saved_at     TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

const RELEASE_COLS: &str =
    "key, folder, artist, artist_key, title, title_key, year, bandcamp_url, item_id, label, label_url, relink_at, relink_url, saved_at";
const ARTIST_COLS: &str = "name_key, name, bandcamp_url, saved_at";
const SAVE: &str = "INSERT OR REPLACE INTO releases(key, folder, artist, artist_key, title, title_key, year, bandcamp_url, item_id,
                        label, label_url, relink_at, relink_url, saved_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)";
const SAVE_FIRST: &str = "INSERT INTO releases(key, folder, artist, artist_key, title, title_key, year, bandcamp_url, item_id,
                        label, label_url, relink_at, relink_url, saved_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                    ON CONFLICT(key) DO UPDATE SET
                        bandcamp_url = coalesce(excluded.bandcamp_url, bandcamp_url),
                        item_id      = coalesce(excluded.item_id, item_id),
                        label        = coalesce(excluded.label, label),
                        label_url    = coalesce(excluded.label_url, label_url),
                        relink_at    = coalesce(excluded.relink_at, relink_at),
                        relink_url   = coalesce(excluded.relink_url, relink_url),
                        saved_at     = excluded.saved_at";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// Releases that got saved links back.
    pub restored: usize,
    pub artists_restored: usize,
    /// Rows written to (or cleared from) the file.
    pub saved: usize,
    /// Library roots the file was copied to.
    pub mirrored: usize,
}

/// One release as the file records it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rec {
    key: String,
    /// Relative to the library root it lives in.
    folder: Option<String>,
    artist: Option<String>,
    artist_key: Option<String>,
    title: String,
    title_key: String,
    year: Option<i64>,
    url: Option<String>,
    item_id: Option<i64>,
    label: Option<String>,
    label_url: Option<String>,
    relink_at: Option<String>,
    relink_url: Option<String>,
}

impl Rec {
    fn has_links(&self) -> bool {
        self.url.is_some() || self.label.is_some() || self.relink_at.is_some()
    }

    fn fingerprint(&self) -> i64 {
        let mut h = Sha256::new();
        let s = |h: &mut Sha256, v: Option<&str>| {
            h.update(v.map_or(&[0u8][..], str::as_bytes));
            h.update([0x1f]);
        };
        s(&mut h, Some(&self.key));
        s(&mut h, self.artist.as_deref());
        s(&mut h, Some(&self.title));
        s(&mut h, self.url.as_deref());
        s(&mut h, self.item_id.map(|i| i.to_string()).as_deref());
        s(&mut h, self.label.as_deref());
        s(&mut h, self.label_url.as_deref());
        s(&mut h, self.relink_at.as_deref());
        s(&mut h, self.relink_url.as_deref());
        let d = h.finalize();
        i64::from_le_bytes(d[..8].try_into().expect("8 bytes"))
    }
}

/// A release in `library.db`, with what was last saved for it.
struct Live {
    id: i64,
    rec: Rec,
    synced: Option<(String, i64)>,
}

fn key_of(folder: Option<&str>, artist_key: Option<&str>, title_key: &str, year: Option<i64>) -> String {
    match folder {
        Some(f) => format!("f:{f}\x1f{title_key}"),
        None => format!("i:{}\x1f{title_key}\x1f{}", artist_key.unwrap_or(""), year.map(|y| y.to_string()).unwrap_or_default()),
    }
}

/// `folder` relative to the deepest root holding it; unchanged when no root does.
fn relative(roots: &[PathBuf], folder: &str) -> String {
    let p = Path::new(folder);
    roots
        .iter()
        .filter_map(|r| p.strip_prefix(r).ok())
        .min_by_key(|rel| rel.as_os_str().len())
        .map(|rel| rel.to_string_lossy().into_owned())
        .unwrap_or_else(|| folder.to_string())
}

/// Save the library's links to the file and restore them into releases new to it. See the module docs.
pub fn sync(ctx: &Ctx) -> ApiResult<SyncReport> {
    let roots: Vec<(PathBuf, String, bool)> = ctx.read(|c| {
        let mut st = c.prepare("SELECT path, kind, enabled FROM library_roots")?;
        let rows = st.query_map([], |r| Ok((PathBuf::from(r.get::<_, String>(0)?), r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    })?;
    let root_paths: Vec<PathBuf> = roots.iter().map(|r| r.0.clone()).collect();
    let mirrors: Vec<PathBuf> =
        roots.iter().filter(|(p, kind, on)| kind == "library" && *on && p.is_dir()).map(|(p, ..)| p.join(MIRROR_NAME)).collect();

    let mut led = open(&ctx.config.data_dir.join(FILE_NAME))?;
    let mut report = SyncReport::default();
    let mut changed = merge_mirrors(&led, &mirrors)?;

    let mut live = load_live(ctx, &root_paths)?;
    if live.iter().any(|l| l.synced.is_none()) {
        let saved = load_saved(&led)?;
        if !saved.is_empty() {
            report.restored = restore(ctx, &live, saved)?;
        }
        if report.restored > 0 {
            report.artists_restored = restore_artists(ctx, &led)?;
            live = load_live(ctx, &root_paths)?;
        }
    }

    report.saved = save(ctx, &mut led, &live)? + save_artists(ctx, &mut led)?;
    changed |= report.saved > 0;
    if changed || saved_at(&led, "main").is_none() {
        led.execute("INSERT OR REPLACE INTO meta(key, value) VALUES ('saved_at', ?1)", [now_db()])?;
    }
    report.mirrored = mirror(&led, &mirrors, changed);

    if report.restored > 0 || report.artists_restored > 0 {
        ctx.bus.invalidate("release", vec![]);
        ctx.bus.invalidate("label", vec![]);
        ctx.bus.invalidate("artist", vec![]);
        ctx.bus.publish(bc_types::library::TOPIC_LIBRARY_CHANGED, &bc_types::library::LibraryChanged::default());
    }
    Ok(report)
}

/// Sync now, then after every full scan and every few minutes, for as long as the app runs.
pub fn spawn(ctx: Ctx) {
    tokio::spawn(async move {
        let mut events = ctx.bus.subscribe();
        loop {
            let c = ctx.clone();
            match tokio::task::spawn_blocking(move || sync(&c)).await {
                Ok(Ok(r)) if r.restored + r.artists_restored + r.saved > 0 => tracing::info!(
                    restored = r.restored,
                    artists_restored = r.artists_restored,
                    saved = r.saved,
                    mirrored = r.mirrored,
                    "bandcamp links file synced"
                ),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "bandcamp links file sync failed"),
                Err(e) => tracing::warn!(error = %e, "bandcamp links file sync panicked"),
            }
            let wait = tokio::time::sleep(SYNC_EVERY);
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    _ = &mut wait => break,
                    ev = events.recv() => match ev {
                        Ok(e) if e.topic == bc_types::library::TOPIC_LIBRARY_SCAN_DONE => break,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            (&mut wait).await;
                            break;
                        }
                        _ => {}
                    },
                }
            }
        }
    });
}

fn open(path: &Path) -> ApiResult<Connection> {
    let c = Connection::open(path)?;
    c.busy_timeout(Duration::from_secs(10))?;
    c.execute_batch(SCHEMA)?;
    Ok(c)
}

fn saved_at(c: &Connection, schema: &str) -> Option<String> {
    c.query_row(&format!("SELECT value FROM {schema}.meta WHERE key = 'saved_at'"), [], |r| r.get(0)).optional().ok().flatten()
}

/// Pull in rows a root's copy has that the data-dir file lacks: after the data dir was wiped, or
/// when a root was unmounted while the file changed. The newer copy wins a conflict.
fn merge_mirrors(led: &Connection, mirrors: &[PathBuf]) -> ApiResult<bool> {
    let mut changed = false;
    for m in mirrors.iter().filter(|m| m.is_file()) {
        let Some(path) = m.to_str() else { continue };
        if let Err(e) = led.execute("ATTACH DATABASE ?1 AS mirror", [path]) {
            tracing::warn!(path, error = %e, "could not open bandcamp links file copy");
            continue;
        }
        let merged = (|| -> ApiResult<usize> {
            let theirs = saved_at(led, "mirror");
            let ours = saved_at(led, "main");
            if theirs.is_none() || theirs == ours {
                return Ok(0);
            }
            let verb = if theirs > ours { "REPLACE" } else { "IGNORE" };
            Ok(led.execute(&format!("INSERT OR {verb} INTO main.releases({RELEASE_COLS}) SELECT {RELEASE_COLS} FROM mirror.releases"), [])?
                + led.execute(&format!("INSERT OR {verb} INTO main.artists({ARTIST_COLS}) SELECT {ARTIST_COLS} FROM mirror.artists"), [])?)
        })();
        led.execute("DETACH DATABASE mirror", [])?;
        match merged {
            Ok(n) => changed |= n > 0,
            Err(e) => tracing::warn!(path, error = %e, "could not read bandcamp links file copy"),
        }
    }
    Ok(changed)
}

/// Copy the file to every library root whose copy is out of date. Failures (an unmounted or
/// read-only drive) are logged and retried on the next sync.
fn mirror(led: &Connection, mirrors: &[PathBuf], changed: bool) -> usize {
    let ours = saved_at(led, "main");
    let mut n = 0;
    for m in mirrors {
        let theirs = if m.is_file() {
            match Connection::open_with_flags(m, OpenFlags::SQLITE_OPEN_READ_ONLY).ok().and_then(|c| saved_at(&c, "main")) {
                Some(at) => Some(at),
                None => {
                    // After a wiped data dir this copy may be the only one: never overwrite what we cannot read.
                    tracing::warn!(path = %m.display(), "bandcamp links file copy is unreadable; leaving it alone");
                    continue;
                }
            }
        } else {
            None
        };
        if theirs > ours || (!changed && theirs == ours) {
            continue;
        }
        let tmp = m.with_file_name(format!("{MIRROR_NAME}.tmp"));
        let _ = std::fs::remove_file(&tmp);
        let done = tmp
            .to_str()
            .ok_or_else(|| "path is not UTF-8".to_string())
            .and_then(|t| led.execute("VACUUM INTO ?1", [t]).map_err(|e| e.to_string()))
            .and_then(|_| std::fs::rename(&tmp, m).map_err(|e| e.to_string()));
        match done {
            Ok(()) => n += 1,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                tracing::warn!(path = %m.display(), error = %e, "could not copy bandcamp links file");
            }
        }
    }
    n
}

fn load_live(ctx: &Ctx, roots: &[PathBuf]) -> ApiResult<Vec<Live>> {
    ctx.read(|c| {
        let mut st = c.prepare(
            "SELECT r.id, r.folder_path, a.name, a.name_key, r.title, r.title_key, r.year, r.bandcamp_url, r.bandcamp_item_id,
                    l.name, l.bandcamp_url, x.tried_at, x.url, g.ledger_key, g.fp
               FROM releases r
               LEFT JOIN artists a ON a.id = r.artist_id
               LEFT JOIN labels l ON l.id = r.label_id
               LEFT JOIN release_relink x ON x.release_id = r.id
               LEFT JOIN release_ledger g ON g.release_id = r.id",
        )?;
        let rows = st.query_map([], |r| {
            let folder = r.get::<_, Option<String>>(1)?.map(|f| relative(roots, &f));
            let artist_key: Option<String> = r.get(3)?;
            let title_key: String = r.get(5)?;
            let year: Option<i64> = r.get(6)?;
            let synced = match (r.get::<_, Option<String>>(13)?, r.get::<_, Option<i64>>(14)?) {
                (Some(k), Some(fp)) => Some((k, fp)),
                _ => None,
            };
            Ok(Live {
                id: r.get(0)?,
                rec: Rec {
                    key: key_of(folder.as_deref(), artist_key.as_deref(), &title_key, year),
                    folder,
                    artist: r.get(2)?,
                    artist_key,
                    title: r.get(4)?,
                    title_key,
                    year,
                    url: r.get(7)?,
                    item_id: r.get(8)?,
                    label: r.get(9)?,
                    label_url: r.get(10)?,
                    relink_at: r.get(11)?,
                    relink_url: r.get(12)?,
                },
                synced,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    })
}

fn load_saved(led: &Connection) -> ApiResult<Vec<Rec>> {
    let mut st = led.prepare(&format!("SELECT {RELEASE_COLS} FROM releases"))?;
    let rows = st.query_map([], |r| {
        Ok(Rec {
            key: r.get(0)?,
            folder: r.get(1)?,
            artist: r.get(2)?,
            artist_key: r.get(3)?,
            title: r.get(4)?,
            title_key: r.get(5)?,
            year: r.get(6)?,
            url: r.get(7)?,
            item_id: r.get(8)?,
            label: r.get(9)?,
            label_url: r.get(10)?,
            relink_at: r.get(11)?,
            relink_url: r.get(12)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Give releases new to the file their saved links. Returns how many changed.
fn restore(ctx: &Ctx, live: &[Live], saved: Vec<Rec>) -> ApiResult<usize> {
    let mut by_key = HashMap::new();
    let mut by_folder: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_identity: HashMap<(&str, &str, Option<i64>), Vec<usize>> = HashMap::new();
    for (i, s) in saved.iter().enumerate() {
        by_key.insert(s.key.as_str(), i);
        if let Some(f) = &s.folder {
            by_folder.entry(f).or_default().push(i);
        }
        by_identity.entry((s.artist_key.as_deref().unwrap_or(""), &s.title_key, s.year)).or_default().push(i);
    }
    let mut live_per_folder: HashMap<&str, usize> = HashMap::new();
    for l in live {
        if let Some(f) = &l.rec.folder {
            *live_per_folder.entry(f).or_default() += 1;
        }
    }
    let only = |v: Option<&Vec<usize>>| v.filter(|v| v.len() == 1).map(|v| v[0]);
    let fixes: Vec<(i64, Rec, Rec)> = live
        .iter()
        .filter(|l| l.synced.is_none())
        .filter_map(|l| {
            let r = &l.rec;
            let hit = by_key
                .get(r.key.as_str())
                .copied()
                // Same folder, renamed title: only when the folder holds one release on both sides.
                .or_else(|| r.folder.as_deref().filter(|f| live_per_folder.get(f) == Some(&1)).and_then(|f| only(by_folder.get(f))))
                // Moved folder: same artist, title and year.
                .or_else(|| only(by_identity.get(&(r.artist_key.as_deref().unwrap_or(""), r.title_key.as_str(), r.year))))?;
            Some((l.id, r.clone(), saved[hit].clone()))
        })
        .collect();
    if fixes.is_empty() {
        return Ok(0);
    }
    ctx.write(move |t| {
        let mut n = 0;
        for (id, now, was) in &fixes {
            n += usize::from(apply(t, *id, now, was)?);
        }
        Ok(n)
    })
}

fn apply(t: &Transaction<'_>, id: i64, now: &Rec, was: &Rec) -> ApiResult<bool> {
    let mut did = false;
    if now.url.is_none()
        && let Some(url) = &was.url
    {
        let taken: bool = t.query_row("SELECT EXISTS(SELECT 1 FROM releases WHERE bandcamp_url = ?1)", [url], |r| r.get(0))?;
        if !taken {
            did |= t.execute(
                "UPDATE releases SET bandcamp_url = ?2, bandcamp_item_id = coalesce(bandcamp_item_id, ?3) WHERE id = ?1",
                params![id, url, was.item_id],
            )? > 0;
        }
    }
    if let Some(name) = &was.label
        && let Some(label) = named_id(t, "labels", name, was.label_url.as_deref())?
    {
        did |= t.execute("UPDATE releases SET label_id = ?2 WHERE id = ?1 AND label_id IS NOT ?2", params![id, label])? > 0;
    }
    if let Some(name) = &was.artist
        && was.artist_key != now.artist_key
        && let Some(artist) = named_id(t, "artists", name, None)?
    {
        // OR IGNORE: another release may already hold this artist + title + year.
        did |= t.execute("UPDATE OR IGNORE releases SET artist_id = ?2 WHERE id = ?1", params![id, artist])? > 0;
    }
    if let Some(at) = &was.relink_at {
        did |= t.execute(
            "INSERT OR IGNORE INTO release_relink(release_id, tried_at, url) VALUES (?1, ?2, ?3)",
            params![id, at, was.relink_url],
        )? > 0;
    }
    Ok(did)
}

/// The label or artist called `name` (or holding `url`), created when missing.
fn named_id(t: &Transaction<'_>, table: &str, name: &str, url: Option<&str>) -> ApiResult<Option<i64>> {
    if let Some(url) = url
        && let Some(id) = t.query_row(&format!("SELECT id FROM {table} WHERE bandcamp_url = ?1"), [url], |r| r.get(0)).optional()?
    {
        return Ok(Some(id));
    }
    let key = name_key(name);
    if key.is_empty() {
        return Ok(None);
    }
    let found: Option<i64> =
        t.query_row(&format!("SELECT id FROM {table} WHERE name_key = ?1 ORDER BY id LIMIT 1"), [&key], |r| r.get(0)).optional()?;
    let id = match found {
        Some(id) => id,
        None if table == "artists" => {
            t.execute("INSERT INTO artists(name, name_key, created_at) VALUES (?1, ?2, ?3)", params![name, key, now_db()])?;
            t.last_insert_rowid()
        }
        None => {
            t.execute("INSERT INTO labels(name, name_key) VALUES (?1, ?2)", params![name, key])?;
            t.last_insert_rowid()
        }
    };
    if let Some(url) = url {
        t.execute(
            &format!(
                "UPDATE {table} SET bandcamp_url = ?2 WHERE id = ?1 AND bandcamp_url IS NULL
                   AND NOT EXISTS (SELECT 1 FROM {table} WHERE bandcamp_url = ?2)"
            ),
            params![id, url],
        )?;
    }
    Ok(Some(id))
}

/// Artist pages, restored only alongside releases (after a rebuild), so clearing one in the app sticks.
fn restore_artists(ctx: &Ctx, led: &Connection) -> ApiResult<usize> {
    let saved: HashMap<String, String> = {
        let mut st = led.prepare("SELECT name_key, bandcamp_url FROM artists")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    if saved.is_empty() {
        return Ok(0);
    }
    ctx.write(move |t| {
        let missing: Vec<(i64, String)> = {
            let mut st = t.prepare("SELECT id, name_key FROM artists WHERE bandcamp_url IS NULL")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut n = 0;
        for (id, key) in missing {
            if let Some(url) = saved.get(&key) {
                n += t.execute(
                    "UPDATE artists SET bandcamp_url = ?2 WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM artists WHERE bandcamp_url = ?2)",
                    params![id, url],
                )?;
            }
        }
        Ok(n)
    })
}

/// Write every release whose links changed since the last sync, and mark it in step.
fn save(ctx: &Ctx, led: &mut Connection, live: &[Live]) -> ApiResult<usize> {
    let now = now_db();
    let mut marks = Vec::new();
    let mut n = 0;
    let tx = led.transaction()?;
    for l in live {
        let r = &l.rec;
        let fp = r.fingerprint();
        if l.synced.as_ref().is_some_and(|(k, f)| *k == r.key && *f == fp) {
            continue;
        }
        if let Some((old, _)) = &l.synced
            && *old != r.key
        {
            tx.execute("DELETE FROM releases WHERE key = ?1", [old])?;
        }
        if r.has_links() {
            // A release seen for the first time only adds to its saved row: what a restore could not
            // apply (a URL another release already holds) stays saved.
            let upsert = if l.synced.is_some() { SAVE } else { SAVE_FIRST };
            n += tx.execute(
                upsert,
                params![
                    r.key, r.folder, r.artist, r.artist_key, r.title, r.title_key, r.year, r.url, r.item_id, r.label, r.label_url,
                    r.relink_at, r.relink_url, now
                ],
            )?;
        } else if l.synced.is_some() {
            // Cleared in the app. A release seen for the first time never clears a saved row.
            n += tx.execute("DELETE FROM releases WHERE key = ?1", [&r.key])?;
        }
        marks.push((l.id, r.key.clone(), fp));
    }
    tx.commit()?;
    if !marks.is_empty() {
        ctx.write(move |t| {
            let mut st = t.prepare_cached("INSERT OR REPLACE INTO release_ledger(release_id, ledger_key, fp) VALUES (?1, ?2, ?3)")?;
            for (id, key, fp) in &marks {
                st.execute(params![id, key, fp])?;
            }
            Ok(())
        })?;
    }
    Ok(n)
}

fn save_artists(ctx: &Ctx, led: &mut Connection) -> ApiResult<usize> {
    // One row per name: when two artists share a name, the later one wins every time.
    let lib: HashMap<String, (String, String)> = ctx.read(|c| {
        let mut st = c.prepare(
            "SELECT name_key, name, bandcamp_url FROM artists WHERE bandcamp_url IS NOT NULL AND name_key != '' ORDER BY id",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
        Ok(rows.collect::<Result<_, _>>()?)
    })?;
    let saved: HashMap<String, String> = {
        let mut st = led.prepare("SELECT name_key, bandcamp_url FROM artists")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let now = now_db();
    let tx = led.transaction()?;
    let mut n = 0;
    for (key, (name, url)) in &lib {
        if saved.get(key) != Some(url) {
            n += tx.execute(
                &format!("INSERT OR REPLACE INTO artists({ARTIST_COLS}) VALUES (?1, ?2, ?3, ?4)"),
                params![key, name, url, now],
            )?;
        }
    }
    tx.commit()?;
    Ok(n)
}
