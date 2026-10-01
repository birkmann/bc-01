//! Workstream 1: the maintenance services of the library and their HTTP routes.
//!
//! * [`snippets`] -- teaser-clip classifier (also called by the scanner) and the stored verdict
//! * [`completeness`] -- expected track counts, "fill missing" queueing
//! * [`blacklist`], [`cleanup`] -- never-download-again list; junk-album finder
//! * [`strays`] -- single tracks filed as releases, and the background merger
//! * [`relocate`] -- move a library root (also across drives)
//! * [`tidy`] -- sidecar / empty-dir / artwork sweeps
//! * [`delete`] -- file-backed deletes confined to registered roots
//! * [`matching`], [`dedup`] -- `(artist, title)` release index; URL dedup and the repair passes
//! * [`loved`] -- loved Bandcamp streams and their conversion to loved library tracks
//! * [`adopt`] -- the write half of the library scope (adopt / assign releases)
//! * [`repairs`] -- the run-once data migrations, with [`repairs::run_all`]
//! * [`routes`] -- [`router`] (legacy paths, no `/api` prefix)
//! * [`lookup`] -- the [`BandcampLookup`] trait the Bandcamp client implements
//!
//! Database logic takes `&Connection` / `&Transaction` (a `Transaction` derefs to `Connection`)
//! so callers decide the transaction boundary; the `Ctx`-taking functions open their own.

#![cfg_attr(test, allow(clippy::unit_arg, clippy::too_many_arguments, clippy::cloned_ref_to_slice_refs, clippy::some_filter, clippy::type_complexity))]

pub mod adopt;
pub mod blacklist;
pub mod cleanup;
pub mod completeness;
pub mod dedup;
pub mod delete;
pub mod lookup;
pub mod loved;
pub mod matching;
pub mod relocate;
pub mod repairs;
pub mod routes;
pub mod snippets;
pub mod strays;
pub mod tidy;
pub mod urls;
pub mod util;

#[cfg(test)]
pub(crate) mod testutil;

pub use lookup::{AlbumInfo, AlbumTrack, BandcampLookup, LookupError};
pub use routes::{loved_router, router, router_with};
