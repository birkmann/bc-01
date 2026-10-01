//! DJ set timing and transition scoring (port of `services/playlists/setmath.py`).
//!
//! Tracks are pitched, so a slot's played time is its cue range divided by the tempo
//! multiplier; consecutive tracks overlap during a blend; and with key-lock off, pitching moves
//! the key, so the planner scores the *effective* key.

use bc_types::analysis::{Compatibility, Verdict};
use bc_types::sets::{SetSummaryOut, TransitionOut};

use crate::camelot::{bpm_compatibility, key_compatibility, pitch_shift_semitones, transpose, DEFAULT_BPM_TOLERANCE};

/// Python-style `round(x, n)` (banker's) for the few places the legacy output depends on it.
pub fn round_to(x: f64, digits: i32) -> f64 {
    let f = 10f64.powi(digits);
    (x * f).round_ties_even() / f
}

#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub index: usize,
    pub track_id: Option<i64>,
    pub title: String,
    pub artist: String,
    pub duration_ms: Option<i64>,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub energy: Option<i64>,
    pub cue_in_ms: Option<i64>,
    pub cue_out_ms: Option<i64>,
    pub tempo_adjust_pct: f64,
    pub key_lock: bool,
    pub transition_type: Option<String>,
    pub transition_beats: Option<i64>,
    pub transition_notes: Option<String>,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            index: 0,
            track_id: None,
            title: String::new(),
            artist: String::new(),
            duration_ms: None,
            bpm: None,
            camelot: None,
            energy: None,
            cue_in_ms: None,
            cue_out_ms: None,
            tempo_adjust_pct: 0.0,
            key_lock: true,
            transition_type: None,
            transition_beats: None,
            transition_notes: None,
        }
    }
}

impl Slot {
    pub fn effective_bpm(&self) -> Option<f64> {
        self.bpm.map(|b| round_to(b * (1.0 + self.tempo_adjust_pct / 100.0), 2))
    }

    /// Key after pitching: unchanged with key-lock on, transposed otherwise.
    pub fn effective_camelot(&self) -> Option<String> {
        let c = self.camelot.as_deref()?;
        if self.key_lock || self.tempo_adjust_pct == 0.0 {
            return Some(c.to_string());
        }
        transpose(Some(c), pitch_shift_semitones(self.tempo_adjust_pct))
    }

