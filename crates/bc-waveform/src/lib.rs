//! `bc-waveform`: `.bcw2` waveform data (format, streaming builder, mip pyramid, LRU store)
//! and the WebGL2 / Canvas2D renderer (feature `webgl`). See `docs/api/waveform-format.md`.

pub mod bars;
pub mod builder;
pub mod format;
pub mod legacy;
pub mod mip;
pub mod scale;
pub mod view;

#[cfg(all(feature = "store", not(target_arch = "wasm32")))]
pub mod store;

#[cfg(feature = "webgl")]
pub mod render;

pub use bars::{BarValue, Refs, compute_bars, reference_levels};
pub use builder::WaveformBuilder;
pub use format::Level;
pub use view::{Style, WaveStyle, WaveTheme};
pub use format::{EncodeOpts, Header, Levels, Waveform, WaveformError, read_header};
