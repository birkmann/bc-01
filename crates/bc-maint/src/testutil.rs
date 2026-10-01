//! Test helpers: a tempfile DB and plain-SQL seeding (the Python tests build ORM rows).
#![allow(dead_code)]

use std::sync::Arc;

use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_db::util::name_key;
use bc_libcore::Ctx;

pub struct TestEnv {
    pub dir: tempfile::TempDir,
    pub ctx: Ctx,
}

impl std::ops::Deref for TestEnv {
    type Target = Ctx;
    fn deref(&self) -> &Ctx {
        &self.ctx
    }
}

pub fn test_db() -> Db {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open(dir.path().join("t.db")).expect("db");
    // Keep the directory alive for the rest of the process: tests are short-lived.
    std::mem::forget(dir);
    db
}

pub fn test_env() -> TestEnv {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open(dir.path().join("library.db")).expect("db");
    let mut cfg = Config::from_env();
    cfg.data_dir = dir.path().join("data");
    cfg.download_dir = dir.path().join("downloads");
    let ctx = Ctx::new(db, Arc::new(EventBus::new()), cfg);
    TestEnv { dir, ctx }
}

pub fn exec(db: &Db, sql: &str) {
    let sql = sql.to_string();
    db.write(move |t| {
        t.execute_batch(&sql)?;
        Ok(())
    })
    .expect("exec");
}

pub fn q_i64(db: &Db, sql: &str) -> i64 {
    let sql = sql.to_string();
    db.read(move |c| Ok(c.query_row(&sql, [], |r| r.get::<_, Option<i64>>(0))?.unwrap_or(0))).expect("q")
}

pub fn q_str(db: &Db, sql: &str) -> Option<String> {
    let sql = sql.to_string();
    db.read(move |c| Ok(c.query_row(&sql, [], |r| r.get::<_, Option<String>>(0))?)).expect("q")
}

pub fn q_ids(db: &Db, sql: &str) -> Vec<i64> {
    let sql = sql.to_string();
    db.read(move |c| {
        let mut st = c.prepare(&sql)?;
        Ok(st.query_map([], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?)
    })
    .expect("q")
}

/// Get-or-create an artist row, returning its id.
pub fn seed_artist(db: &Db, name: &str) -> i64 {
    let (n, k) = (name.to_string(), name_key(name));
    db.write(move |t| {
        if let Ok(id) = t.query_row("SELECT id FROM artists WHERE name_key = ?1", [&k], |r| r.get::<_, i64>(0)) {
            return Ok(id);
        }
        t.execute("INSERT INTO artists(name,name_key,created_at) VALUES (?1,?2,'2024-01-01 00:00:00')", (&n, &k))?;
        Ok(t.last_insert_rowid())
    })
    .expect("artist")
}

/// Insert a release for `artist` (created if needed).
pub fn seed_release(db: &Db, artist: &str, title: &str, url: Option<&str>, year: Option<i64>) -> i64 {
    let aid = seed_artist(db, artist);
    let (t, k, u) = (title.to_string(), name_key(title), url.map(str::to_string));
    db.write(move |tx| {
        tx.execute(
            "INSERT INTO releases(title,title_key,artist_id,kind,year,bandcamp_url,added_at) VALUES (?1,?2,?3,'album',?4,?5,'2024-01-01 00:00:00')",
            (&t, &k, aid, year, &u),
        )?;
        Ok(tx.last_insert_rowid())
    })
    .expect("release")
}

pub fn seed_track(db: &Db, release_id: i64, title: &str, no: Option<i64>) -> i64 {
    let (t, k) = (title.to_string(), name_key(title));
    db.write(move |tx| {
        let artist_id: Option<i64> = tx.query_row("SELECT artist_id FROM releases WHERE id=?1", [release_id], |r| r.get(0))?;
        tx.execute(
            "INSERT INTO tracks(release_id,artist_id,title,title_key,track_no,loved,play_count,skip_count,added_at)
             VALUES (?1,?2,?3,?4,?5,0,0,0,'2024-01-01 00:00:00')",
            (release_id, artist_id, &t, &k, no),
        )?;
        Ok(tx.last_insert_rowid())
    })
    .expect("track")
}

pub fn seed_root(db: &Db, path: &str, kind: &str) -> i64 {
    let (p, k) = (path.to_string(), kind.to_string());
    db.write(move |tx| {
        tx.execute("INSERT INTO library_roots(path,kind,watch,enabled) VALUES (?1,?2,0,1)", (&p, &k))?;
        Ok(tx.last_insert_rowid())
    })
    .expect("root")
}

pub fn seed_file(db: &Db, track_id: i64, root_id: i64, path: &str, rel: &str, size: i64) -> i64 {
    let (p, r) = (path.to_string(), rel.to_string());
    db.write(move |tx| {
        tx.execute(
            "INSERT INTO files(track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at)
             VALUES (?1,?2,?3,?4,'.mp3',?5,0,'2024-01-01 00:00:00','2024-01-01 00:00:00')",
            (track_id, root_id, &p, &r, size),
        )?;
        Ok(tx.last_insert_rowid())
    })
    .expect("file")
}

pub fn seed_fan(db: &Db, username: &str) -> i64 {
    let (u, url) = (username.to_string(), format!("https://bandcamp.com/{username}"));
    db.write(move |tx| {
        tx.execute("INSERT INTO fans(username,url,is_self,created_at) VALUES (?1,?2,0,'2024-01-01 00:00:00')", (&u, &url))?;
        Ok(tx.last_insert_rowid())
    })
    .expect("fan")
}
