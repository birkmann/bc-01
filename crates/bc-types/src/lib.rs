//! Shared DTOs between server, services and the WASM UI. Must compile for wasm32:
//! no tokio, no rusqlite, no filesystem. Each module is owned by one workstream
//! (see COORDINATION.md); add types to your own module only.

pub mod analysis; // workstream 3
pub mod bandcamp; // workstream 2
pub mod common; // orchestrator (shared)
pub mod events; // orchestrator (shared)
pub mod jobs; // workstream 2
pub mod library; // workstream 1
pub mod player; // workstream 4
pub mod sets; // workstream 3
pub mod suggest; // workstream 3
pub mod theme; // workstream 5
pub mod ui; // workstream 5

pub use common::*;
