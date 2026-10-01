//! Versioned migrations via `PRAGMA user_version`.
//! * v1 = the legacy Python schema (applied only on a fresh DB; a legacy DB copy
//!   already has every table and reports user_version 0, so it is just stamped).
//! * v2+ = additive changes from PLAN §4.1. Workstream 1 owns this list; other
//!   workstreams request new tables/columns by dropping
//!   `migrations/req_wsN_<what>.sql`, which `build.rs` embeds and which is applied
//!   once, by file name, after the numbered migrations (recorded in `schema_extras`).
//!
//! Data migrations (one-off repair passes, backfills) are separate: see
//! [`data_migration_done`] / [`mark_data_migration`] (recorded in `settings`).

use rusqlite::{Connection, OptionalExtension};

const LEGACY: &str = include_str!("../migrations/0001_legacy.sql");
const ADDITIONS: &str = include_str!("../migrations/0002_additions.sql");

include!(concat!(env!("OUT_DIR"), "/req_migrations.rs"));

/// (version, sql). Append only.
pub const MIGRATIONS: &[(i64, &str)] = &[(1, LEGACY), (2, ADDITIONS)];

/// Highest schema version this build knows.
pub fn latest_version() -> i64 {
    MIGRATIONS.last().map(|m| m.0).unwrap_or(0)
}

pub fn migrate(c: &mut Connection) -> rusqlite::Result<()> {
    let mut v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v == 0 {
        let has_legacy: bool = c.query_row(
            "SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name='tracks'",
            [],
            |r| r.get(0),
        )?;
        if has_legacy {
            c.execute_batch("PRAGMA user_version = 1")?;
            v = 1;
        }
    }
    for (ver, sql) in MIGRATIONS {
        if *ver > v {
            tracing::info!(version = ver, "applying schema migration");
            let t = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            t.execute_batch(sql)?;
            t.execute_batch(&format!("PRAGMA user_version = {ver}"))?;
            t.commit()?;
        }
    }
    apply_requested(c)?;
    Ok(())
}

fn apply_requested(c: &mut Connection) -> rusqlite::Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_extras (name TEXT PRIMARY KEY, applied_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP)",
    )?;
    for (name, sql) in REQUESTED {
        let done: bool = c
            .query_row("SELECT 1 FROM schema_extras WHERE name = ?1", [name], |r| r.get::<_, i64>(0))
            .optional()?
            .is_some();
        if done {
            continue;
        }
        tracing::info!(name, "applying requested schema extra");
        let t = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        t.execute_batch(sql)?;
        t.execute("INSERT INTO schema_extras(name) VALUES (?1)", [name])?;
        t.commit()?;
    }
    Ok(())
}

/// Key under which a one-off data migration records completion in `settings`.
fn dm_key(name: &str) -> String {
    format!("data_migration.{name}")
}

pub fn data_migration_done(c: &Connection, name: &str) -> rusqlite::Result<bool> {
    Ok(c.query_row("SELECT 1 FROM settings WHERE key = ?1", [dm_key(name)], |r| r.get::<_, i64>(0))
        .optional()?
        .is_some())
}

