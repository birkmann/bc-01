//! Key detection.
//!
//! Primary path ([`classify_hpcp`]): a port of essentia's `KeyExtractor` with the `edma`
//! profiles over the essentia-style HPCP of [`crate::features::EssHpcp`], which reproduces the
//! imported essentia keys on ~93 % of a real library (the fitted-profile model below reached
//! ~38 %).
//!
//! Fallback (no HPCP peaks) and tooling: HPCP-style chroma channels scored against fitted key
//! profiles. Scoring is rotation-equivariant:
//! `score(k, mode) = bias[mode] + sum_c sum_i w[mode][c][(i-k)%12] f_c[i]` with
//! `f_c = ln(chroma_c / sum(chroma_c) + 1e-3)`, a multinomial logistic regression over the 24 keys
//! fitted on the user's library using the imported essentia keys as labels (`fit_profiles`,
//! exposed as `bc analyze --fit-key-profiles`). A default profile ships for fresh installs
//! (`KeyProfiles::default()`, see `key_defaults.rs`).

use bc_music::camelot::{Mode, to_camelot};
use serde::{Deserialize, Serialize};

use crate::features::{BINS, fold_with_tuning};

/// Chroma channels: 0 bass peaks, 1 mid peaks, 2 harmonic summation, 3 all-bin spectrum.
pub const K: usize = 4;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyProfiles {
    /// `w[mode][channel][pitch offset]`, mode 0 = major, 1 = minor.
    pub w: [[[f32; 12]; K]; 2],
    pub bias: [f32; 2],
    #[serde(default)]
    pub name: String,
}

// Krumhansl-Kessler profiles, used to initialise the fit and as the fallback default.
const KK_MAJOR: [f32; 12] = [6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88];
const KK_MINOR: [f32; 12] = [6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17];

fn centred(p: &[f32; 12], scale: f32) -> [f32; 12] {
    let mean = p.iter().sum::<f32>() / 12.0;
    let norm = (p.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>()).sqrt();
    let mut o = [0.0; 12];
    for i in 0..12 {
        o[i] = (p[i] - mean) / norm * scale;
    }
    o
}

impl KeyProfiles {
    /// Template (Krumhansl) initialisation: correlation-like scoring on the mid channel.
    pub fn template() -> Self {
        let m = centred(&KK_MAJOR, 6.0);
        let n = centred(&KK_MINOR, 6.0);
        let z = [0.0f32; 12];
        Self { w: [[z, m, z, z], [z, n, z, z]], bias: [0.0, 0.0], name: "template-kk".into() }
    }
}

impl Default for KeyProfiles {
    fn default() -> Self {
        crate::key_defaults::default_profiles()
    }
}

/// Feature vector: one log-compressed (`ln(c/sum + 1e-3)`) L1-normalised chroma per channel.
pub type Feat = [[f32; 12]; K];

/// Build features from the extractor's four 36-bin accumulators.
pub fn features(c36: [&[f64; BINS]; K]) -> Option<Feat> {
    let mut out = [[0.0f32; 12]; K];
    let mut any = 0.0;
    for (c, acc) in c36.iter().enumerate() {
        let (f, _) = fold_with_tuning(acc);
        let s: f64 = f.iter().sum();
        any += s;
        if s > 0.0 {
            for i in 0..12 {
                out[c][i] = (f[i] / s + 1e-3).ln() as f32;
            }
        }
    }
    (any > 0.0).then_some(out)
}

/// Features from stored 36-bin vectors (tooling).
pub fn features_from_vecs(v: &[Vec<f64>]) -> Option<Feat> {
    // 8 (or 9) vectors = [whole x4, excerpt x4 (, excerpt HPCP)]: the excerpt is what
    // production uses
    let v = if v.len() >= 2 * K { &v[K..2 * K] } else { v };
    if v.len() != K || v.iter().any(|x| x.len() != BINS) {
        return None;
    }
    let mut arrs = [[0.0f64; BINS]; K];
    for (c, x) in v.iter().enumerate() {
        arrs[c].copy_from_slice(x);
    }
    features([&arrs[0], &arrs[1], &arrs[2], &arrs[3]])
}

#[derive(Debug, Clone, PartialEq)]
pub struct KeyResult {
    pub pitch_class: u8,
    pub minor: bool,
    pub camelot: &'static str,
    /// 0..1: the winner's profile correlation ([`classify_hpcp`], like essentia's `strength`) or
    /// its softmax probability ([`classify`]).
    pub strength: f64,
    pub scores: [f32; 24],
}

/// essentia `edma` profiles (tonic first), fitted on electronic dance music.
const EDMA_MAJOR: [f64; 12] = [1.00, 0.29, 0.50, 0.40, 0.60, 0.56, 0.32, 0.80, 0.31, 0.45, 0.42, 0.39];
const EDMA_MINOR: [f64; 12] = [1.00, 0.31, 0.44, 0.58, 0.33, 0.49, 0.29, 0.78, 0.43, 0.29, 0.53, 0.32];
/// essentia `Key` `pcpThreshold`: bins below this fraction of the peak are zeroed.
const HPCP_GATE: f64 = 0.2;

