//! `test_download_admission.py`: the worker must claim only as much work as it can actually start.
//!
//! Regression cover for a queue-scale defect: the loop claimed an item and spun straight round to
//! claim the next, because the concurrency semaphore was held inside the item task rather than
//! around the claim. With a 9,679-item wishlist job that flipped every row to 'running' within
//! seconds, which made `lease_expires_at` start counting down on work that had not begun, left
//! `cancel_job` (which only touches 'pending' rows) unable to stop anything, and made startup
//! `reconcile` treat the entire queue as interrupted.

mod worker_common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use bc_bandcamp::download::worker::DownloadWorker;
use bc_jobs::{NewItem, NewJob};
use worker_common::*;

const CONCURRENCY: usize = 2;
const TOTAL: usize = 25;

fn queue(env: &Env) -> bc_jobs::Job {
    let items = (0..TOTAL).map(|n| NewItem::url(format!("https://a.bandcamp.com/album/r{n}"), "album")).collect();
    env.store().create_job(NewJob::new("download", items).label("test")).expect("job")
}

#[tokio::test]
async fn never_claims_more_items_than_it_can_run() {
    let env = Env::with(|c| c.download_concurrency = CONCURRENCY);
    let gate = GateDownloader::new(false);
    let deps = std::sync::Arc::new(env.deps().with_downloader(gate.clone()));
    let worker = DownloadWorker::new(deps, env.store().clone());
    queue(&env);

    worker.start().await;
    wait_for(5.0, "the first item to start", || gate.calls.load(Ordering::SeqCst) > 0).await;
    // Long enough that an unbounded loop would have drained the table.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let running = env.count("SELECT count(*) FROM job_items WHERE status = 'running'");
    let pending = env.count("SELECT count(*) FROM job_items WHERE status = 'pending'");
    assert!(running <= CONCURRENCY as i64, "claimed {running} items with concurrency {CONCURRENCY}");
    assert!(pending >= (TOTAL - CONCURRENCY) as i64);
    assert!(gate.peak.load(Ordering::SeqCst) <= CONCURRENCY);

    gate.open.store(true, Ordering::SeqCst);
    gate.release.notify_waiters();
    worker.stop().await;
}

#[tokio::test]
async fn a_finished_item_frees_its_slot() {
    // Admission control must not deadlock the queue: the slot has to come back even when the
    // item's task fails (here: the downloader panics).
    let env = Env::with(|c| c.download_concurrency = CONCURRENCY);
    let gate = GateDownloader::new(true);
    gate.open.store(true, Ordering::SeqCst);
    let deps = std::sync::Arc::new(env.deps().with_downloader(gate.clone()));
    let worker = DownloadWorker::new(deps, env.store().clone());
    queue(&env);

    worker.start().await;
    wait_for(20.0, "every item to be attempted", || gate.calls.load(Ordering::SeqCst) >= TOTAL).await;
    worker.stop().await;

    assert!(gate.calls.load(Ordering::SeqCst) >= TOTAL, "the queue stalled instead of reclaiming slots");
    assert_eq!(env.count("SELECT count(*) FROM job_items WHERE status = 'running'"), 0);
}
