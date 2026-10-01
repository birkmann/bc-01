//! Bar-style waveform maths shared by the WebGL shader, the Canvas2D fallback and the row
//! mini-waves. Pure and wasm-safe (no WebGL, no I/O).
//!
//! A bar's height is the **energy mean** of the RMS plane over the bar's time span, placed in
//! dB between the track's quiet and loud reference ([`Refs::rms_lo`], [`Refs::rms`]) and
//! expanded with `^1.3`. Each track's own loudness range fills the height, so a flat, loud
//! master still shows its structure and breakdowns drop clearly below drops. The shader
//! implements the same formula; the constants below are spliced into its source.

use crate::format::{HIGH, LOW, Levels, PEAK_NEG, PEAK_POS, RMS};
use crate::mip::Pyramid;
use crate::scale::lin_lut;

/// Exponent applied to the position between the quiet and loud reference (expansion).
pub const BAR_EXPONENT: f32 = 1.3;
/// Height of a bar at the quiet reference; quieter bars shrink linearly towards 0.
pub const BAR_FLOOR: f32 = 0.12;
/// Smallest dB span between the quiet and loud reference, so tracks with almost no dynamics
/// do not turn tiny level differences into full-height jumps.
pub const BAR_MIN_RANGE_DB: f32 = 6.0;
/// Percentile of the RMS plane used as the quiet reference.
pub const REF_PERCENTILE_LO: f32 = 0.25;
/// Minimum drawn bar height in device pixels (total, both halves).
pub const BAR_MIN_PX: f32 = 1.5;
/// Percentile used for the reference levels.
pub const REF_PERCENTILE: f32 = 0.95;
/// Percentile for the RMS and peak references: high, so loud sections sit below 1.0 and keep
/// bar-to-bar variation instead of clamping into a flat slab.
pub const REF_PERCENTILE_HI: f32 = 0.99;

/// Per-track reference levels (linear amplitude, 1.0 = full scale): the 95th percentile of each
/// plane over the non-silent points. Heights are divided by these, so a loud master and a quiet
/// ambient piece both fill the available height.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Refs {
    pub rms: f32,
    /// Quiet RMS reference for bars: p25 of the RMS plane, at least [`BAR_MIN_RANGE_DB`] below
    /// `rms`.
    pub rms_lo: f32,
    /// p99 of `max(peak_pos, peak_neg)` (rms and peak use p99, bands p95).
    pub peak: f32,
    pub low: f32,
    pub mid: f32,
    pub high: f32,
}

impl Refs {
    /// Fixed absolute references (no per-track normalisation): about -9 dBFS RMS fills the
    /// height, bars bottom out 24 dB below that.
    pub const fn absolute() -> Refs {
        Refs { rms: 0.35, rms_lo: 0.022, peak: 1.0, low: 0.3, mid: 0.2, high: 0.12 }
    }
}

impl Default for Refs {
    fn default() -> Self {
        Refs::absolute()
    }
}

/// Lower bounds so a (near-)silent track does not blow up the normalisation.
const MIN_REF: Refs = Refs { rms: 0.02, rms_lo: 0.0, peak: 0.05, low: 0.01, mid: 0.01, high: 0.005 };

