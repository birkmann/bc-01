//! Unit tests: ingest rules (hand-made tags), idempotency, scanner, roots.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use bc_libcore::Ctx;

use crate::ingest::{IngestOptions, ReadFile, RootInfo, WorkItem, ingest_batch, ingest_paths};
use crate::media::FileTags;
use crate::scanner::{ScanHooks, scan_paths, scan_root};
use crate::testutil::*;

fn tags(title: &str, artist: &str, album: &str, year: &str, genres: &[&str]) -> FileTags {
    let mut t = FileTags {
        title: Some(title.into()),
        artist: Some(artist.into()),
        album: Some(album.into()),
        date: Some(year.into()),
        genres: genres.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    t.tag_hash = t.compute_hash();
    t
}

fn rf(path: &str, t: FileTags) -> ReadFile {
    ReadFile { item: WorkItem { path: PathBuf::from(path), size: 100, mtime_ns: 1, inode: Some(1) }, tags: t, cover: None }
}

fn ingest(ctx: &Ctx, root: &RootInfo, files: Vec<ReadFile>, opts: IngestOptions) -> crate::ingest::BatchOut {
    let root = root.clone();
    ctx.db.write_with::<_, bc_libcore::ApiError>(move |tx| ingest_batch(tx, &root, &files, &opts)).unwrap()
}

fn fake_root(ctx: &Ctx) -> RootInfo {
    let id = ctx
        .write(|tx| {
            tx.execute("INSERT INTO library_roots(path, kind, watch, enabled) VALUES ('/fake', 'library', 0, 1)", [])?;
            Ok(tx.last_insert_rowid())
        })
        .unwrap();
    RootInfo { id, path: PathBuf::from("/fake"), kind: "library".into() }
}

#[test]
fn tag_counts_do_not_double_on_reingest() {
    let e = env();
    let root = fake_root(&e.ctx);
    let t = || tags("A", "Art", "Alb", "2020", &["techno; dub techno", "Techno"]);
    ingest(&e.ctx, &root, vec![rf("/fake/a/1.mp3", t())], IngestOptions::default());
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tags"), 2);
    assert_eq!(scalar(&e.ctx, "SELECT SUM(track_count) FROM tags"), 2);
    // changed hash forces a full re-ingest
    let mut t2 = t();
    t2.comment_noop();
    t2.tag_hash = "other".into();
    ingest(&e.ctx, &root, vec![rf("/fake/a/1.mp3", t2)], IngestOptions::default());
    assert_eq!(scalar(&e.ctx, "SELECT SUM(track_count) FROM tags"), 2);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM track_tags"), 2);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tracks"), 1);
    // dropping a genre decrements its count
    let mut t3 = tags("A", "Art", "Alb", "2020", &["techno"]);
    t3.tag_hash = "third".into();
    ingest(&e.ctx, &root, vec![rf("/fake/a/1.mp3", t3)], IngestOptions::default());
    assert_eq!(scalar(&e.ctx, "SELECT SUM(track_count) FROM tags"), 1);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM track_tags"), 1);
}

impl FileTags {
    fn comment_noop(&mut self) {}
}

#[test]
fn release_identity_and_folder_wins_over_year() {
    let e = env();
    let root = fake_root(&e.ctx);
    ingest(&e.ctx, &root, vec![rf("/fake/x/Alb/1.mp3", tags("One", "Art", "Alb", "2020", &[]))], IngestOptions::default());
    // same album/artist, other year, same folder: same release
    ingest(&e.ctx, &root, vec![rf("/fake/x/Alb/2.mp3", tags("Two", "Art", "Alb", "2021", &[]))], IngestOptions::default());
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM releases"), 1);
    // other folder, other year: a different record
    ingest(&e.ctx, &root, vec![rf("/fake/y/Alb/1.mp3", tags("Three", "Art", "Alb", "2022", &[]))], IngestOptions::default());
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM releases"), 2);
    // album artist decides identity (compilation does not fragment)
    let mut a = tags("T1", "A1", "Comp", "2020", &[]);
    a.album_artist = Some("Various".into());
    let mut b = tags("T2", "A2", "Comp", "2020", &[]);
    b.album_artist = Some("Various".into());
    ingest(&e.ctx, &root, vec![rf("/fake/c/1.mp3", a), rf("/fake/c/2.mp3", b)], IngestOptions::default());
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM releases WHERE title = 'Comp'"), 1);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM artists"), 4);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM releases WHERE kind = 'album'"), 3);
}

