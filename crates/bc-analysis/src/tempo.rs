//! Tempo, beat tracking and beat-grid fitting over the whole-track onset envelope.
//!
//! 1. envelope preparation (local-mean subtraction, std normalisation);
//! 2. windowed autocorrelation tempogram averaged over the track, with a log-normal EDM prior
//!    (octave candidates kept); a linear re-ranker then chooses among the metrical relatives
//!    (1/2, 2/3, 3/4, 1, 4/3, 3/2, 2) of the best lag;
//! 3. Ellis dynamic-programming beat tracking at the estimated period, re-run at the refined one;
//! 4. robust linear regression of beat times -> constant grid (sub-0.1 % tempo precision on a
//!    click track), or a segmented variable grid when a block-wise drift test fails.

use bc_types::analysis::{BeatGrid, GridKind, TempoSegment};
use realfft::RealFftPlanner;

pub const BPM_MIN: f64 = 55.0;
pub const BPM_MAX: f64 = 215.0;
/// EDM prior (log-normal in the octave domain). Centre and comb weights were grid-searched
/// against the essentia reference on 1500 library tracks (`tempo-eval`); the score is flat
/// within +-0.2 pt for centres 126-134 BPM.
pub const PRIOR_BPM: f64 = 130.0;
pub const PRIOR_SIGMA_OCT: f64 = 0.85;
/// Weights of the autocorrelation at twice and four times the lag in the candidate score.
pub const COMB2: f64 = 1.0;
pub const COMB4: f64 = 0.0;
pub const TIGHTNESS: f32 = 100.0;

#[derive(Debug, Clone)]
pub struct TempoEstimate {
    pub bpm: f64,
    /// lag in (fractional) frames
    pub lag: f64,
    /// 0..1 : normalised autocorrelation at the chosen lag
    pub strength: f64,
    /// Alternatives incl. octave variants, best first (BPM).
    pub candidates: Vec<f64>,
    /// The metrical relatives the re-ranker chose among (tooling: `tempo-feats`).
    pub hypotheses: Vec<Hypothesis>,
}

/// BPM ratios to the best-scoring lag that the re-ranker may switch to.
pub const RELATIVES: [f64; 7] = [0.5, 2.0 / 3.0, 0.75, 1.0, 4.0 / 3.0, 1.5, 2.0];
/// Re-ranker features: autocorrelation at 1, 2, 3, 4, 1/2, 3/2 and 1/3 of the lag; log2 tempo
/// over the prior centre and its square; relative comb score; one-hot of the relative.
pub const N_HYP_FEAT: usize = 16;
/// Linear re-ranker weights (softmax over a track's hypotheses), fitted on library tracks
/// against the essentia reference.
const RERANK_W: [f64; N_HYP_FEAT] = [
    -0.4845, 4.3808, -1.1044, 2.5547, 0.9422, 0.7186, -1.0699, -1.0193, -3.4163, 2.5671, -0.3424, -0.7995, 1.2286, -1.0570, -1.5399,
    -0.0214,
];

/// One tempo hypothesis.
#[derive(Debug, Clone)]
pub struct Hypothesis {
    pub bpm: f64,
    /// fractional frames
    pub lag: f64,
    /// BPM ratio to the best-scoring lag
    pub ratio: f64,
    pub feat: [f64; N_HYP_FEAT],
}

#[derive(Debug, Clone)]
pub struct TempoResult {
    pub bpm: f64,
    pub confidence: f64,
    pub candidates: Vec<f64>,
    /// refined beat times, ms (frame-time already converted)
    pub beats_ms: Vec<f64>,
    /// the same beats as fractional onset-frame indices
    pub beats_frames: Vec<f64>,
    pub grid: BeatGrid,
    pub inlier_ratio: f64,
}

/// Subtract a local mean, half-wave rectify, scale to unit std.
pub fn prepare_envelope(o: &[f32], fps: f32) -> Vec<f32> {
    let n = o.len();
    if n == 0 {
        return vec![];
    }
    let w = ((fps * 0.4) as usize).max(3) | 1; // ~0.4 s
    let half = w / 2;
    let mut prefix = vec![0.0f64; n + 1];
    for i in 0..n {
        prefix[i + 1] = prefix[i] + o[i] as f64;
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let a = i.saturating_sub(half);
        let b = (i + half + 1).min(n);
        let mean = (prefix[b] - prefix[a]) / (b - a) as f64;
        out.push((o[i] as f64 - mean).max(0.0) as f32);
    }
    let mean = out.iter().map(|v| *v as f64).sum::<f64>() / n as f64;
    let var = out.iter().map(|v| (*v as f64 - mean).powi(2)).sum::<f64>() / n as f64;
    let sd = var.sqrt().max(1e-9);
    for v in &mut out {
        *v = (*v as f64 / sd) as f32;
    }
    out
}

