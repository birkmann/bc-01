//! Arrange a pool of tracks into a set that mixes well (port of `playlists/automix.py`).
//!
//! Beam search (5 wide, 8 branches) over a memoised edge score, then adjacent-swap and Or-opt
//! polish. Fully deterministic: every tie breaks on track id.

use std::collections::{HashMap, HashSet};

use bc_types::analysis::Verdict;

use crate::camelot::{bpm_compatibility, key_compatibility};

/// The most tracks one automix call will arrange.
pub const AUTOMIX_MAX: usize = 200;
pub const BEAM_WIDTH: usize = 5;
pub const BRANCH: usize = 8;
pub const W_KEY: f64 = 0.50;
pub const W_BPM: f64 = 0.35;
pub const W_ARC: f64 = 0.15;
pub const PEN_SAME_ARTIST: f64 = 0.10;
pub const EDGE_BPM_TOLERANCE: f64 = 0.08;
pub const NEUTRAL: f64 = 0.5;
pub const TRANSITION_TYPES: [&str; 5] = ["cut", "blend", "bass_swap", "filter", "echo_out"];
pub const ARC_PEAK_AT: f64 = 0.7;
const SWAP_PASSES: usize = 3;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MixTrack {
    pub track_id: i64,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub duration_ms: Option<i64>,
    pub artist_id: Option<i64>,
}

impl MixTrack {
    pub fn new(track_id: i64) -> Self {
        Self { track_id, ..Default::default() }
    }
}

/// Transition defaults for one ordered slot (the join *into* it).
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedSlot {
    pub track_id: i64,
    pub transition_type: Option<&'static str>,
    pub transition_beats: Option<i64>,
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

fn percentile(ordered: &[f64], q: f64) -> Option<f64> {
    if ordered.is_empty() {
        return None;
    }
    Some(ordered[((ordered.len() as f64 * q) as usize).min(ordered.len() - 1)])
}

struct Arc {
    n: usize,
    anchor: Option<f64>,
    lo: Option<f64>,
    hi: Option<f64>,
    end: Option<f64>,
}

impl Arc {
    fn new(tracks: &[MixTrack], n: usize) -> Self {
        let mut bpms: Vec<f64> = tracks.iter().filter_map(|t| t.bpm).filter(|b| *b > 0.0).collect();
        if bpms.is_empty() {
            return Arc { n, anchor: None, lo: None, hi: None, end: None };
        }
        let anchor = median(&mut bpms);
        let mut a = Arc { n, anchor: Some(anchor), lo: None, hi: None, end: None };
        let mut folded: Vec<f64> = bpms.iter().filter_map(|b| a.fold(Some(*b))).collect();
        folded.sort_by(|x, y| x.total_cmp(y));
        a.lo = percentile(&folded, 0.25);
        a.hi = percentile(&folded, 0.90);
        a.end = percentile(&folded, 0.60);
        a
    }

    fn fold(&self, bpm: Option<f64>) -> Option<f64> {
        let mut b = bpm?;
        let anchor = self.anchor?;
        if b <= 0.0 {
            return None;
        }
        while b > anchor * 1.5 {
            b /= 2.0;
        }
        while b < anchor / 1.5 {
            b *= 2.0;
        }
        Some(b)
    }

    fn target(&self, index: usize) -> Option<f64> {
        let (lo, hi, end) = (self.lo?, self.hi?, self.end?);
        self.anchor?;
        let x = if self.n > 1 { index as f64 / (self.n - 1) as f64 } else { 0.0 };
        Some(if x <= ARC_PEAK_AT {
            lo + (hi - lo) * (x / ARC_PEAK_AT)
        } else {
            hi - (hi - end) * ((x - ARC_PEAK_AT) / (1.0 - ARC_PEAK_AT))
        })
    }

    fn term(&self, track: &MixTrack, index: usize) -> f64 {
        let (Some(target), Some(folded), Some(anchor)) =
            (self.target(index), self.fold(track.bpm), self.anchor)
        else {
            return NEUTRAL;
        };
        1.0 - ((folded - target).abs() / (0.15 * anchor)).min(1.0)
    }
}

fn edge_score(a: &MixTrack, b: &MixTrack) -> f64 {
    let key = match (&a.camelot, &b.camelot) {
        (Some(x), Some(y)) if !x.is_empty() && !y.is_empty() => {
            key_compatibility(Some(x), Some(y)).score
        }
        _ => NEUTRAL,
    };
    let bpm = match (a.bpm, b.bpm) {
        (Some(x), Some(y)) if x != 0.0 && y != 0.0 => {
            bpm_compatibility(Some(x), Some(y), EDGE_BPM_TOLERANCE).score
        }
        _ => NEUTRAL,
    };
    let mut total = W_KEY * key + W_BPM * bpm;
    if a.artist_id.is_some() && a.artist_id == b.artist_id {
        total -= PEN_SAME_ARTIST;
    }
    total
}

struct Ctx<'a> {
    by_id: HashMap<i64, &'a MixTrack>,
    arc: Arc,
    anchor: Option<&'a MixTrack>,
    edges: std::cell::RefCell<HashMap<(i64, i64), f64>>,
}