#[test]
fn unknown_album_label_and_expected_track_count() {
    let e = env();
    let root = fake_root(&e.ctx);
    let mut t1 = tags("One", "Art", "", "", &[]);
    t1.album = None;
    t1.date = None;
    t1.label = Some("Some Label".into());
    t1.track_total = Some(10);
    let mut t2 = t1.clone();
    t2.title = Some("Two".into());
    t2.track_total = Some(8);
    t2.tag_hash = "x".into();
    ingest(&e.ctx, &root, vec![rf("/fake/u/1.mp3", t1), rf("/fake/u/2.mp3", t2)], IngestOptions::default());
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM releases WHERE title = 'Unknown Album' AND year IS NULL"), 1);
    assert_eq!(scalar(&e.ctx, "SELECT expected_track_count FROM releases"), 10, "raised, never lowered");
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM releases r JOIN labels l ON l.id = r.label_id WHERE l.name = 'Some Label'"), 1);
    assert_eq!(scalar(&e.ctx, "SELECT folder_path = '/fake/u' FROM releases"), 1);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tracks WHERE disc_no = 1"), 2);
}

#[test]
fn fan_shelf_snippet_flags_and_fts() {
    let e = env();
    let root = fake_root(&e.ctx);
    let opts = IngestOptions { source_fan_id: None, snippet: Some(true), skip_art: true };
    ctx_fan(&e.ctx);
    let o = IngestOptions { source_fan_id: Some(7), ..opts.clone() };
    let out = ingest(&e.ctx, &root, vec![rf("/fake/f/1.mp3", tags("Clip", "Art", "Teaser", "2020", &[]))], o);
    assert_eq!(out.tally.releases_created.len(), 1);
    assert_eq!(scalar(&e.ctx, "SELECT source_fan_id FROM releases"), 7);
    assert_eq!(scalar(&e.ctx, "SELECT is_snippet FROM tracks"), 1);
    assert_eq!(scalar(&e.ctx, "SELECT snippet_only FROM releases"), 1);
    // a later arrival must not re-file an existing release
    let o2 = IngestOptions { source_fan_id: Some(8), skip_art: true, ..Default::default() };
    ctx_fan_id(&e.ctx, 8);
    let out2 = ingest(&e.ctx, &root, vec![rf("/fake/f/2.mp3", tags("Full", "Art", "Teaser", "2020", &[]))], o2);
    assert!(out2.tally.releases_created.is_empty());
    assert_eq!(scalar(&e.ctx, "SELECT source_fan_id FROM releases"), 7);
    // one real track: no longer snippet-only
    assert_eq!(scalar(&e.ctx, "SELECT snippet_only FROM releases"), 0);
}

fn ctx_fan(ctx: &Ctx) {
    ctx_fan_id(ctx, 7)
}
fn ctx_fan_id(ctx: &Ctx, id: i64) {
    ctx.write(move |tx| {
        tx.execute("INSERT INTO fans(id, username, url, is_self, created_at) VALUES (?1, ?2, ?2, 0, CURRENT_TIMESTAMP)", bc_db::rusqlite::params![id, format!("fan{id}")])?;
        Ok(())
    })
    .unwrap();
}

#[test]
fn fts_follows_a_tag_change() {
    let e = env();
    let root = fake_root(&e.ctx);
    ingest(&e.ctx, &root, vec![rf("/fake/t/1.mp3", tags("Iron Lung", "Somatic", "Grid", "2026", &[]))], IngestOptions::default());
    let hits = |q: &str| {
        let q = q.to_string();
        e.ctx.db.read(|c| Ok(c.query_row("SELECT COUNT(*) FROM search_index WHERE search_index MATCH ?1", [q], |r| r.get::<_, i64>(0))?)).unwrap()
    };
    assert_eq!(hits("\"dub\""), 0);
    let mut t = tags("Iron Lung (Dub)", "Somatic", "Grid", "2026", &[]);
    t.tag_hash = "edited".into();
    ingest(&e.ctx, &root, vec![rf("/fake/t/1.mp3", t)], IngestOptions::default());
    assert_eq!(hits("\"dub\""), 1);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM search_index"), 1);
}

