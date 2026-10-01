//! Generic per-kind worker loop over the job store.
//!
//! Every long task (download, scan, move, analyze, harvest, walk, sweep, enrich)
//! is a job whose items are claimed by *kind*. `KindWorker` is the loop each of
//! them shares, so WS1/WS3 write only an [`ItemHandler`]:
//!
//! * the semaphore is taken **before** the claim (a 9,679-item job must not flip
//!   every item to `running` in one burst: leases, cancel and `reconcile` all lie
//!   otherwise);
//! * every claimed item is heartbeated (lease renewal) while its handler runs;
//! * a handler that panics, errors or leaves the item `running` is always
//!   resolved: no item can stay stuck in `running`;
//! * `Interrupt::Cancel` / `Interrupt::Pause` reach in-flight items through the
//!   [`JobHooks`] impl (cancel => `cancel_item`, pause => `release_item`);
//!   a shutdown (no reason) fails the item as a retryable `cancelled`.
//!
//! ```ignore
//! let w = KindWorker::new(store.clone(), WorkerSpec::new("scan", 1), Arc::new(MyHandler));
//! jobs.add_hooks(w.clone());
//! w.start();
//! ```

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use parking_lot::Mutex;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::hooks::{Interrupt, JobHooks};
use crate::store::{Claimed, Complete, Job, JobItem, JobStore, LEASE_SECONDS};
use crate::throttle::ProgressReporter;

/// What an [`ItemHandler`] did with the item it was given.
#[derive(Debug)]
pub enum HandlerOutcome {
    /// Finished: settle `done`.
    Done(Complete),
    /// Nothing to do (already owned, blacklisted, ...): settle `skipped`.
    Skipped(String),
    /// Attempt failed. `retryable` + attempts left => back to `pending` with backoff.
    Failed { error: String, class: String, retryable: bool },
    /// The handler observed `ctx.cancel` and stopped. The runner settles it
    /// according to *why* it was interrupted (cancel / pause / shutdown).
    Interrupted,
    /// The handler already settled the item itself (e.g. `expand_item`, `skip_item`).
    Handled,
}

/// Per-item context handed to the handler.
#[derive(Clone)]
pub struct ItemCtx {
    pub store: JobStore,
    pub job: Job,
    pub item: JobItem,
    /// Cancelled on interrupt (cancel / pause / shutdown / lost lease).
    pub cancel: CancellationToken,
}

impl ItemCtx {
    /// A progress reporter limited to 4 events/s.
    pub fn reporter(&self) -> ProgressReporter {
        ProgressReporter::new(&self.store, &self.job.id, self.item.id)
    }
}

#[async_trait]
pub trait ItemHandler: Send + Sync + 'static {
    async fn run(&self, ctx: ItemCtx) -> HandlerOutcome;
}

/// Asked before every claim; `true` = hold (nothing is claimed). The disk guard
/// is one. Called at most once per loop turn; keep it cheap.
#[async_trait]
pub trait Gate: Send + Sync + 'static {
    async fn hold(&self, worker: &KindWorker) -> bool;
}

#[derive(Debug, Clone)]
pub struct WorkerSpec {
    pub kind: String,
    pub concurrency: usize,
    pub lease_secs: f64,
    /// How often to poll when idle (backoff-scheduled retries become due without a wake).
    pub poll: Duration,
    /// Poll interval while a [`Gate`] holds.
    pub hold_poll: Duration,
}

impl WorkerSpec {
    pub fn new(kind: impl Into<String>, concurrency: usize) -> Self {
        Self {
            kind: kind.into(),
            concurrency: concurrency.max(1),
            lease_secs: LEASE_SECONDS,
            poll: Duration::from_secs(2),
            hold_poll: Duration::from_secs(5),
        }
    }
}

struct Inflight {
    job_id: String,
    cancel: CancellationToken,
    reason: Arc<Mutex<Option<Interrupt>>>,
    done: Arc<Notify>,
    finished: Arc<std::sync::atomic::AtomicBool>,
}

