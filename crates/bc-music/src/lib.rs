//! Music logic that exists exactly once (PLAN 3.1): compiles to native and `wasm32`.
//! No tokio, no rusqlite, no filesystem.

pub mod automix;
pub mod beatgrid;
pub mod camelot;
pub mod mixpoints;
pub mod ordering;
pub mod setmath;
pub mod shuffle;

pub use bc_types::analysis::{BeatGrid, Compatibility, GridKind, MixPoints, TempoSegment, Verdict};