// ---------------------------------------------------------------- ported: test_ingest_idempotency

fn junk_root(e: &Env) -> (i64, PathBuf) {
    let root = e.dir.path().join("downloads");
    let f = root.join("Somatic/Grid Failure/01 - track.mp3");
    write_junk_mp3(&f, 1024);
    (add_root(&e.ctx, &root, "downloads"), f)
}

#[test]
fn ingesting_the_same_path_twice_updates_in_place() {
    let e = env();
    let (rid, f) = junk_root(&e);
    let first = ingest_paths(&e.ctx, rid, std::slice::from_ref(&f), &IngestOptions::default()).unwrap();
    let second = ingest_paths(&e.ctx, rid, std::slice::from_ref(&f), &IngestOptions::default()).unwrap();
    assert_eq!(first.files_added, 1);
    assert_eq!(second.files_added, 0, "the second pass must not insert a duplicate");
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM files"), 1);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tracks"), 1);
    assert_eq!(first.track_ids, second.track_ids);
}

#[test]
fn re_ingest_refreshes_stat_metadata() {
    let e = env();
    let (rid, f) = junk_root(&e);
    ingest_paths(&e.ctx, rid, std::slice::from_ref(&f), &IngestOptions::default()).unwrap();
    let before = scalar(&e.ctx, "SELECT size_bytes FROM files");
    write_junk_mp3(&f, 4096);
    ingest_paths(&e.ctx, rid, std::slice::from_ref(&f), &IngestOptions::default()).unwrap();
    assert_ne!(scalar(&e.ctx, "SELECT size_bytes FROM files"), before);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM files"), 1);
}

#[test]
fn a_failing_file_does_not_block_the_rest_of_a_batch() {
    let e = env();
    let (rid, good) = junk_root(&e);
    let gone = good.parent().unwrap().join("02 - gone.mp3");
    let rep = ingest_paths(&e.ctx, rid, &[gone, good], &IngestOptions::default()).unwrap();
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM files"), 1, "the readable file must still be indexed");
    assert_eq!(rep.errors.len(), 1);
}

#[test]
fn ingest_dir_finds_root_by_ancestry() {
    let e = env();
    let (_, f) = junk_root(&e);
    let rep = crate::ingest::ingest_dir(&e.ctx, f.parent().unwrap(), &IngestOptions::default()).unwrap();
    assert_eq!(rep.files_added, 1);
    let outside = e.dir.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    assert!(crate::ingest::ingest_dir(&e.ctx, &outside, &IngestOptions::default()).is_err());
}

// ---------------------------------------------------------------- scanner

fn scan(e: &Env, rid: i64) -> bc_types::library::ScanResult {
    scan_root(&e.ctx, rid, ScanHooks::none()).unwrap()
}

fn lib(e: &Env) -> (i64, PathBuf) {
    let m = e.music();
    for (a, al) in [("Somatic", "Grid Failure"), ("Ferric", "Oxide Bloom")] {
        for n in 1..=2 {
            write_junk_mp3(&m.join(a).join(al).join(format!("{n:02} - t{n}.mp3")), 256 * n);
        }
    }
    (add_root(&e.ctx, &m, "library"), m)
}

#[test]
fn scan_indexes_then_rescan_writes_nothing() {
    let e = env();
    let (rid, _) = lib(&e);
    let r = scan(&e, rid);
    assert_eq!((r.files_seen, r.files_added, r.tracks_added), (4, 4, 4));
    assert!(r.errors.is_empty(), "{:?}", r.errors);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tracks WHERE available = 1"), 4);
    let gen0 = e.ctx.db.generation();
    let r2 = scan(&e, rid);
    assert_eq!((r2.files_unchanged, r2.files_added, r2.files_updated, r2.files_missing), (4, 0, 0, 0));
    assert_eq!(e.ctx.db.generation(), gen0 + 1, "only the root's last_scan row is written");
    assert!(scalar(&e.ctx, "SELECT last_scan_ms IS NOT NULL FROM library_roots") == 1);
}