pub struct KindWorker {
    store: JobStore,
    spec: WorkerSpec,
    handler: Arc<dyn ItemHandler>,
    gate: Mutex<Option<Arc<dyn Gate>>>,
    sem: Arc<Semaphore>,
    inflight: Mutex<HashMap<i64, Inflight>>,
    wake: Notify,
    stop: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl KindWorker {
    pub fn new(store: JobStore, spec: WorkerSpec, handler: Arc<dyn ItemHandler>) -> Arc<Self> {
        Arc::new(Self {
            sem: Arc::new(Semaphore::new(spec.concurrency)),
            store,
            spec,
            handler,
            gate: Mutex::new(None),
            inflight: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            stop: CancellationToken::new(),
            task: Mutex::new(None),
        })
    }

    pub fn set_gate(&self, g: Arc<dyn Gate>) {
        *self.gate.lock() = Some(g);
    }

    pub fn store(&self) -> &JobStore {
        &self.store
    }
    pub fn kind(&self) -> &str {
        &self.spec.kind
    }

    /// Spawn the claim loop. Idempotent.
    pub fn start(self: &Arc<Self>) {
        let mut slot = self.task.lock();
        if slot.is_some() {
            return;
        }
        let me = self.clone();
        *slot = Some(tokio::spawn(async move { me.run_loop().await }));
    }

    /// Stop claiming, cancel in-flight items (they are settled as a shutdown: a
    /// retryable failure the next boot's `reconcile` requeues) and wait for them.
    pub async fn stop(&self) {
        self.stop.cancel();
        self.wake.notify_waiters();
        let task = self.task.lock().take();
        if let Some(t) = task {
            let _ = t.await;
        }
        self.interrupt_where(|_| true, None).await;
    }

    /// New work was enqueued: claim now instead of waiting for the poll.
    pub fn notify(&self) {
        self.wake.notify_one();
    }

    /// Ids of this job's items being worked on right now.
    pub fn inflight_items(&self, job_id: &str) -> Vec<i64> {
        self.inflight.lock().iter().filter(|(_, f)| f.job_id == job_id).map(|(id, _)| *id).collect()
    }

    pub fn inflight_count(&self) -> usize {
        self.inflight.lock().len()
    }

    /// Interrupt every in-flight item of every job (e.g. disk guard: pause).
    pub async fn interrupt_all(&self, reason: Interrupt) -> usize {
        self.interrupt_where(|_| true, Some(reason)).await
    }

    async fn interrupt_where(&self, pred: impl Fn(&str) -> bool, reason: Option<Interrupt>) -> usize {
        let targets: Vec<(CancellationToken, Arc<Mutex<Option<Interrupt>>>, Arc<Notify>, Arc<std::sync::atomic::AtomicBool>)> = self
            .inflight
            .lock()
            .values()
            .filter(|f| pred(&f.job_id))
            .map(|f| (f.cancel.clone(), f.reason.clone(), f.done.clone(), f.finished.clone()))
            .collect();
        for (tok, why, _, _) in &targets {
            *why.lock() = reason;
            tok.cancel();
        }
        // Await settlement so the caller can close or delete the job without racing us.
        for (_, _, done, finished) in &targets {
            let notified = done.notified();
            tokio::pin!(notified);
            // Register before checking so a finish between check and wait is not lost.
            notified.as_mut().enable();
            if !finished.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = tokio::time::timeout(Duration::from_secs(60), notified).await;
            }
        }
        targets.len()
    }

