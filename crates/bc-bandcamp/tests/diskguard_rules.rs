//! Port of `test_diskguard.py` (the rule, the setting and the free-space probe).
//!
//! Wave 2 (they need the download worker / HTTP API):
//! `test_worker_holds_and_hands_back_what_is_in_flight` (worker `_disk_guard`,
//! `downloads.disk` events, hand-back of in-flight items) and
//! `test_disk_endpoint_reports_and_sets_the_limit` (`/api/downloads/disk`).

use bc_bandcamp::download::diskguard::*;
use bc_db::Db;

const GB: i64 = 1024 * 1024 * 1024;

// -- the rule -----------------------------------------------------------------

#[test]
fn holds_below_the_limit_and_releases_only_with_margin() {
    let limit = 5 * GB;
    assert!(should_hold(Some(4 * GB), limit, false));
    assert!(!should_hold(Some(6 * GB), limit, false));
    // Right at the line: not yet.
    assert!(!should_hold(Some(limit), limit, false));
    // Once held, a hair above the limit is not enough to let go...
    assert!(should_hold(Some(limit + 1), limit, true));
    // ...but the margin is.
    assert!(!should_hold(Some(limit + RESUME_MARGIN_BYTES), limit, true));
}

#[test]
fn an_unreadable_drive_holds() {
    assert!(should_hold(None, 5 * GB, false));
    assert!(should_hold(None, 5 * GB, true));
}

#[test]
fn free_bytes_walks_up_to_an_existing_ancestor() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("not").join("yet").join("here");
    assert!(free_bytes(tmp.path()).is_some());
    // Same volume, so the same free space (allow for concurrent writers).
    let (a, b) = (free_bytes(&missing).unwrap(), free_bytes(tmp.path()).unwrap());
    assert!((a - b).abs() < 64 * 1024 * 1024, "{a} vs {b}");
}

#[test]
fn free_bytes_of_a_relative_missing_path_and_of_root() {
    assert!(free_bytes(std::path::Path::new("definitely/not/here/at/all")).is_some());
    assert!(free_bytes(std::path::Path::new("/")).is_some());
}

#[test]
fn constants_match_the_python() {
    assert_eq!(MIN_FREE_KEY, "downloads.min_free_bytes");
    assert_eq!(DEFAULT_MIN_FREE_BYTES, 5 * GB);
    assert_eq!(RESUME_MARGIN_BYTES, 512 * 1024 * 1024);
}

// -- the setting ----------------------------------------------------------------

fn db() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (Db::open(dir.path().join("guard.db")).unwrap(), dir)
}

#[test]
fn limit_defaults_to_five_gigabytes_and_round_trips() {
    let (db, _dir) = db();
    assert_eq!(read_min_free_db(&db).unwrap(), 5 * GB);
    assert_eq!(write_min_free_db(&db, 2 * GB).unwrap(), 2 * GB);
    assert_eq!(read_min_free_db(&db).unwrap(), 2 * GB);
    // Nonsense is clamped, not stored.
    assert_eq!(write_min_free_db(&db, -1).unwrap(), 0);
    assert_eq!(read_min_free_db(&db).unwrap(), 0);
}

#[test]
fn an_unparsable_or_negative_stored_value_is_handled() {
    let (db, _dir) = db();
    let put = |v: &'static str| {
        db.write(move |t| {
            bc_db::settings::set(t, MIN_FREE_KEY, v)?;
            Ok(())
        })
        .unwrap();
    };
    put("not a number");
    assert_eq!(read_min_free_db(&db).unwrap(), DEFAULT_MIN_FREE_BYTES);
    put("-7");
    assert_eq!(read_min_free_db(&db).unwrap(), 0);
    put(" 123 ");
    assert_eq!(read_min_free_db(&db).unwrap(), 123);
}

#[tokio::test]
async fn async_wrappers_round_trip() {
    let (db, _dir) = db();
    assert_eq!(write_min_free_async(&db, 3 * GB).await.unwrap(), 3 * GB);
    assert_eq!(read_min_free_async(&db).await.unwrap(), 3 * GB);
}

#[test]
fn disk_state_serialises_for_the_api() {
    let s = DiskState { path: "/x".into(), free_bytes: None, min_free_bytes: 5 * GB, held: false };
    let v = serde_json::to_value(&s).unwrap();
    assert_eq!(v["min_free_bytes"], 5 * GB);
    assert!(v["free_bytes"].is_null());
}