/// Tunables (env overrides exist for calibration runs only: BC_TEMPO_PRIOR, BC_TEMPO_SIGMA,
/// BC_TEMPO_COMB2, BC_TEMPO_COMB4, BC_TEMPO_HALF).
fn tune() -> &'static (f64, f64, f64, f64, f64) {
    static T: std::sync::OnceLock<(f64, f64, f64, f64, f64)> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let g = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        (g("BC_TEMPO_PRIOR", PRIOR_BPM), g("BC_TEMPO_SIGMA", PRIOR_SIGMA_OCT), g("BC_TEMPO_COMB2", COMB2), g("BC_TEMPO_COMB4", COMB4), g("BC_TEMPO_HALF", 0.0))
    })
}

fn prior(bpm: f64) -> f64 {
    let x = (bpm / tune().0).log2() / tune().1;
    (-0.5 * x * x).exp()
}

/// Windowed autocorrelation averaged over the track. Returns acf[0..max_lag] normalised so that
/// acf[0] = 1 per window (windows weighted by their energy).
fn averaged_acf(env: &[f32], max_lag: usize) -> Vec<f64> {
    let n = env.len();
    let win = (1usize << 14).min(n.next_power_of_two().max(256));
    let hop = (win / 2).max(1);
    let fft_n = (win * 2).next_power_of_two();
    let mut planner = RealFftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(fft_n);
    let inv = planner.plan_fft_inverse(fft_n);
    let mut acc = vec![0.0f64; max_lag + 1];
    let mut total_w = 0.0f64;
    let mut start = 0usize;
    loop {
        let end = (start + win).min(n);
        if end <= start + 16 {
            break;
        }
        let mut buf = vec![0.0f32; fft_n];
        buf[..end - start].copy_from_slice(&env[start..end]);
        // centre: the rectified envelope has a positive mean that would lift every lag
        let m = buf[..end - start].iter().sum::<f32>() / (end - start) as f32;
        for v in &mut buf[..end - start] {
            *v -= m;
        }
        let energy: f64 = buf.iter().map(|v| (*v as f64) * (*v as f64)).sum();
        if energy > 1e-6 {
            let mut spec = fwd.make_output_vec();
            fwd.process(&mut buf, &mut spec).expect("fft");
            for c in spec.iter_mut() {
                let p = c.re * c.re + c.im * c.im;
                c.re = p;
                c.im = 0.0;
            }
            let mut out = vec![0.0f32; fft_n];
            inv.process(&mut spec, &mut out).expect("ifft");
            let a0 = out[0].max(1e-9);
            let len = end - start;
            for lag in 0..=max_lag.min(len.saturating_sub(1)) {
                // unbiased
                let unb = len as f64 / (len - lag) as f64;
                acc[lag] += energy.sqrt() * (out[lag] / a0) as f64 * unb;
            }
            total_w += energy.sqrt();
        }
        if end == n {
            break;
        }
        start += hop;
    }
    if total_w > 0.0 {
        for v in &mut acc {
            *v /= total_w;
        }
    }
    acc
}

