//! Where the beats fall in a track, as far as the library knows.
//!
//! Port of `web/frontend/src/player/beatGrid.ts`. Analysis measured tempo and
//! the first beat of a 120-second window a quarter of the way into the track;
//! from those two numbers a grid is extrapolated across the whole track. A grid
//! here is a starting point that the phase-lock loop (or a nudge) finishes,
//! never a promise.

/// The backend's excerpt: 120 s from 25 % in; the whole track when shorter.
pub const EXCERPT_S: f64 = 120.0;
pub const EXCERPT_START_PCT: f64 = 0.25;
/// Below this the tempo is a guess and the grid would mislead more than help.
pub const MIN_CONFIDENCE: f64 = 0.3;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BeatGrid {
    /// Seconds into the track of some beat.
    pub origin_s: f64,
    /// Seconds per beat.
    pub period_s: f64,
    pub confidence: f64,
    /// Which beat (relative to `origin_s`, modulo 4) starts a bar. 0 when unknown.
    pub downbeat_beat: u32,
}

/// Where the analysed excerpt started, in seconds into the track.
pub fn excerpt_start(dur_s: f64) -> f64 {
    if dur_s <= EXCERPT_S {
        return 0.0;
    }
    (dur_s * EXCERPT_START_PCT).min(dur_s - EXCERPT_S)
}

/// A grid from the legacy analysis columns (`bpm`, `beat_offset_ms` relative to
/// the excerpt, `bpm_confidence`). `None` for a guessed tempo or missing offset.
pub fn grid_for(bpm: Option<f64>, beat_offset_ms: Option<f64>, confidence: Option<f64>, dur_s: f64) -> Option<BeatGrid> {
    let bpm = bpm.filter(|b| *b > 0.0)?;
    let off = beat_offset_ms?;
    let confidence = confidence.unwrap_or(0.0);
    if confidence < MIN_CONFIDENCE {
        return None;
    }
    Some(BeatGrid {
        origin_s: excerpt_start(dur_s) + off / 1000.0,
        period_s: 60.0 / bpm,
        confidence,
        downbeat_beat: 0,
    })
}

/// A grid from the v2 analysis (`beat_grids`): origin is already absolute.
pub fn grid_absolute(origin_s: f64, bpm: f64, downbeat_beat: u32, confidence: f64) -> Option<BeatGrid> {
    (bpm > 0.0).then(|| BeatGrid { origin_s, period_s: 60.0 / bpm, confidence, downbeat_beat })
}

/// Seconds since the last beat at time `t` (0 <= phase < period).
pub fn phase_at(grid: &BeatGrid, t: f64) -> f64 {
    let p = (t - grid.origin_s) % grid.period_s;
    if p < 0.0 { p + grid.period_s } else { p }
}

/// Seconds since the last `beats`-beat boundary (a bar when 4) at time `t`.
pub fn phase_in(grid: &BeatGrid, t: f64, beats: u32) -> f64 {
    let len = grid.period_s * beats.max(1) as f64;
    let origin = grid.origin_s + grid.downbeat_beat as f64 * grid.period_s;
    let p = (t - origin) % len;
    if p < 0.0 { p + len } else { p }
}

/// How far the incoming beat is from the outgoing beat, by the shortest way
/// round: positive means the incoming is late (behind) and should be pushed
/// forward, negative that it is early. `|result| <= period / 2`.
pub fn residual(out_phase: f64, in_phase: f64, period_s: f64) -> f64 {
    let mut r = out_phase - in_phase;
    r = ((r % period_s) + period_s) % period_s;
    if r > period_s / 2.0 {
        r -= period_s;
    }
    r
}

/// The nearest grid time at or before `t`, so a start point can be beat-snapped.
pub fn snap_to_beat(grid: &BeatGrid, t: f64) -> f64 {
    let at = t - phase_at(grid, t);
    if at < 1e-9 { 0.0 } else { at }
}

/// The nearest beat to `t` (before or after), never negative.
pub fn snap_nearest(grid: &BeatGrid, t: f64) -> f64 {
    let ph = phase_at(grid, t);
    let at = if ph > grid.period_s / 2.0 { t - ph + grid.period_s } else { t - ph };
    at.max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    #[test]
    fn excerpt_start_cases() {
        assert_eq!(excerpt_start(90.0), 0.0);
        assert_eq!(excerpt_start(120.0), 0.0);
        assert_eq!(excerpt_start(140.0), 20.0);
        assert_eq!(excerpt_start(400.0), 100.0);
    }

    #[test]
    fn grid_for_extrapolates_from_excerpt() {
        let g = grid_for(Some(120.0), Some(250.0), Some(0.7), 400.0).unwrap();
        assert_eq!(g.origin_s, 100.25);
        assert_eq!(g.period_s, 0.5);
        assert_eq!(g.confidence, 0.7);
    }

    #[test]
    fn grid_for_refuses_guess_or_missing() {
        assert!(grid_for(Some(120.0), Some(250.0), Some(0.1), 400.0).is_none());
        assert!(grid_for(Some(120.0), None, Some(0.9), 400.0).is_none());
        assert!(grid_for(None, Some(1.0), Some(0.9), 400.0).is_none());
    }

    #[test]
    fn phase_at_wraps_both_ways() {
        let g = BeatGrid { origin_s: 10.0, period_s: 0.5, confidence: 1.0, downbeat_beat: 0 };
        close(phase_at(&g, 10.2), 0.2);
        close(phase_at(&g, 9.9), 0.4);
        close(phase_at(&g, 12.0), 0.0);
    }

    #[test]
    fn residual_takes_the_short_way() {
        close(residual(0.1, 0.0, 0.5), 0.1);
        close(residual(0.0, 0.1, 0.5), -0.1);
        close(residual(0.45, 0.05, 0.5), -0.1);
        close(residual(0.05, 0.45, 0.5), 0.1);
    }

    #[test]
    fn snap_to_beat_lands_on_previous_beat() {
        let g = BeatGrid { origin_s: 10.0, period_s: 0.5, confidence: 1.0, downbeat_beat: 0 };
        close(snap_to_beat(&g, 12.3), 12.0);
        assert_eq!(snap_to_beat(&g, 0.1), 0.0);
    }

    #[test]
    fn snap_nearest_and_bar_phase() {
        let g = BeatGrid { origin_s: 10.0, period_s: 0.5, confidence: 1.0, downbeat_beat: 1 };
        close(snap_nearest(&g, 12.3), 12.5);
        close(snap_nearest(&g, 12.2), 12.0);
        // bar origin = 10.5, bar length 2.0
        close(phase_in(&g, 12.0, 4), 1.5);
        close(phase_in(&g, 10.5, 4), 0.0);
    }
}
