//! Workstream 1 shared base. Every WS1 crate (`bc-scan`, `bc-meta`, `bc-maint`, `bc-plist`,
//! `bc-library`) builds on this: the [`Ctx`] handle, [`ApiError`], the library [`Scope`],
//! row hydration and the task host.

pub mod availability;
pub mod error;
pub mod extract;
pub mod files;
pub mod hydrate;
pub mod jobs;
pub mod queue;
pub mod secrets;
pub mod scope;

use std::sync::Arc;

pub use bc_core::{Config, EventBus};
pub use bc_db::Db;
pub use files::{ResolvedFile, resolve_track_file};
pub use error::{ApiError, ApiResult};
pub use extract::Q;
pub use jobs::{JobHandle, JobHost, LocalJobs, TaskInfo};
pub use scope::Scope;

/// What every service and router holds. Cheap to clone.
#[derive(Clone)]
pub struct Ctx {
    pub db: Db,
    pub bus: Arc<EventBus>,
    pub config: Arc<Config>,
    pub jobs: Arc<dyn JobHost>,
    memo: Arc<parking_lot::Mutex<std::collections::HashMap<String, (u64, serde_json::Value)>>>,
}

impl Ctx {
    pub fn new(db: Db, bus: Arc<EventBus>, config: Config) -> Self {
        let jobs: Arc<dyn JobHost> = Arc::new(LocalJobs::new(bus.clone()));
        Self { db, bus, config: Arc::new(config), jobs, memo: Default::default() }
    }

    pub fn with_jobs(mut self, jobs: Arc<dyn JobHost>) -> Self {
        self.jobs = jobs;
        self
    }

    /// Derived values (library stats, facets) computed at most once per DB write generation.
    pub fn memo<T: serde::Serialize + serde::de::DeserializeOwned>(&self, key: &str, f: impl FnOnce() -> ApiResult<T>) -> ApiResult<T> {
        let gen_now = self.db.generation();
        if let Some((g, v)) = self.memo.lock().get(key)
            && *g == gen_now
            && let Ok(t) = serde_json::from_value(v.clone())
        {
            return Ok(t);
        }
        let t = f()?;
        if let Ok(v) = serde_json::to_value(&t) {
            let mut m = self.memo.lock();
            if m.len() > 256 {
                m.clear();
            }
            m.insert(key.to_string(), (gen_now, v));
        }
        Ok(t)
    }

    /// Blocking read on the pool, error-mapped to [`ApiError`].
    pub fn read<T>(&self, f: impl FnOnce(&bc_db::rusqlite::Connection) -> ApiResult<T>) -> ApiResult<T> {
        self.db.read_with::<T, ApiError>(f)
    }

    /// Async read: never blocks a tokio worker.
    pub async fn read_async<T: Send + 'static>(
        &self,
        f: impl FnOnce(&bc_db::rusqlite::Connection) -> ApiResult<T> + Send + 'static,
    ) -> ApiResult<T> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || db.read_with::<T, ApiError>(f))
            .await
            .map_err(ApiError::internal)?
    }

    /// Async write on the single writer thread (`BEGIN IMMEDIATE`); `Err` rolls back.
    pub async fn write_async<T: Send + 'static>(
        &self,
        f: impl FnOnce(&bc_db::rusqlite::Transaction<'_>) -> ApiResult<T> + Send + 'static,
    ) -> ApiResult<T> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || db.write_with::<T, ApiError>(f))
            .await
            .map_err(ApiError::internal)?
    }

    /// Blocking write (use from `spawn_blocking` / worker threads).
    pub fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&bc_db::rusqlite::Transaction<'_>) -> ApiResult<T> + Send + 'static,
    ) -> ApiResult<T> {
        self.db.write_with::<T, ApiError>(f)
    }
}
