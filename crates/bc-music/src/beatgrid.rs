//! Beat-grid maths over [`bc_types::analysis::BeatGrid`]: constant or variable (tempo
//! segments), downbeats, phrases. All times are **milliseconds** into the track.
//!
//! Replaces `frontend/src/player/beatGrid.ts` (which extrapolated one beat offset measured on
//! a 120 s excerpt) and keeps its helper semantics (`phase`, `residual`, `snap`).

use bc_types::analysis::{BeatGrid, GridKind, TempoSegment};

/// Legacy essentia analysis measured on a 120 s window a quarter of the way in.
pub const LEGACY_EXCERPT_S: f64 = 120.0;
pub const LEGACY_EXCERPT_START_PCT: f64 = 0.25;
/// Below this the tempo is a guess and a grid would mislead more than help.
pub const MIN_CONFIDENCE: f64 = 0.3;

/// Where the legacy analysed excerpt started, in seconds into the track.
pub fn legacy_excerpt_start_s(dur_s: f64) -> f64 {
    if dur_s <= LEGACY_EXCERPT_S {
        return 0.0;
    }
    (dur_s * LEGACY_EXCERPT_START_PCT).min(dur_s - LEGACY_EXCERPT_S)
}

/// Extrapolated grid from a legacy (`essentia-import`) row: bpm + first beat in the excerpt.
/// `None` when the tempo was a guess or the offset is missing.
pub fn legacy_grid(
    bpm: Option<f64>,
    beat_offset_ms: Option<f64>,
    confidence: Option<f64>,
    dur_s: f64,
) -> Option<BeatGrid> {
    let bpm = bpm.filter(|b| *b > 0.0)?;
    let off = beat_offset_ms?;
    let conf = confidence.unwrap_or(0.0);
    if conf < MIN_CONFIDENCE {
        return None;
    }
    let origin = legacy_excerpt_start_s(dur_s) * 1000.0 + off;
    Some(BeatGrid::constant(bpm, origin, conf, "essentia-import"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snap {
    Nearest,
    Floor,
    Ceil,
}

/// One tick for a renderer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tick {
    pub t_ms: f64,
    /// Global beat index (0 = first segment origin).
    pub beat: i64,
    /// 0-based position inside the bar (0 = downbeat) when the downbeat phase is known.
    pub beat_in_bar: Option<u8>,
    pub is_downbeat: bool,
    /// Downbeat that also starts a phrase of `phrase_bars` bars.
    pub is_phrase: bool,
}

pub trait GridExt {
    fn constant(bpm: f64, origin_ms: f64, confidence: f64, source: &str) -> BeatGrid;
    /// Build from tempo segments (sorted by origin).
    fn variable(segments: Vec<TempoSegment>, confidence: f64, source: &str) -> BeatGrid;
}

impl GridExt for BeatGrid {
    fn constant(bpm: f64, origin_ms: f64, confidence: f64, source: &str) -> BeatGrid {
        BeatGrid {
            kind: GridKind::Constant,
            segments: vec![TempoSegment { origin_ms, bpm, beats: None }],
            downbeat_phase: None,
            beats_per_bar: 4,
            phrase_starts_ms: vec![],
            confidence,
            source: source.to_string(),
        }
    }
    fn variable(segments: Vec<TempoSegment>, confidence: f64, source: &str) -> BeatGrid {
        BeatGrid {
            kind: GridKind::Variable,
            segments,
            downbeat_phase: None,
            beats_per_bar: 4,
            phrase_starts_ms: vec![],
            confidence,
            source: source.to_string(),
        }
    }
}

/// Query helpers, implemented as an extension trait because the DTO lives in `bc-types`.
pub trait GridQuery {
    fn is_usable(&self) -> bool;
    /// Representative tempo (first segment, or beat-weighted mean for variable grids).
    fn bpm(&self) -> f64;
    fn period_ms_at(&self, t_ms: f64) -> f64;
    /// Fractional beat index at `t_ms` (0 = origin of the first segment; negative before it).
    fn beat_pos(&self, t_ms: f64) -> f64;
    /// Time of (fractional) beat index.
    fn beat_time_ms(&self, beat: f64) -> f64;
    /// Milliseconds since the previous beat (`0 <= phase < period`).
    fn phase_ms(&self, t_ms: f64) -> f64;
    fn snap_beat_ms(&self, t_ms: f64, mode: Snap) -> f64;
    /// Snap to the nearest downbeat (needs `downbeat_phase`; falls back to beats).
    fn snap_bar_ms(&self, t_ms: f64, mode: Snap) -> f64;
    /// Snap to the `bars`-bar lattice anchored on the first phrase start (or first downbeat).
    fn snap_phrase_ms(&self, t_ms: f64, bars: u32, mode: Snap) -> f64;
    /// Ticks inside `[from_ms, to_ms]`, capped at `max` (renderer / grid drawing).
    fn ticks(&self, from_ms: f64, to_ms: f64, phrase_bars: u32, max: usize) -> Vec<Tick>;
    fn bar_ms(&self) -> f64;
}

fn seg_beats(g: &BeatGrid, k: usize) -> f64 {
    let s = &g.segments[k];
    if let Some(b) = s.beats {
        return b as f64;
    }
    match g.segments.get(k + 1) {
        Some(n) => ((n.origin_ms - s.origin_ms) / (60_000.0 / s.bpm)).round(),
        None => f64::INFINITY,
    }
}

fn seg_starts(g: &BeatGrid) -> Vec<f64> {
    let mut out = Vec::with_capacity(g.segments.len());
    let mut acc = 0.0;
    for k in 0..g.segments.len() {
        out.push(acc);
        let b = seg_beats(g, k);
        if b.is_finite() {
            acc += b;
        }
    }
    out
}

fn seg_for_time(g: &BeatGrid, t_ms: f64) -> usize {
    let mut k = 0;
    for (i, s) in g.segments.iter().enumerate() {
        if s.origin_ms <= t_ms {
            k = i;
        } else {
            break;
        }
    }
    k
}

impl GridQuery for BeatGrid {
    fn is_usable(&self) -> bool {
        !self.segments.is_empty() && self.segments.iter().all(|s| s.bpm.is_finite() && s.bpm > 0.0)
    }

    fn bpm(&self) -> f64 {
        if self.segments.len() <= 1 {
            return self.segments.first().map_or(0.0, |s| s.bpm);
        }
        let (mut w, mut sum) = (0.0, 0.0);
        for k in 0..self.segments.len() {
            let b = seg_beats(self, k);
            let b = if b.is_finite() { b } else { 1.0 };
            w += b;
            sum += b * self.segments[k].bpm;
        }
        if w > 0.0 { sum / w } else { self.segments[0].bpm }
    }

    fn period_ms_at(&self, t_ms: f64) -> f64 {
        60_000.0 / self.segments[seg_for_time(self, t_ms)].bpm
    }

    fn beat_pos(&self, t_ms: f64) -> f64 {
        let k = seg_for_time(self, t_ms);
        let starts = seg_starts(self);
        let s = &self.segments[k];
        starts[k] + (t_ms - s.origin_ms) / (60_000.0 / s.bpm)
    }

    fn beat_time_ms(&self, beat: f64) -> f64 {
        let starts = seg_starts(self);
        let mut k = 0;
        for (i, st) in starts.iter().enumerate() {
            if *st <= beat {
                k = i;
            }
        }
        // before the first segment: extrapolate with k = 0 (negative offset)
        let s = &self.segments[k];
        s.origin_ms + (beat - starts[k]) * (60_000.0 / s.bpm)
    }

    fn phase_ms(&self, t_ms: f64) -> f64 {
        let pos = self.beat_pos(t_ms);
        let frac = pos - pos.floor();
        frac * self.period_ms_at(t_ms)
    }

    fn snap_beat_ms(&self, t_ms: f64, mode: Snap) -> f64 {
        let pos = self.beat_pos(t_ms);
        let n = match mode {
            Snap::Floor => (pos + 1e-9).floor(),
            Snap::Ceil => (pos - 1e-9).ceil(),
            Snap::Nearest => pos.round(),
        };
        self.beat_time_ms(n)
    }

    fn snap_bar_ms(&self, t_ms: f64, mode: Snap) -> f64 {
        let Some(phase) = self.downbeat_phase else { return self.snap_beat_ms(t_ms, mode) };
        let bpb = self.beats_per_bar.max(1) as f64;
        let rel = (self.beat_pos(t_ms) - phase as f64) / bpb;
        let n = match mode {
            Snap::Floor => (rel + 1e-9).floor(),
            Snap::Ceil => (rel - 1e-9).ceil(),
            Snap::Nearest => rel.round(),
        };
        self.beat_time_ms(n * bpb + phase as f64)
    }

    fn snap_phrase_ms(&self, t_ms: f64, bars: u32, mode: Snap) -> f64 {
        let bpb = self.beats_per_bar.max(1) as f64;
        let bars = bars.max(1) as f64;
        // Anchor: first stored phrase start, else the first downbeat at/after beat 0.
        let anchor_beat = match self.phrase_starts_ms.first() {
            Some(p) => self.beat_pos(*p).round(),
            None => self.downbeat_phase.map_or(0.0, |p| p as f64),
        };
        let lattice = bpb * bars;
        let rel = (self.beat_pos(t_ms) - anchor_beat) / lattice;
        let n = match mode {
            Snap::Floor => (rel + 1e-9).floor(),
            Snap::Ceil => (rel - 1e-9).ceil(),
            Snap::Nearest => rel.round(),
        };
        self.beat_time_ms(anchor_beat + n * lattice)
    }

    fn ticks(&self, from_ms: f64, to_ms: f64, phrase_bars: u32, max: usize) -> Vec<Tick> {
        let mut out = Vec::new();
        if !self.is_usable() || to_ms < from_ms {
            return out;
        }
        let bpb = self.beats_per_bar.max(1) as i64;
        let phrase_beats = bpb * phrase_bars.max(1) as i64;
        let anchor = match self.phrase_starts_ms.first() {
            Some(p) => self.beat_pos(*p).round() as i64,
            None => self.downbeat_phase.map_or(0, |p| p as i64),
        };
        let mut n = self.beat_pos(from_ms).floor() as i64;
        while out.len() < max {
            let t = self.beat_time_ms(n as f64);
            if t > to_ms {
                break;
            }
            if t >= from_ms {
                let (bib, down) = match self.downbeat_phase {
                    Some(p) => {
                        let r = (n - p as i64).rem_euclid(bpb);
                        (Some(r as u8), r == 0)
                    }
                    None => (None, false),
                };
                let phrase = down && (n - anchor).rem_euclid(phrase_beats) == 0;
                out.push(Tick { t_ms: t, beat: n, beat_in_bar: bib, is_downbeat: down, is_phrase: phrase });
            }
            n += 1;
        }
        out
    }

    fn bar_ms(&self) -> f64 {
        self.beats_per_bar.max(1) as f64 * 60_000.0 / self.bpm().max(1e-9)
    }
}

/// Position (`ms`) of the grid, for beat-matching: the incoming beat is how far from the
/// outgoing beat, by the shortest way round: positive = incoming is late and should be pushed
/// forward. `|result| <= period/2`. (Parity with `residual` in `beatGrid.ts`.)
pub fn residual(out_phase: f64, in_phase: f64, period: f64) -> f64 {
    let mut r = out_phase - in_phase;
    r = ((r % period) + period) % period;
    if r > period / 2.0 {
        r -= period;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn excerpt_start() {
        assert_eq!(legacy_excerpt_start_s(90.0), 0.0);
        assert_eq!(legacy_excerpt_start_s(120.0), 0.0);
        assert_eq!(legacy_excerpt_start_s(140.0), 20.0);
        assert_eq!(legacy_excerpt_start_s(400.0), 100.0);
    }

    #[test]
    fn legacy_grid_extrapolates_from_the_excerpt() {
        let g = legacy_grid(Some(120.0), Some(250.0), Some(0.7), 400.0).unwrap();
        assert!(close(g.segments[0].origin_ms, 100_250.0));
        assert!(close(60_000.0 / g.segments[0].bpm, 500.0));
        assert_eq!(g.confidence, 0.7);
        assert!(legacy_grid(Some(120.0), Some(250.0), Some(0.1), 400.0).is_none());
        assert!(legacy_grid(Some(120.0), None, Some(0.9), 400.0).is_none());
        assert!(legacy_grid(None, Some(1.0), Some(0.9), 400.0).is_none());
    }

    #[test]
    fn phase_wraps_both_ways() {
        let g = BeatGrid::constant(120.0, 10_000.0, 1.0, "t");
        assert!(close(g.phase_ms(10_200.0), 200.0));
        assert!(close(g.phase_ms(9_900.0), 400.0));
        assert!(g.phase_ms(12_000.0).abs() < 1e-6 || close(g.phase_ms(12_000.0), 500.0));
    }

    #[test]
    fn residual_takes_the_short_way_round() {
        assert!(close(residual(0.1, 0.0, 0.5), 0.1));
        assert!(close(residual(0.0, 0.1, 0.5), -0.1));
        assert!(close(residual(0.45, 0.05, 0.5), -0.1));
        assert!(close(residual(0.05, 0.45, 0.5), 0.1));
    }

    #[test]
    fn snap_to_the_previous_beat() {
        let g = BeatGrid::constant(120.0, 10_000.0, 1.0, "t");
        assert!(close(g.snap_beat_ms(12_300.0, Snap::Floor), 12_000.0));
        assert!(close(g.snap_beat_ms(12_300.0, Snap::Nearest), 12_500.0));
        assert!(close(g.snap_beat_ms(12_300.0, Snap::Ceil), 12_500.0));
    }

    #[test]
    fn downbeats_and_phrases() {
        let mut g = BeatGrid::constant(120.0, 0.0, 1.0, "t");
        g.downbeat_phase = Some(1); // beat 1 is "the one"
        let ticks = g.ticks(0.0, 40_000.0, 8, 10_000);
        let downs: Vec<_> = ticks.iter().filter(|t| t.is_downbeat).map(|t| t.t_ms).collect();
        assert_eq!(downs[0], 500.0);
        assert_eq!(downs[1], 2500.0);
        // 8-bar phrase = 32 beats = 16 s, anchored on the first downbeat
        let phrases: Vec<_> = ticks.iter().filter(|t| t.is_phrase).map(|t| t.t_ms).collect();
        assert_eq!(phrases, [500.0 - 16_000.0 + 16_000.0, 16_500.0, 32_500.0]);
        assert!(close(g.snap_bar_ms(3_000.0, Snap::Floor), 2_500.0));
        assert!(close(g.snap_phrase_ms(20_000.0, 8, Snap::Floor), 16_500.0));
        assert!(close(g.snap_phrase_ms(20_000.0, 8, Snap::Ceil), 32_500.0));
    }

    #[test]
    fn variable_grid_follows_segments() {
        let g = BeatGrid::variable(
            vec![
                TempoSegment { origin_ms: 0.0, bpm: 120.0, beats: Some(8) }, // 4 s
                TempoSegment { origin_ms: 4_000.0, bpm: 150.0, beats: None },
            ],
            0.9,
            "t",
        );
        assert!(close(g.beat_time_ms(8.0), 4_000.0));
        assert!(close(g.beat_time_ms(9.0), 4_400.0));
        assert!(close(g.beat_pos(4_400.0), 9.0));
        assert!(close(g.period_ms_at(5_000.0), 400.0));
        let b = g.bpm();
        assert!(b > 120.0 && b < 150.0);
        // round trip
        for t in [0.0, 1_234.0, 3_999.0, 4_001.0, 9_000.0] {
            assert!(close(g.beat_time_ms(g.beat_pos(t)), t), "{t}");
        }
    }

    #[test]
    fn before_the_origin_extrapolates() {
        let g = BeatGrid::constant(60.0, 500.0, 1.0, "t");
        assert!(close(g.beat_time_ms(-1.0), -500.0));
        assert!(close(g.phase_ms(0.0), 500.0));
    }
}
