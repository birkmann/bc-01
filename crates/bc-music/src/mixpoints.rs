//! Where a track should be mixed into and out of (port of `analysis/mixpoints.py` and
//! `player/mixPoints.ts`, which MUST stay in parity: plan and playback share these numbers),
//! plus the PLAN 7.1 upgrade: phrase-snapped points from the detail envelope.

use bc_types::analysis::{BeatGrid, MixPoints};

use crate::beatgrid::{GridQuery, Snap};

/// Tracks shorter than this are played whole: there is nothing to skip or blend.
pub const MIN_MIX_TRACK_MS: i64 = 90_000;
/// Sixteen bars: the classic length of a DJ intro.
pub const INTRO_BEATS: f64 = 64.0;
pub const INTRO_FALLBACK_MS: f64 = 30_000.0;
pub const INTRO_MAX_FRACTION: f64 = 0.15;
/// How long before the end the automatic blend starts, absent a better idea.
pub const OUT_BEFORE_END_MS: i64 = 10_000;
pub const PEAK_POINTS: usize = 200;

/// The tempo heuristic, for tracks without usable peaks.
pub fn defaults(duration_ms: Option<i64>, bpm: Option<f64>) -> MixPoints {
    let Some(dur) = duration_ms.filter(|d| *d >= MIN_MIX_TRACK_MS) else {
        return MixPoints { cue_in_ms: 0, cue_out_ms: None };
    };
    let intro = match bpm {
        Some(b) if b > 0.0 => INTRO_BEATS * 60_000.0 / b,
        _ => INTRO_FALLBACK_MS,
    };
    MixPoints {
        cue_in_ms: intro.min(dur as f64 * INTRO_MAX_FRACTION) as i64,
        cue_out_ms: Some(dur - OUT_BEFORE_END_MS),
    }
}

fn smooth(raw: &[f64]) -> Vec<f64> {
    let n = raw.len();
    (0..n).map(|i| (raw[i.saturating_sub(1)] + raw[i] + raw[(i + 1).min(n - 1)]) / 3.0).collect()
}

/// Python `round()` on a positive float with .5 ties to even.
fn py_round(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Raw findings from an envelope: seconds of the first sustained loud stretch and the end of the
/// last one. `level` is 0..1 per point.
fn sustained_bounds(level: &[f64], sec_per_point: f64) -> Option<(Option<f64>, Option<f64>)> {
    let n = level.len();
    let mut sorted = level.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let p90 = sorted[((n as f64 * 0.9) as usize).min(n - 1)];
    let threshold = p90 * 0.6;
    if threshold <= 0.0 {
        return None;
    }
    let window = (py_round(4.0 / sec_per_point)).max(2) as usize;
    if window > n {
        return Some((None, None));
    }
    let sustained = |i: usize| (i..i + window).all(|k| level[k] >= threshold);
    let first = (0..=n - window).find(|&i| sustained(i));
    let last_end = (0..=n - window).rev().find(|&i| sustained(i)).map(|i| i + window);
    Some((first.map(|i| i as f64 * sec_per_point), last_end.map(|i| i as f64 * sec_per_point)))
}

/// Refine the heuristic from the legacy peak envelope: int8 (min, max) pairs, peak-normalised.
/// Exact parity with `mixpoints.from_peaks` / `fromPeaks` in `mixPoints.ts`.
pub fn from_peaks(pairs: &[(i8, i8)], duration_ms: i64, base: MixPoints) -> MixPoints {
    let n = pairs.len();
    if n < 20 || duration_ms <= 0 {
        return base;
    }
    let dur = duration_ms as f64 / 1000.0;
    let sec_per_point = dur / n as f64;
    let raw: Vec<f64> =
        pairs.iter().map(|(lo, hi)| (i32::from(*lo).abs().max(i32::from(*hi).abs())) as f64 / 127.0).collect();
    apply_bounds(&smooth(&raw), dur, sec_per_point, base)
}

fn apply_bounds(level: &[f64], dur: f64, sec_per_point: f64, base: MixPoints) -> MixPoints {
    let Some((first, last_end)) = sustained_bounds(level, sec_per_point) else { return base };
    let mut cue_in = base.cue_in_ms;
    if let Some(f) = first {
        let t = f - 2.0;
        if (8.0..=dur * 0.4).contains(&t) {
            cue_in = (t * 1000.0) as i64;
        }
    }
    let mut cue_out = base.cue_out_ms;
    if let Some(t) = last_end.filter(|t| (dur - 45.0..=dur - 8.0).contains(t)) {
        cue_out = Some((t * 1000.0) as i64);
    }
    MixPoints { cue_in_ms: cue_in, cue_out_ms: cue_out }
}

/// Envelope input for the v2 planner: per-point levels in 0..1 over the whole track.
#[derive(Debug, Clone)]
pub struct Envelope<'a> {
    /// Overall level (e.g. max(peak, rms) mapped back to linear), uniform spacing.
    pub level: &'a [f32],
    /// Low-band level, same length (kick/bass: where the drop lands).
    pub low: &'a [f32],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MixPlan {
    pub points: MixPoints,
    /// Where the main sounds (kick/bass) first come in: the "drop".
    pub drop_ms: Option<i64>,
    /// True when the points were snapped to the phrase lattice.
    pub snapped: bool,
}

