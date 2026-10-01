//! Workstream 2: the durable job queue (`store`), shared helpers for workers
//! (`throttle`, `hooks`) and the generic job-control API (`JobsService`).
//!
//! Every long task in the app is a job (PLAN §3.4): download, scan, move,
//! analyze, metadata, harvest, walk, sweep, enrich. Producers create jobs with
//! [`JobStore::create_job`]; workers claim items by kind. See `docs/api/ws2.md`.
#![allow(clippy::collapsible_if, clippy::type_complexity)]

pub mod api_error;
pub mod hooks;
pub mod runner;
pub mod service;
pub mod store;
pub mod throttle;
pub mod time;

pub use api_error::ApiError;
pub use hooks::{Interrupt, JobHooks};
pub use service::JobsService;
pub use store::{
    Claimed, Complete, create_job_in, FailOutcome, ItemGroup, Job, JobItem, JobStore, LEASE_SECONDS, NewItem, NewJob, Place,
    ReconcileReport, RemoveReport, backoff_delay, host_of,
};
pub use runner::{Gate, HandlerOutcome, ItemCtx, ItemHandler, KindWorker, WorkerSpec};
pub use throttle::{ProgressReporter, Throttle};
