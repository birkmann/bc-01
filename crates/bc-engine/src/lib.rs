//! `bc-engine`: the native playback host (PLAN §5.2).
//!
//! * [`decode`]: symphonia decode workers (memory-mapped files, HTTP range
//!   reader with read-ahead for Bandcamp streams) resampled with rubato into
//!   lock-free rings.
//! * [`host`]: cpal output (ALSA, served by PipeWire), device and buffer
//!   selection, an optional second cue/headphone device, a null output for
//!   tests, and the control-side [`host::Engine`] handle.
//! * [`session`]: `PlayerSession` -- queue, history, true shuffle, repeat,
//!   sources, the DJ-mix trigger, auto-fill, gapless priming, preview.
//! * [`service`]: `PlayerService` (`new` / `start` / `router`) and
//!   `handle_command` for the WebSocket hub.
//! * [`mpris`]: media keys and `playerctl`.
//! * [`render`]: offline render of a planned set through the same graph.
//! * [`ports`]: traits for the other workstreams plus a DB-backed shim.

pub mod autofill;
pub mod decode;
pub mod host;
pub mod mpris;
pub mod plan;
pub mod ports;
pub mod queue_ops;
pub mod mixpoints;
pub mod render;
pub mod service;
pub mod similar;
pub mod session;

pub use bc_dsp;
pub use service::{PlayerHandle, PlayerService};
pub use session::{NullPublisher, PlayerError, PlayerReply, Publisher, Session, SessionConfig, SessionMsg};
