//! In-process accessors for the player (WS4): mix points and the waveform envelope for a track id.

use bc_db::Db;
use bc_music::beatgrid::legacy_grid;
use bc_music::mixpoints::{self, Envelope, MixPlan};
use bc_types::analysis::BeatGrid;
use bc_waveform::store::WaveformStore;

/// Mix-in/out (+ drop) for a track, from the stored overview envelope and beat grid, snapped to
/// phrases when a grid with downbeats exists. Falls back to the tempo heuristic when the track
/// has no waveform yet. `None` only when the track row does not exist.
pub fn mix_plan_for(db: &Db, waveforms: &WaveformStore, track_id: i64) -> Option<MixPlan> {
    let (dur, bpm, offset, conf, grid_json) = db
        .read(|c| {
            Ok(c.query_row(
                "SELECT t.duration_ms, a.bpm, a.beat_offset_ms, a.bpm_confidence, (SELECT grid FROM beat_grids g WHERE g.track_id = t.id) \
                 FROM tracks t LEFT JOIN analysis a ON a.track_id = t.id WHERE t.id = ?1",
                [track_id],
                |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?,
                        r.get::<_, Option<f64>>(1)?,
                        r.get::<_, Option<f64>>(2)?,
                        r.get::<_, Option<f64>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .ok())
        })
        .ok()
        .flatten()?;
    let base = mixpoints::defaults(dur, bpm);
    let Some(duration_ms) = dur.filter(|_| base.cue_out_ms.is_some()) else {
        return Some(MixPlan { points: base, drop_ms: None, snapped: false });
    };
    let grid: Option<BeatGrid> = grid_json
        .and_then(|j| serde_json::from_str(&j).ok())
        .or_else(|| legacy_grid(bpm, offset, conf, duration_ms as f64 / 1000.0));
    let Some(wf) = waveforms.get_overview_only(track_id) else {
        return Some(MixPlan { points: base, drop_ms: None, snapped: false });
    };
    let (level, low) = bc_waveform::legacy::mix_envelope(&wf);
    let env = Envelope { level: &level, low: &low };
    // legacy (essentia-import) grids are extrapolated, low-trust and have no downbeat phase:
    // plan_from_envelope then degrades to the plain envelope rule (exact legacy parity)
    Some(mixpoints::plan_from_envelope(&env, duration_ms, bpm, grid.as_ref(), 16))
}

/// Waveform-aware cue planner for automix: plug into
/// `bc_recommend::RecommendService::with_cue_source(Arc::new(analysis.cue_source()))`.
pub struct WaveformCues {
    pub db: Db,
    pub waveforms: std::sync::Arc<WaveformStore>,
}

impl bc_recommend::CueSource for WaveformCues {
    fn plan(&self, track_id: i64, _duration_ms: Option<i64>, _bpm: Option<f64>) -> Option<bc_types::analysis::MixPoints> {
        mix_plan_for(&self.db, &self.waveforms, track_id).map(|p| p.points)
    }
}
