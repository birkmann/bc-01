//! Port of `test_job_store.py` and the store-level cases of `test_job_control.py`.
mod common;
use bc_jobs::{Complete, NewItem, backoff_delay};
use common::*;

const A1: &str = "https://a.bandcamp.com/album/1";
const A2: &str = "https://a.bandcamp.com/album/2";

#[test]
fn create_job_persists_items() {
    let (_d, s) = open();
    let j = job(&s, &[A1, "https://b.bandcamp.com/album/y"]);
    let stored = s.get_job(&j.id).unwrap().unwrap();
    assert_eq!(stored.total, 2);
    let items = s.list_items(&j.id, None, &[], 0, None).unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|i| i.status == "pending"));
}

#[test]
fn claim_takes_items_in_order_and_marks_the_job_running() {
    let (_d, s) = open();
    job(&s, &["https://a.bandcamp.com/album/1", "https://a.bandcamp.com/album/2"]);
    let first = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    assert_eq!(first.item.seq, 0);
    assert_eq!(first.item.status, "running");
    assert_eq!(first.item.attempts, 1);
    assert_eq!(first.job.status, "running");
    assert!(first.item.lease_expires_at.is_some());
    let second = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    assert_eq!(second.item.seq, 1);
    assert!(s.claim_item("download", 1, LEASE).unwrap().is_none());
}

#[test]
fn claim_ignores_other_kinds() {
    let (_d, s) = open();
    job(&s, &[A1]);
    assert!(s.claim_item("analyze", 1, LEASE).unwrap().is_none());
}

#[test]
fn job_completes_when_every_item_finishes() {
    let (_d, s) = open();
    let j = job(&s, &[A1, A2]);
    for _ in 0..2 {
        let c = s.claim_item("download", 1, LEASE).unwrap().unwrap();
        assert!(s.complete_item(c.item.id, Complete::msg("done")).unwrap());
    }
    let stored = s.get_job(&j.id).unwrap().unwrap();
    assert_eq!(stored.status, "completed");
    assert_eq!(stored.completed, 2);
    assert!(stored.finished_at.is_some());
}

#[test]
fn retryable_failure_requeues_with_backoff() {
    let (_d, s) = open();
    job(&s, &[A1]);
    let c = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    let out = s.fail_item(c.item.id, "network blip", "network", true).unwrap();
    assert!(out.will_retry && out.applied);
    let item = s.get_item(c.item.id).unwrap().unwrap();
    assert_eq!(item.status, "pending");
    assert!(item.next_attempt_at.unwrap() > bc_jobs::time::now());
    // Backed off, so it is not immediately claimable again.
    assert!(s.claim_item("download", 1, LEASE).unwrap().is_none());
}

#[test]
fn non_retryable_failure_burns_no_further_attempts() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/album/gone"]);
    let c = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    let out = s.fail_item(c.item.id, "404", "not_found", false).unwrap();
    assert!(!out.will_retry);
    let item = s.get_item(c.item.id).unwrap().unwrap();
    assert_eq!(item.status, "failed");
    assert_eq!(item.attempts, 1, "must not consume the remaining attempts");
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "failed");
}

#[test]
fn attempts_are_exhausted_then_the_item_fails() {
    let (_d, s) = open();
    job(&s, &[A1]);
    let mut last = None;
    for attempt in 1..=3 {
        s.db().write(|tx| Ok(tx.execute("UPDATE job_items SET next_attempt_at = NULL", [])?)).unwrap();
        let c = s.claim_item("download", 1, LEASE).unwrap().unwrap_or_else(|| panic!("attempt {attempt} claimable"));
        let out = s.fail_item(c.item.id, "boom", "no_output", true).unwrap();
        last = Some((c.item.id, out));
    }
    let (id, out) = last.unwrap();
    assert!(!out.will_retry);
    let item = s.get_item(id).unwrap().unwrap();
    assert_eq!(item.status, "failed");
    assert_eq!(item.attempts, 3);
}

