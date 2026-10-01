//! Workstream 2: Bandcamp client, extractors, sources, downloads, harvest
//! services and their API routers (PLAN §8, §3.4).
//!
//! Layout (one owner per file while the crate is being built):
//! * `error`, `identity`, `urls`, `net/*`      — HTTP layer (rate limit, page cache, cookie)
//! * `extract/*`                               — extraction ladder (blob -> JSON-LD -> CSS)
//! * `sources/*`                               — search/discover/band/release/fan/collectors
//! * `download/*`                              — Downloader trait, native + bandcamp-dl, worker, dedup, disk guard
//! * `harvest/*`                               — inbox, fan walker, feed/label/favourites sweeps, enrich
//! * `tracklist/*`, `loved`                    — tracklist parse+match, loved-stream reconcile
//! * `api/*`, `service`                        — axum routers + `BandcampService`
#![allow(clippy::collapsible_if, clippy::type_complexity)]

pub mod error;
pub mod identity;
pub mod net;
pub mod replay;
pub mod urls;

pub mod extract;
pub mod sources;

pub mod download;
pub mod harvest;
pub mod loved;
pub mod lookup;
pub mod stream;
pub mod tracklist;

pub mod api;
pub mod service;

pub use error::{HarvestError, Result};