/// Linear percentile of a plane, ignoring digital silence (byte 0).
fn pct_lin(plane: &[u8], pct: f32) -> Option<f32> {
    let lut = lin_lut();
    let mut hist = [0u32; 256];
    let mut n = 0u32;
    for &v in plane {
        if v != 0 {
            hist[v as usize] += 1;
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    let target = ((n as f32 * pct).ceil() as u32).max(1);
    let mut acc = 0u32;
    for (v, &c) in hist.iter().enumerate() {
        acc += c;
        if acc >= target {
            return Some(lut[v]);
        }
    }
    Some(lut[255])
}

/// Reference levels of a level (use the overview, or the coarsest pyramid level, so every
/// renderer sees the same numbers).
pub fn reference_levels(l: &Levels) -> Refs {
    let peak: Vec<u8> = (0..l.n).map(|i| l.planes[PEAK_POS][i].max(l.planes[PEAK_NEG][i])).collect();
    let f = |plane: &[u8], pct: f32, min: f32, abs: f32| pct_lin(plane, pct).map_or(abs, |v| v.max(min));
    let a = Refs::absolute();
    let rms = f(&l.planes[RMS], REF_PERCENTILE_HI, MIN_REF.rms, a.rms);
    let max_lo = rms * 10f32.powf(-BAR_MIN_RANGE_DB / 20.0);
    let rms_lo = pct_lin(&l.planes[RMS], REF_PERCENTILE_LO).map_or(a.rms_lo, |v| v.min(max_lo)).max(1e-5);
    Refs {
        rms,
        rms_lo,
        peak: f(&peak, REF_PERCENTILE_HI, MIN_REF.peak, a.peak),
        low: f(&l.planes[LOW], REF_PERCENTILE, MIN_REF.low, a.low),
        mid: f(&l.planes[LOW + 1], REF_PERCENTILE, MIN_REF.mid, a.mid),
        high: f(&l.planes[HIGH], REF_PERCENTILE, MIN_REF.high, a.high),
    }
}

/// One bar. `h` is the height 0..1 of the full (mirrored) height, peak term included.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BarValue {
    pub h: f32,
}

/// Height from the span's RMS energy mean (linear amplitude): [`BAR_FLOOR`] at the quiet
/// reference, 1.0 at the loud one, linear towards 0 below the quiet reference.
pub fn bar_height(rms: f32, refs: &Refs) -> f32 {
    let (lo, hi) = (refs.rms_lo.max(1e-6), refs.rms.max(1e-6));
    if rms < lo {
        return BAR_FLOOR * (rms / lo).max(0.0);
    }
    let t = ((rms / lo).ln() / (hi / lo).max(1.0001).ln()).clamp(0.0, 1.0);
    BAR_FLOOR + (1.0 - BAR_FLOOR) * t.powf(BAR_EXPONENT)
}

/// Energy-mean RMS (linear) over `[p0, p1)` measured in points of the level, each point
/// weighted by how much of it the span covers (so neighbouring bars do not flicker).
/// A span narrower than a point reads the nearest point.
fn span_rms(l: &Levels, p0: f64, p1: f64) -> f32 {
    let lut = lin_lut();
    let a = (p0.max(0.0).floor() as usize).min(l.n - 1);
    let b = (p1.ceil() as usize).clamp(a + 1, l.n);
    let (mut ss, mut wsum) = (0.0f64, 0.0f64);
    for i in a..b {
        let w = (p1.min((i + 1) as f64) - p0.max(i as f64)).max(1e-4);
        let r = lut[l.planes[RMS][i] as usize] as f64;
        ss += w * r * r;
        wsum += w;
    }
    (ss / wsum.max(1e-12)).sqrt() as f32
}

/// `n_bars` bar heights over the fraction `[t0, t1]` of a level (the whole level spans 0..1,
/// so a mini-wave of a whole track passes `0.0, 1.0`). Bars outside the level give `h = 0`.
/// Each bar is the energy mean over exactly the points its span covers (at least the nearest
/// point).
pub fn compute_bars(level: &Levels, refs: &Refs, t0: f64, t1: f64, n_bars: usize) -> Vec<BarValue> {
    if n_bars == 0 {
        return vec![];
    }
    let (n, span) = (level.n, t1 - t0);
    (0..n_bars)
        .map(|i| {
            let f0 = t0 + span * i as f64 / n_bars as f64;
            let f1 = t0 + span * (i + 1) as f64 / n_bars as f64;
            if n == 0 || f1 <= 0.0 || f0 >= 1.0 {
                return BarValue::default();
            }
            let rms = span_rms(level, f0.max(0.0) * n as f64, f1.min(1.0) * n as f64);
            BarValue { h: bar_height(rms, refs) }
        })
        .collect()
}

/// Bar height over `[t0_s, t1_s)` of a pyramid (used by the Canvas2D renderer): picks the level
/// that gives roughly four points per bar and averages in energy. `None` outside the track.
pub fn bar_in_pyramid(p: &Pyramid, refs: &Refs, t0_s: f64, t1_s: f64) -> Option<f32> {
    let ppp = (t1_s - t0_s) / p.dt0_s;
    let k = if ppp < 8.0 { 0 } else { p.level_for(ppp / 4.0) };
    let l = &p.levels[k];
    let dt = p.dt_s(k);
    if l.n == 0 || t1_s <= 0.0 || t0_s >= dt * l.n as f64 {
        return None;
    }
    let rms = span_rms(l, t0_s.max(0.0) / dt, t1_s / dt);
    Some(bar_height(rms, refs))
}
