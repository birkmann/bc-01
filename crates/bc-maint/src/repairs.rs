//! The run-once repair passes of the legacy startup (`main.lifespan`), exposed as plain functions
//! over a transaction plus [`run_all`], which the importer calls once as a versioned data
//! migration ([`run_once`] adds the "already done" guard in `settings`).
//!
//! Every pass is idempotent (a second [`run_all`] changes nothing) and linear in the table sizes:
//! one aggregate scan and primary-key updates, never a per-row query against an unindexed column
//! (the legacy comments name the correlated subqueries that once wedged startup for minutes on the
//! 200k-row harvest inbox).
//!
//! Order matters and follows the legacy startup:
//! 1. [`backfill_release_urls`] -- URLs from the done job items;
//! 2. [`backfill_release_labels`] -- before the URL repair, whose take-back branch needs the
//!    corrupt URLs still in place to prove a label was inherited from a foreign page;
//! 3. [`repair_release_urls`], 4. [`repair_harvest_links`], 5. [`merge_folder_twins`] (after the URL
//!    repairs so the fold inherits a corrected URL), 6. [`resolve_stale_queued`];
//! 7. [`loved::reconcile_all`], 8. [`backfill_expected_counts`] (after the link repairs),
//!    9. [`snippets::backfill_tx`].
//!
//! Not here: `backfill_artist_urls`, `backfill_label_urls` and `backfill_harvest_label_names` live
//! in the legacy *harvest* services (Bandcamp-aware naming rules), not the library layer.

use std::collections::BTreeMap;
use std::time::Instant;

use bc_db::rusqlite::Transaction;
use bc_libcore::{ApiResult, Ctx};
use serde::Serialize;

pub use crate::completeness::backfill_expected_counts;
pub use crate::dedup::{backfill_release_labels, backfill_release_urls, merge_folder_twins, repair_harvest_links, repair_release_urls, resolve_stale_queued};
use crate::{loved, snippets};

/// Name of the data-migration marker [`run_once`] records in `settings`.
pub const MIGRATION_NAME: &str = "maint-repairs-v1";

/// What one [`run_all`] changed.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RepairReport {
    /// `bandcamp_url` copied onto releases from finished download items.
    pub release_urls_backfilled: usize,
    /// Releases filed under (or taken off) a label from the harvest evidence.
    pub release_labels_changed: usize,
    /// Releases whose corrupt `bandcamp_url` was re-derived.
    pub release_urls_corrected: usize,
    /// ... and those cleared for lack of a confident match.
    pub release_urls_cleared: usize,
    /// Harvest items unlinked from a release that was not their record.
    pub harvest_links_reset: usize,
    /// Duplicate release rows folded away.
    pub folder_twins_merged: usize,
    /// Stale `queued` inbox rows now `in_library`.
    pub queued_resolved: usize,
    /// ... and those sent back to `new`.
    pub queued_reopened: usize,
    /// Loved streams converted to loved library tracks.
    pub loved_streams_adopted: i64,
    pub expected_counts_learned: usize,
    pub snippet_tracks_changed: usize,
    pub snippet_releases_changed: usize,
    /// Wall time per pass (ms).
    pub timings_ms: BTreeMap<String, u64>,
}

impl RepairReport {
    /// Whether any pass changed anything.
    pub fn changed_anything(&self) -> bool {
        self.release_urls_backfilled
            + self.release_labels_changed
            + self.release_urls_corrected
            + self.release_urls_cleared
            + self.harvest_links_reset
            + self.folder_twins_merged
            + self.queued_resolved
            + self.queued_reopened
            + self.expected_counts_learned
            + self.snippet_tracks_changed
            + self.snippet_releases_changed
            > 0
            || self.loved_streams_adopted > 0
    }
}

fn step<T: Send + 'static>(ctx: &Ctx, name: &str, report: &mut RepairReport, f: impl FnOnce(&Transaction<'_>) -> ApiResult<T> + Send + 'static) -> ApiResult<T> {
    let t0 = Instant::now();
    let out = ctx.write(f)?;
    let ms = t0.elapsed().as_millis() as u64;
    tracing::info!(pass = name, ms, "repair pass finished");
    report.timings_ms.insert(name.to_string(), ms);
    Ok(out)
}