#[test]
fn backoff_grows_and_is_jittered() {
    assert!((8.0..=12.0).contains(&backoff_delay(1)));
    assert!((16.0..=24.0).contains(&backoff_delay(2)));
    assert!(backoff_delay(50) <= bc_jobs::store::MAX_BACKOFF_S * 1.2);
}

#[test]
fn cancel_stops_pending_items() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/album/0", "https://a.bandcamp.com/album/1", "https://a.bandcamp.com/album/2"]);
    s.cancel_job(&j.id).unwrap();
    assert!(s.claim_item("download", 1, LEASE).unwrap().is_none());
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "cancelled");
    assert!(statuses(&s, &j.id).iter().all(|x| x == "cancelled"));
}

#[test]
fn retry_failed_requeues_only_failed_items() {
    let (_d, s) = open();
    let j = job(&s, &[A1, A2]);
    let good = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    let bad = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    s.complete_item(good.item.id, Complete::default()).unwrap();
    s.fail_item(bad.item.id, "404", "not_found", false).unwrap();
    assert_eq!(s.retry_failed(&j.id).unwrap(), 1);
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "queued");
    let mut st = statuses(&s, &j.id);
    st.sort();
    assert_eq!(st, ["done", "pending"]);
}

// -- crash recovery ---------------------------------------------------------------

#[test]
fn reconcile_requeues_items_left_running_by_a_crash() {
    let (_d, s) = open();
    let j = job(&s, &[A1]);
    let c = s.claim_item("download", 999, LEASE).unwrap().unwrap();
    assert_eq!(c.item.status, "running");
    let report = s.reconcile().unwrap();
    assert_eq!(report.requeued, 1);
    let item = s.get_item(c.item.id).unwrap().unwrap();
    assert_eq!(item.status, "pending");
    assert_eq!(item.error_class.as_deref(), Some("crash"));
    assert!(item.next_attempt_at.is_none(), "immediate, not backed off");
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "queued");
}

#[test]
fn reconcile_fails_items_that_already_exhausted_their_attempts() {
    let (_d, s) = open();
    job(&s, &[A1]);
    s.db().write(|tx| Ok(tx.execute("UPDATE job_items SET status='running', attempts=3", [])?)).unwrap();
    let report = s.reconcile().unwrap();
    assert_eq!(report.failed, 1);
    let st = s.db().read(|c| Ok(c.query_row("SELECT status FROM job_items", [], |r| r.get::<_, String>(0))?)).unwrap();
    assert_eq!(st, "failed");
}

#[test]
fn reconcile_is_idempotent() {
    let (_d, s) = open();
    job(&s, &[A1]);
    s.claim_item("download", 1, LEASE).unwrap();
    assert_eq!(s.reconcile().unwrap().requeued, 1);
    assert_eq!(s.reconcile().unwrap().requeued, 0);
}

#[test]
fn a_finished_job_is_settled_on_startup() {
    let (_d, s) = open();
    let j = job(&s, &[A1]);
    s.db()
        .write(|tx| {
            tx.execute("UPDATE job_items SET status='done', finished_at='2026-01-01 00:00:00'", [])?;
            tx.execute("UPDATE jobs SET status='running', completed=1", [])?;
            Ok(())
        })
        .unwrap();
    s.reconcile().unwrap();
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "completed");
}

#[test]
fn scheduled_retry_becomes_claimable_once_due() {
    let (_d, s) = open();
    job(&s, &[A1]);
    let c = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    s.fail_item(c.item.id, "blip", "network", true).unwrap();
    s.db().write(|tx| Ok(tx.execute("UPDATE job_items SET next_attempt_at = '2000-01-01 00:00:00'", [])?)).unwrap();
    let again = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    assert_eq!(again.item.attempts, 2);
}

#[test]
fn skip_items_skips_only_pending_and_settles() {
    let (_d, s) = open();
    let j = job(&s, &[A1, A2]);
    let ids: Vec<i64> = s.list_items(&j.id, None, &[], 0, None).unwrap().iter().map(|i| i.id).collect();
    let running = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    let skipped = s.skip_items(&ids, "Already in library — skipped").unwrap();
    assert_eq!(skipped, 1, "the claimed item is running, not pending");
    let job_row = s.get_job(&j.id).unwrap().unwrap();
    assert_eq!(job_row.skipped, 1);
    assert_eq!(job_row.status, "running");
    s.complete_item(running.item.id, Complete::msg("ok")).unwrap();
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "completed");
}