/// Estimate the tempo of an envelope sampled at `fps`.
pub fn estimate_tempo(env: &[f32], fps: f32) -> Option<TempoEstimate> {
    let fps = fps as f64;
    let lag_min = (60.0 / BPM_MAX * fps).floor() as usize;
    let lag_max = (60.0 / BPM_MIN * fps).ceil() as usize;
    let acf_len = lag_max * 4 + 4;
    if env.len() < acf_len.min(env.len()).max(lag_max * 3) {
        return None;
    }
    let acf = averaged_acf(env, acf_len);
    let at = |l: f64| -> f64 {
        // linear interpolation
        let i = l.floor() as usize;
        if i + 1 >= acf.len() {
            return 0.0;
        }
        let f = l - i as f64;
        acf[i] * (1.0 - f) + acf[i + 1] * f
    };
    // comb-enhanced score with prior
    let score = |lag: f64| -> f64 {
        let bpm = 60.0 * fps / lag;
        let t = tune();
        (at(lag) + t.2 * at(lag * 2.0) + t.3 * at(lag * 4.0) - t.4 * at(lag * 0.5).max(0.0)) * prior(bpm)
    };
    let mut best = (0.0f64, -1.0f64);
    let mut peaks: Vec<(f64, f64)> = Vec::new(); // (lag, score)
    let mut prev2 = 0.0;
    let mut prev1 = 0.0;
    for lag in lag_min.max(2)..=lag_max {
        let s = score(lag as f64);
        if lag > lag_min.max(2) + 1 && prev1 > prev2 && prev1 >= s {
            peaks.push(((lag - 1) as f64, prev1));
        }
        if s > best.1 {
            best = (lag as f64, s);
        }
        prev2 = prev1;
        prev1 = s;
    }
    if best.1 <= 0.0 {
        return None;
    }
    // parabolic refinement around the best
    let refine = |lag: f64| -> f64 {
        let (a, b, c) = (at(lag - 1.0), at(lag), at(lag + 1.0));
        let d = a - 2.0 * b + c;
        if d.abs() < 1e-12 { lag } else { lag + (0.5 * (a - c) / d).clamp(-1.0, 1.0) }
    };
    let lag0 = refine(best.0);
    // metrical relatives of the best lag, each at its own autocorrelation peak (+-2 %)
    let local_peak = |l: f64| -> f64 {
        let (lo, hi) = ((l * 0.98).floor() as usize, (l * 1.02).ceil() as usize);
        let i = (lo..=hi).max_by(|a, b| at(*a as f64).total_cmp(&at(*b as f64))).unwrap_or(l as usize);
        refine(i as f64)
    };
    let s0 = score(lag0).max(1e-9);
    let mut hyps: Vec<Hypothesis> = Vec::with_capacity(RELATIVES.len());
    for (ri, r) in RELATIVES.iter().enumerate() {
        let l = if *r == 1.0 { lag0 } else { local_peak(lag0 / r) };
        if *r != 1.0 && !(lag_min as f64..=lag_max as f64).contains(&l) {
            continue;
        }
        let x = (60.0 * fps / l / PRIOR_BPM).log2();
        let mut f = [0.0f64; N_HYP_FEAT];
        f[..7].copy_from_slice(&[at(l), at(2.0 * l), at(3.0 * l), at(4.0 * l), at(0.5 * l), at(1.5 * l), at(l / 3.0)]);
        f[7] = x;
        f[8] = x * x;
        f[9] = (score(l) - s0) / s0;
        // one-hot of the relative (the best lag itself is the reference)
        let oh = [0, 1, 2, usize::MAX, 3, 4, 5][ri];
        if oh != usize::MAX {
            f[10 + oh] = 1.0;
        }
        hyps.push(Hypothesis { bpm: 60.0 * fps / l, lag: l, ratio: *r, feat: f });
    }
    // re-rank (ties keep the best-scoring lag)
    let mut pick = (lag0, f64::NEG_INFINITY);
    for h in &hyps {
        let s: f64 = h.feat.iter().zip(RERANK_W.iter()).map(|(a, b)| a * b).sum();
        if s > pick.1 || (s == pick.1 && h.ratio == 1.0) {
            pick = (h.lag, s);
        }
    }
    let lag = pick.0;
    let bpm = 60.0 * fps / lag;
    // candidates: strongest distinct peaks, then octave variants
    peaks.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut cands: Vec<f64> = vec![bpm];
    for (l, _) in peaks.iter().take(6) {
        let b = 60.0 * fps / refine(*l);
        if cands.iter().all(|c| (c - b).abs() / c > 0.03) {
            cands.push(b);
        }
    }
    for m in [0.5, 2.0] {
        let b = bpm * m;
        if (BPM_MIN * 0.7..=BPM_MAX * 1.2).contains(&b) && cands.iter().all(|c| (c - b).abs() / c > 0.03) {
            cands.push(b);
        }
    }
    let strength = at(lag).clamp(0.0, 1.0);
    Some(TempoEstimate { bpm, lag, strength, candidates: cands, hypotheses: hyps })
}

