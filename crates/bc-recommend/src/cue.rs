//! Where automix gets cue-in/cue-out from.

use bc_music::mixpoints;
use bc_types::analysis::MixPoints;

/// Source of mix points for a track. The parent wires a waveform-based implementation later;
/// `None` falls back to the tempo heuristic ([`HeuristicCues`]).
pub trait CueSource: Send + Sync {
    fn plan(&self, track_id: i64, duration_ms: Option<i64>, bpm: Option<f64>) -> Option<MixPoints>;
}

/// The legacy duration/tempo heuristic only (no waveforms): `mixpoints::defaults`.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicCues;

impl CueSource for HeuristicCues {
    fn plan(&self, _track_id: i64, duration_ms: Option<i64>, bpm: Option<f64>) -> Option<MixPoints> {
        Some(mixpoints::defaults(duration_ms, bpm))
    }
}

/// Plan with `source`, falling back to the heuristic when it has nothing for the track.
pub fn plan_or_default(source: &dyn CueSource, track_id: i64, duration_ms: Option<i64>, bpm: Option<f64>) -> MixPoints {
    source.plan(track_id, duration_ms, bpm).unwrap_or_else(|| mixpoints::defaults(duration_ms, bpm))
}