    async fn run_loop(self: Arc<Self>) {
        let mut store_wake = self.store.subscribe_wake();
        loop {
            if self.stop.is_cancelled() {
                return;
            }
            let permit = tokio::select! {
                p = self.sem.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => return },
                _ = self.stop.cancelled() => return,
            };
            let gate = self.gate.lock().clone();
            if let Some(g) = gate {
                if g.hold(&self).await {
                    drop(permit);
                    self.idle(&mut store_wake, self.spec.hold_poll).await;
                    continue;
                }
            }
            let kind = self.spec.kind.clone();
            let lease = self.spec.lease_secs;
            let pid = i64::from(std::process::id());
            let claimed = match self.store.run(move |s| s.claim_item(&kind, pid, lease)).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("claim failed for kind {}: {e}", self.spec.kind);
                    drop(permit);
                    self.idle(&mut store_wake, Duration::from_secs(5)).await;
                    continue;
                }
            };
            match claimed {
                None => {
                    drop(permit);
                    self.idle(&mut store_wake, self.spec.poll).await;
                }
                Some(c) => {
                    let me = self.clone();
                    // Register in-flight *before* the task runs so an interrupt that
                    // arrives straight after the claim finds it.
                    let inflight = Inflight {
                        job_id: c.job.id.clone(),
                        cancel: CancellationToken::new(),
                        reason: Arc::new(Mutex::new(None)),
                        done: Arc::new(Notify::new()),
                        finished: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    };
                    let handles = (inflight.cancel.clone(), inflight.reason.clone(), inflight.done.clone(), inflight.finished.clone());
                    self.inflight.lock().insert(c.item.id, inflight);
                    tokio::spawn(async move {
                        me.run_item(c, handles).await;
                        drop(permit);
                    });
                }
            }
        }
    }

    async fn idle(&self, store_wake: &mut tokio::sync::watch::Receiver<u64>, max: Duration) {
        tokio::select! {
            _ = self.wake.notified() => {}
            _ = store_wake.changed() => {}
            _ = tokio::time::sleep(max) => {}
            _ = self.stop.cancelled() => {}
        }
    }

    async fn run_item(
        self: &Arc<Self>,
        c: Claimed,
        (cancel, reason, done, finished): (
            CancellationToken,
            Arc<Mutex<Option<Interrupt>>>,
            Arc<Notify>,
            Arc<std::sync::atomic::AtomicBool>,
        ),
    ) {
        let item_id = c.item.id;
        let job_id = c.job.id.clone();

        // Heartbeat: renew the lease; losing it (reaped / cancelled elsewhere) cancels the handler.
        let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hb = {
            let store = self.store.clone();
            let cancel = cancel.clone();
            let lost = lost.clone();
            let lease = self.spec.lease_secs;
            let every = Duration::from_secs_f64((lease / 3.0).clamp(0.02, 30.0));
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    match store.run(move |s| s.heartbeat(item_id, lease)).await {
                        Ok(true) => {}
                        Ok(false) => {
                            lost.store(true, std::sync::atomic::Ordering::SeqCst);
                            cancel.cancel();
                            return;
                        }
                        Err(e) => tracing::warn!("heartbeat for item {item_id} failed: {e}"),
                    }
                }
            })
        };

        let ctx = ItemCtx { store: self.store.clone(), job: c.job.clone(), item: c.item.clone(), cancel: cancel.clone() };
        let handler = self.handler.clone();
        let outcome = AssertUnwindSafe(handler.run(ctx)).catch_unwind().await;
        hb.abort();

        let why = *reason.lock();
        let lease_lost = lost.load(std::sync::atomic::Ordering::SeqCst);
        if lease_lost {
            tracing::warn!("item {item_id} of job {job_id}: lease lost, result discarded");
        } else {
            self.settle(item_id, outcome, why, cancel.is_cancelled()).await;
        }
        self.inflight.lock().remove(&item_id);
        finished.store(true, std::sync::atomic::Ordering::SeqCst);
        done.notify_waiters();
        self.store.notify();
    }

    async fn settle(
        &self,
        item_id: i64,
        outcome: Result<HandlerOutcome, Box<dyn std::any::Any + Send>>,
        why: Option<Interrupt>,
        was_cancelled: bool,
    ) {
        let store = self.store.clone();
        let outcome = match outcome {
            Ok(o) => o,
            Err(p) => {
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".into());
                tracing::error!("handler for item {item_id} panicked: {msg}");
                HandlerOutcome::Failed { error: format!("Internal error: {msg}"), class: "internal".into(), retryable: true }
            }
        };
        let interrupted = was_cancelled || matches!(outcome, HandlerOutcome::Interrupted);
        let res: bc_db::Result<()> = match outcome {
            // The work finished: let it stand even if a cancel raced in.
            HandlerOutcome::Done(c) => store.run(move |s| s.complete_item(item_id, c).map(|_| ())).await,
            HandlerOutcome::Handled => Ok(()),
            _ if interrupted => match why {
                Some(Interrupt::Cancel) => store.run(move |s| s.cancel_item(item_id, "Cancelled")).await,
                Some(Interrupt::Pause) => store.run(move |s| s.release_item(item_id, "Paused")).await,
                None => {
                    // Shutdown (or a handler's own cancel): retryable, requeued on restart.
                    store
                        .run(move |s| s.fail_item(item_id, "Cancelled", "cancelled", true).map(|_| ()))
                        .await
                }
            },
            HandlerOutcome::Skipped(m) => store.run(move |s| s.skip_item(item_id, &m).map(|_| ())).await,
            HandlerOutcome::Failed { error, class, retryable } => {
                store.run(move |s| s.fail_item(item_id, &error, &class, retryable).map(|_| ())).await
            }
            HandlerOutcome::Interrupted => unreachable!("handled by `interrupted`"),
        };
        if let Err(e) = res {
            tracing::error!("settling item {item_id} failed: {e}");
        }
        // Safety net: nothing may stay `running` because a handler forgot to settle.
        let still = store.run(move |s| s.get_item(item_id)).await;
        if let Ok(Some(it)) = still {
            if it.status == "running" {
                tracing::error!("item {item_id} left running by its handler; failing it");
                let _ = store
                    .run(move |s| s.fail_item(item_id, "Internal error: handler did not settle the item", "internal", true))
                    .await;
            }
        }
    }
}

#[async_trait]
impl JobHooks for KindWorker {
    async fn interrupt_job(&self, job_id: &str, reason: Interrupt) {
        let id = job_id.to_string();
        self.interrupt_where(move |j| j == id.as_str(), Some(reason)).await;
    }
    fn wake(&self) {
        self.notify();
    }
}