/// Ellis DP beat tracker. `period` in frames. Returns beat frame indices (ascending).
pub fn track_beats(env: &[f32], period: f64, tightness: f32) -> Vec<usize> {
    let n = env.len();
    if n == 0 || period < 2.0 {
        return vec![];
    }
    let tau_min = ((period / 2.0).round() as usize).max(1);
    let tau_max = (period * 2.0).round() as usize;
    // precompute penalties
    let pen: Vec<f32> =
        (tau_min..=tau_max).map(|t| -tightness * ((t as f64 / period).ln() as f32).powi(2)).collect();
    let mut score = vec![0.0f32; n];
    let mut back = vec![-1i32; n];
    for t in 0..n {
        let mut best = f32::NEG_INFINITY;
        let mut arg = -1i32;
        if t >= tau_min {
            let hi = tau_max.min(t);
            for tau in tau_min..=hi {
                let s = score[t - tau] + pen[tau - tau_min];
                if s > best {
                    best = s;
                    arg = (t - tau) as i32;
                }
            }
        }
        if arg >= 0 {
            score[t] = env[t] + best;
            back[t] = arg;
        } else {
            score[t] = env[t];
        }
    }
    // end: best score in the last period (prefer later beats for tie)
    let from = n.saturating_sub(period.round() as usize + 1);
    let mut end = from;
    for t in from..n {
        if score[t] >= score[end] {
            end = t;
        }
    }
    let mut beats = vec![end];
    let mut cur = end as i32;
    while back[cur as usize] >= 0 {
        cur = back[cur as usize];
        beats.push(cur as usize);
    }
    beats.reverse();
    beats
}

/// Sub-frame peak position by parabolic interpolation, searching +-`radius` frames.
pub fn refine_peak(env: &[f32], idx: usize, radius: usize) -> f64 {
    let n = env.len();
    let lo = idx.saturating_sub(radius);
    let hi = (idx + radius).min(n - 1);
    let mut k = lo;
    for i in lo..=hi {
        if env[i] > env[k] {
            k = i;
        }
    }
    if k == 0 || k + 1 >= n {
        return k as f64;
    }
    let (a, b, c) = (env[k - 1] as f64, env[k] as f64, env[k + 1] as f64);
    let d = a - 2.0 * b + c;
    if d.abs() < 1e-12 { k as f64 } else { k as f64 + (0.5 * (a - c) / d).clamp(-0.5, 0.5) }
}

/// Least-squares line through (index, time).
fn fit(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = points.len() as f64;
    if points.len() < 2 {
        return None;
    }
    let sx: f64 = points.iter().map(|p| p.0).sum();
    let sy: f64 = points.iter().map(|p| p.1).sum();
    let sxx: f64 = points.iter().map(|p| p.0 * p.0).sum();
    let sxy: f64 = points.iter().map(|p| p.0 * p.1).sum();
    let d = n * sxx - sx * sx;
    if d.abs() < 1e-12 {
        return None;
    }
    let slope = (n * sxy - sx * sy) / d;
    Some(((sy - slope * sx) / n, slope)) // (origin, period)
}

/// Robust constant-grid regression. Beats are times in ms; `period_hint` ms. Returns
/// (origin_ms, period_ms, inlier_ratio, residuals).
pub fn fit_constant(beats_ms: &[f64], period_hint: f64) -> Option<(f64, f64, f64, Vec<f64>)> {
    if beats_ms.len() < 4 {
        return None;
    }
    let t0 = beats_ms[0];
    let mut pts: Vec<(f64, f64)> =
        beats_ms.iter().map(|t| (((t - t0) / period_hint).round(), *t)).collect();
    let mut period = period_hint;
    let mut origin = t0;
    let tol = 0.10 * period_hint;
    for _ in 0..4 {
        let inl: Vec<(f64, f64)> = pts.iter().cloned().filter(|(k, t)| (t - (origin + k * period)).abs() < tol * 2.0).collect();
        if inl.len() < 4 {
            break;
        }
        let (o, p) = fit(&inl)?;
        origin = o;
        period = p;
        // trim again at 1x tol then refit once more on the tighter set
        let inl2: Vec<(f64, f64)> = pts.iter().cloned().filter(|(k, t)| (t - (origin + k * period)).abs() < tol).collect();
        if inl2.len() >= 4 {
            let (o, p) = fit(&inl2)?;
            origin = o;
            period = p;
        }
        for pt in pts.iter_mut() {
            pt.0 = ((pt.1 - origin) / period).round();
        }
    }
    let res: Vec<f64> = pts.iter().map(|(k, t)| t - (origin + k * period)).collect();
    let inl = res.iter().filter(|r| r.abs() < tol).count() as f64 / res.len() as f64;
    Some((origin, period, inl, res))
}