pub fn mark_data_migration(c: &Connection, name: &str) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO settings(key, value, updated_at) VALUES (?1, 'done', CURRENT_TIMESTAMP)
         ON CONFLICT(key) DO UPDATE SET value='done', updated_at=CURRENT_TIMESTAMP",
        [dm_key(name)],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(LEGACY).unwrap();
        c
    }

    #[test]
    fn fresh_db_reaches_latest() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, latest_version());
        for t in ["beat_grids", "cue_points", "waveform_meta", "artwork", "ui_state", "harvest_item_tags", "release_search", "artist_search", "label_search"] {
            let n: i64 = c
                .query_row("SELECT count(*) FROM sqlite_master WHERE name = ?1", [t], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1, "missing {t}");
        }
    }

    #[test]
    fn legacy_db_is_stamped_then_upgraded_with_data() {
        let mut c = legacy_conn();
        c.execute_batch(
            "INSERT INTO artists(id,name,name_key,created_at) VALUES (1,'Motörhead','motorhead','2020-01-01');
             INSERT INTO releases(id,title,title_key,artist_id,kind,year,added_at) VALUES (1,'Ace','ace',1,'album',1980,'2020-01-01');
             INSERT INTO tracks(id,release_id,artist_id,title,title_key,loved,play_count,skip_count,added_at,is_snippet) VALUES (1,1,1,'Ace of Spades','ace of spades',0,0,0,'2020-01-01',0);
             INSERT INTO library_roots(id,path,kind,watch,enabled) VALUES (1,'/x','library',0,1);
             INSERT INTO files(id,track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at) VALUES (1,1,1,'/x/a.mp3','a.mp3','mp3',1,1,'2020','2020');
             INSERT INTO analysis(track_id,analyzer_version,backend,status,analyzed_at,bpm) VALUES (1,1,'essentia','ok','2020',140.0);",
        )
        .unwrap();
        migrate(&mut c).unwrap();
        let (avail, ak, albk, year, bpm): (i64, String, String, i64, f64) = c
            .query_row("SELECT available, artist_key, album_key, year, bpm FROM tracks WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap();
        assert_eq!((avail, ak.as_str(), albk.as_str(), year, bpm), (1, "motorhead", "ace", 1980, 140.0));
        let hit: i64 = c
            .query_row("SELECT count(*) FROM artist_search WHERE artist_search MATCH '\"otör\"'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hit, 1);
    }

    #[test]
    fn triggers_keep_denormalised_columns() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        c.execute_batch(
            "INSERT INTO artists(id,name,name_key,created_at) VALUES (1,'A','a','x'),(2,'B','b','x');
             INSERT INTO releases(id,title,title_key,artist_id,kind,year,added_at) VALUES (1,'R','r',1,'album',2001,'x');
             INSERT INTO tracks(id,release_id,title,title_key,loved,play_count,skip_count,added_at) VALUES (1,1,'t','t',0,0,0,'x');
             INSERT INTO library_roots(id,path,kind,watch,enabled) VALUES (1,'/x','library',0,1);
             INSERT INTO files(id,track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at) VALUES (1,1,1,'/x/a.mp3','a.mp3','mp3',1,1,'x','x');",
        )
        .unwrap();
        let get = |c: &Connection| -> (i64, Option<String>, Option<i64>) {
            c.query_row("SELECT available, artist_key, year FROM tracks WHERE id=1", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap()
        };
        assert_eq!(get(&c), (1, Some("a".into()), Some(2001)));
        c.execute_batch("UPDATE files SET missing_since='now' WHERE id=1").unwrap();
        assert_eq!(get(&c).0, 0);
        c.execute_batch("UPDATE files SET missing_since=NULL WHERE id=1; UPDATE releases SET artist_id=2, year=1999 WHERE id=1").unwrap();
        assert_eq!(get(&c), (1, Some("b".into()), Some(1999)));
        c.execute_batch("UPDATE artists SET name_key='bb', name='BB' WHERE id=2").unwrap();
        assert_eq!(get(&c).1, Some("bb".into()));
        let n: i64 = c.query_row("SELECT count(*) FROM release_search WHERE release_search MATCH '\"BB\"' ", [], |r| r.get(0)).unwrap_or(-1);
        assert!(n == 0 || n == 1); // two-letter query is below the trigram minimum: no hit, no error
        c.execute_batch("DELETE FROM files WHERE id=1").unwrap();
        assert_eq!(get(&c).0, 0);
    }

    #[test]
    fn data_migration_marker() {
        let mut c = Connection::open_in_memory().unwrap();
        migrate(&mut c).unwrap();
        assert!(!data_migration_done(&c, "x").unwrap());
        mark_data_migration(&c, "x").unwrap();
        assert!(data_migration_done(&c, "x").unwrap());
    }
}
