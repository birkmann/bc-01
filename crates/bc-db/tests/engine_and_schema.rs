//! Ports of tests/unit/test_engine.py and test_schema_migration.py.

use bc_db::rusqlite::Connection;
use bc_db::{Db, DbError, migrate};

fn db() -> (tempfile::TempDir, Db) {
    let d = tempfile::tempdir().unwrap();
    let db = Db::open(d.path().join("test.db")).unwrap();
    (d, db)
}

fn count(db: &Db, sql: &str) -> i64 {
    let sql = sql.to_string();
    db.read(move |c| Ok(c.query_row(&sql, [], |r| r.get(0))?)).unwrap()
}

// ---- test_engine ----------------------------------------------------------------------

#[test]
fn added_rows_actually_persist() {
    let (_d, db) = db();
    db.write(|t| {
        t.execute("INSERT INTO artists(name,name_key,created_at) VALUES ('Somatic','somatic','x')", [])?;
        Ok(())
    })
    .unwrap();
    // read through a *separate* connection (the reader pool)
    assert_eq!(count(&db, "SELECT COUNT(*) FROM artists WHERE name_key='somatic'"), 1);
}

#[test]
fn mutation_of_a_loaded_row_persists() {
    let (_d, db) = db();
    db.write(|t| {
        t.execute("INSERT INTO library_roots(path,kind,watch,enabled) VALUES ('/music','library',0,1)", [])?;
        Ok(())
    })
    .unwrap();
    db.write(|t| {
        t.execute("UPDATE library_roots SET enabled = 0", [])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(count(&db, "SELECT enabled FROM library_roots"), 0);
}

#[test]
fn rollback_on_error_discards_everything() {
    let (_d, db) = db();
    let r: Result<(), DbError> = db.write(|t| {
        t.execute("INSERT INTO artists(name,name_key,created_at) VALUES ('Ghost','ghost','x')", [])?;
        Err(DbError::Other("boom".into()))
    });
    assert!(r.is_err());
    assert_eq!(count(&db, "SELECT COUNT(*) FROM artists"), 0);
}

#[test]
fn a_read_before_a_write_is_safe() {
    let (_d, db) = db();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM artists"), 0);
    db.write(|t| {
        t.execute("INSERT INTO artists(name,name_key,created_at) VALUES ('Ferric','ferric','x')", [])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM artists WHERE name_key='ferric'"), 1);
}

#[test]
fn wal_and_foreign_keys_are_enabled() {
    let (_d, db) = db();
    let mode: String = db.read(|c| Ok(c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?)).unwrap();
    assert_eq!(mode, "wal");
    assert_eq!(count(&db, "PRAGMA foreign_keys"), 1);
}

/// WAL gives one writer plus N readers: a held write transaction must not stall reads.
#[test]
fn readers_are_not_blocked_by_an_open_writer() {
    let (_d, db) = db();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let db2 = db.clone();
    let writer = std::thread::spawn(move || {
        db2.write(move |t| {
            t.execute("INSERT INTO artists(name,name_key,created_at) VALUES ('Nul Object','nul object','x')", [])?;
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
        .unwrap();
    });
    started_rx.recv().unwrap();
    // sees the pre-commit state, but crucially does not hang
    assert_eq!(count(&db, "SELECT COUNT(*) FROM artists"), 0);
    release_tx.send(()).unwrap();
    writer.join().unwrap();
    assert_eq!(count(&db, "SELECT COUNT(*) FROM artists WHERE name_key='nul object'"), 1);
}

#[test]
fn name_key_folds_case_accents_and_punctuation() {
    use bc_db::util::name_key;
    assert_eq!(name_key("Motörhead"), name_key("motorhead"));
    assert_eq!(name_key("Motörhead"), "motorhead");
    assert_eq!(name_key("Sunn O)))"), "sunn o");
    assert_eq!(name_key("  The   Field  "), "the field");
}

#[test]
fn background_writes_are_batched_per_chunk() {
    let (_d, db) = db();
    let g0 = db.generation();
    let items: Vec<i64> = (0..450).collect();
    let results = db
        .write_chunks(items, 200, |t, chunk| {
            for i in chunk {
                t.execute("INSERT INTO tags(name,name_key,kind,track_count) VALUES (?1,?1,'genre',0)", [i.to_string()])?;
            }
            Ok(chunk.len())
        })
        .unwrap();
    assert_eq!(results, vec![200, 200, 50]);
    assert_eq!(db.generation() - g0, 3, "one BEGIN IMMEDIATE transaction per chunk");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM tags"), 450);
}

// ---- test_schema_migration ------------------------------------------------------------------


fn columns(c: &Connection, table: &str) -> Vec<String> {
    let mut st = c.prepare(&format!("PRAGMA table_info({table})")).unwrap();
    st.query_map([], |r| r.get::<_, String>(1)).unwrap().map(|r| r.unwrap()).collect()
}

/// A legacy (v1) database keeps its rows through the upgrade; a second open changes nothing.
#[test]
fn a_legacy_database_boots_and_keeps_its_rows() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("library.db");
    {
        let c = Connection::open(&p).unwrap();
        c.execute_batch(include_str!("../migrations/0001_legacy.sql")).unwrap();
        c.execute_batch("INSERT INTO releases (title, title_key, kind, added_at) VALUES ('Old', 'old', 'album', '2024-01-01 00:00:00')").unwrap();
    }
    let db = Db::open(&p).unwrap();
    let (n, fan): (i64, Option<i64>) = db.read(|c| Ok(c.query_row("SELECT COUNT(*), MAX(source_fan_id) FROM releases", [], |r| Ok((r.get(0)?, r.get(1)?)))?)).unwrap();
    assert_eq!((n, fan), (1, None));
    let v: i64 = db.read(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?)).unwrap();
    assert_eq!(v, migrate::latest_version());
    let extras: i64 = db.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM schema_extras", [], |r| r.get(0))?)).unwrap();
    drop(db);
    let db = Db::open(&p).unwrap();
    let extras2: i64 = db.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM schema_extras", [], |r| r.get(0))?)).unwrap();
    assert_eq!(extras, extras2, "requested schema extras are applied exactly once");
}

#[test]
fn set_pool_column_present_with_empty_default() {
    let (_d, db) = db();
    db.write(|t| {
        t.execute("INSERT INTO dj_sets (name, status, created_at, updated_at) VALUES ('Old', 'draft', 'x', 'x')", [])?;
        Ok(())
    })
    .unwrap();
    let pool: String = db.read(|c| Ok(c.query_row("SELECT pool_sources FROM dj_sets", [], |r| r.get(0))?)).unwrap();
    assert_eq!(pool, "[]", "existing rows get the empty pool, not NULL");
}

#[test]
fn indexes_exist_and_migration_is_idempotent_on_an_empty_db() {
    let (_d, db) = db();
    let idx: Vec<String> = db
        .read(|c| {
            let mut st = c.prepare("PRAGMA index_list(harvest_items)")?;
            Ok(st.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<_, _>>()?)
        })
        .unwrap();
    assert!(idx.contains(&"ix_harvest_items_release_id".to_string()), "the inbox release_id index exists");
    let cols = db.read(|c| Ok(columns(c, "releases"))).unwrap();
    for want in ["source_fan_id", "expected_track_count", "snippet_only"] {
        assert!(cols.iter().any(|c| c == want), "releases.{want}");
    }
    let tcols = db.read(|c| Ok(columns(c, "tracks"))).unwrap();
    assert!(tcols.iter().any(|c| c == "is_snippet"));
    // migrating an already migrated connection is a no-op
    let mut c = Connection::open(db.path()).unwrap();
    migrate::migrate(&mut c).unwrap();
}