/// essentia `Key` (profile `edma`, no polyphony) on a summed HPCP (index 0 = C): peak-normalise,
/// zero the bins below 0.2, Pearson-correlate with every rotation of the major and minor
/// profiles; the best minor wins ties with the best major. `None` for an empty HPCP.
pub fn classify_hpcp(h: &[f64; 12]) -> Option<KeyResult> {
    let max = h.iter().cloned().fold(0.0f64, f64::max);
    if max <= 0.0 || !max.is_finite() {
        return None;
    }
    let p: Vec<f64> = h.iter().map(|v| if v / max < HPCP_GATE { 0.0 } else { v / max }).collect();
    let centre = |v: &[f64]| -> (Vec<f64>, f64) {
        let m = v.iter().sum::<f64>() / 12.0;
        let c: Vec<f64> = v.iter().map(|x| x - m).collect();
        let sd = c.iter().map(|x| x * x).sum::<f64>().sqrt();
        (c, sd)
    };
    let (pc, sp) = centre(&p);
    if sp <= 0.0 {
        return None;
    }
    let mut sc = [0.0f32; 24];
    for (mode, prof) in [EDMA_MAJOR, EDMA_MINOR].iter().enumerate() {
        let (q, sq) = centre(prof);
        for k in 0..12 {
            let r: f64 = (0..12).map(|i| pc[i] * q[(i + 12 - k) % 12]).sum();
            sc[mode * 12 + k] = (r / (sp * sq)) as f32;
        }
    }
    let best = |r: std::ops::Range<usize>| r.max_by(|a, b| sc[*a].total_cmp(&sc[*b]).then(b.cmp(a))).unwrap_or(0);
    let (maj, min) = (best(0..12), best(12..24));
    let bi = if sc[min] >= sc[maj] { min } else { maj };
    let minor = bi >= 12;
    let pc = (bi % 12) as u8;
    Some(KeyResult {
        pitch_class: pc,
        minor,
        camelot: to_camelot(pc as i32, if minor { Mode::Minor } else { Mode::Major }),
        strength: (sc[bi] as f64).clamp(0.0, 1.0),
        scores: sc,
    })
}

pub fn scores(f: &Feat, p: &KeyProfiles) -> [f32; 24] {
    let mut out = [0.0f32; 24];
    for mode in 0..2 {
        for k in 0..12 {
            let mut s = p.bias[mode];
            for c in 0..K {
                for i in 0..12 {
                    s += p.w[mode][c][(i + 12 - k) % 12] * f[c][i];
                }
            }
            out[mode * 12 + k] = s;
        }
    }
    out
}

pub fn classify(f: &Feat, p: &KeyProfiles) -> KeyResult {
    let sc = scores(f, p);
    let (mut best, mut bi) = (f32::NEG_INFINITY, 0);
    for (i, s) in sc.iter().enumerate() {
        if *s > best {
            best = *s;
            bi = i;
        }
    }
    let z: f32 = sc.iter().map(|s| (s - best).exp()).sum();
    let minor = bi >= 12;
    let pc = (bi % 12) as u8;
    KeyResult {
        pitch_class: pc,
        minor,
        camelot: to_camelot(pc as i32, if minor { Mode::Minor } else { Mode::Major }),
        strength: (1.0 / z) as f64,
        scores: sc,
    }
}

/// One labelled example for fitting: label = mode*12 + tonic.
#[derive(Debug, Clone)]
pub struct Example {
    pub feat: Feat,
    pub label: usize,
}

const N_PAR: usize = 2 * K * 12 + 2;

fn flatten(p: &KeyProfiles) -> Vec<f32> {
    let mut v = Vec::with_capacity(N_PAR);
    for m in 0..2 {
        for c in 0..K {
            v.extend_from_slice(&p.w[m][c]);
        }
    }
    v.extend_from_slice(&p.bias);
    v
}

fn unflatten(p: &mut KeyProfiles, v: &[f32]) {
    let mut o = 0;
    for m in 0..2 {
        for c in 0..K {
            p.w[m][c].copy_from_slice(&v[o..o + 12]);
            o += 12;
        }
    }
    p.bias.copy_from_slice(&v[o..o + 2]);
}

