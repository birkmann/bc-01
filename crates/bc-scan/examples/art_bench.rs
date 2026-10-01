//! `art_bench <legacy-art-dir> <n> <work-dir>`: converts the first `n` real legacy covers
//! (READ-ONLY source) into WebP under `<work-dir>` and prints covers/s.
use std::sync::Arc;
use std::time::Instant;

use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_libcore::Ctx;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (src, n, work) = (std::path::PathBuf::from(&a[1]), a[2].parse::<usize>().unwrap(), std::path::PathBuf::from(&a[3]));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).unwrap();
    let mut found: Vec<(i64, std::path::PathBuf)> = Vec::new();
    'outer: for shard in std::fs::read_dir(&src).unwrap().flatten() {
        for f in std::fs::read_dir(shard.path()).into_iter().flatten().flatten() {
            let name = f.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".jpg")
                && let Ok(id) = stem.parse::<i64>()
            {
                found.push((id, f.path()));
                if found.len() >= n {
                    break 'outer;
                }
            }
        }
    }
    println!("found {} covers", found.len());
    let mut config = Config::from_env();
    config.data_dir = work.clone();
    let db = Db::open(work.join("library.db")).unwrap();
    let rows = found.clone();
    db.write(move |tx| {
        for (id, p) in &rows {
            tx.execute(
                "INSERT INTO releases(id, title, title_key, kind, added_at, cover_path, snippet_only) VALUES (?1, 't', 't', 'album', CURRENT_TIMESTAMP, ?2, 0)",
                bc_db::rusqlite::params![id, p.to_string_lossy()],
            )?;
            tx.execute("INSERT INTO artwork(release_id, version, sizes, source) VALUES (?1, '0', 0, 'legacy')", [id])?;
        }
        Ok(())
    })
    .unwrap();
    let ctx = Ctx::new(db, Arc::new(EventBus::new()), config);
    let h = ctx.jobs.begin("art", "bench");
    // warm the page cache for the source files so the timing is CPU, not disk
    let t = Instant::now();
    for (_, p) in &found {
        let _ = std::fs::read(p);
    }
    println!("read-through (cold/warm cache): {:?}", t.elapsed());
    let t = Instant::now();
    let r = bc_scan::art::convert_legacy_blocking(&ctx, &src, &h).unwrap();
    let el = t.elapsed();
    println!("{r:?} in {el:?} = {:.1} covers/s with {} threads", r.converted as f64 / el.as_secs_f64(), (ctx.config.analysis_threads() / 2).max(1));
}