/// v2: points from the (detail) envelope, **snapped to phrase boundaries** when a grid with
/// downbeats is available (PLAN 7.1). Without a grid it degrades to the legacy behaviour.
///
/// * cue-in: the legacy "first sustained level" candidate; snapped to the phrase boundary at or
///   before it (the DJ starts the next record on a phrase). The 2 s pull-back is replaced by the
///   snap.
/// * cue-out: where the last sustained stretch ends; snapped to the nearest phrase boundary at
///   or before it (so the blend starts a phrase early rather than mid-phrase), kept within
///   `[dur-45s, dur-8s]`.
/// * drop: first point where the low band stays loud (>= 0.6 of its own p90).
pub fn plan_from_envelope(
    env: &Envelope<'_>,
    duration_ms: i64,
    bpm: Option<f64>,
    grid: Option<&BeatGrid>,
    phrase_bars: u32,
) -> MixPlan {
    let base = defaults(Some(duration_ms), bpm);
    let n = env.level.len();
    if base.cue_out_ms.is_none() || n < 20 || duration_ms <= 0 || env.low.len() != n {
        return MixPlan { points: base, drop_ms: None, snapped: false };
    }
    let dur = duration_ms as f64 / 1000.0;
    let spp = dur / n as f64;
    let level = smooth(&env.level.iter().map(|v| *v as f64).collect::<Vec<_>>());
    let low = smooth(&env.low.iter().map(|v| *v as f64).collect::<Vec<_>>());

    let Some((first, last_end)) = sustained_bounds(&level, spp) else {
        return MixPlan { points: base, drop_ms: None, snapped: false };
    };
    let drop = sustained_bounds(&low, spp).and_then(|(f, _)| f).map(|t| (t * 1000.0) as i64);

    let usable = grid.filter(|g| g.is_usable() && g.downbeat_phase.is_some());
    let Some(g) = usable else {
        return MixPlan { points: apply_bounds(&level, dur, spp, base), drop_ms: drop, snapped: false };
    };

    let mut points = base;
    // In: nearest phrase start at/before the sustained onset; allow half a bar of slack so an
    // onset a hair after a phrase boundary snaps to it rather than a whole phrase earlier.
    if let Some(f) = first {
        let t_ms = f * 1000.0;
        let slack = g.bar_ms() / 2.0;
        let snapped = g.snap_phrase_ms(t_ms + slack, phrase_bars, Snap::Floor);
        if (8_000.0..=duration_ms as f64 * 0.4).contains(&snapped) {
            points.cue_in_ms = snapped.round() as i64;
        } else {
            let bar = g.snap_bar_ms(t_ms + slack, Snap::Floor);
            if (8_000.0..=duration_ms as f64 * 0.4).contains(&bar) {
                points.cue_in_ms = bar.round() as i64;
            }
        }
    }
    if let Some(e) = last_end {
        let t_ms = e * 1000.0;
        let lo = (duration_ms - 45_000) as f64;
        let hi = (duration_ms - 8_000) as f64;
        if (lo..=hi).contains(&t_ms) {
            let mut s = g.snap_phrase_ms(t_ms, phrase_bars, Snap::Floor);
            if s < lo {
                s = g.snap_phrase_ms(t_ms, phrase_bars, Snap::Ceil);
            }
            if !(lo..=hi).contains(&s) {
                s = g.snap_bar_ms(t_ms, Snap::Floor);
            }
            points.cue_out_ms = Some(s.round() as i64);
        }
    }
    MixPlan { points, drop_ms: drop, snapped: true }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beatgrid::GridExt;

    #[test]
    fn short_tracks_play_whole() {
        assert_eq!(defaults(Some(80_000), Some(128.0)), MixPoints { cue_in_ms: 0, cue_out_ms: None });
        assert_eq!(defaults(None, Some(128.0)), MixPoints { cue_in_ms: 0, cue_out_ms: None });
    }

    #[test]
    fn intro_is_sixty_four_beats_at_the_tempo() {
        let got = defaults(Some(300_000), Some(128.0));
        assert_eq!(got.cue_in_ms, 30_000);
        assert_eq!(got.cue_out_ms, Some(290_000));
    }

    #[test]
    fn no_bpm_falls_back_to_thirty_seconds_capped_at_fifteen_percent() {
        let got = defaults(Some(100_000), None);
        assert_eq!(got.cue_in_ms, 15_000);
        assert_eq!(got.cue_out_ms, Some(90_000));
    }

    fn synthetic(n: usize, loud: std::ops::Range<usize>) -> Vec<(i8, i8)> {
        (0..n).map(|i| if loud.contains(&i) { (-100, 100) } else { (-5, 5) }).collect()
    }

    #[test]
    fn peaks_refine_both_cues() {
        let dur = 300_000;
        let base = defaults(Some(dur), Some(128.0));
        let got = from_peaks(&synthetic(200, 20..180), dur, base);
        assert!((24_000..=30_000).contains(&got.cue_in_ms), "{got:?}");
        let out = got.cue_out_ms.unwrap();
        assert!((255_000..=285_000).contains(&out), "{got:?}");
    }

    #[test]
    fn out_of_window_findings_keep_the_heuristic() {
        let dur = 300_000;
        let base = defaults(Some(dur), Some(128.0));
        let got = from_peaks(&synthetic(200, 0..200), dur, base);
        assert_eq!(got.cue_in_ms, base.cue_in_ms);
    }

    #[test]
    fn too_few_points_return_the_base() {
        let base = MixPoints { cue_in_ms: 1_000, cue_out_ms: Some(2_000) };
        assert_eq!(from_peaks(&[(-5, 5); 10], 300_000, base), base);
    }

    #[test]
    fn for_track_without_a_waveform_uses_defaults() {
        // the file-less path: `plan_from_envelope` with too few points is the defaults
        let env = Envelope { level: &[0.1; 5], low: &[0.1; 5] };
        let plan = plan_from_envelope(&env, 300_000, Some(128.0), None, 16);
        assert_eq!(plan.points, defaults(Some(300_000), Some(128.0)));
    }

    #[test]
    fn peak_parity_with_the_typescript_algorithm() {
        // Same vector as the Python test: 200 points, loud 20..180 -> inS = 18*1.5... pinned numbers
        let dur = 300_000;
        let base = defaults(Some(dur), Some(128.0));
        let got = from_peaks(&synthetic(200, 20..180), dur, base);
        // first sustained index: smoothing lowers index 19 -> level (5+5+100)/3 < threshold,
        // so first = 20 -> t = 20*1.5 - 2 = 28 s.
        assert_eq!(got.cue_in_ms, 28_000);
        // last sustained window ends at index 180 -> 270 s
        assert_eq!(got.cue_out_ms, Some(270_000));
    }

    #[test]
    fn phrase_snapping_puts_points_on_the_phrase_lattice() {
        // 128 BPM => 1 bar = 1875 ms, 16 bars = 30 s. First downbeat at 0.
        let mut g = BeatGrid::constant(128.0, 0.0, 1.0, "t");
        g.downbeat_phase = Some(0);
        let n = 600usize; // 0.5 s per point over 300 s
        let level: Vec<f32> = (0..n).map(|i| if (62..540).contains(&i) { 0.8 } else { 0.05 }).collect();
        let low = level.clone();
        let env = Envelope { level: &level, low: &low };
        let plan = plan_from_envelope(&env, 300_000, Some(128.0), Some(&g), 16);
        assert!(plan.snapped);
        let phrase = 30_000;
        assert_eq!(plan.points.cue_in_ms % phrase, 0, "{:?}", plan.points);
        let out = plan.points.cue_out_ms.unwrap();
        assert_eq!(out % phrase, 0, "{out}");
        // onset at 31 s -> phrase at 30 s
        assert_eq!(plan.points.cue_in_ms, 30_000);
        // loud until 270 s -> phrase boundary 270 s
        assert_eq!(out, 270_000);
        assert!(plan.drop_ms.is_some());
        // without a grid: same as the legacy envelope rule
        let plain = plan_from_envelope(&env, 300_000, Some(128.0), None, 16);
        assert!(!plain.snapped);
        assert_eq!(plain.points.cue_in_ms, 29_000);
    }
}
