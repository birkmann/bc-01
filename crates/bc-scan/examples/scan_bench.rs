//! `scan_bench <legacy.db> <work-dir>`: builds a bench DB in `<work-dir>` holding only the legacy
//! `library_roots` + `files` rows (so the stat comparison is real) and times `scan_root` on the
//! first root three times (the legacy root path already points at the real library).
//! cargo run --release -p bc-scan --example scan_bench -- /tmp/legacy-copy.db /tmp/scan-work
use std::sync::Arc;
use std::time::Instant;

use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_libcore::Ctx;
use bc_scan::scanner::{ScanHooks, scan_root};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let work = std::path::PathBuf::from(&args[2]);
    std::fs::create_dir_all(&work).unwrap();
    let dbp = work.join("library.db");
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", dbp.display()));
    }
    drop(Db::open(&dbp).expect("create"));
    {
        let t = Instant::now();
        let c = bc_db::rusqlite::Connection::open(&dbp).unwrap();
        c.execute_batch(&format!(
            "PRAGMA foreign_keys=OFF; ATTACH DATABASE 'file:{}?mode=ro' AS old;
             INSERT INTO library_roots SELECT * FROM old.library_roots;
             INSERT INTO files SELECT * FROM old.files;",
            args[1]
        ))
        .unwrap();
        println!("loaded legacy rows in {:?}", t.elapsed());
    }
    let db = Db::open(&dbp).expect("open");
    let id: i64 = db.read(|c| Ok(c.query_row("SELECT id FROM library_roots ORDER BY id LIMIT 1", [], |r| r.get(0))?)).expect("root row");
    let n: i64 = db.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM files WHERE root_id = ?1", [id], |r| r.get(0))?)).unwrap();
    println!("files rows for root {id}: {n}");
    let mut config = Config::from_env();
    config.data_dir = work.clone();
    let ctx = Ctx::new(db, Arc::new(EventBus::new()), config);
    for i in 0..3 {
        let t = Instant::now();
        let r = scan_root(&ctx, id, ScanHooks::none()).expect("scan");
        println!(
            "run {i}: {:?} seen={} unchanged={} added={} updated={} missing={} errors={:?}",
            t.elapsed(),
            r.files_seen,
            r.files_unchanged,
            r.files_added,
            r.files_updated,
            r.files_missing,
            r.errors.iter().take(3).collect::<Vec<_>>()
        );
    }
}
