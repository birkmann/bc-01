//! `bc-media`: tag reading and DJ-field write-back, cover art processing and HTTP file streaming.
//!
//! * [`tags`] -- canonical [`tags::TrackTags`] model, `read_tags`, cover extraction.
//! * [`write`] -- gap-fill of DJ fields with an atomic copy-edit-fsync-rename strategy.
//! * [`artwork`] -- WebP thumbnails, blurhash, dominant colour, on-disk layout.
//! * [`stream`] -- range-capable axum responses for audio files and art.
//! * [`camelot`] -- minimal key helpers.
//! * [`paths`] -- re-export of `bc_core::paths` (path safety lives there).

pub mod artwork;
pub mod camelot;
pub mod error;
pub mod stream;
pub mod tags;
pub mod write;

pub use bc_core::paths;
pub use error::{MediaError, Result};
