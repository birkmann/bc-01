//! Mip pyramid (peaks max-pooled, RMS and bands energy-mean pooled) over a [`Levels`], used by the renderer and tests.

use crate::format::{Levels, OVERVIEW_POINTS, PLANES, Waveform};

/// `levels[0]` is the finest; level `k` has points `2^k` times as wide as level 0, until a level
/// has at most [`OVERVIEW_POINTS`] points.
#[derive(Debug, Clone)]
pub struct Pyramid {
    pub levels: Vec<Levels>,
    /// Seconds covered by one level-0 point.
    pub dt0_s: f64,
}

/// Pool x2 (odd tails keep their last point): max for the peak planes, energy mean for RMS
/// and the band planes.
pub fn pool2(l: &Levels) -> Levels {
    let n = l.n.div_ceil(2);
    let mut out = Levels::zeros(n);
    for p in 0..PLANES {
        for i in 0..n {
            let a = l.planes[p][2 * i];
            let b = l.planes[p].get(2 * i + 1).copied().unwrap_or(0);
            out.planes[p][i] = if p >= crate::format::RMS {
                // energy mean (a missing odd tail counts as the single point it is)
                match l.planes[p].get(2 * i + 1) {
                    Some(&b) => crate::scale::energy_mean_u8([a, b]),
                    None => a,
                }
            } else {
                a.max(b)
            };
        }
    }
    out
}

impl Pyramid {
    /// Pyramid from a detail (or any) level; `dt0_s` is the seconds one point covers.
    pub fn build(base: &Levels, dt0_s: f64) -> Pyramid {
        let mut levels = vec![base.clone()];
        while levels.last().is_some_and(|l| l.n > OVERVIEW_POINTS) {
            let next = pool2(levels.last().expect("non-empty"));
            levels.push(next);
        }
        Pyramid { levels, dt0_s }
    }

    /// Pyramid of a waveform: from the detail when present, otherwise from the overview alone
    /// (a client holding only the overview still renders the whole track). Empty waveforms
    /// give a single empty level.
    pub fn from_waveform(w: &Waveform) -> Pyramid {
        match &w.detail {
            Some(d) if d.n > 0 => Self::build(d, w.detail_dt_s()),
            _ if w.overview.n > 0 => Self::build(&w.overview, w.duration_s() / w.overview.n as f64),
            _ => Pyramid {
                levels: vec![Levels::empty()],
                dt0_s: w.detail_dt_s().max(1e-9),
            },
        }
    }

    pub fn level_count(&self) -> usize {
        self.levels.len()
    }

    /// Seconds one point of level `k` covers.
    pub fn dt_s(&self, k: usize) -> f64 {
        self.dt0_s * (1u64 << k.min(40)) as f64
    }

    /// Best level for `points_per_px` level-0 points per output pixel: the coarsest level that
    /// still has at least one of its points per pixel (`floor(log2)`), clamped.
    pub fn level_for(&self, points_per_px: f64) -> usize {
        level_for(points_per_px, self.levels.len())
    }

    /// Like [`Pyramid::sample_column`] but the RMS and band planes are energy means over the
    /// column, weighted by how much of each point the column covers (what the renderers use);
    /// peaks stay max. It reads from a level with about four or more points per column so
    /// neighbouring columns do not flicker, and zoomed in past one point per column it is the
    /// nearest point, never an interpolation.
    pub fn sample_column_mean(&self, t0_s: f64, t1_s: f64) -> [u8; PLANES] {
        let (t0, t1) = if t1_s >= t0_s { (t0_s, t1_s) } else { (t1_s, t0_s) };
        let ppp = (t1 - t0) / self.dt0_s;
        let k = if ppp < 8.0 { 0 } else { level_for(ppp / 4.0, self.levels.len()) };
        let l = &self.levels[k];
        let dt = self.dt_s(k);
        if l.n == 0 || t1 < 0.0 || t0 >= dt * l.n as f64 {
            return [0; PLANES];
        }
        let a = ((t0.max(0.0) / dt).floor() as usize).min(l.n - 1);
        let b = ((t1 / dt).ceil() as usize).clamp(a + 1, l.n);
        let lut = crate::scale::lin_lut();
        let mut ss = [0.0f64; PLANES];
        let (mut wsum, mut pk) = (0.0f64, [0u8; 2]);
        for i in a..b {
            let w = ((t1.min((i + 1) as f64 * dt) - t0.max(i as f64 * dt)).max(1e-4 * dt)) / dt;
            wsum += w;
            for q in crate::format::RMS..PLANES {
                let v = lut[l.planes[q][i] as usize] as f64;
                ss[q] += w * v * v;
            }
            pk[0] = pk[0].max(l.planes[0][i]);
            pk[1] = pk[1].max(l.planes[1][i]);
        }
        std::array::from_fn(|q| match q {
            0 | 1 => pk[q],
            _ => crate::scale::lin_to_u8((ss[q] / wsum.max(1e-12)).sqrt() as f32),
        })
    }

    /// Max over `[t0_s, t1_s)` at the best mip. Out-of-range time gives zeros.
    pub fn sample_column(&self, t0_s: f64, t1_s: f64) -> [u8; PLANES] {
        let (t0, t1) = if t1_s >= t0_s {
            (t0_s, t1_s)
        } else {
            (t1_s, t0_s)
        };
        let k = self.level_for((t1 - t0) / self.dt0_s);
        let l = &self.levels[k];
        let dt = self.dt_s(k);
        if l.n == 0 || t1 < 0.0 || t0 >= dt * l.n as f64 {
            return [0; PLANES];
        }
        let a = ((t0.max(0.0) / dt).floor() as usize).min(l.n - 1);
        let b = ((t1 / dt).ceil() as usize).clamp(a + 1, l.n);
        let mut out = [0u8; PLANES];
        for i in a..b {
            let p = l.point(i);
            for q in 0..PLANES {
                out[q] = out[q].max(p[q]);
            }
        }
        out
    }
}

/// See [`Pyramid::level_for`].
pub fn level_for(points_per_px: f64, level_count: usize) -> usize {
    if points_per_px.is_nan() || points_per_px < 2.0 || level_count <= 1 {
        return 0;
    }
    (points_per_px.log2().floor() as usize).min(level_count - 1)
}