impl<'a> Ctx<'a> {
    fn edge(&self, a: &MixTrack, b: &MixTrack) -> f64 {
        let key = (a.track_id, b.track_id);
        if let Some(v) = self.edges.borrow().get(&key) {
            return *v;
        }
        let v = edge_score(a, b);
        self.edges.borrow_mut().insert(key, v);
        v
    }
    fn step(&self, prev: Option<&MixTrack>, cand: &MixTrack, index: usize) -> f64 {
        let base = prev.map_or(0.0, |p| self.edge(p, cand));
        base + W_ARC * self.arc.term(cand, index)
    }
    fn window_score(&self, seq: &[i64], lo: isize, hi: isize) -> f64 {
        let mut total = 0.0;
        let a = lo.max(0) as usize;
        let b = (hi.max(0) as usize).min(seq.len());
        for i in a..b {
            let prev = if i > 0 { Some(self.by_id[&seq[i - 1]]) } else { self.anchor };
            total += self.step(prev, self.by_id[&seq[i]], i);
        }
        total
    }
    fn edge_delta(&self, seq: &[i64], i: usize, seg: usize, k: usize) -> f64 {
        let moved_first = self.by_id[&seq[i]];
        let moved_last = self.by_id[&seq[i + seg - 1]];
        let before_gap = if i > 0 { Some(self.by_id[&seq[i - 1]]) } else { self.anchor };
        let after_gap = if i + seg < seq.len() { Some(self.by_id[&seq[i + seg]]) } else { None };
        let mut rest: Vec<i64> = seq[..i].to_vec();
        rest.extend_from_slice(&seq[i + seg..]);
        let new_prev = if k > 0 { Some(self.by_id[&rest[k - 1]]) } else { self.anchor };
        let new_next = if k < rest.len() { Some(self.by_id[&rest[k]]) } else { None };

        let mut removed = 0.0;
        if let Some(b) = before_gap {
            removed += self.edge(b, moved_first);
        }
        if let Some(a) = after_gap {
            removed += self.edge(moved_last, a);
        }
        if let (Some(p), Some(n)) = (new_prev, new_next) {
            removed += self.edge(p, n);
        }
        let mut added = 0.0;
        if let (Some(b), Some(a)) = (before_gap, after_gap) {
            added += self.edge(b, a);
        }
        if let Some(p) = new_prev {
            added += self.edge(p, moved_first);
        }
        if let Some(n) = new_next {
            added += self.edge(moved_last, n);
        }
        added - removed
    }
}

