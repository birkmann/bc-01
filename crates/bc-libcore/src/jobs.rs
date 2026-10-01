//! Long work (scan, move, metadata bulk, stray merge, art) runs as a tracked task: the request
//! returns `202 { job_id }` and progress arrives as `library.*` events (PLAN §3.4).
//!
//! `JobHost` is the seam to workstream 2's durable `bc-jobs`; until it is wired in,
//! [`LocalJobs`] tracks tasks in memory and publishes the `library.*` topics itself.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use bc_core::EventBus;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Public view of a task (`GET /library/tasks`, `GET /library/tasks/{id}`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct TaskInfo {
    pub id: String,
    pub kind: String,
    pub label: String,
    /// `running` | `done` | `failed` | `cancelled`
    pub state: String,
    pub done: i64,
    pub total: Option<i64>,
    pub message: Option<String>,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
    pub cancel_requested: bool,
}

pub trait JobSink: Send + Sync {
    fn progress(&self, id: &str, done: i64, total: Option<i64>, message: Option<&str>);
    fn finish(&self, id: &str, outcome: Result<serde_json::Value, String>);
    fn cancelled(&self, id: &str) -> bool;
}

/// Handle given to the running task.
#[derive(Clone)]
pub struct JobHandle {
    pub id: String,
    sink: Arc<dyn JobSink>,
}

impl JobHandle {
    pub fn new(id: String, sink: Arc<dyn JobSink>) -> Self {
        Self { id, sink }
    }
    pub fn progress(&self, done: i64, total: Option<i64>, message: Option<&str>) {
        self.sink.progress(&self.id, done, total, message);
    }
    pub fn finish_ok(&self, result: serde_json::Value) {
        self.sink.finish(&self.id, Ok(result));
    }
    pub fn finish_err(&self, error: impl ToString) {
        self.sink.finish(&self.id, Err(error.to_string()));
    }
    /// Cooperative cancel flag; long loops poll it.
    pub fn cancelled(&self) -> bool {
        self.sink.cancelled(&self.id)
    }
}

pub trait JobHost: Send + Sync {
    /// Register a task and return its handle. The caller spawns the work.
    fn begin(&self, kind: &str, label: &str) -> JobHandle;
    fn get(&self, id: &str) -> Option<TaskInfo>;
    fn list(&self) -> Vec<TaskInfo>;
    fn request_cancel(&self, id: &str) -> bool;
    /// Hint that download-queue rows were just inserted (so WS2's worker wakes up).
    fn notify_download_queue(&self) {}
}

/// In-memory implementation. Keeps the last 100 finished tasks.
pub struct LocalJobs {
    bus: Arc<EventBus>,
    inner: Arc<Inner>,
}

struct Inner {
    bus: Arc<EventBus>,
    tasks: Mutex<BTreeMap<String, TaskInfo>>,
    finished: Mutex<VecDeque<String>>,
}

impl LocalJobs {
    pub fn new(bus: Arc<EventBus>) -> Self {
        let inner = Arc::new(Inner { bus: bus.clone(), tasks: Mutex::new(BTreeMap::new()), finished: Mutex::new(VecDeque::new()) });
        Self { bus, inner }
    }
}

impl JobSink for Inner {
    fn progress(&self, id: &str, done: i64, total: Option<i64>, message: Option<&str>) {
        let snapshot = {
            let mut t = self.tasks.lock();
            let Some(task) = t.get_mut(id) else { return };
            task.done = done;
            task.total = total.or(task.total);
            if let Some(m) = message {
                task.message = Some(m.to_string());
            }
            task.clone()
        };
        self.bus.publish("library.task.progress", &snapshot);
    }

    fn finish(&self, id: &str, outcome: Result<serde_json::Value, String>) {
        let snapshot = {
            let mut t = self.tasks.lock();
            let Some(task) = t.get_mut(id) else { return };
            match outcome {
                Ok(v) => {
                    task.state = if task.cancel_requested { "cancelled".into() } else { "done".into() };
                    task.result = Some(v);
                }
                Err(e) => {
                    task.state = "failed".into();
                    task.error = Some(e);
                }
            }
            task.clone()
        };
        self.bus.publish("library.task.done", &snapshot);
        let mut f = self.finished.lock();
        f.push_back(id.to_string());
        while f.len() > 100 {
            if let Some(old) = f.pop_front() {
                self.tasks.lock().remove(&old);
            }
        }
    }

    fn cancelled(&self, id: &str) -> bool {
        self.tasks.lock().get(id).map(|t| t.cancel_requested).unwrap_or(false)
    }
}

impl JobHost for LocalJobs {
    fn begin(&self, kind: &str, label: &str) -> JobHandle {
        let id = uuid::Uuid::new_v4().to_string();
        let info = TaskInfo { id: id.clone(), kind: kind.into(), label: label.into(), state: "running".into(), ..Default::default() };
        self.inner.tasks.lock().insert(id.clone(), info.clone());
        self.bus.publish("library.task.started", &info);
        JobHandle::new(id, self.inner.clone())
    }
    fn get(&self, id: &str) -> Option<TaskInfo> {
        self.inner.tasks.lock().get(id).cloned()
    }
    fn list(&self) -> Vec<TaskInfo> {
        self.inner.tasks.lock().values().cloned().collect()
    }
    fn request_cancel(&self, id: &str) -> bool {
        match self.inner.tasks.lock().get_mut(id) {
            Some(t) if t.state == "running" => {
                t.cancel_requested = true;
                true
            }
            _ => false,
        }
    }
}

pub const TOPIC_TASK_STARTED: &str = "library.task.started";
pub const TOPIC_TASK_PROGRESS: &str = "library.task.progress";
pub const TOPIC_TASK_DONE: &str = "library.task.done";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_and_cancel() {
        let bus = Arc::new(EventBus::new());
        let jobs = LocalJobs::new(bus);
        let h = jobs.begin("scan", "scan root 1");
        assert!(!h.cancelled());
        h.progress(5, Some(10), Some("walking"));
        assert_eq!(jobs.get(&h.id).unwrap().done, 5);
        assert!(jobs.request_cancel(&h.id));
        assert!(h.cancelled());
        h.finish_ok(serde_json::json!({"n": 1}));
        let t = jobs.get(&h.id).unwrap();
        assert_eq!(t.state, "cancelled");
        assert!(!jobs.request_cancel(&h.id));
    }
}
