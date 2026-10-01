//! Downbeats and phrases from beat-synchronous band accents.

use crate::features::FrameSeries;

#[derive(Debug, Clone, PartialEq)]
pub struct DownbeatResult {
    /// Index (0..3) of the first beat of the track that is a downbeat.
    pub phase: u8,
    /// 0..1: how clearly one phase wins.
    pub confidence: f64,
}

fn z(v: &[f64; 4]) -> [f64; 4] {
    let mean = v.iter().sum::<f64>() / 4.0;
    let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / 4.0).sqrt().max(1e-9);
    [(v[0] - mean) / sd, (v[1] - mean) / sd, (v[2] - mean) / sd, (v[3] - mean) / sd]
}

fn max_in(v: &[f32], c: isize, r: isize) -> f64 {
    let lo = (c - r).max(0) as usize;
    let hi = ((c + r) as usize).min(v.len().saturating_sub(1));
    if lo > hi {
        return 0.0;
    }
    v[lo..=hi].iter().cloned().fold(0.0f32, f32::max) as f64
}

/// Find the downbeat phase among `beats_frames` (frame indices of tracked beats).
pub fn find_downbeats(beats_frames: &[f64], s: &FrameSeries) -> Option<DownbeatResult> {
    if beats_frames.len() < 16 || s.low_flux.is_empty() {
        return None;
    }
    let r = 3isize;
    let mut a_low = [0.0f64; 4];
    let mut a_mid = [0.0f64; 4];
    let mut a_all = [0.0f64; 4];
    let mut rise = [0.0f64; 4];
    let mut cnt = [0.0f64; 4];
    for (i, bf) in beats_frames.iter().enumerate() {
        let c = bf.round() as isize;
        let p = i % 4;
        a_low[p] += max_in(&s.low_flux, c, r);
        a_mid[p] += max_in(&s.mid_flux, c, r);
        a_all[p] += max_in(&s.onset, c, r);
        // low+mid energy just after the beat vs just before (arrival of a bar)
        let after = mean_at(&s.e_low, c, c + 6) + mean_at(&s.e_mid, c, c + 6);
        let before = mean_at(&s.e_low, c - 8, c - 1) + mean_at(&s.e_mid, c - 8, c - 1);
        rise[p] += after - before;
        cnt[p] += 1.0;
    }
    for p in 0..4 {
        let n = cnt[p].max(1.0);
        a_low[p] /= n;
        a_mid[p] /= n;
        a_all[p] /= n;
        rise[p] /= n;
    }
    let (zl, zm, za, zr) = (z(&a_low), z(&a_mid), z(&a_all), z(&rise));
    let mut score = [0.0f64; 4];
    for p in 0..4 {
        // kick/bass accent on the one, total flux on the one, bar-arrival energy, and claps
        // (mid band) on 2 and 4 rather than on 1 and 3
        let clap = (zm[(p + 1) % 4] + zm[(p + 3) % 4] - zm[p] - zm[(p + 2) % 4]) / 2.0;
        score[p] = zl[p] + 0.5 * za[p] + 0.5 * zr[p] + 0.5 * clap;
    }
    let (mut best, mut second) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    let mut bp = 0usize;
    for (p, v) in score.iter().enumerate() {
        if *v > best {
            second = best;
            best = *v;
            bp = p;
        } else if *v > second {
            second = *v;
        }
    }
    let conf = ((best - second) / 2.0).clamp(0.0, 1.0);
    Some(DownbeatResult { phase: bp as u8, confidence: conf })
}

fn mean_at(v: &[f32], lo: isize, hi: isize) -> f64 {
    let lo = lo.max(0) as usize;
    let hi = (hi.max(0) as usize).min(v.len().saturating_sub(1));
    if lo > hi {
        return 0.0;
    }
    v[lo..=hi].iter().map(|x| *x as f64).sum::<f64>() / (hi - lo + 1) as f64
}

