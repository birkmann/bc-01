//! Recommenders (workstream 3): suggest-next (`nextup`), the loved-shelf recommender (`taste`),
//! similar tracks (`similar`), DJ-set pool / suggestions / automix (`sets`) and similar playlists
//! (`playlists`). Pure scorers keep the exact legacy weights; the database layer goes through
//! [`bc_db::Db`] only and follows the hot-path SQL rules of PLAN 9l (no `OR` across tables,
//! correlated `EXISTS` for "file on disk").
//!
//! In-process entry points (no HTTP): [`nextup::suggest`], [`nextup::suggest_among`],
//! [`similar::suggest`], [`taste::suggest_loved`], [`sets::pool_page`], [`sets::suggest`],
//! [`sets::automix_in`], [`playlists::similar_playlist`].

pub mod cue;
pub mod error;
pub mod hydrate;
pub mod nextup;
pub mod playlists;
pub mod pool;
pub mod pooling;
pub mod scope;
pub mod service;
pub mod sets;
pub mod similar;
pub mod similar_to;
pub mod sqlutil;
pub mod taste;

pub use cue::{CueSource, HeuristicCues};
pub use error::RecommendError;
pub use scope::{Scope, ScopeParams};
pub use service::RecommendService;

#[cfg(test)]
mod testutil;
#[cfg(test)]
mod tests_db;
#[cfg(test)]
mod tests_routes;
