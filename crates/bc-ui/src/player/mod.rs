//! Player UI: store (state + clock), bar, queue, deck view, planner and similar panels.
pub mod accent;
pub mod bar;
pub mod deck;
pub mod local;
pub mod plan;
pub mod queue;
pub mod similar;
pub mod store;
pub mod waveform;

pub use store::{PlayerCtx, provide_player, use_player};
