//! KindWorker: concurrency, heartbeat, interrupts, panics, gate, shutdown.
mod common;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bc_jobs::{Complete, Gate, HandlerOutcome, Interrupt, ItemCtx, ItemHandler, JobHooks, KindWorker, NewItem, NewJob, WorkerSpec};
use common::*;

fn spec(conc: usize) -> WorkerSpec {
    let mut s = WorkerSpec::new("download", conc);
    s.poll = Duration::from_millis(30);
    s.hold_poll = Duration::from_millis(30);
    s
}

async fn wait_until(mut f: impl FnMut() -> bool) {
    for _ in 0..400 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out");
}

struct Quick;
#[async_trait]
impl ItemHandler for Quick {
    async fn run(&self, _c: ItemCtx) -> HandlerOutcome {
        HandlerOutcome::Done(Complete::msg("ok"))
    }
}

#[tokio::test]
async fn runs_items_to_completion() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/album/1", "https://a.bandcamp.com/album/2", "https://a.bandcamp.com/album/3"]);
    let w = KindWorker::new(s.clone(), spec(2), Arc::new(Quick));
    w.start();
    wait_until(|| s.get_job(&j.id).unwrap().unwrap().status == "completed").await;
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().completed, 3);
    w.stop().await;
}

struct Counting {
    cur: AtomicUsize,
    max: AtomicUsize,
}
#[async_trait]
impl ItemHandler for Counting {
    async fn run(&self, _c: ItemCtx) -> HandlerOutcome {
        let now = self.cur.fetch_add(1, Ordering::SeqCst) + 1;
        self.max.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(60)).await;
        self.cur.fetch_sub(1, Ordering::SeqCst);
        HandlerOutcome::Done(Complete::default())
    }
}

#[tokio::test]
async fn never_claims_more_items_than_it_can_run() {
    // test_download_admission: only `concurrency` items are ever `running`.
    let (_d, s) = open();
    let urls: Vec<String> = (0..12).map(|n| format!("https://a.bandcamp.com/album/{n}")).collect();
    let j = s.create_job(NewJob::new("download", urls.iter().map(|u| NewItem::url(u.clone(), "album")).collect())).unwrap();
    let h = Arc::new(Counting { cur: 0.into(), max: 0.into() });
    let w = KindWorker::new(s.clone(), spec(2), h.clone());
    w.start();
    let mut max_running = 0;
    for _ in 0..200 {
        let running = s.item_counts(&j.id).unwrap().get("running").copied().unwrap_or(0);
        max_running = max_running.max(running);
        if s.get_job(&j.id).unwrap().unwrap().status == "completed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "completed");
    assert!(max_running <= 2, "running rows peaked at {max_running}");
    assert_eq!(h.max.load(Ordering::SeqCst), 2);
    w.stop().await;
}

struct Waits;
#[async_trait]
impl ItemHandler for Waits {
    async fn run(&self, c: ItemCtx) -> HandlerOutcome {
        c.cancel.cancelled().await;
        HandlerOutcome::Interrupted
    }
}

#[tokio::test]
async fn cancel_interrupts_in_flight_and_closes_the_job() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/album/1", "https://a.bandcamp.com/album/2", "https://a.bandcamp.com/album/3"]);
    let w = KindWorker::new(s.clone(), spec(1), Arc::new(Waits));
    w.start();
    wait_until(|| !w.inflight_items(&j.id).is_empty()).await;
    s.cancel_job(&j.id).unwrap();
    w.interrupt_job(&j.id, Interrupt::Cancel).await;
    let row = s.get_job(&j.id).unwrap().unwrap();
    assert_eq!(row.status, "cancelled");
    assert_eq!(row.failed, 0);
    assert!(statuses(&s, &j.id).iter().all(|x| x == "cancelled"));
    w.stop().await;
}

#[tokio::test]
async fn pause_hands_the_item_back_and_resume_picks_it_up() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/album/1"]);
    let flip = Arc::new(AtomicBool::new(false));
    struct Phase(Arc<AtomicBool>);
    #[async_trait]
    impl ItemHandler for Phase {
        async fn run(&self, c: ItemCtx) -> HandlerOutcome {
            if self.0.load(Ordering::SeqCst) {
                return HandlerOutcome::Done(Complete::msg("resumed"));
            }
            c.cancel.cancelled().await;
            HandlerOutcome::Interrupted
        }
    }
    let w = KindWorker::new(s.clone(), spec(1), Arc::new(Phase(flip.clone())));
    w.start();
    wait_until(|| !w.inflight_items(&j.id).is_empty()).await;
    s.pause_job(&j.id).unwrap();
    w.interrupt_job(&j.id, Interrupt::Pause).await;
    let item = s.list_items(&j.id, None, &[], 0, None).unwrap().remove(0);
    assert_eq!((item.status.as_str(), item.attempts), ("pending", 0), "attempt refunded");
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "paused");
    flip.store(true, Ordering::SeqCst);
    s.resume_job(&j.id).unwrap();
    wait_until(|| s.get_job(&j.id).unwrap().unwrap().status == "completed").await;
    w.stop().await;
}

