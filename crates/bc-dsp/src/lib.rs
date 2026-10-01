//! `bc-dsp`: the realtime mixer core shared by the native engine (cpal), the
//! offline renderer and the browser AudioWorklet (wasm32). See PLAN §5.1.
//!
//! * [`envelope`], [`beatmatch`], [`beatgrid`]: exact ports of the browser
//!   player's planning code (`envelope.ts`, `beatmatch.ts`, `beatGrid.ts`).
//! * [`deck`], [`stretch`]: ring-fed decks with cubic vinyl resampling and a
//!   phase-vocoder key lock.
//! * [`filters`], [`echo`], [`limiter`]: LR4 3-band EQ with kills, one-knob
//!   filter, tempo-synced echo, look-ahead true-peak limiter.
//! * [`mixer`]: the graph, the transitions (blend, bass swap, filter, echo-out,
//!   cut), the phase-lock loop, gapless hand-over and the command/event ABI.
//!
//! Nothing in `process`/`render` allocates, locks or makes syscalls.

#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]

pub mod beatgrid;
pub mod beatmatch;
pub mod deck;
pub mod echo;
pub mod envelope;
pub mod fft;
pub mod filters;
pub mod limiter;
pub mod mixer;
pub mod norm;
pub mod offline;
#[cfg(test)]
mod golden;
pub mod shared;
pub mod stretch;

pub use deck::{CHUNK_FRAMES, Chunk};
pub use mixer::{Cmd, CueOut, Event, Mixer, MixerPorts};
pub use shared::{DeckSnap, SharedState, Snapshot};
