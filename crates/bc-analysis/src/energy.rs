//! Energy v2 (1..10): short-term loudness, onset density and spectral centroid, calibrated to
//! library percentiles. The legacy measure was `min(1, RMS*4)` (saturates at 1.0 for most dance
//! music).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct EnergyRaw {
    /// 75th percentile of momentary loudness (LUFS), gated at -50.
    pub loud_p75: f32,
    /// Mean onset flux per frame.
    pub onset_density: f32,
    /// Mean spectral centroid (Hz), log2-compressed by the scorer.
    pub centroid_hz: f32,
}

/// Calibration: combines the three features into a scalar and maps it through a quantile table
/// (knots of the library CDF at 0, 5, 10, ... 100 %) to 1..10.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnergyCalibration {
    pub mean: [f32; 3],
    pub sd: [f32; 3],
    pub weights: [f32; 3],
    /// 21 ascending raw-score knots; `quantiles[i]` is the score at percentile `5*i`.
    pub quantiles: Vec<f32>,
}

impl Default for EnergyCalibration {
    fn default() -> Self {
        crate::energy_defaults::default_calibration()
    }
}

impl EnergyCalibration {
    pub fn raw_score(&self, r: &EnergyRaw) -> f32 {
        let v = [r.loud_p75, r.onset_density, (r.centroid_hz.max(50.0)).log2()];
        (0..3).map(|i| self.weights[i] * (v[i] - self.mean[i]) / self.sd[i].max(1e-6)).sum()
    }

    /// 1.0 .. 10.0
    pub fn energy_v2(&self, r: &EnergyRaw) -> f32 {
        let s = self.raw_score(r);
        let q = &self.quantiles;
        if q.len() < 2 {
            return 5.5;
        }
        let pct = if s <= q[0] {
            0.0
        } else if s >= q[q.len() - 1] {
            1.0
        } else {
            let i = q.partition_point(|x| *x <= s) - 1;
            let span = (q[i + 1] - q[i]).max(1e-9);
            (i as f32 + (s - q[i]) / span) / (q.len() - 1) as f32
        };
        1.0 + 9.0 * pct
    }

    /// Fit quantile knots from a population of raw features (weights/means stay as given, with
    /// means/sds re-estimated from the sample).
    pub fn fit(samples: &[EnergyRaw]) -> Self {
        let mut c = Self::default();
        if samples.len() < 20 {
            return c;
        }
        let n = samples.len() as f32;
        let cols = |r: &EnergyRaw| [r.loud_p75, r.onset_density, (r.centroid_hz.max(50.0)).log2()];
        for i in 0..3 {
            let m = samples.iter().map(|s| cols(s)[i]).sum::<f32>() / n;
            let v = samples.iter().map(|s| (cols(s)[i] - m).powi(2)).sum::<f32>() / n;
            c.mean[i] = m;
            c.sd[i] = v.sqrt().max(1e-6);
        }
        let mut raw: Vec<f32> = samples.iter().map(|s| c.raw_score(s)).collect();
        raw.sort_by(|a, b| a.total_cmp(b));
        c.quantiles = (0..=20)
            .map(|i| {
                let idx = ((raw.len() - 1) as f32 * i as f32 / 20.0).round() as usize;
                raw[idx]
            })
            .collect();
        c
    }
}

/// Build raw energy features from momentary-loudness samples and frame series stats.
pub fn raw_from(momentary: &[f32], onset: &[f32], centroid: &[f32]) -> EnergyRaw {
    let mut m: Vec<f32> = momentary.iter().cloned().filter(|v| v.is_finite() && *v > -50.0).collect();
    m.sort_by(|a, b| a.total_cmp(b));
    let loud_p75 = if m.is_empty() { -50.0 } else { m[((m.len() as f32 * 0.75) as usize).min(m.len() - 1)] };
    let onset_density = if onset.is_empty() { 0.0 } else { onset.iter().sum::<f32>() / onset.len() as f32 };
    let valid: Vec<f32> = centroid.iter().cloned().filter(|c| *c > 0.0).collect();
    let centroid_hz = if valid.is_empty() { 0.0 } else { valid.iter().sum::<f32>() / valid.len() as f32 };
    EnergyRaw { loud_p75, onset_density, centroid_hz }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn louder_busier_brighter_scores_higher() {
        let cal = EnergyCalibration::default();
        let calm = EnergyRaw { loud_p75: -24.0, onset_density: 0.1, centroid_hz: 900.0 };
        let hot = EnergyRaw { loud_p75: -8.0, onset_density: 0.5, centroid_hz: 3500.0 };
        assert!(cal.energy_v2(&hot) > cal.energy_v2(&calm));
        assert!((1.0..=10.0).contains(&cal.energy_v2(&hot)));
        assert!((1.0..=10.0).contains(&cal.energy_v2(&calm)));
    }

    #[test]
    fn fitted_calibration_spreads_the_population() {
        let mut seed = 7u64;
        let mut rnd = |lo: f32, hi: f32| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            lo + (hi - lo) * ((seed >> 40) as f32 / (1u64 << 24) as f32)
        };
        let pop: Vec<EnergyRaw> = (0..500)
            .map(|_| EnergyRaw { loud_p75: rnd(-20.0, -6.0), onset_density: rnd(0.1, 0.6), centroid_hz: rnd(800.0, 4000.0) })
            .collect();
        let cal = EnergyCalibration::fit(&pop);
        let mut hist = [0usize; 10];
        for p in &pop {
            hist[((cal.energy_v2(p) - 1.0) / 9.0 * 9.999) as usize] += 1;
        }
        assert!(hist.iter().all(|h| *h > 10), "{hist:?}");
    }
}
