//! Where a track should be mixed into and out of.
//!
//! Port of `web/frontend/src/player/mixPoints.ts`. Both points come from the
//! waveform peaks when the track has been analysed (the first and last stretch
//! of sustained level), and from a tempo heuristic when it has not: sixteen
//! bars at the track's BPM, or thirty seconds, is a fair guess at an intro;
//! ten seconds before the end a fair guess at an outro.
//!
//! The numbers come from WS3's `bc_music::mixpoints` (one copy of the logic for
//! plan and playback); this module only converts to seconds and adds the
//! per-track `blend_s` pin of a DJ set.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MixPoints {
    /// Seconds into the track to start it at when it comes in.
    pub in_s: f64,
    /// Seconds into the track at which to start blending out; `None` = never (hard cut).
    pub out_s: Option<f64>,
    /// Planned length of the blend INTO this track (a DJ set's pinned transition).
    pub blend_s: Option<f64>,
}

/// Tracks shorter than this are played whole: there is nothing to skip or blend.
pub const MIN_MIX_TRACK_S: f64 = bc_music::mixpoints::MIN_MIX_TRACK_MS as f64 / 1000.0;
/// How long before the end the automatic blend starts, absent a better idea.
pub const OUT_BEFORE_END_S: f64 = bc_music::mixpoints::OUT_BEFORE_END_MS as f64 / 1000.0;

fn from_ms(p: bc_types::analysis::MixPoints) -> MixPoints {
    MixPoints { in_s: p.cue_in_ms as f64 / 1000.0, out_s: p.cue_out_ms.map(|o| o as f64 / 1000.0), blend_s: None }
}

fn to_ms(p: MixPoints) -> bc_types::analysis::MixPoints {
    bc_types::analysis::MixPoints { cue_in_ms: (p.in_s * 1000.0) as i64, cue_out_ms: p.out_s.map(|o| (o * 1000.0) as i64) }
}

/// The tempo heuristic for a track of `dur_s` seconds at `bpm` (`bc_music::mixpoints::defaults`).
pub fn heuristic(dur_s: f64, bpm: Option<f64>) -> MixPoints {
    from_ms(bc_music::mixpoints::defaults(Some((dur_s * 1000.0) as i64), bpm))
}

/// Refine the heuristic from the legacy peak envelope (`bc_music::mixpoints::from_peaks`).
pub fn from_peaks(peaks: &[(i8, i8)], dur: f64, base: MixPoints) -> MixPoints {
    let r = from_ms(bc_music::mixpoints::from_peaks(peaks, (dur * 1000.0) as i64, to_ms(base)));
    MixPoints { blend_s: base.blend_s, ..r }
}

/// Points from analysed cues (`MixIn` / `MixOut`, in ms) over a base.
pub fn with_cues(base: MixPoints, mix_in_ms: Option<i64>, mix_out_ms: Option<i64>) -> MixPoints {
    MixPoints {
        in_s: mix_in_ms.map(|v| v as f64 / 1000.0).unwrap_or(base.in_s),
        out_s: mix_out_ms.map(|v| v as f64 / 1000.0).or(base.out_s),
        blend_s: base.blend_s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_tracks_play_whole() {
        let p = heuristic(80.0, Some(128.0));
        assert_eq!(p.out_s, None);
        assert_eq!(p.in_s, 0.0);
    }

    #[test]
    fn heuristic_is_sixteen_bars_capped_at_a_fraction() {
        // 64 beats at 128 = 30 s; 300 s * 0.15 = 45 -> 30
        let p = heuristic(300.0, Some(128.0));
        assert!((p.in_s - 30.0).abs() < 1e-9);
        assert_eq!(p.out_s, Some(290.0));
        // slow tempo: 64 beats at 60 = 64 s, capped at 15 % of 300 = 45
        assert!((heuristic(300.0, Some(60.0)).in_s - 45.0).abs() < 1e-9);
        // no tempo: 30 s fallback
        assert!((heuristic(300.0, None).in_s - 30.0).abs() < 1e-9);
    }

    #[test]
    fn peaks_find_sustained_stretch() {
        // 300 s over 200 points = 1.5 s/point: quiet intro for 30 points, loud, quiet tail 6 points
        let mut peaks = vec![(-5i8, 5i8); 30];
        peaks.extend(vec![(-120i8, 120i8); 164]);
        peaks.extend(vec![(-5i8, 5i8); 6]);
        let base = heuristic(300.0, Some(128.0));
        let p = from_peaks(&peaks, 300.0, base);
        assert!((p.in_s - (30.0 * 1.5 - 2.0)).abs() < 1.6, "in {}", p.in_s);
        assert!(p.out_s.unwrap() > 290.0 - 1.0 && p.out_s.unwrap() <= 292.0, "out {:?}", p.out_s);
    }

    #[test]
    fn too_few_peaks_keep_the_base() {
        let base = heuristic(300.0, Some(128.0));
        assert_eq!(from_peaks(&[(1, 1); 5], 300.0, base), base);
    }
}