/// Run every repair pass once, each in its own transaction (a failing pass does not undo the
/// others). Idempotent.
pub fn run_all(ctx: &Ctx) -> ApiResult<RepairReport> {
    let mut r = RepairReport::default();
    r.release_urls_backfilled = step(ctx, "backfill_release_urls", &mut r, backfill_release_urls)?;
    r.release_labels_changed = step(ctx, "backfill_release_labels", &mut r, backfill_release_labels)?;
    let (fixed, cleared) = step(ctx, "repair_release_urls", &mut r, repair_release_urls)?;
    r.release_urls_corrected = fixed;
    r.release_urls_cleared = cleared;
    r.harvest_links_reset = step(ctx, "repair_harvest_links", &mut r, repair_harvest_links)?;
    r.folder_twins_merged = step(ctx, "merge_folder_twins", &mut r, merge_folder_twins)?;
    let (resolved, reopened) = step(ctx, "resolve_stale_queued", &mut r, resolve_stale_queued)?;
    r.queued_resolved = resolved;
    r.queued_reopened = reopened;
    r.loved_streams_adopted = step(ctx, "loved_reconcile_all", &mut r, |t| Ok(loved::reconcile_all(t)?.streams_cleared))?;
    r.expected_counts_learned = step(ctx, "backfill_expected_counts", &mut r, backfill_expected_counts)?;
    let (tracks, releases) = step(ctx, "snippets_backfill", &mut r, snippets::backfill_tx)?;
    r.snippet_tracks_changed = tracks;
    r.snippet_releases_changed = releases;
    Ok(r)
}