#[test]
fn all_skipped_job_settles_completed() {
    let (_d, s) = open();
    let j = job(&s, &[A1, A2]);
    let ids: Vec<i64> = s.list_items(&j.id, None, &[], 0, None).unwrap().iter().map(|i| i.id).collect();
    assert_eq!(s.skip_items(&ids, "Already in library — skipped").unwrap(), 2);
    let row = s.get_job(&j.id).unwrap().unwrap();
    assert_eq!(row.skipped, 2);
    assert_eq!(row.status, "completed");
    assert!(statuses(&s, &j.id).iter().all(|x| x == "skipped"));
    assert!(s.claim_item("download", 1, LEASE).unwrap().is_none());
}

// Session-ownership trap of the ORM version cannot exist here: everything is by id.
#[test]
fn completing_by_id_settles_the_job() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/album/x"]);
    let c = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    assert!(s.complete_item(c.item.id, Complete::msg("done")).unwrap());
    assert_eq!(s.get_item(c.item.id).unwrap().unwrap().status, "done");
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "completed");
    // A second completion (e.g. a worker whose lease was reaped) is a no-op.
    assert!(!s.complete_item(c.item.id, Complete::msg("again")).unwrap());
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().completed, 1);
}

#[test]
fn expand_item_appends_after_the_last_seq_and_keeps_the_tallies_honest() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/music", "https://b.bandcamp.com/album/keep"]);
    let stand_in = s.list_items(&j.id, None, &[], 0, None).unwrap()[0].id;
    let added = s
        .expand_item(
            stand_in,
            vec![
                NewItem::url("https://a.bandcamp.com/album/one", "album"),
                NewItem::url("https://a.bandcamp.com/album/two", "album"),
            ],
            "A: expanded into 2 releases",
        )
        .unwrap();
    assert_eq!(added, 2);
    let stored = s.get_job(&j.id).unwrap().unwrap();
    assert_eq!((stored.total, stored.skipped, stored.completed), (4, 1, 0));
    assert_ne!(stored.status, "completed", "two releases are still to run");
    let items = s.list_items(&j.id, None, &[], 0, None).unwrap();
    assert_eq!(items.iter().map(|i| i.seq).collect::<Vec<_>>(), [0, 1, 2, 3]);
    assert_eq!(items.iter().map(|i| i.status.as_str()).collect::<Vec<_>>(), ["skipped", "pending", "pending", "pending"]);
    assert_eq!(items[0].message.as_deref(), Some("A: expanded into 2 releases"));
    assert_eq!(items[0].progress, 1.0);
}

#[test]
fn expand_item_leaves_a_job_that_was_one_page_claimable() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/music"]);
    let stand_in = s.list_items(&j.id, None, &[], 0, None).unwrap()[0].id;
    s.expand_item(stand_in, vec![NewItem::url("https://a.bandcamp.com/album/one", "album")], "A: expanded into 1 release")
        .unwrap();
    let c = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    assert_eq!(c.item.url.as_deref(), Some("https://a.bandcamp.com/album/one"));
}

#[test]
fn removing_an_expanded_release_brings_total_back_down() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/music"]);
    let stand_in = s.list_items(&j.id, None, &[], 0, None).unwrap()[0].id;
    s.expand_item(
        stand_in,
        vec![NewItem::url("https://a.bandcamp.com/album/one", "album"), NewItem::url("https://a.bandcamp.com/album/two", "album")],
        "A: expanded into 2 releases",
    )
    .unwrap();
    let doomed = s.list_items(&j.id, None, &["pending".to_string()], 0, None).unwrap()[0].id;
    s.remove_items(&j.id, &[doomed]).unwrap();
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().total, 2);
}

// -- test_job_control.py: store semantics -------------------------------------------

