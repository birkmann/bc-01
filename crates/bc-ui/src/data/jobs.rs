//! Live jobs store. Seeded from `GET /jobs`, then patched in place by `job.*` WS
//! events: progress never triggers a refetch (the legacy app refetched the whole
//! list on every progress tick). Pages (Downloads) and the sidebar activity dots
//! read the same store.
use bc_types::Page;
use bc_types::jobs::*;
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::ws::{resync_counter, use_topic};
use crate::api;

#[derive(Clone, Copy)]
pub struct JobsStore {
    pub jobs: RwSignal<Vec<JobOut>>,
    pub loaded: RwSignal<bool>,
    /// `downloads.disk` hold state.
    pub disk_held: RwSignal<bool>,
}

pub fn is_active(j: &JobOut) -> bool {
    matches!(j.status.as_str(), JOB_QUEUED | JOB_RUNNING)
}

impl JobsStore {
    pub fn active_of(&self, kind: &str) -> Vec<JobOut> {
        self.jobs.with(|v| v.iter().filter(|j| j.kind == kind && is_active(j)).cloned().collect())
    }
    pub fn active_count(&self, kind: &str) -> usize {
        self.jobs.with(|v| v.iter().filter(|j| j.kind == kind && is_active(j)).count())
    }
    /// Aggregate progress of the active jobs of a kind, 0..1.
    pub fn progress_of(&self, kind: &str) -> Option<f64> {
        self.jobs.with(|v| {
            let (mut done, mut total) = (0i64, 0i64);
            for j in v.iter().filter(|j| j.kind == kind && is_active(j)) {
                done += j.completed + j.failed + j.skipped;
                total += j.total;
            }
            (total > 0).then(|| done as f64 / total as f64)
        })
    }
    pub async fn reload(&self) {
        if let Ok(p) = api::get::<Page<JobOut>>("/jobs?limit=200").await {
            self.jobs.set(p.items);
        }
        self.loaded.set(true);
    }
    pub fn upsert(&self, job: JobOut) {
        self.jobs.update(|v| match v.iter_mut().find(|j| j.id == job.id) {
            Some(j) => *j = job,
            None => v.insert(0, job),
        });
    }
}

pub fn provide_jobs() -> JobsStore {
    let store = JobsStore { jobs: RwSignal::new(vec![]), loaded: RwSignal::new(false), disk_held: RwSignal::new(false) };
    provide_context(store);
    spawn_local(async move { store.reload().await });
    Effect::new(move |prev: Option<u64>| {
        let n = resync_counter().get();
        if prev.is_some() {
            spawn_local(async move { store.reload().await });
        }
        n
    });

    use_topic::<JobCreated>(TOPIC_JOB_CREATED, move |e| {
        let id = e.job_id.clone();
        spawn_local(async move {
            if let Ok(j) = api::get::<JobOut>(&format!("/jobs/{id}")).await {
                store.upsert(j);
            }
        });
    });
    use_topic::<JobProgress>(TOPIC_JOB_PROGRESS, move |p| {
        store.jobs.update(|v| {
            if let Some(j) = v.iter_mut().find(|j| j.id == p.job_id) {
                j.status = p.status.clone();
                j.total = p.total;
                j.completed = p.completed;
                j.failed = p.failed;
                j.skipped = p.skipped;
                j.progress = if p.total > 0 { (p.completed + p.failed + p.skipped) as f64 / p.total as f64 } else { 0.0 };
            }
        });
    });
    for (topic, status) in [
        (TOPIC_JOB_PAUSED, JOB_PAUSED),
        (TOPIC_JOB_RESUMED, JOB_RUNNING),
        (TOPIC_JOB_CANCELLED, JOB_CANCELLED),
    ] {
        use_topic::<JobRef>(topic, move |r| {
            store.jobs.update(|v| {
                if let Some(j) = v.iter_mut().find(|j| j.id == r.job_id) {
                    j.status = status.to_string();
                }
            });
        });
    }
    use_topic::<JobRef>(TOPIC_JOB_DELETED, move |r| store.jobs.update(|v| v.retain(|j| j.id != r.job_id)));
    use_topic::<serde_json::Value>(TOPIC_JOB_RETRIED, move |_| spawn_local(async move { store.reload().await }));
    use_topic::<DiskOut>(TOPIC_DOWNLOADS_DISK, move |d| store.disk_held.set(d.held));
    store
}

pub fn use_jobs() -> JobsStore {
    expect_context::<JobsStore>()
}