/// Fit profiles by softmax regression with Adam. Returns the fitted profiles and the training
/// accuracy.
pub fn fit_profiles(data: &[Example], l2: f32, iters: usize, init: &KeyProfiles) -> (KeyProfiles, f64) {
    let mut p = init.clone();
    p.name = "fitted".into();
    let mut w = flatten(&p);
    let (mut m1, mut m2) = (vec![0.0f32; N_PAR], vec![0.0f32; N_PAR]);
    let (b1, b2, lr, eps) = (0.9f32, 0.999f32, 0.05f32, 1e-8f32);
    let n = data.len().max(1) as f32;
    for it in 1..=iters {
        unflatten(&mut p, &w);
        let mut g = vec![0.0f32; N_PAR];
        for ex in data {
            let sc = scores(&ex.feat, &p);
            let mx = sc.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            let mut z = 0.0;
            let mut pr = [0.0f32; 24];
            for c in 0..24 {
                pr[c] = (sc[c] - mx).exp();
                z += pr[c];
            }
            for c in 0..24 {
                pr[c] /= z;
                let d = pr[c] - if c == ex.label { 1.0 } else { 0.0 };
                if d.abs() < 1e-7 {
                    continue;
                }
                let (mode, k) = (c / 12, c % 12);
                for ch in 0..K {
                    for i in 0..12 {
                        let r = (i + 12 - k) % 12;
                        g[(mode * K + ch) * 12 + r] += d * ex.feat[ch][i];
                    }
                }
                g[2 * K * 12 + mode] += d;
            }
        }
        for i in 0..N_PAR {
            g[i] = g[i] / n + if i < 2 * K * 12 { l2 * w[i] } else { 0.0 };
            m1[i] = b1 * m1[i] + (1.0 - b1) * g[i];
            m2[i] = b2 * m2[i] + (1.0 - b2) * g[i] * g[i];
            let mh = m1[i] / (1.0 - b1.powi(it as i32));
            let vh = m2[i] / (1.0 - b2.powi(it as i32));
            w[i] -= lr * mh / (vh.sqrt() + eps);
        }
    }
    unflatten(&mut p, &w);
    let acc = accuracy(data, &p);
    (p, acc)
}

pub fn accuracy(data: &[Example], p: &KeyProfiles) -> f64 {
    let ok = data
        .iter()
        .filter(|e| {
            let sc = scores(&e.feat, p);
            sc.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|x| x.0).unwrap_or(0) == e.label
        })
        .count();
    ok as f64 / data.len().max(1) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chroma_for(notes: &[(usize, f64)]) -> [f64; BINS] {
        let mut c = [0.0; BINS];
        for (pc, w) in notes {
            c[pc * 3] += w;
        }
        c
    }

    fn feat_of(c: &[f64; BINS]) -> Feat {
        features([c, c, c, c]).unwrap()
    }

    #[test]
    fn template_finds_a_minor_and_c_major() {
        let p = KeyProfiles::template();
        let am = chroma_for(&[(9, 1.0), (0, 0.8), (4, 0.8)]);
        assert_eq!(classify(&feat_of(&am), &p).camelot, "8A");
        let cm = chroma_for(&[(0, 1.0), (4, 0.8), (7, 0.8)]);
        assert_eq!(classify(&feat_of(&cm), &p).camelot, "8B");
    }

    #[test]
    fn hpcp_classifier_follows_essentia_edma() {
        // A minor scale weights (tonic, third, fifth strongest) and C major
        let mut am = [0.05f64; 12];
        for (pc, w) in [(9, 1.0), (0, 0.7), (4, 0.8), (2, 0.3), (11, 0.3), (5, 0.3), (7, 0.3)] {
            am[pc] = w;
        }
        let r = classify_hpcp(&am).unwrap();
        assert_eq!(r.camelot, "8A");
        assert!(r.strength > 0.5 && r.strength <= 1.0, "{}", r.strength);
        let mut cm = [0.05f64; 12];
        for (pc, w) in [(0, 1.0), (4, 0.7), (7, 0.8), (2, 0.3), (5, 0.3), (9, 0.3), (11, 0.3)] {
            cm[pc] = w;
        }
        assert_eq!(classify_hpcp(&cm).unwrap().camelot, "8B");
        assert!(classify_hpcp(&[0.0; 12]).is_none());
        // flat after the 0.2 gate: no information
        assert!(classify_hpcp(&[1.0; 12]).is_none());
    }

    #[test]
    fn fitting_recovers_labels() {
        let mut data = Vec::new();
        let mut seed = 99u64;
        let mut rnd = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((seed >> 40) as f32) / (1u64 << 24) as f32
        };
        for label in 0..24usize {
            let (mode, k) = (label / 12, label % 12);
            let tpl = if mode == 0 { KK_MAJOR } else { KK_MINOR };
            for _ in 0..8 {
                let mut f = [[0.0f32; 12]; K];
                for ch in 0..K {
                    let mut c = [0.0f32; 12];
                    for i in 0..12 {
                        c[i] = tpl[(i + 12 - k) % 12] * (0.5 + 0.25 * ch as f32) + rnd() * 1.5;
                    }
                    let s: f32 = c.iter().sum();
                    for i in 0..12 {
                        f[ch][i] = (c[i] / s + 1e-3).ln();
                    }
                }
                data.push(Example { feat: f, label });
            }
        }
        let (_p, acc) = fit_profiles(&data, 1e-3, 200, &KeyProfiles::template());
        assert!(acc > 0.95, "{acc}");
    }
}
