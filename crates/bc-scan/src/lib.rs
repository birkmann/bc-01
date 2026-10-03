//! Workstream 1: scanner, ingest, watcher, art pipeline, roots + scan routes.
//!
//! * [`scanner`] - three-stage incremental scan of a root, [`scanner::scan_paths`] for partial rescans.
//! * [`ingest`] - DB-side ingest (idempotent) and the public API for the download worker:
//!   [`ingest::ingest_paths`], [`ingest::ingest_dir`], [`ingest::root_for_path`].
//! * [`watcher`] - hot-root filesystem watcher.
//! * [`excluded`] - files removed from the library (kept on disk) and restoring them.
//! * [`art`] - legacy cover conversion job and on-demand conversion.
//! * [`roots`] / [`routes`] - roots CRUD, `ensure_roots`, `router(ctx)`.
//! * [`browse`] - the folder picker behind `GET /library/browse`.

pub mod art;
pub mod browse;
pub mod excluded;
pub mod ingest;
pub mod media;
pub mod roots;
pub mod routes;
pub mod scanner;
pub mod watcher;

pub use ingest::{IngestOptions, ingest_dir, ingest_paths, root_for_path};
pub use roots::ensure_roots;
pub use routes::router;
pub use scanner::{scan_paths, scan_root};
pub use watcher::Watcher;

#[cfg(test)]
mod testutil;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod route_tests;
#[cfg(test)]
mod art_tests;