/// [`run_all`] guarded by a data-migration marker: `Ok(None)` when it already ran.
pub fn run_once(ctx: &Ctx) -> ApiResult<Option<RepairReport>> {
    let done = ctx.read(|c| Ok(bc_db::migrate::data_migration_done(c, MIGRATION_NAME)?))?;
    if done {
        return Ok(None);
    }
    let report = run_all(ctx)?;
    ctx.write(|t| Ok(bc_db::migrate::mark_data_migration(t, MIGRATION_NAME)?))?;
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    fn hi(url: &str, kind: &str, state: &str, artist: &str, title: &str, label: Option<&str>, release: Option<i64>, track_count: Option<i64>) -> String {
        format!(
            "INSERT INTO harvest_items(url,url_kind,state,title,artist_name,label_name,tags,in_collection,in_wishlist,is_free_download,is_purchasable,is_preorder,discovered_at,release_id,track_count)
             VALUES ('{url}','{kind}','{state}','{title}','{artist}',{},'[]',0,0,0,0,0,'2024-01-01 00:00:00',{},{});",
            label.map(|l| format!("'{l}'")).unwrap_or("NULL".into()),
            release.map(|r| r.to_string()).unwrap_or("NULL".into()),
            track_count.map(|r| r.to_string()).unwrap_or("NULL".into()),
        )
    }

    fn url_of(env: &TestEnv, id: i64) -> Option<String> {
        q_str(&env.db, &format!("SELECT bandcamp_url FROM releases WHERE id={id}"))
    }

    #[test]
    fn backfill_release_urls_copies_done_album_items_once() {
        let env = test_env();
        let a = seed_release(&env.db, "A", "One", None, None);
        let b = seed_release(&env.db, "A", "Two", Some("https://a.bandcamp.com/album/two"), None);
        exec(&env.db, "INSERT INTO jobs(id,kind,status,priority,params,total,completed,failed,skipped,cancel_requested,created_at) VALUES ('j','download','completed',1,'{}',3,3,0,0,0,'x');");
        for (seq, url, rid) in [(0, "https://A.bandcamp.com/album/one/", a), (1, "https://a.bandcamp.com/album/two-x", b), (2, "https://a.bandcamp.com/album/one", b)] {
            exec(&env.db, &format!("INSERT INTO job_items(job_id,seq,status,url,url_kind,attempts,max_attempts,progress,release_id) VALUES ('j',{seq},'done','{url}','album',0,3,1,{rid})"));
        }
        let n = env.db.write(|t| Ok(backfill_release_urls(t).unwrap())).unwrap();
        assert_eq!(n, 1, "b already has a URL; its other item's URL is claimed by a");
        assert_eq!(url_of(&env, a).as_deref(), Some("https://a.bandcamp.com/album/one"));
        assert_eq!(env.db.write(|t| Ok(backfill_release_urls(t).unwrap())).unwrap(), 0);
    }

    #[test]
    fn repair_release_urls_rederives_by_name_and_clears_the_unprovable() {
        let env = test_env();
        // Two releases that swapped URLs.
        let alpha = seed_release(&env.db, "Alpha", "Alpha Record", Some("https://x.bandcamp.com/album/beta-record"), None);
        let beta = seed_release(&env.db, "Beta", "Beta Record", Some("https://x.bandcamp.com/album/alpha-record"), None);
        // One whose page the inbox knows nothing about is left alone.
        let calm = seed_release(&env.db, "Calm", "Calm Record", Some("https://x.bandcamp.com/album/calm-record"), None);
        // One whose URL names a page about someone else with no name match anywhere: cleared.
        let lost = seed_release(&env.db, "Lost", "Lost Record", Some("https://x.bandcamp.com/album/other-thing"), None);
        let sql = [
            hi("https://x.bandcamp.com/album/alpha-record", "album", "new", "Alpha", "Alpha Record", None, None, None),
            hi("https://x.bandcamp.com/album/beta-record", "album", "new", "Beta", "Beta Record", None, None, None),
            hi("https://x.bandcamp.com/album/other-thing", "album", "new", "Someone", "Other Thing", None, None, None),
        ]
        .join("\n");
        exec(&env.db, &sql);
        let (fixed, cleared) = env.db.write(|t| Ok(repair_release_urls(t).unwrap())).unwrap();
        assert_eq!((fixed, cleared), (2, 1));
        assert_eq!(url_of(&env, alpha).as_deref(), Some("https://x.bandcamp.com/album/alpha-record"));
        assert_eq!(url_of(&env, beta).as_deref(), Some("https://x.bandcamp.com/album/beta-record"));
        assert_eq!(url_of(&env, calm).as_deref(), Some("https://x.bandcamp.com/album/calm-record"));
        assert_eq!(url_of(&env, lost), None);
        assert_eq!(env.db.write(|t| Ok(repair_release_urls(t).unwrap())).unwrap(), (0, 0));
    }

    #[test]
    fn repair_harvest_links_resets_foreign_links() {
        let env = test_env();
        let mine = seed_release(&env.db, "Alpha", "Alpha Record", Some("https://x.bandcamp.com/album/alpha-record"), None);
        let other = seed_release(&env.db, "Beta", "Beta Record", None, None);
        exec(&env.db, &[
            hi("https://x.bandcamp.com/album/alpha-record", "album", "in_library", "Alpha", "Alpha Record", None, Some(mine), None),
            hi("https://x.bandcamp.com/album/wrong", "album", "in_library", "Alpha", "Different", None, Some(other), None),
            hi("https://y.bandcamp.com/album/by-name", "album", "in_library", "Beta", "Beta Record", None, Some(other), None),
        ].join("\n"));
        assert_eq!(env.db.write(|t| Ok(repair_harvest_links(t).unwrap())).unwrap(), 1);
        assert_eq!(q_str(&env.db, "SELECT state FROM harvest_items WHERE title='Different'").as_deref(), Some("new"));
        assert_eq!(q_i64(&env.db, "SELECT COUNT(*) FROM harvest_items WHERE state='in_library'"), 2);
    }

    #[test]
    fn merge_folder_twins_folds_the_shorter_row_into_the_keeper() {
        let env = test_env();
        let folder = "/music/a/rec";
        let old = seed_release(&env.db, "A", "Rec", Some("https://a.bandcamp.com/album/rec"), Some(2020));
        let new = seed_release(&env.db, "A", "Rec", None, Some(2021));
        exec(&env.db, &format!("UPDATE releases SET folder_path='{folder}' WHERE id IN ({old},{new}); UPDATE releases SET added_at='2022-01-01 00:00:00' WHERE id={old};"));
        for n in 1..=2 {
            seed_track(&env.db, old, &format!("o{n}"), Some(n));
        }
        for n in 1..=5 {
            seed_track(&env.db, new, &format!("n{n}"), Some(n));
        }
        // A shelf root shared by different titles is never folded on.
        let x = seed_release(&env.db, "B", "Shelf One", None, None);
        let y = seed_release(&env.db, "B", "Shelf Two", None, None);
        exec(&env.db, &format!("UPDATE releases SET folder_path='/music/b' WHERE id IN ({x},{y})"));
        assert_eq!(env.db.write(|t| Ok(merge_folder_twins(t).unwrap())).unwrap(), 1);
        assert_eq!(q_i64(&env.db, &format!("SELECT COUNT(*) FROM releases WHERE id={old}")), 0, "the shorter row folds away");
        assert_eq!(q_i64(&env.db, &format!("SELECT COUNT(*) FROM tracks WHERE release_id={new}")), 7);
        assert_eq!(url_of(&env, new).as_deref(), Some("https://a.bandcamp.com/album/rec"), "a Bandcamp URL is inherited");
        assert_eq!(q_str(&env.db, &format!("SELECT added_at FROM releases WHERE id={new}")).as_deref(), Some("2022-01-01 00:00:00"), "the older added_at wins");
        assert_eq!(env.db.write(|t| Ok(merge_folder_twins(t).unwrap())).unwrap(), 0);
        assert_eq!(q_i64(&env.db, &format!("SELECT COUNT(*) FROM releases WHERE id IN ({x},{y})")), 2);
    }

    #[test]
    fn resolve_stale_queued_matches_by_url_then_name_and_reopens_the_rest() {
        let env = test_env();
        let by_url = seed_release(&env.db, "A", "One", Some("https://a.bandcamp.com/album/one"), None);
        let by_name = seed_release(&env.db, "B", "Two", None, None);
        exec(&env.db, &[
            hi("https://a.bandcamp.com/album/one", "album", "queued", "A", "One", None, None, None),
            hi("https://b.bandcamp.com/album/two", "album", "queued", "B", "Two", None, None, None),
            hi("https://c.bandcamp.com/album/three", "album", "queued", "C", "Three", None, None, None),
            hi("https://d.bandcamp.com/album/live", "album", "queued", "D", "Live", None, None, None),
        ].join("\n"));
        // A live job item keeps its inbox row queued.
        exec(&env.db, "INSERT INTO jobs(id,kind,status,priority,params,total,completed,failed,skipped,cancel_requested,created_at) VALUES ('j','download','running',1,'{}',1,0,0,0,0,'x');
                       INSERT INTO job_items(job_id,seq,status,url,attempts,max_attempts,progress) VALUES ('j',0,'running','https://D.bandcamp.com/album/live',0,3,0);");
        let (resolved, reopened) = env.db.write(|t| Ok(resolve_stale_queued(t).unwrap())).unwrap();
        assert_eq!((resolved, reopened), (2, 1));
        assert_eq!(q_str(&env.db, "SELECT state FROM harvest_items WHERE title='One'").as_deref(), Some("in_library"));
        assert_eq!(q_i64(&env.db, "SELECT release_id FROM harvest_items WHERE title='One'"), by_url);
        assert_eq!(q_i64(&env.db, "SELECT release_id FROM harvest_items WHERE title='Two'"), by_name);
        assert_eq!(url_of(&env, by_name).as_deref(), Some("https://b.bandcamp.com/album/two"), "the URL is recorded while it is known");
        assert_eq!(q_str(&env.db, "SELECT state FROM harvest_items WHERE title='Three'").as_deref(), Some("new"));
        assert_eq!(q_str(&env.db, "SELECT state FROM harvest_items WHERE title='Live'").as_deref(), Some("queued"));
        assert_eq!(env.db.write(|t| Ok(resolve_stale_queued(t).unwrap())).unwrap(), (0, 0));
    }

    #[test]
    fn backfill_release_labels_files_by_name_and_takes_back_foreign_labels() {
        let env = test_env();
        let a = seed_release(&env.db, "Alpha", "Alpha Record", Some("https://l.bandcamp.com/album/alpha-record"), None);
        // `foreign` holds a label inherited from another record's page.
        let foreign = seed_release(&env.db, "Zed", "Zed Record", Some("https://l.bandcamp.com/album/other"), None);
        exec(&env.db, &format!("INSERT INTO labels(name,name_key) VALUES ('Inherited','inherited'); UPDATE releases SET label_id=(SELECT id FROM labels WHERE name_key='inherited') WHERE id={foreign};"));
        exec(&env.db, &[
            hi("https://l.bandcamp.com/album/alpha-record", "album", "new", "Alpha", "Alpha Record", Some("Great Label"), None, None),
            hi("https://l.bandcamp.com/album/other", "album", "new", "Other", "Other", Some("Inherited"), None, None),
        ].join("\n"));
        assert_eq!(env.db.write(|t| Ok(backfill_release_labels(t).unwrap())).unwrap(), 2);
        assert_eq!(q_str(&env.db, &format!("SELECT l.name FROM releases r JOIN labels l ON l.id=r.label_id WHERE r.id={a}")).as_deref(), Some("Great Label"));
        assert_eq!(q_i64(&env.db, &format!("SELECT COALESCE(label_id,-1) FROM releases WHERE id={foreign}")), -1);
        assert_eq!(env.db.write(|t| Ok(backfill_release_labels(t).unwrap())).unwrap(), 0);
    }

    #[test]
    fn run_all_is_idempotent_on_seeded_data() {
        let env = test_env();
        // A bit of everything: strays for snippets/expected counts/loved/queued.
        let r = seed_release(&env.db, "Sporting Life", "Slam Dunk Vol. I", None, Some(2024));
        seed_track(&env.db, r, "Hydrate The Hustle", Some(1));
        let clip = seed_track(&env.db, r, "Badminton [SNIPPET]", Some(2));
        exec(&env.db, "INSERT INTO loved_streams(page_url,track_key,track_index,title,artist_name,release_title,added_at) VALUES ('https://x.bandcamp.com/album/slam','1',0,'Hydrate The Hustle','Sporting Life','Slam Dunk Vol. I','x');");
        exec(&env.db, &hi("https://sportinglife.bandcamp.com/album/slam-dunk-vol-i", "album", "queued", "Sporting Life", "Slam Dunk Vol. I", Some("Sporting Records"), None, Some(8)));
        let first = run_all(&env).unwrap();
        assert_eq!(first.queued_resolved, 1);
        assert_eq!(first.loved_streams_adopted, 1);
        assert_eq!(first.snippet_tracks_changed, 1);
        assert_eq!(first.release_labels_changed, 1);
        assert_eq!(first.expected_counts_learned, 1);
        assert_eq!(q_i64(&env.db, &format!("SELECT is_snippet FROM tracks WHERE id={clip}")), 1);
        assert!(first.changed_anything());
        let second = run_all(&env).unwrap();
        assert!(!second.changed_anything(), "second run changes nothing: {second:?}");
        // The guarded variant records itself and then declines.
        assert!(run_once(&env).unwrap().is_some());
        assert!(run_once(&env).unwrap().is_none());
    }

    #[test]
    fn run_all_is_fast_on_a_large_seeded_library() {
        // 20k releases, 60k tracks, 40k inbox rows: no pass may be quadratic.
        let env = test_env();
        env.db
            .write(|t| {
                t.execute_batch("INSERT INTO artists(id,name,name_key,created_at) SELECT value, 'A'||value, 'a'||value, 'x' FROM generate_series(1,5000);")?;
                Ok(())
            })
            .ok();
        let ok = q_i64(&env.db, "SELECT COUNT(*) FROM artists") > 0;
        if !ok {
            // generate_series is not compiled into this SQLite: seed with a recursive CTE.
            exec(&env.db, "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<5000) INSERT INTO artists(id,name,name_key,created_at) SELECT i,'A'||i,'a'||i,'x' FROM n;");
        }
        exec(&env.db, "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<20000)
            INSERT INTO releases(id,title,title_key,artist_id,kind,added_at,bandcamp_url,folder_path)
            SELECT i,'R'||i,'r'||i,(i%5000)+1,'album','2024-01-01 00:00:00', CASE WHEN i%3=0 THEN 'https://h'||(i%700)||'.bandcamp.com/album/r'||i END, '/m/'||(i%5000)||'/r'||i FROM n;");
        exec(&env.db, "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<60000)
            INSERT INTO tracks(release_id,artist_id,title,title_key,track_no,loved,play_count,skip_count,added_at)
            SELECT (i%20000)+1,((i%20000)%5000)+1,'t'||i,'t'||i,(i%12)+1,0,0,0,'2024-01-01 00:00:00' FROM n;");
        exec(&env.db, "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<40000)
            INSERT INTO harvest_items(url,url_kind,state,title,artist_name,label_name,tags,in_collection,in_wishlist,is_free_download,is_purchasable,is_preorder,discovered_at,release_id,track_count)
            SELECT 'https://h'||(i%900)||'.bandcamp.com/album/x'||i,'album',CASE WHEN i%4=0 THEN 'queued' WHEN i%4=1 THEN 'in_library' ELSE 'new' END,'R'||(i%20000),'A'||(i%5000),
                   CASE WHEN i%5=0 THEN 'Label '||(i%200) END,'[]',0,0,0,0,0,'2024-01-01 00:00:00',CASE WHEN i%4=1 THEN (i%20000)+1 END, 8 FROM n;");
        let t0 = Instant::now();
        let first = run_all(&env).unwrap();
        let took = t0.elapsed();
        assert!(took.as_secs() < 30, "run_all took {took:?}: {:?}", first.timings_ms);
        eprintln!("run_all on 20k releases / 60k tracks / 40k inbox rows: {took:?} {:?}", first.timings_ms);
        let second = run_all(&env).unwrap();
        assert!(!second.changed_anything(), "{second:?}");
    }
}

#[cfg(test)]
mod real_db {
    use super::*;
    use crate::testutil::*;
    use std::time::Instant;

    /// `BC_MAINT_REAL_DB=/path/to/legacy-copy.db cargo test -p bc-maint real_db -- --ignored --nocapture`
    /// Copies the DB first (the original is never touched) and times every pass and listing.
    #[test]
    #[ignore]
    fn timings_on_the_real_library() {
        let Some(src) = std::env::var_os("BC_MAINT_REAL_DB") else { return };
        let work = std::env::var_os("BC_MAINT_SCRATCH").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let dst = work.join("maint-real.db");
        std::fs::copy(&src, &dst).unwrap();
        let t0 = Instant::now();
        let db = bc_db::Db::open(&dst).unwrap();
        eprintln!("open+migrate: {:?}", t0.elapsed());
        let mut cfg = bc_core::Config::from_env();
        cfg.data_dir = work.join("maint-data");
        let ctx = bc_libcore::Ctx::new(db, std::sync::Arc::new(bc_core::EventBus::new()), cfg);
        let t0 = Instant::now();
        let r = run_all(&ctx).unwrap();
        eprintln!("run_all #1: {:?}\n{r:#?}", t0.elapsed());
        let t0 = Instant::now();
        let r2 = run_all(&ctx).unwrap();
        eprintln!("run_all #2: {:?} changed={}", t0.elapsed(), r2.changed_anything());
        assert!(!r2.changed_anything(), "{r2:?}");
        let t0 = Instant::now();
        let n = ctx.read(|c| Ok(crate::cleanup::find_candidates(c, 60_000, true).unwrap().len())).unwrap();
        eprintln!("cleanup candidates: {n} in {:?}", t0.elapsed());
        let t0 = Instant::now();
        let s = ctx.read(|c| Ok(crate::strays::strays_out(c, None, 50).unwrap())).unwrap();
        eprintln!("strays: total {} resolvable {} in {:?}", s.total, s.resolvable, t0.elapsed());
        let t0 = Instant::now();
        let ids = ctx.read(|c| Ok(bc_libcore::hydrate::missing_release_ids(c, None).unwrap())).unwrap();
        eprintln!("missing releases: {} in {:?}", ids.len(), t0.elapsed());
        let t0 = Instant::now();
        let sweep = crate::completeness::queue_fills(&ctx, &ids).unwrap();
        eprintln!("queue_fills: {sweep:?} in {:?}", t0.elapsed());
        let _ = q_i64;
    }
}