    /// Time this slot occupies, honouring cue points and pitch (truncated like `int()`).
    pub fn played_ms(&self) -> i64 {
        let Some(dur) = self.duration_ms else { return 0 };
        let start = self.cue_in_ms.unwrap_or(0);
        let end = self.cue_out_ms.unwrap_or(dur);
        let span = (end - start).max(0) as f64;
        (span / (1.0 + self.tempo_adjust_pct / 100.0)) as i64
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub from_index: usize,
    pub to_index: usize,
    pub key: Compatibility,
    pub bpm: Compatibility,
    pub overlap_ms: i64,
    pub tempo_delta_pct: f64,
}

impl Transition {
    pub fn ok(&self) -> bool {
        self.key.ok() && self.bpm.ok()
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SetSummary {
    pub total_ms: i64,
    pub played_ms: i64,
    pub overlap_ms: i64,
    pub track_count: usize,
    pub avg_bpm: Option<f64>,
    pub bpm_range: Option<(f64, f64)>,
    pub keys: Vec<String>,
    pub transitions: Vec<Transition>,
    pub energy_curve: Vec<Option<i64>>,
    pub target_ms: Option<i64>,
}

impl SetSummary {
    pub fn over_target_ms(&self) -> Option<i64> {
        self.target_ms.map(|t| self.total_ms - t)
    }
    pub fn problem_transitions(&self) -> usize {
        self.transitions.iter().filter(|t| !t.ok()).count()
    }
}

/// Milliseconds spanned by `beats` at `bpm`.
pub fn overlap_ms(beats: Option<i64>, bpm: Option<f64>) -> i64 {
    match (beats, bpm) {
        (Some(b), Some(bpm)) if b != 0 && bpm > 0.0 => (b as f64 * 60_000.0 / bpm) as i64,
        _ => 0,
    }
}

/// Score the join from `a` into `b`, using effective key and tempo.
pub fn score_transition(a: &Slot, b: &Slot) -> Transition {
    let (ea, eb) = (a.effective_bpm(), b.effective_bpm());
    let key = key_compatibility(a.effective_camelot().as_deref(), b.effective_camelot().as_deref());
    let bpm = bpm_compatibility(ea, eb, DEFAULT_BPM_TOLERANCE);
    let delta = match (ea, eb) {
        (Some(x), Some(y)) if x != 0.0 && y != 0.0 => round_to((y - x) / x * 100.0, 2),
        _ => 0.0,
    };
    Transition {
        from_index: a.index,
        to_index: b.index,
        key,
        bpm,
        overlap_ms: overlap_ms(b.transition_beats, eb),
        tempo_delta_pct: delta,
    }
}

/// Total duration, transitions, and the energy curve.
pub fn summarise(slots: &[Slot], target_minutes: Option<i64>) -> SetSummary {
    let mut s = SetSummary { track_count: slots.len(), ..Default::default() };
    s.target_ms = target_minutes.filter(|m| *m != 0).map(|m| m * 60_000);
    if slots.is_empty() {
        return s;
    }
    s.played_ms = slots.iter().map(Slot::played_ms).sum();
    s.energy_curve = slots.iter().map(|x| x.energy).collect();
    s.keys = slots.iter().map(|x| x.effective_camelot().unwrap_or_else(|| "?".into())).collect();
    for w in slots.windows(2) {
        let t = score_transition(&w[0], &w[1]);
        s.overlap_ms += t.overlap_ms;
        s.transitions.push(t);
    }
    let tempos: Vec<f64> = slots.iter().filter_map(Slot::effective_bpm).filter(|b| *b != 0.0).collect();
    if !tempos.is_empty() {
        s.avg_bpm = Some(round_to(tempos.iter().sum::<f64>() / tempos.len() as f64, 1));
        let min = tempos.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = tempos.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        s.bpm_range = Some((min, max));
    }
    s.total_ms = (s.played_ms - s.overlap_ms).max(0);
    s
}

/// Cumulative start offset of each slot, accounting for overlaps.
pub fn start_times(slots: &[Slot]) -> Vec<i64> {
    let mut out = Vec::with_capacity(slots.len());
    let mut clock = 0i64;
    for (i, slot) in slots.iter().enumerate() {
        if i > 0 {
            clock -= overlap_ms(slot.transition_beats, slot.effective_bpm());
        }
        out.push(clock.max(0));
        clock = clock.max(0) + slot.played_ms();
    }
    out
}

// --- DTO conversion ----------------------------------------------------------------------

pub fn summary_out(s: &SetSummary) -> SetSummaryOut {
    SetSummaryOut {
        total_ms: s.total_ms,
        played_ms: s.played_ms,
        overlap_ms: s.overlap_ms,
        track_count: s.track_count as i64,
        avg_bpm: s.avg_bpm,
        bpm_min: s.bpm_range.map(|r| r.0),
        bpm_max: s.bpm_range.map(|r| r.1),
        target_ms: s.target_ms,
        over_target_ms: s.over_target_ms(),
        problem_transitions: s.problem_transitions() as i64,
    }
}

pub fn transition_out(t: &Transition) -> TransitionOut {
    TransitionOut {
        from_index: t.from_index,
        to_index: t.to_index,
        key_verdict: t.key.verdict,
        key_reason: t.key.reason.clone(),
        key_score: round_to(t.key.score, 3),
        bpm_verdict: t.bpm.verdict,
        bpm_reason: t.bpm.reason.clone(),
        bpm_score: round_to(t.bpm.score, 3),
        tempo_delta_pct: t.tempo_delta_pct,
        overlap_ms: t.overlap_ms,
        ok: t.ok(),
    }
}

/// Verdict of an arbitrary pair, for the UI.
pub fn join_verdict(a: &Slot, b: &Slot) -> bc_types::sets::JoinVerdict {
    let t = score_transition(a, b);
    let ok = t.ok();
    bc_types::sets::JoinVerdict { key: t.key, bpm: t.bpm, ok }
}

#[allow(unused)]
fn _v(_: Verdict) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(f: impl FnOnce(&mut Slot)) -> Slot {
        let mut s = Slot {
            index: 0,
            track_id: Some(1),
            duration_ms: Some(300_000),
            bpm: Some(128.0),
            camelot: Some("8A".into()),
            ..Default::default()
        };
        f(&mut s);
        s
    }

    #[test]
    fn played_time_uses_the_full_track_by_default() {
        assert_eq!(slot(|_| {}).played_ms(), 300_000);
    }

    #[test]
    fn cue_points_shorten_a_slot() {
        let s = slot(|s| {
            s.cue_in_ms = Some(30_000);
            s.cue_out_ms = Some(210_000);
        });
        assert_eq!(s.played_ms(), 180_000);
    }

    #[test]
    fn pitching_up_makes_a_slot_finish_sooner() {
        assert!(slot(|s| s.tempo_adjust_pct = 4.0).played_ms() < 300_000);
        assert!(slot(|s| s.tempo_adjust_pct = -4.0).played_ms() > 300_000);
    }

    #[test]
    fn effective_bpm_follows_the_pitch() {
        assert_eq!(slot(|s| s.tempo_adjust_pct = 0.0).effective_bpm(), Some(128.0));
        let b = slot(|s| s.tempo_adjust_pct = 5.0).effective_bpm().unwrap();
        assert!((b - 134.4).abs() < 0.1);
    }

    #[test]
    fn key_lock_keeps_the_key_but_pitching_moves_it() {
        let locked = slot(|s| {
            s.tempo_adjust_pct = 6.0;
            s.key_lock = true;
        });
        assert_eq!(locked.effective_camelot().as_deref(), Some("8A"));
        let shifted = slot(|s| {
            s.tempo_adjust_pct = 6.0;
            s.key_lock = false;
        });
        assert_ne!(shifted.effective_camelot().as_deref(), Some("8A"));
        assert_eq!(shifted.effective_camelot().as_deref(), Some("3A"));
    }

    #[test]
    fn small_pitch_changes_do_not_move_the_key() {
        let s = slot(|s| {
            s.tempo_adjust_pct = 1.0;
            s.key_lock = false;
        });
        assert_eq!(s.effective_camelot().as_deref(), Some("8A"));
    }

    #[test]
    fn empty_set_summarises_to_zero() {
        let s = summarise(&[], None);
        assert_eq!(s.total_ms, 0);
        assert!(s.transitions.is_empty());
    }

    #[test]
    fn total_subtracts_overlap() {
        let a = slot(|s| s.index = 0);
        let b = slot(|s| {
            s.index = 1;
            s.transition_beats = Some(32);
        });
        let s = summarise(&[a, b], None);
        assert_eq!(s.played_ms, 600_000);
        assert!(s.overlap_ms > 0);
        assert_eq!(s.total_ms, s.played_ms - s.overlap_ms);
    }

    #[test]
    fn overlap_is_computed_from_beats_and_tempo() {
        assert_eq!(overlap_ms(Some(32), Some(128.0)), 15_000);
        assert_eq!(overlap_ms(None, Some(128.0)), 0);
        assert_eq!(overlap_ms(Some(32), None), 0);
    }

    #[test]
    fn average_and_range_reflect_effective_tempo() {
        let s = summarise(
            &[
                slot(|s| {
                    s.index = 0;
                    s.bpm = Some(128.0)
                }),
                slot(|s| {
                    s.index = 1;
                    s.bpm = Some(132.0);
                    s.tempo_adjust_pct = -3.0
                }),
            ],
            None,
        );
        let r = s.bpm_range.unwrap();
        assert!(s.avg_bpm.is_some());
        assert!(r.0 < r.1);
    }

    #[test]
    fn target_overrun_is_reported() {
        let s = summarise(&[slot(|s| s.index = 0)], Some(2));
        assert!((s.over_target_ms().unwrap() - 180_000).abs() <= 1000);
    }

    #[test]
    fn no_target_means_no_overrun() {
        assert_eq!(summarise(&[slot(|_| {})], None).over_target_ms(), None);
    }

    #[test]
    fn matching_key_and_tempo_is_a_clean_transition() {
        let t = score_transition(&slot(|s| s.index = 0), &slot(|s| s.index = 1));
        assert!(t.ok());
        assert_eq!(t.key.verdict, Verdict::Perfect);
    }

    #[test]
    fn clashing_key_is_flagged() {
        let t = score_transition(
            &slot(|s| {
                s.index = 0;
                s.camelot = Some("8A".into())
            }),
            &slot(|s| {
                s.index = 1;
                s.camelot = Some("2A".into())
            }),
        );
        assert!(!t.ok());
        assert_eq!(t.key.verdict, Verdict::Clash);
    }

    #[test]
    fn distant_tempo_is_flagged() {
        let t = score_transition(
            &slot(|s| {
                s.index = 0;
                s.bpm = Some(128.0)
            }),
            &slot(|s| {
                s.index = 1;
                s.bpm = Some(100.0)
            }),
        );
        assert!(!t.ok());
    }

    #[test]
    fn pitching_can_rescue_a_tempo_mismatch() {
        let raw = score_transition(
            &slot(|s| {
                s.index = 0;
                s.bpm = Some(128.0)
            }),
            &slot(|s| {
                s.index = 1;
                s.bpm = Some(120.0)
            }),
        );
        assert!(!raw.bpm.ok());
        let pitched = score_transition(
            &slot(|s| {
                s.index = 0;
                s.bpm = Some(128.0)
            }),
            &slot(|s| {
                s.index = 1;
                s.bpm = Some(120.0);
                s.tempo_adjust_pct = 6.0
            }),
        );
        assert!(pitched.bpm.ok());
    }

    #[test]
    fn problem_transitions_are_counted() {
        let s = summarise(
            &[
                slot(|s| {
                    s.index = 0;
                    s.camelot = Some("8A".into())
                }),
                slot(|s| {
                    s.index = 1;
                    s.camelot = Some("9A".into())
                }),
                slot(|s| {
                    s.index = 2;
                    s.camelot = Some("2A".into())
                }),
            ],
            None,
        );
        assert_eq!(s.problem_transitions(), 1);
    }

    #[test]
    fn tempo_delta_is_reported_as_a_percentage() {
        let t = score_transition(
            &slot(|s| {
                s.index = 0;
                s.bpm = Some(128.0)
            }),
            &slot(|s| {
                s.index = 1;
                s.bpm = Some(130.0)
            }),
        );
        assert!((t.tempo_delta_pct - 1.56).abs() < 0.05);
    }

    #[test]
    fn start_times_accumulate() {
        let starts = start_times(&[
            slot(|s| s.index = 0),
            slot(|s| s.index = 1),
            slot(|s| s.index = 2),
        ]);
        assert_eq!(starts, [0, 300_000, 600_000]);
    }

    #[test]
    fn overlap_pulls_later_slots_earlier() {
        let plain = start_times(&[slot(|s| s.index = 0), slot(|s| s.index = 1)]);
        let blended = start_times(&[
            slot(|s| s.index = 0),
            slot(|s| {
                s.index = 1;
                s.transition_beats = Some(32)
            }),
        ]);
        assert!(blended[1] < plain[1]);
    }

    #[test]
    fn slot_without_analysis_does_not_break_the_maths() {
        let unknown = Slot { index: 0, ..Default::default() };
        let s = summarise(&[unknown, slot(|s| s.index = 1)], None);
        assert!(s.total_ms >= 0);
        assert_eq!(s.transitions[0].key.verdict, Verdict::Clash);
        assert!(!s.transitions[0].ok());
    }
}
