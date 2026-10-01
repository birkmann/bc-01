//! Metadata write-back (PLAN §9): DJ fields from analysis into the audio files' tags.
//!
//! Gap-fill only (a value already in the file wins), mandatory dry-run preview, atomic writes via
//! `bc_media::write` (copy, edit, fsync, rename, fsync dir), a JSONL undo journal written BEFORE each
//! file, conflict detection, and the new stat + `tag_hash` committed in the same transaction that
//! settles each batch so the scanner never mistakes our own write for an external edit.

pub mod journal;
pub mod routes;
pub mod runner;

pub use routes::router;
pub use runner::{BatchReport, JobParams, TrackReport, pending_track_ids, plan_track, undo_job, write_track};