#[test]
fn changed_file_is_reingested_and_missing_marked_then_reappears() {
    let e = env();
    let (rid, m) = lib(&e);
    scan(&e, rid);
    let f = m.join("Somatic/Grid Failure/01 - t1.mp3");
    write_junk_mp3(&f, 9000);
    let r = scan(&e, rid);
    assert_eq!((r.files_updated, r.files_unchanged), (1, 3));
    // vanish
    let victim = m.join("Ferric/Oxide Bloom/02 - t2.mp3");
    let hidden = victim.with_extension("hidden");
    std::fs::rename(&victim, &hidden).unwrap();
    let r = scan(&e, rid);
    assert_eq!(r.files_missing, 1);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tracks"), 4, "the track row must survive");
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL"), 1);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tracks WHERE available = 1"), 3);
    // already marked: not counted again
    assert_eq!(scan(&e, rid).files_missing, 0);
    // re-appears
    std::fs::rename(&hidden, &victim).unwrap();
    let r = scan(&e, rid);
    assert_eq!(r.files_updated, 1);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL"), 0);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tracks WHERE available = 1"), 4);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tracks"), 4);
}

#[test]
fn unmounted_root_marks_nothing() {
    let e = env();
    let (rid, m) = lib(&e);
    scan(&e, rid);
    // the drive disappears: the mountpoint is an empty directory
    std::fs::remove_dir_all(&m).unwrap();
    std::fs::create_dir_all(&m).unwrap();
    let r = scan(&e, rid);
    assert_eq!(r.files_seen, 0);
    assert_eq!(r.files_missing, 0);
    assert!(!r.errors.is_empty());
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL"), 0);
    // the directory itself gone
    std::fs::remove_dir_all(&m).unwrap();
    let r = scan(&e, rid);
    assert_eq!(r.files_missing, 0);
    assert!(r.errors[0].contains("does not exist"));
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL"), 0);
}

#[test]
fn scan_paths_ingests_and_marks() {
    let e = env();
    let (rid, m) = lib(&e);
    scan(&e, rid);
    let new = m.join("New/Rec/01 - n.mp3");
    write_junk_mp3(&new, 10);
    let r = scan_paths(&e.ctx, rid, std::slice::from_ref(&new)).unwrap();
    assert_eq!((r.files_added, r.tracks_added), (1, 1));
    // a whole directory vanishes
    std::fs::remove_dir_all(m.join("Somatic")).unwrap();
    let r = scan_paths(&e.ctx, rid, &[m.join("Somatic")]).unwrap();
    assert_eq!(r.files_missing, 2);
    let r = scan_paths(&e.ctx, rid, std::slice::from_ref(&new)).unwrap();
    assert_eq!(r.files_unchanged, 1);
}

