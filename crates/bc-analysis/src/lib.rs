//! bc-analysis: streaming single-pass track analysis (PLAN 7).
//!
//! One symphonia decode feeds BS.1770 loudness (`ebur128`), the waveform builder, the onset
//! envelope and a chroma accumulator; whole-track tempo / beat / downbeat / phrase / key /
//! energy / mix-point stages then run on the small accumulated series.

// Numeric/DSP code is clearer with indexed loops and explicit nested conditions.
#![allow(
    clippy::needless_range_loop,
    clippy::collapsible_if,
    clippy::type_complexity,
    clippy::manual_is_multiple_of,
    clippy::field_reassign_with_default,
    clippy::too_many_arguments
)]

pub mod decode;
pub mod dsp;
pub mod energy;
pub mod energy_defaults;
pub mod features;
pub mod key;
pub mod key_defaults;
pub mod pipeline;
pub mod rhythm;
pub mod tempo;

pub use pipeline::{AnalyzeError, AnalyzeOptions, TrackAnalysis, analyze_file};
pub mod error;
pub mod persist;
pub mod pool;
pub mod routes;
pub mod runner;
pub mod schema;
pub mod service;
pub mod sidecar;

pub use service::AnalysisService;

pub(crate) fn time_now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}
pub mod analyzer;
pub mod bench;
pub mod mixplan;