struct Panics;
#[async_trait]
impl ItemHandler for Panics {
    async fn run(&self, _c: ItemCtx) -> HandlerOutcome {
        panic!("boom");
    }
}

#[tokio::test]
async fn a_panicking_handler_never_strands_an_item() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/album/1"]);
    let w = KindWorker::new(s.clone(), spec(1), Arc::new(Panics));
    w.start();
    // Retryable: back to `pending` with a backoff (never stuck in `running`).
    wait_until(|| s.list_items(&j.id, None, &[], 0, None).unwrap()[0].error_class.is_some()).await;
    let item = s.list_items(&j.id, None, &[], 0, None).unwrap().remove(0);
    assert_eq!(item.status, "pending");
    assert_eq!(item.error_class.as_deref(), Some("internal"));
    assert_eq!(item.attempts, 1);
    assert!(item.next_attempt_at.is_some());
    w.stop().await;
}

struct Forgets;
#[async_trait]
impl ItemHandler for Forgets {
    async fn run(&self, _c: ItemCtx) -> HandlerOutcome {
        HandlerOutcome::Handled // but did not settle
    }
}

#[tokio::test]
async fn an_item_left_running_by_its_handler_is_failed() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/album/1"]);
    let w = KindWorker::new(s.clone(), spec(1), Arc::new(Forgets));
    w.start();
    wait_until(|| s.list_items(&j.id, None, &[], 0, None).unwrap()[0].error_class.is_some()).await;
    let item = s.list_items(&j.id, None, &[], 0, None).unwrap().remove(0);
    assert_eq!(item.status, "pending", "failed retryably, not left running");
    assert_eq!(item.error_class.as_deref(), Some("internal"));
    w.stop().await;
}

struct SlowOk;
#[async_trait]
impl ItemHandler for SlowOk {
    async fn run(&self, _c: ItemCtx) -> HandlerOutcome {
        tokio::time::sleep(Duration::from_millis(900)).await;
        HandlerOutcome::Done(Complete::msg("slow"))
    }
}

#[tokio::test]
async fn heartbeats_keep_a_long_item_alive_past_its_lease() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/album/1"]);
    let mut sp = spec(1);
    sp.lease_secs = 0.3; // heartbeat every 100 ms
    let w = KindWorker::new(s.clone(), sp, Arc::new(SlowOk));
    w.start();
    // The reaper runs throughout; the item must survive it.
    for _ in 0..30 {
        s.reap_expired().unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    wait_until(|| s.get_job(&j.id).unwrap().unwrap().status == "completed").await;
    let item = s.list_items(&j.id, None, &[], 0, None).unwrap().remove(0);
    assert_eq!(item.attempts, 1, "never reaped");
    w.stop().await;
}

struct Hold(Arc<AtomicBool>);
#[async_trait]
impl Gate for Hold {
    async fn hold(&self, _w: &KindWorker) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn a_gate_holds_claims_until_released() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/album/1"]);
    let held = Arc::new(AtomicBool::new(true));
    let w = KindWorker::new(s.clone(), spec(1), Arc::new(Quick));
    w.set_gate(Arc::new(Hold(held.clone())));
    w.start();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(s.get_job(&j.id).unwrap().unwrap().status, "queued", "nothing claimed while held");
    held.store(false, Ordering::SeqCst);
    wait_until(|| s.get_job(&j.id).unwrap().unwrap().status == "completed").await;
    w.stop().await;
}

#[tokio::test]
async fn shutdown_requeues_in_flight_items_for_the_restart() {
    let (_d, s) = open();
    let j = job(&s, &["https://a.bandcamp.com/album/1"]);
    let w = KindWorker::new(s.clone(), spec(1), Arc::new(Waits));
    w.start();
    wait_until(|| !w.inflight_items(&j.id).is_empty()).await;
    w.stop().await;
    let item = s.list_items(&j.id, None, &[], 0, None).unwrap().remove(0);
    assert_eq!(item.status, "pending", "retryable failure => back to pending");
    assert_eq!(item.error_class.as_deref(), Some("cancelled"));
}
