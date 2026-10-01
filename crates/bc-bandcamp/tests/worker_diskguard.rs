//! `test_diskguard.py`, the worker and endpoint cases (the rule and the setting are covered by
//! `diskguard_rules.rs`): stop before the disk is full, hand back what is in flight, release with
//! margin; `GET/PUT /downloads/disk`.

mod worker_common;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use bc_bandcamp::download::worker::DownloadWorker;
use worker_common::*;

const GB: i64 = 1024 * 1024 * 1024;

#[tokio::test]
async fn worker_holds_and_hands_back_what_is_in_flight() {
    let env = Env::new();
    let free = Arc::new(AtomicI64::new(50 * GB));
    let f2 = free.clone();
    let hang = HangDownloader::new();
    let deps = Arc::new(env.deps().with_downloader(hang.clone()).with_free_bytes(Arc::new(move |_| Some(f2.load(Ordering::SeqCst)))));
    let worker = DownloadWorker::new(deps, env.store().clone());
    let mut events = env.bus.subscribe();

    let job = env.create_download(&["https://a.bandcamp.com/album/x"]);
    worker.start().await;
    wait_for(5.0, "the item to be in flight", || env.statuses(&job.id)[0] == "running" && hang.started.load(Ordering::SeqCst) > 0).await;

    // Plenty of room: nothing happens.
    let held_before = worker.check_disk().await;
    // The drive fills up.
    free.store(GB, Ordering::SeqCst);
    let held_now = worker.check_disk().await;
    // Handed back, not cancelled: it resumes once the hold lifts.
    assert_eq!(env.statuses(&job.id), ["pending"]);
    assert!(["queued", "running"].contains(&env.job(&job.id).status.as_str()));
    // Space is freed, but only just: still held.
    free.store(5 * GB + 1, Ordering::SeqCst);
    let held_still = worker.check_disk().await;
    // And while held nothing is claimed.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(env.statuses(&job.id), ["pending"], "a held worker claims nothing");
    // Enough room: let go.
    free.store(20 * GB, Ordering::SeqCst);
    let held_after = worker.check_disk().await;

    assert_eq!([held_before, held_now, held_still, held_after], [false, true, true, false]);

    let mut held_events = Vec::new();
    while let Ok(ev) = events.try_recv() {
        if ev.topic == "downloads.disk" {
            held_events.push(ev.payload["held"].as_bool().expect("held"));
        }
    }
    assert_eq!(held_events, [true, false]);

    // Released: the item is claimed again.
    wait_for(5.0, "the item to resume", || env.statuses(&job.id)[0] == "running").await;
    worker.stop().await;
}

#[tokio::test]
async fn an_unreadable_drive_holds() {
    let env = Env::new();
    let deps = Arc::new(env.deps().with_downloader(HangDownloader::new()).with_free_bytes(Arc::new(|_| None)));
    let worker = DownloadWorker::new(deps, env.store().clone());
    assert!(worker.check_disk().await);
    assert!(worker.disk_state().held);
    worker.stop().await;
}

#[tokio::test]
async fn disk_endpoint_reports_and_sets_the_limit() {
    let app = App::new(None).await;
    let (status, body) = app.get("/downloads/disk").await;
    assert_eq!(status, 200);
    assert_eq!(body["min_free_bytes"], 5 * GB);
    assert!(body["free_bytes"].is_null() || body["free_bytes"].as_i64().unwrap_or(0) > 0);
    assert_eq!(body["held"], false);
    assert!(!body["path"].as_str().unwrap_or("").is_empty());

    let (status, changed) = app.put("/downloads/disk", serde_json::json!({"min_free_bytes": 10 * GB})).await;
    assert_eq!(status, 200);
    assert_eq!(changed["min_free_bytes"], 10 * GB);
    assert_eq!(app.get("/downloads/disk").await.1["min_free_bytes"], 10 * GB);
    assert_eq!(app.put("/downloads/disk", serde_json::json!({"min_free_bytes": -5})).await.0, 422);
}

#[tokio::test]
async fn raising_the_limit_over_the_free_space_is_reflected_and_lifting_it_releases() {
    // PUT re-reads at once: the hold lifts without waiting for the loop's next look.
    let env = Env::new();
    let free = Arc::new(AtomicI64::new(6 * GB));
    let f2 = free.clone();
    let deps = Arc::new(env.deps().with_downloader(HangDownloader::new()).with_free_bytes(Arc::new(move |_| Some(f2.load(Ordering::SeqCst)))));
    let worker = DownloadWorker::new(deps, env.store().clone());
    assert!(!worker.check_disk().await);
    // Limit above free space: the guard trips on the next look.
    env.db.write(|t| bc_bandcamp::download::diskguard::write_min_free(t, 10 * GB)).expect("write");
    assert!(worker.check_disk().await);
    assert!(worker.reread_disk().held);
    // Limit lowered below free space minus margin: `reread_disk` releases it.
    env.db.write(|t| bc_bandcamp::download::diskguard::write_min_free(t, GB)).expect("write");
    assert!(!worker.reread_disk().held);
    assert!(!worker.disk_state().held);
    worker.stop().await;
}
