//! Workstream 1 umbrella crate: the read API (tracks, releases, artists, labels, tags, stats, history,
//! favorites, home), media routes, the importer, and `LibraryService` which assembles every WS1 router.
//! See `docs/api/ws1.md`.

#![allow(clippy::too_many_arguments, clippy::type_complexity)]

pub mod artists;
pub mod doctor;
pub mod favorites;
pub mod history;
pub mod home;
pub mod import;
pub mod labels;
pub mod releases;
pub mod routes;
pub mod sqlb;
pub mod stats;
pub mod tags;
pub mod tracks;
pub mod urls;

pub use bc_libcore::{ApiError, ApiResult, Ctx, JobHandle, JobHost, LocalJobs, Scope, hydrate};
pub mod media;
pub mod service;

pub use service::LibraryService;
pub use bc_scan::{art, ingest};