const ALBUMS: [&str; 4] = [
    "https://a.bandcamp.com/album/r0",
    "https://a.bandcamp.com/album/r1",
    "https://a.bandcamp.com/album/r2",
    "https://a.bandcamp.com/album/r3",
];

#[test]
fn pause_holds_the_queue_and_resume_lets_it_go() {
    let (_d, s) = open();
    let j = job(&s, &ALBUMS);
    assert_eq!(s.pause_job(&j.id).unwrap().unwrap().status, "paused");
    assert!(s.claim_item("download", 1, LEASE).unwrap().is_none());
    assert_eq!(s.resume_job(&j.id).unwrap().unwrap().status, "queued");
    assert!(s.claim_item("download", 1, LEASE).unwrap().is_some());
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "running");
}

#[test]
fn a_finished_job_cannot_be_paused() {
    let (_d, s) = open();
    let j = job(&s, &ALBUMS[..1]);
    let c = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    s.complete_item(c.item.id, Complete::msg("ok")).unwrap();
    assert_eq!(s.pause_job(&j.id).unwrap().unwrap().status, "completed");
    assert_eq!(s.resume_job(&j.id).unwrap().unwrap().status, "completed");
}

#[test]
fn cancel_leaves_in_flight_items_to_the_worker() {
    let (_d, s) = open();
    let j = job(&s, &ALBUMS);
    let running = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    s.cancel_job(&j.id).unwrap();
    assert_eq!(statuses(&s, &j.id), ["running", "cancelled", "cancelled", "cancelled"]);
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "running");
    s.cancel_item(running.item.id, "Cancelled").unwrap();
    assert_eq!(statuses(&s, &j.id), ["cancelled"; 4]);
    let row = s.get_job(&j.id).unwrap().unwrap();
    assert_eq!(row.status, "cancelled");
    assert_eq!(row.failed, 0, "not a failure");
    let item = s.get_item(running.item.id).unwrap().unwrap();
    assert_eq!(item.message.as_deref(), Some("Cancelled"));
    assert!(item.finished_at.is_some());
}

#[test]
fn release_item_refunds_the_attempt() {
    let (_d, s) = open();
    let j = job(&s, &ALBUMS[..1]);
    let c = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    assert_eq!(c.item.attempts, 1);
    s.pause_job(&j.id).unwrap();
    s.release_item(c.item.id, "Paused").unwrap();
    let item = s.get_item(c.item.id).unwrap().unwrap();
    assert_eq!((item.status.as_str(), item.attempts), ("pending", 0));
    assert!(item.next_attempt_at.is_none());
    assert_eq!(item.message.as_deref(), Some("Paused"));
    assert!(s.claim_item("download", 1, LEASE).unwrap().is_none(), "held by the job's status");
    s.resume_job(&j.id).unwrap();
    let again = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    assert_eq!((again.item.id, again.item.attempts), (c.item.id, 1));
}

#[test]
fn resume_with_nothing_left_closes_the_job() {
    let (_d, s) = open();
    let j = job(&s, &ALBUMS[..1]);
    s.pause_job(&j.id).unwrap();
    let id = s.list_items(&j.id, None, &[], 0, None).unwrap()[0].id;
    s.skip_items(&[id], "Already in library — skipped").unwrap();
    assert_eq!(s.resume_job(&j.id).unwrap().unwrap().status, "completed");
}

#[test]
fn reconcile_settles_a_paused_job_with_nothing_left() {
    let (_d, s) = open();
    let j = job(&s, &ALBUMS[..1]);
    let c = s.claim_item("download", 1, LEASE).unwrap().unwrap();
    s.complete_item(c.item.id, Complete::msg("ok")).unwrap();
    s.db().write(|tx| Ok(tx.execute("UPDATE jobs SET status='paused'", [])?)).unwrap();
    s.reconcile().unwrap();
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "completed");
}

#[test]
fn reconcile_leaves_a_paused_job_paused() {
    let (_d, s) = open();
    let j = job(&s, &ALBUMS);
    s.claim_item("download", 1, LEASE).unwrap();
    s.pause_job(&j.id).unwrap();
    s.reconcile().unwrap();
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "paused");
    assert_eq!(statuses(&s, &j.id)[0], "pending");
}