/// Order `tracks` for flow. `anchor` is the slot the arrangement must mix out of (the tail of a
/// kept set); `start_track_id` pins the opener.
pub fn arrange(tracks: &[MixTrack], anchor: Option<&MixTrack>, start_track_id: Option<i64>) -> Vec<i64> {
    if tracks.is_empty() {
        return vec![];
    }
    let mut tracks: Vec<&MixTrack> = tracks.iter().collect();
    tracks.sort_by_key(|t| t.track_id);
    if tracks.len() == 1 {
        return vec![tracks[0].track_id];
    }
    let n = tracks.len();
    let owned: Vec<MixTrack> = tracks.iter().map(|t| (*t).clone()).collect();
    let ctx = Ctx {
        by_id: owned.iter().map(|t| (t.track_id, t)).collect(),
        arc: Arc::new(&owned, n),
        anchor,
        edges: Default::default(),
    };

    // openers
    let starts: Vec<&MixTrack> = if let Some(s) = start_track_id.filter(|s| ctx.by_id.contains_key(s)) {
        vec![ctx.by_id[&s]]
    } else if let Some(a) = anchor {
        let mut ranked: Vec<&MixTrack> = owned.iter().collect();
        ranked.sort_by(|x, y| {
            ctx.step(Some(a), y, 0)
                .total_cmp(&ctx.step(Some(a), x, 0))
                .then(x.track_id.cmp(&y.track_id))
        });
        ranked.into_iter().take(BEAM_WIDTH).collect()
    } else {
        let target = ctx.arc.target(0);
        let dist = |t: &MixTrack| match (target, ctx.arc.fold(t.bpm)) {
            (Some(tg), Some(f)) => (f - tg).abs(),
            _ => f64::INFINITY,
        };
        let mut v: Vec<&MixTrack> = owned.iter().collect();
        v.sort_by(|x, y| dist(x).total_cmp(&dist(y)).then(x.track_id.cmp(&y.track_id)));
        v.into_iter().take(BEAM_WIDTH).collect()
    };

    // beam
    let mut beam: Vec<(f64, Vec<i64>)> =
        starts.iter().map(|s| (ctx.step(anchor, s, 0), vec![s.track_id])).collect();
    for index in 1..n {
        let mut expanded: Vec<(f64, Vec<i64>)> = Vec::new();
        for (score, seq) in &beam {
            let used: HashSet<i64> = seq.iter().copied().collect();
            let tail = ctx.by_id[seq.last().expect("non-empty beam entry")];
            let mut ranked: Vec<&MixTrack> = owned.iter().filter(|t| !used.contains(&t.track_id)).collect();
            ranked.sort_by(|x, y| {
                ctx.step(Some(tail), y, index)
                    .total_cmp(&ctx.step(Some(tail), x, index))
                    .then(x.track_id.cmp(&y.track_id))
            });
            for cand in ranked.into_iter().take(BRANCH) {
                let mut s2 = seq.clone();
                s2.push(cand.track_id);
                expanded.push((score + ctx.step(Some(tail), cand, index), s2));
            }
        }
        expanded.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        expanded.truncate(BEAM_WIDTH);
        beam = expanded;
    }
    let mut best = beam.swap_remove(0).1;

    // polish: adjacent swaps
    for _ in 0..SWAP_PASSES {
        let mut improved = false;
        for i in 0..best.len() - 1 {
            let before = ctx.window_score(&best, i as isize, i as isize + 3);
            best.swap(i, i + 1);
            let after = ctx.window_score(&best, i as isize, i as isize + 3);
            if after > before + 1e-9 {
                improved = true;
            } else {
                best.swap(i, i + 1);
            }
        }
        if !improved {
            break;
        }
    }

    // polish: Or-opt
    let seq_score = |s: &[i64]| ctx.window_score(s, 0, s.len() as isize);
    let mut best_score = seq_score(&best);
    for _ in 0..n * 2 {
        let mut improved = false;
        'outer: for seg in 1..=2usize {
            if best.len() < seg {
                continue;
            }
            for i in 0..=best.len() - seg {
                let mut rest: Vec<i64> = best[..i].to_vec();
                rest.extend_from_slice(&best[i + seg..]);
                let segment: Vec<i64> = best[i..i + seg].to_vec();
                for k in 0..=rest.len() {
                    if k == i {
                        continue;
                    }
                    if ctx.edge_delta(&best, i, seg, k) <= 1e-9 {
                        continue;
                    }
                    let mut cand: Vec<i64> = rest[..k].to_vec();
                    cand.extend_from_slice(&segment);
                    cand.extend_from_slice(&rest[k..]);
                    let score = seq_score(&cand);
                    if score > best_score + 1e-9 {
                        best = cand;
                        best_score = score;
                        improved = true;
                        break 'outer;
                    }
                }
            }
        }
        if !improved {
            break;
        }
    }
    best
}

/// Default transition per slot: a long blend where the join is clean, a shorter one where only
/// the tempo agrees, a cut otherwise. (Legacy vocabulary: blend / cut.)
pub fn plan_transitions(ordered: &[MixTrack], anchor: Option<&MixTrack>) -> Vec<PlannedSlot> {
    ordered
        .iter()
        .enumerate()
        .map(|(i, cur)| {
            let prev = if i > 0 { Some(&ordered[i - 1]) } else { anchor };
            let Some(prev) = prev else {
                return PlannedSlot { track_id: cur.track_id, transition_type: None, transition_beats: None };
            };
            let key = key_compatibility(prev.camelot.as_deref(), cur.camelot.as_deref());
            let bpm = bpm_compatibility(prev.bpm, cur.bpm, 0.06);
            let (t, b) = if key.ok() && matches!(bpm.verdict, Verdict::Perfect | Verdict::Good) {
                (Some("blend"), Some(32))
            } else if bpm.ok() {
                (Some("blend"), Some(16))
            } else {
                (Some("cut"), None)
            };
            PlannedSlot { track_id: cur.track_id, transition_type: t, transition_beats: b }
        })
        .collect()
}