#[test]
fn removed_tracks_keep_their_files_stay_out_of_scans_and_can_be_restored() {
    let e = env();
    let (rid, m) = lib(&e);
    scan(&e, rid);
    let ids = |sql: &str| -> Vec<i64> {
        e.ctx.read(|c| Ok(c.prepare(sql)?.query_map([], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?)).unwrap()
    };
    // untagged, so all four share one release; three go, the release stays
    let gone = ids("SELECT track_id FROM files WHERE path NOT LIKE '%Oxide Bloom/02%'");
    assert_eq!(gone.len(), 3);
    let out = bc_maint::delete::remove_tracks(&e.ctx, &gone).unwrap();
    assert_eq!((out.tracks, out.releases, out.excluded), (3, 0, 3));
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tracks"), 1);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM releases"), 1);
    let f = m.join("Somatic/Grid Failure/01 - t1.mp3");
    assert!(f.is_file(), "the file stays on disk");

    // neither a full scan, nor the watcher, nor an explicit ingest brings them back
    let r = scan(&e, rid);
    assert_eq!((r.files_seen, r.files_added, r.files_missing), (1, 0, 0));
    let r = scan_paths(&e.ctx, rid, &[m.join("Somatic")]).unwrap();
    assert_eq!(r.files_added, 0);
    ingest_paths(&e.ctx, rid, std::slice::from_ref(&f), &IngestOptions::default()).unwrap();
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tracks"), 1);

    // the last track takes the release with it
    let out = bc_maint::delete::remove_tracks(&e.ctx, &ids("SELECT id FROM tracks")).unwrap();
    assert_eq!((out.tracks, out.releases), (1, 1));
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM releases"), 0);
    assert_eq!(scan(&e, rid).files_added, 0);

    // restoring lifts the exclusion and ingests straight away
    let listed = e.ctx.read(bc_maint::excluded::list).unwrap();
    assert_eq!(listed.len(), 4);
    assert!(listed.iter().all(|x| !x.title.is_empty()), "the list names what was removed: {listed:?}");
    let paths: Vec<String> = listed.into_iter().map(|x| x.path).collect();
    let r = crate::excluded::restore(&e.ctx, &paths).unwrap();
    assert_eq!((r.restored, r.tracks_added), (4, 4));
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tracks"), 4);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM excluded_files"), 0);
}

#[test]
fn cancel_stops_before_marking() {
    let e = env();
    let (rid, m) = lib(&e);
    let cancel = AtomicBool::new(true);
    let r = scan_root(&e.ctx, rid, ScanHooks { progress: None, cancel: &cancel }).unwrap();
    assert_eq!((r.files_missing, r.tracks_added), (0, 0));
    assert_eq!(scalar(&e.ctx, "SELECT last_scan_at IS NULL FROM library_roots"), 1, "a stopped scan is no scan");

    // after a full scan, a stopped rescan neither marks the vanished folder nor moves last_scan_at
    scan(&e, rid);
    e.ctx.write(|tx| Ok(tx.execute("UPDATE library_roots SET last_scan_ms = -1", [])?)).unwrap();
    std::fs::remove_dir_all(m.join("Somatic")).unwrap();
    let r = scan_root(&e.ctx, rid, ScanHooks { progress: None, cancel: &cancel }).unwrap();
    assert_eq!(r.files_missing, 0);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL"), 0);
    assert_eq!(scalar(&e.ctx, "SELECT last_scan_ms FROM library_roots"), -1);
}

// ---------------------------------------------------------------- roots

#[test]
fn roots_add_is_idempotent_validated_and_cascades() {
    let e = env();
    let (rid, m) = lib(&e);
    scan(&e, rid);
    let again = crate::roots::add_root(&e.ctx, &m.to_string_lossy(), "library").unwrap();
    assert_eq!(again.id, rid);
    assert_eq!(again.track_count, 4);
    assert!(crate::roots::add_root(&e.ctx, "/nonexistent/zzz", "library").is_err());
    let f = m.join("Somatic/Grid Failure/01 - t1.mp3");
    assert!(crate::roots::add_root(&e.ctx, &f.to_string_lossy(), "library").is_err());
    assert!(crate::roots::add_root(&e.ctx, &m.to_string_lossy(), "weird").is_err());
    let p = crate::roots::patch_root(&e.ctx, rid, bc_types::library::RootPatch { enabled: None, watch: Some(true) }).unwrap();
    assert!(p.watch);
    crate::roots::remove_root(&e.ctx, rid).unwrap();
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM files"), 0);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM tracks"), 4);
    assert!(crate::roots::remove_root(&e.ctx, rid).is_err());
}

#[test]
fn ensure_roots_registers_configured_dirs() {
    let e = env();
    std::fs::create_dir_all(&e.ctx.config.download_dir).unwrap();
    let ids = crate::roots::ensure_roots(&e.ctx).unwrap();
    assert_eq!(ids.len(), 2);
    assert_eq!(crate::roots::ensure_roots(&e.ctx).unwrap(), ids);
    assert_eq!(scalar(&e.ctx, "SELECT COUNT(*) FROM library_roots WHERE kind = 'downloads'"), 1);
}
