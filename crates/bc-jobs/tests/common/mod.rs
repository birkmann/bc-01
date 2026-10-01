#![allow(dead_code)]
use bc_db::Db;
use bc_jobs::{Job, JobStore, NewItem, NewJob};

pub fn open() -> (tempfile::TempDir, JobStore) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("jobs.db")).unwrap();
    (dir, JobStore::new(db, None))
}

pub fn job(store: &JobStore, urls: &[&str]) -> Job {
    store
        .create_job(NewJob::new("download", urls.iter().map(|u| NewItem::url(*u, "album")).collect()).label("test"))
        .unwrap()
}

pub fn statuses(store: &JobStore, job_id: &str) -> Vec<String> {
    store.list_items(job_id, None, &[], 0, None).unwrap().into_iter().map(|i| i.status).collect()
}

pub const LEASE: f64 = bc_jobs::LEASE_SECONDS;