#[test]
fn cancelling_a_large_job_is_one_statement_not_thousands() {
    let (_d, s) = open();
    let urls: Vec<String> = (0..20_000).map(|n| format!("https://a.bandcamp.com/album/r{n}")).collect();
    let j = s
        .create_job(bc_jobs::NewJob::new("download", urls.iter().map(|u| NewItem::url(u.clone(), "album")).collect()))
        .unwrap();
    let t = std::time::Instant::now();
    s.cancel_job(&j.id).unwrap();
    assert!(t.elapsed().as_secs() < 5, "cancelling 20k pending items took {:?}", t.elapsed());
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "cancelled");
}

// -- new: enforced leases ----------------------------------------------------------

#[test]
fn expired_leases_are_reaped_and_heartbeats_keep_them_alive() {
    let (_d, s) = open();
    let j = job(&s, &[A1, A2]);
    let a = s.claim_item("download", 1, 0.05).unwrap().unwrap(); // tiny lease
    let b = s.claim_item("download", 1, 0.05).unwrap().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(120));
    assert!(s.heartbeat(b.item.id, 300.0).unwrap(), "heartbeat renews a live lease");
    let (requeued, failed) = s.reap_expired().unwrap();
    assert_eq!((requeued, failed), (1, 0));
    let ia = s.get_item(a.item.id).unwrap().unwrap();
    assert_eq!((ia.status.as_str(), ia.error_class.as_deref()), ("pending", Some("lease_expired")));
    assert_eq!(s.get_item(b.item.id).unwrap().unwrap().status, "running");
    // The worker that lost its lease can no longer settle the item or heartbeat.
    assert!(!s.heartbeat(a.item.id, 300.0).unwrap());
    assert!(!s.complete_item(a.item.id, Complete::default()).unwrap());
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().completed, 0);
}

#[test]
fn reaper_fails_items_out_of_attempts() {
    let (_d, s) = open();
    let j = job(&s, &[A1]);
    s.claim_item("download", 1, 0.01).unwrap().unwrap();
    s.db().write(|tx| Ok(tx.execute("UPDATE job_items SET attempts = max_attempts", [])?)).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(s.reap_expired().unwrap(), (0, 1));
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "failed");
}

#[test]
fn claim_order_is_priority_then_age_then_seq() {
    let (_d, s) = open();
    let low = s.create_job(bc_jobs::NewJob::new("download", vec![NewItem::url(A1, "album")]).priority(200)).unwrap();
    let hi = s.create_job(bc_jobs::NewJob::new("download", vec![NewItem::url(A2, "album")]).priority(10)).unwrap();
    assert_eq!(s.claim_item("download", 1, LEASE).unwrap().unwrap().job.id, hi.id);
    assert_eq!(s.claim_item("download", 1, LEASE).unwrap().unwrap().job.id, low.id);
}

#[test]
fn migration_for_new_job_kinds_preserves_rows() {
    // req_ws2_jobs_kinds.sql: the legacy CHECK on `kind` is dropped by a table rebuild.
    let (_d, s) = open();
    let j = job(&s, &[A1, A2]);
    let sql = include_str!("../../bc-db/migrations/req_ws2_jobs_kinds.sql");
    s.db().write(move |tx| Ok(tx.execute_batch(sql)?)).unwrap();
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().total, 2);
    assert_eq!(s.list_items(&j.id, None, &[], 0, None).unwrap().len(), 2, "items survive the rebuild (no cascade)");
    let h = s.create_job(bc_jobs::NewJob::new("harvest", vec![NewItem::default()])).unwrap();
    assert_eq!(h.kind, "harvest");
    assert!(s.claim_item("harvest", 1, LEASE).unwrap().is_some());
    let idx: i64 = s
        .db()
        .read(|c| Ok(c.query_row("SELECT count(*) FROM sqlite_master WHERE type='index' AND name LIKE 'ix_job%'", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(idx, 7);
}