/// Like [`plan_transitions`] but choosing between the engine's real transition types
/// (`bass_swap`, `filter`, `echo_out`, `blend`, `cut`):
///
/// * perfect/same key and tight tempo: `bass_swap` over 32 beats (swap the low end on the bar);
/// * clean key + good tempo: `blend` 32;
/// * tempo fine but key only risky/clash: `filter` 16 (filter the outgoing away, masks clashes);
/// * key fine, tempo far: `echo_out` 8 (echo tail covers the tempo jump) when tempo within 15 %;
/// * tempo ok only: `blend` 16; otherwise `cut`.
pub fn plan_transitions_rich(ordered: &[MixTrack], anchor: Option<&MixTrack>) -> Vec<PlannedSlot> {
    ordered
        .iter()
        .enumerate()
        .map(|(i, cur)| {
            let prev = if i > 0 { Some(&ordered[i - 1]) } else { anchor };
            let Some(prev) = prev else {
                return PlannedSlot { track_id: cur.track_id, transition_type: None, transition_beats: None };
            };
            let key = key_compatibility(prev.camelot.as_deref(), cur.camelot.as_deref());
            let bpm = bpm_compatibility(prev.bpm, cur.bpm, 0.06);
            let wide = bpm_compatibility(prev.bpm, cur.bpm, 0.15);
            let (t, b) = if key.verdict == Verdict::Perfect && bpm.verdict == Verdict::Perfect {
                ("bass_swap", Some(32))
            } else if key.ok() && matches!(bpm.verdict, Verdict::Perfect | Verdict::Good) {
                ("blend", Some(32))
            } else if bpm.ok() && !key.ok() {
                ("filter", Some(16))
            } else if key.ok() && wide.ok() {
                ("echo_out", Some(8))
            } else if bpm.ok() {
                ("blend", Some(16))
            } else {
                ("cut", None)
            };
            PlannedSlot { track_id: cur.track_id, transition_type: Some(t), transition_beats: b }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(id: i64, bpm: Option<f64>, cam: Option<&str>) -> MixTrack {
        MixTrack { track_id: id, bpm, camelot: cam.map(String::from), ..Default::default() }
    }

    /// Small deterministic shuffle (no rand dep).
    fn shuffled<T: Clone>(v: &[T], mut seed: u64) -> Vec<T> {
        let mut out = v.to_vec();
        for i in (1..out.len()).rev() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            out.swap(i, (seed >> 33) as usize % (i + 1));
        }
        out
    }

    #[test]
    fn same_pool_always_arranges_the_same_way() {
        let tracks: Vec<MixTrack> = (1..=20)
            .map(|i| MixTrack {
                track_id: i,
                bpm: Some(120.0 + i as f64),
                camelot: Some(format!("{}A", (i % 12) + 1)),
                artist_id: Some(i % 3),
                ..Default::default()
            })
            .collect();
        let a = arrange(&tracks, None, None);
        assert_eq!(a, arrange(&shuffled(&tracks, 7), None, None));
        let rev: Vec<_> = tracks.iter().rev().cloned().collect();
        assert_eq!(a, arrange(&rev, None, None));
    }

    #[test]
    fn a_harmonic_chain_is_recovered_without_clashes() {
        let tracks: Vec<_> = (0..5).map(|i| tr(i + 1, Some(128.0), Some(&format!("{}A", 7 + i)))).collect();
        let ids = arrange(&tracks, None, None);
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(sorted, [1, 2, 3, 4, 5]);
        let by: HashMap<i64, &MixTrack> = tracks.iter().map(|t| (t.track_id, t)).collect();
        let ordered: Vec<_> = ids.iter().map(|i| by[i].clone()).collect();
        let planned = plan_transitions(&ordered, None);
        assert!(planned[1..].iter().all(|p| p.transition_type == Some("blend")));
        assert_eq!(planned[0].transition_type, None);
    }

    #[test]
    fn start_track_is_honoured_and_anchor_steers_the_opener() {
        let good = tr(1, Some(128.0), Some("8A"));
        let bad = tr(2, Some(90.0), Some("2B"));
        assert_eq!(arrange(&[good.clone(), bad.clone()], None, Some(2))[0], 2);
        let anchor = tr(99, Some(128.0), Some("8A"));
        assert_eq!(arrange(&[bad, good], Some(&anchor), None)[0], 1);
    }

    #[test]
    fn the_arc_does_not_open_on_the_fastest_track() {
        let tracks: Vec<_> = (120..136).step_by(2).enumerate().map(|(i, b)| tr(i as i64 + 1, Some(b as f64), None)).collect();
        let ids = arrange(&tracks, None, None);
        let by: HashMap<i64, f64> = tracks.iter().map(|t| (t.track_id, t.bpm.unwrap())).collect();
        let bpms: Vec<f64> = ids.iter().map(|i| by[i]).collect();
        let max = bpms.iter().cloned().fold(f64::MIN, f64::max);
        assert_ne!(bpms[0], max, "the peak belongs in the body of the set");
        let peak_at = bpms.iter().position(|b| *b == max).unwrap();
        assert!(peak_at as f64 >= bpms.len() as f64 * 0.3);
    }

    #[test]
    fn half_and_double_time_are_neighbours() {
        let planned = plan_transitions(&[tr(1, Some(87.0), None), tr(2, Some(174.0), None)], None);
        assert_eq!(planned[1].transition_type, Some("blend"));
    }

    #[test]
    fn unanalysed_tracks_do_not_crash_and_stay_deterministic() {
        let tracks = vec![tr(1, None, None), tr(2, Some(128.0), None), tr(3, None, Some("8A")), tr(4, None, None)];
        let rev: Vec<_> = tracks.iter().rev().cloned().collect();
        assert_eq!(arrange(&tracks, None, None), arrange(&rev, None, None));
        assert_eq!(plan_transitions(&tracks, None).len(), 4);
    }

    #[test]
    fn transition_defaults_follow_the_join_quality() {
        let a = tr(1, Some(128.0), Some("8A"));
        let clean = tr(2, Some(128.0), Some("9A"));
        let tempo_only = tr(3, Some(128.0), Some("2B"));
        let far = tr(4, Some(100.0), Some("2B"));
        let p = plan_transitions(&[a, clean, tempo_only, far], None);
        assert_eq!((p[1].transition_type, p[1].transition_beats), (Some("blend"), Some(32)));
        assert_eq!((p[2].transition_type, p[2].transition_beats), (Some("blend"), Some(16)));
        assert_eq!((p[3].transition_type, p[3].transition_beats), (Some("cut"), None));
    }

    #[test]
    fn rich_transitions_pick_real_types() {
        let a = tr(1, Some(128.0), Some("8A"));
        let same = tr(2, Some(128.0), Some("8A"));
        let tempo_only = tr(3, Some(128.0), Some("2B"));
        let far = tr(4, Some(100.0), Some("2B"));
        let p = plan_transitions_rich(&[a, same, tempo_only, far], None);
        assert_eq!(p[1].transition_type, Some("bass_swap"));
        assert_eq!(p[2].transition_type, Some("filter"));
        assert_eq!(p[3].transition_type, Some("cut"));
        assert!(p.iter().filter_map(|s| s.transition_type).all(|t| TRANSITION_TYPES.contains(&t)));
    }

    #[test]
    fn single_and_empty_pools() {
        assert!(arrange(&[], None, None).is_empty());
        assert_eq!(arrange(&[tr(5, Some(128.0), None)], None, None), [5]);
    }

    #[test]
    fn minority_keys_are_woven_in_rather_than_stranded() {
        let bpms = [128.5, 128.6, 128.8, 129.0, 129.6, 129.7, 130.0];
        let mut tracks: Vec<MixTrack> = bpms.iter().enumerate().map(|(i, b)| tr(i as i64 + 1, Some(*b), Some("9A"))).collect();
        tracks.extend([
            tr(8, Some(129.0), Some("9B")),
            tr(9, Some(128.1), Some("9B")),
            tr(10, Some(128.0), Some("8A")),
            tr(11, Some(128.2), Some("8A")),
            tr(12, Some(131.9), Some("10A")),
        ]);
        let ids = arrange(&tracks, None, None);
        let by: HashMap<i64, &MixTrack> = tracks.iter().map(|t| (t.track_id, t)).collect();
        let ordered: Vec<_> = ids.iter().map(|i| by[i]).collect();
        let verdicts: Vec<_> = ordered
            .windows(2)
            .map(|w| key_compatibility(w[0].camelot.as_deref(), w[1].camelot.as_deref()).verdict)
            .collect();
        assert!(!verdicts.contains(&Verdict::Clash), "{:?} -> {:?}", ordered.iter().map(|t| &t.camelot).collect::<Vec<_>>(), verdicts);
    }
}