/// Block-wise drift of the residuals (max |median| over blocks of `block` beats), in ms.
fn block_drift(res: &[f64], block: usize) -> f64 {
    let mut worst = 0.0f64;
    for c in res.chunks(block.max(4)) {
        if c.len() < block.max(4) / 2 {
            continue;
        }
        let mut v: Vec<f64> = c.to_vec();
        v.sort_by(|a, b| a.total_cmp(b));
        worst = worst.max(v[v.len() / 2].abs());
    }
    worst
}

/// Segment beats into constant-tempo stretches.
pub fn fit_variable(beats_ms: &[f64], period_hint: f64) -> Vec<TempoSegment> {
    let tol = 0.08 * period_hint;
    let mut segs: Vec<TempoSegment> = Vec::new();
    let n = beats_ms.len();
    let mut i = 0;
    while i < n {
        // grow a window from i while a line through it explains all beats within tol
        let mut j = (i + 8).min(n);
        let mut cur: Option<(f64, f64)> = None;
        let mut idx: Vec<(f64, f64)> = (i..j).map(|k| ((k - i) as f64, beats_ms[k])).collect();
        if let Some(f) = fit(&idx) {
            cur = Some(f);
        }
        while j < n {
            let Some((o, p)) = cur else { break };
            let next_t = beats_ms[j];
            let pred = o + (j - i) as f64 * p;
            if (next_t - pred).abs() > tol {
                break;
            }
            idx.push(((j - i) as f64, next_t));
            cur = fit(&idx);
            j += 1;
        }
        let (o, p) = cur.unwrap_or((beats_ms[i], period_hint));
        segs.push(TempoSegment { origin_ms: o, bpm: 60_000.0 / p, beats: Some((j - i) as u32) });
        i = j;
    }
    // merge neighbours whose tempos agree within 0.4 % and which are phase-consistent
    let mut merged: Vec<TempoSegment> = Vec::new();
    for s in segs {
        if let Some(last) = merged.last_mut() {
            let p_last = 60_000.0 / last.bpm;
            let expected = last.origin_ms + last.beats.unwrap_or(0) as f64 * p_last;
            if (60_000.0 / s.bpm - p_last).abs() / p_last < 0.004 && (s.origin_ms - expected).abs() < tol {
                last.beats = Some(last.beats.unwrap_or(0) + s.beats.unwrap_or(0));
                continue;
            }
        }
        merged.push(s);
    }
    if let Some(l) = merged.last_mut() {
        l.beats = None;
    }
    merged
}

/// Convert an octave-ambiguous tempo into the octave closest to `target`.
pub fn nearest_octave(bpm: f64, target: f64) -> f64 {
    let mut b = bpm;
    while b < target / 1.5 {
        b *= 2.0;
    }
    while b > target * 1.5 {
        b /= 2.0;
    }
    b
}

