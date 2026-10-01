//! Fixtures for the dedup tests: a temp legacy-schema DB and raw-SQL row builders
//! (the Python tests used the ORM; these are the equivalent inserts).
#![allow(dead_code)]

use bc_bandcamp::download::dedup::{name_key, normalise};
use bc_db::Db;
use bc_db::rusqlite::{OptionalExtension, params};

pub const ALBUM: &str = "https://artist.bandcamp.com/album/great-record";

pub struct Fx {
    pub db: Db,
    _dir: tempfile::TempDir,
}

pub fn fx() -> Fx {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open(dir.path().join("lib.db")).expect("open db");
    Fx { db, _dir: dir }
}

impl Fx {
    /// A `done` download job item for `url` (what `store.complete_item` leaves).
    pub fn done_item(&self, url: &str, release_id: Option<i64>) {
        let url = url.to_string();
        self.db
            .write(move |t| {
                let n: i64 = t.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))?;
                let job = format!("job-{n}");
                t.execute(
                    "INSERT INTO jobs(id, kind, status, priority, params, total, completed, failed, skipped, \
                     cancel_requested, created_at) VALUES (?1,'download','completed',0,'{}',1,1,0,0,0,datetime('now'))",
                    [&job],
                )?;
                t.execute(
                    "INSERT INTO job_items(job_id, seq, status, url, url_kind, attempts, max_attempts, progress, \
                     release_id, finished_at) VALUES (?1,0,'done',?2,'album',1,3,1.0,?3,datetime('now', ?4))",
                    params![job, url, release_id, format!("+{n} seconds")],
                )?;
                Ok(())
            })
            .expect("done_item");
    }

    /// A `pending` job item (an unfinished download).
    pub fn pending_item(&self, url: &str) {
        let url = url.to_string();
        self.db
            .write(move |t| {
                let n: i64 = t.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))?;
                let job = format!("job-{n}");
                t.execute(
                    "INSERT INTO jobs(id, kind, status, priority, params, total, completed, failed, skipped, \
                     cancel_requested, created_at) VALUES (?1,'download','queued',0,'{}',1,0,0,0,0,datetime('now'))",
                    [&job],
                )?;
                t.execute(
                    "INSERT INTO job_items(job_id, seq, status, url, url_kind, attempts, max_attempts, progress) \
                     VALUES (?1,0,'pending',?2,'album',0,3,0.0)",
                    params![job, url],
                )?;
                Ok(())
            })
            .expect("pending_item");
    }

    /// A release with no artist (`title_key` = lowercased title, like the Python helper).
    pub fn release(&self, title: &str, bandcamp_url: Option<&str>) -> i64 {
        let (title, url) = (title.to_string(), bandcamp_url.map(str::to_string));
        self.db
            .write(move |t| {
                t.execute(
                    "INSERT INTO releases(title, title_key, kind, bandcamp_url, added_at) \
                     VALUES (?1, ?2, 'album', ?3, datetime('now'))",
                    params![title, title.to_lowercase(), url],
                )?;
                Ok(t.last_insert_rowid())
            })
            .expect("release")
    }

    pub fn artist(&self, name: &str) -> i64 {
        let (name, key) = (name.to_string(), name_key(name));
        self.db
            .write(move |t| {
                if let Some(id) = t.query_row("SELECT id FROM artists WHERE name_key=?1", [&key], |r| r.get(0)).optional()? {
                    return Ok(id);
                }
                t.execute("INSERT INTO artists(name, name_key, created_at) VALUES (?1, ?2, datetime('now'))", params![name, key])?;
                Ok(t.last_insert_rowid())
            })
            .expect("artist")
    }

    /// A release as a scan off disk leaves it: named, and with no URL at all.
    pub fn owned(&self, artist: &str, title: &str) -> i64 {
        let artist_id = self.artist(artist);
        let (title, key) = (title.to_string(), name_key(title));
        self.db
            .write(move |t| {
                t.execute(
                    "INSERT INTO releases(title, title_key, kind, artist_id, added_at) \
                     VALUES (?1, ?2, 'album', ?3, datetime('now'))",
                    params![title, key, artist_id],
                )?;
                Ok(t.last_insert_rowid())
            })
            .expect("owned")
    }

    pub fn set_release_url(&self, id: i64, url: Option<&str>) {
        let url = url.map(str::to_string);
        self.db
            .write(move |t| {
                t.execute("UPDATE releases SET bandcamp_url=?1 WHERE id=?2", params![url, id])?;
                Ok(())
            })
            .expect("set url");
    }

    pub fn release_url(&self, id: i64) -> Option<String> {
        self.db.read(|c| Ok(c.query_row("SELECT bandcamp_url FROM releases WHERE id=?1", [id], |r| r.get(0))?)).expect("url")
    }

    pub fn release_label(&self, id: i64) -> Option<(i64, String, Option<String>)> {
        self.db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT l.id, l.name, l.bandcamp_url FROM releases r JOIN labels l ON l.id=r.label_id WHERE r.id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?)
            })
            .expect("label")
    }

    pub fn release_label_id(&self, id: i64) -> Option<i64> {
        self.db.read(|c| Ok(c.query_row("SELECT label_id FROM releases WHERE id=?1", [id], |r| r.get(0))?)).expect("label id")
    }

    pub fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        self.db.read(move |c| Ok(c.query_row(&sql, [], |r| r.get(0))?)).expect("count")
    }

    pub fn add_label(&self, name: &str, url: Option<&str>) -> i64 {
        let (name, key, url) = (name.to_string(), name_key(name), url.map(str::to_string));
        self.db
            .write(move |t| {
                t.execute("INSERT INTO labels(name, name_key, bandcamp_url) VALUES (?1,?2,?3)", params![name, key, url])?;
                Ok(t.last_insert_rowid())
            })
            .expect("label")
    }

    pub fn set_release_label(&self, id: i64, label: i64) {
        self.db
            .write(move |t| {
                t.execute("UPDATE releases SET label_id=?1 WHERE id=?2", params![label, id])?;
                Ok(())
            })
            .expect("set label");
    }

    pub fn set_folder(&self, id: i64, folder: Option<&str>) {
        let folder = folder.map(str::to_string);
        self.db
            .write(move |t| {
                t.execute("UPDATE releases SET folder_path=?1 WHERE id=?2", params![folder, id])?;
                Ok(())
            })
            .expect("folder");
    }

    /// An inbox row. `url` is stored in harvest-normalised form.
    pub fn harvest(
        &self,
        url: &str,
        state: &str,
        artist: &str,
        title: &str,
        label: Option<&str>,
        release_id: Option<i64>,
    ) -> i64 {
        let (url, state, artist, title, label) =
            (normalise(url), state.to_string(), artist.to_string(), title.to_string(), label.map(str::to_string));
        self.db
            .write(move |t| {
                t.execute(
                    "INSERT INTO harvest_items(url, url_kind, state, title, artist_name, label_name, tags, \
                     in_collection, in_wishlist, is_free_download, is_purchasable, is_preorder, release_id, \
                     discovered_at, resolved_at) VALUES (?1,'album',?2,?3,?4,?5,'[]',0,0,0,0,0,?6,datetime('now'), \
                     CASE WHEN ?6 IS NULL THEN NULL ELSE datetime('now') END)",
                    params![url, state, title, artist, label, release_id],
                )?;
                Ok(t.last_insert_rowid())
            })
            .expect("harvest")
    }

    pub fn harvest_state(&self, id: i64) -> (String, Option<i64>, Option<String>) {
        self.db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT state, release_id, resolved_at FROM harvest_items WHERE id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?)
            })
            .expect("state")
    }

    pub fn blacklist(&self, url: Option<&str>, artist: &str, title: &str) {
        let (key, url, ak, tk, artist, title) = (
            url.map(bc_bandcamp::download::dedup::url_key),
            url.map(str::to_string),
            name_key(artist),
            name_key(title),
            artist.to_string(),
            title.to_string(),
        );
        self.db
            .write(move |t| {
                t.execute(
                    "INSERT INTO blacklist(url_key, artist_key, title_key, url, artist_name, title, added_at) \
                     VALUES (?1,?2,?3,?4,?5,?6,datetime('now'))",
                    params![key, ak, tk, url, artist, title],
                )?;
                Ok(())
            })
            .expect("blacklist");
    }
}