/// Phrase starts (ms), snapped to an 8-bar lattice anchored by novelty.
///
/// `bar_frames`: frame index of each bar start (downbeats). Returns bar start times through
/// `frame_time_ms`.
pub fn find_phrases(bar_frames: &[f64], s: &FrameSeries, frame_time_ms: impl Fn(f64) -> f64) -> Vec<f64> {
    let nb = bar_frames.len();
    if nb < 16 {
        return vec![];
    }
    // per-bar features: mean of low/mid/high energy and onset density
    let mut feats: Vec<[f64; 4]> = Vec::with_capacity(nb - 1);
    for w in bar_frames.windows(2) {
        let (a, b) = (w[0].round() as isize, w[1].round() as isize - 1);
        feats.push([
            mean_at(&s.e_low, a, b),
            mean_at(&s.e_mid, a, b),
            mean_at(&s.e_high, a, b),
            mean_at(&s.onset, a, b),
        ]);
    }
    let n = feats.len();
    // z-score per dimension
    for d in 0..4 {
        let mean = feats.iter().map(|f| f[d]).sum::<f64>() / n as f64;
        let sd = (feats.iter().map(|f| (f[d] - mean).powi(2)).sum::<f64>() / n as f64).sqrt().max(1e-9);
        for f in feats.iter_mut() {
            f[d] = (f[d] - mean) / sd;
        }
    }
    const L: usize = 8;
    let mut nov = vec![0.0f64; n];
    for b in 1..n {
        let lo = b.saturating_sub(L);
        let hi = (b + L).min(n);
        if b - lo < 2 || hi - b < 2 {
            continue;
        }
        let mut before = [0.0; 4];
        let mut after = [0.0; 4];
        for d in 0..4 {
            before[d] = feats[lo..b].iter().map(|f| f[d]).sum::<f64>() / (b - lo) as f64;
            after[d] = feats[b..hi].iter().map(|f| f[d]).sum::<f64>() / (hi - b) as f64;
        }
        nov[b] = (0..4).map(|d| (after[d] - before[d]).powi(2)).sum::<f64>().sqrt();
    }
    // lattice offset (mod 8 bars) maximising novelty
    let mut off_score = [0.0f64; 8];
    for b in 0..n {
        off_score[b % 8] += nov[b];
    }
    let off = off_score.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|x| x.0).unwrap_or(0);
    let mean = nov.iter().sum::<f64>() / n as f64;
    let sd = (nov.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
    let thr = mean + 0.5 * sd;
    let mut out: Vec<f64> = Vec::new();
    // the anchor: first lattice bar
    out.push(frame_time_ms(bar_frames[off]));
    for b in (off + 8..n).step_by(8) {
        if nov[b] >= thr {
            out.push(frame_time_ms(bar_frames[b]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(n: usize) -> FrameSeries {
        FrameSeries {
            fps: 172.0,
            onset: vec![0.1; n],
            low_flux: vec![0.1; n],
            mid_flux: vec![0.1; n],
            e_low: vec![-6.0; n],
            e_mid: vec![-6.0; n],
            e_high: vec![-6.0; n],
            centroid: vec![1000.0; n],
        }
    }

    #[test]
    fn kick_accent_on_the_one() {
        // beats every 80 frames; beat 2 mod 4 has the strongest kick
        let mut s = series(80 * 80);
        let beats: Vec<f64> = (0..70).map(|i| 40.0 + i as f64 * 80.0).collect();
        for (i, b) in beats.iter().enumerate() {
            let v = if i % 4 == 2 { 2.0 } else { 0.8 };
            s.low_flux[*b as usize] = v;
            s.onset[*b as usize] = v;
        }
        let r = find_downbeats(&beats, &s).unwrap();
        assert_eq!(r.phase, 2);
        assert!(r.confidence > 0.3);
    }

    #[test]
    fn phrases_follow_energy_changes_on_the_lattice() {
        // 8-bar blocks alternating loud/quiet; bars of 4 beats * 80 frames
        let bar = 320usize;
        let nbars = 64usize;
        let mut s = series(bar * nbars);
        for b in 0..nbars {
            let loud = (b / 8) % 2 == 1;
            for f in b * bar..(b + 1) * bar {
                s.e_low[f] = if loud { -2.0 } else { -6.0 };
                s.e_mid[f] = if loud { -3.0 } else { -6.0 };
                s.onset[f] = if loud { 0.6 } else { 0.1 };
            }
        }
        let bars: Vec<f64> = (0..nbars).map(|b| (b * bar) as f64).collect();
        let ph = find_phrases(&bars, &s, |f| f / 172.0 * 1000.0);
        assert!(ph.len() >= 3, "{ph:?}");
        // all phrase starts sit on an 8-bar multiple
        for p in &ph {
            let bar_idx = (p / 1000.0 * 172.0 / bar as f64).round() as usize;
            assert_eq!(bar_idx % 8, 0, "{p}");
        }
    }
}