/// Full tempo analysis. `frame_time_ms(f)` converts a (fractional) frame index to track time.
pub fn analyze(env_raw: &[f32], fps: f32, frame_time_ms: impl Fn(f64) -> f64) -> Option<TempoResult> {
    let env = prepare_envelope(env_raw, fps);
    let est = estimate_tempo(&env, fps)?;
    let mut period = est.lag;
    let mut beats = track_beats(&env, period, TIGHTNESS);
    if beats.len() < 4 {
        return None;
    }
    // refine period by regression, track again at the refined period
    let to_frames = |bf: &[usize]| -> Vec<f64> { bf.iter().map(|b| refine_peak(&env, *b, 2)).collect() };
    let to_ms = |fr: &[f64]| -> Vec<f64> { fr.iter().map(|f| frame_time_ms(*f)).collect() };
    let mut beats_fr = to_frames(&beats);
    let mut beats_ms = to_ms(&beats_fr);
    let frame_ms = 1000.0 / fps as f64;
    let mut period_ms = period * frame_ms;
    if let Some((_, p, _, _)) = fit_constant(&beats_ms, period_ms) {
        if (p - period_ms).abs() / period_ms < 0.03 {
            period_ms = p;
            period = p / frame_ms;
            beats = track_beats(&env, period, TIGHTNESS);
            if beats.len() >= 4 {
                beats_fr = to_frames(&beats);
                beats_ms = to_ms(&beats_fr);
            }
        }
    }
    let (origin, p, inlier, res) = fit_constant(&beats_ms, period_ms)?;
    let drift = block_drift(&res, 16);
    let constant = inlier >= 0.85 && drift < 0.12 * p;
    let (grid_kind, segments, bpm) = if constant {
        // origin: first grid beat at t >= 0
        let k0 = (-origin / p).ceil().max(0.0);
        let o = origin + k0 * p;
        (GridKind::Constant, vec![TempoSegment { origin_ms: o, bpm: 60_000.0 / p, beats: None }], 60_000.0 / p)
    } else {
        // The headline tempo stays the robust whole-track fit: on real tracks a variable grid
        // mostly means breakdowns or loose tracking rather than a tempo change, and the
        // beat-weighted mean of short segments drifted off the true tempo (bench: +1.1 pt
        // within 0.5 % vs the segment mean).
        (GridKind::Variable, fit_variable(&beats_ms, period_ms), 60_000.0 / p)
    };
    // confidence: autocorrelation strength x beat consistency
    let consistency = (inlier).clamp(0.0, 1.0);
    let confidence = (0.15 + 1.1 * est.strength).clamp(0.0, 1.0) * (0.35 + 0.65 * consistency);
    let mut cands: Vec<f64> = est.candidates.clone();
    if cands.is_empty() || (cands[0] - bpm).abs() / bpm > 0.03 {
        cands.insert(0, bpm);
    }
    cands[0] = bpm;
    let grid = BeatGrid {
        kind: grid_kind,
        segments,
        downbeat_phase: None,
        beats_per_bar: 4,
        phrase_starts_ms: vec![],
        confidence,
        source: "bc-rs-1".into(),
    };
    Some(TempoResult { bpm, confidence, candidates: cands, beats_ms, beats_frames: beats_fr, grid, inlier_ratio: inlier })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pulse train at `bpm`, fps 172.27.
    fn pulses(bpm: f64, secs: f64, fps: f32) -> Vec<f32> {
        let n = (secs * fps as f64) as usize;
        let period = 60.0 / bpm * fps as f64;
        let mut v = vec![0.02f32; n];
        let mut t = 10.3;
        while (t as usize) < n - 2 {
            let i = t as usize;
            let f = (t - i as f64) as f32;
            v[i] += 1.0 - f;
            v[i + 1] += f;
            t += period;
        }
        v
    }

    #[test]
    fn pulse_train_128() {
        let fps = 22050.0 / 128.0;
        let env = pulses(128.0, 40.0, fps);
        let r = analyze(&env, fps, |f| f * 1000.0 / fps as f64).unwrap();
        assert!((r.bpm - 128.0).abs() / 128.0 < 0.001, "{}", r.bpm);
        assert_eq!(r.grid.kind, GridKind::Constant);
        assert!(r.confidence > 0.5, "{}", r.confidence);
    }

    #[test]
    fn pulse_train_90_exact() {
        let fps = 22050.0 / 128.0;
        let env = pulses(90.0, 40.0, fps);
        let r = analyze(&env, fps, |f| f * 1000.0 / fps as f64).unwrap();
        assert!((r.bpm - 90.0).abs() < 0.05, "{}", r.bpm);
    }

    #[test]
    fn noise_has_low_confidence() {
        let fps = 22050.0 / 128.0;
        let mut seed = 12345u64;
        let env: Vec<f32> = (0..(40.0 * fps) as usize)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((seed >> 40) as f32 / (1u64 << 24) as f32) + 0.0
            })
            .collect();
        if let Some(r) = analyze(&env, fps, |f| f * 1000.0 / fps as f64) {
            assert!(r.confidence < 0.45, "{}", r.confidence);
        }
    }

    #[test]
    fn tempo_change_makes_a_variable_grid() {
        let fps = 22050.0 / 128.0;
        let mut a = pulses(120.0, 30.0, fps);
        let b = pulses(132.0, 30.0, fps);
        a.extend(b);
        let r = analyze(&a, fps, |f| f * 1000.0 / fps as f64).unwrap();
        assert_eq!(r.grid.kind, GridKind::Variable, "{:?}", r.grid.segments);
        assert!(r.grid.segments.len() >= 2);
    }

    #[test]
    fn octave_helper() {
        assert!((nearest_octave(64.0, 128.0) - 128.0).abs() < 1e-9);
        assert!((nearest_octave(256.0, 128.0) - 128.0).abs() < 1e-9);
    }
}
