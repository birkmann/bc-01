//! Progress coalescing: at most 4 `job.item.progress` events per second per item
//! (a 50-track album otherwise emits thousands of frames).

use std::sync::Arc;
use std::time::{Duration, Instant};

use bc_core::EventBus;
use bc_types::jobs::{JobItemProgress, TOPIC_JOB_ITEM_PROGRESS};

use crate::store::JobStore;

pub const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Allows an action at most once per interval. The first call always passes.
#[derive(Debug)]
pub struct Throttle {
    interval: Duration,
    last: Option<Instant>,
}

impl Throttle {
    pub fn new(interval: Duration) -> Self {
        Self { interval, last: None }
    }
    pub fn per_sec(n: u32) -> Self {
        Self::new(Duration::from_millis(1000 / u64::from(n.max(1))))
    }
    /// True when enough time has passed; records the emission.
    pub fn ready(&mut self) -> bool {
        let now = Instant::now();
        match self.last {
            Some(t) if now.duration_since(t) < self.interval => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

/// Persist + publish one item's progress at <= 4 Hz. `report` is cheap to call on
/// every parsed line; `finish` is not needed (completion events carry the end state).
pub struct ProgressReporter {
    store: JobStore,
    bus: Option<Arc<EventBus>>,
    job_id: String,
    item_id: i64,
    throttle: Throttle,
}

impl ProgressReporter {
    pub fn new(store: &JobStore, job_id: &str, item_id: i64) -> Self {
        Self {
            store: store.clone(),
            bus: store.bus().cloned(),
            job_id: job_id.to_string(),
            item_id,
            throttle: Throttle::per_sec(4),
        }
    }

    /// Blocking (does a DB write when it fires). Use from `spawn_blocking`, or
    /// [`report_async`](Self::report_async).
    pub fn report(&mut self, progress: f64, message: &str) {
        if !self.throttle.ready() {
            return;
        }
        let _ = self.store.update_progress(self.item_id, progress, message);
        if let Some(b) = &self.bus {
            b.publish(
                TOPIC_JOB_ITEM_PROGRESS,
                &JobItemProgress {
                    job_id: self.job_id.clone(),
                    item_id: self.item_id,
                    progress,
                    message: message.to_string(),
                },
            );
        }
    }

    pub async fn report_async(&mut self, progress: f64, message: &str) {
        if !self.throttle.ready() {
            return;
        }
        let (store, item_id, msg) = (self.store.clone(), self.item_id, message.to_string());
        let _ = store.run(move |s| s.update_progress(item_id, progress, &msg)).await;
        if let Some(b) = &self.bus {
            b.publish(
                TOPIC_JOB_ITEM_PROGRESS,
                &JobItemProgress {
                    job_id: self.job_id.clone(),
                    item_id: self.item_id,
                    progress,
                    message: message.to_string(),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces() {
        let mut t = Throttle::new(Duration::from_millis(40));
        assert!(t.ready());
        assert!(!t.ready());
        std::thread::sleep(Duration::from_millis(50));
        assert!(t.ready());
    }
}
