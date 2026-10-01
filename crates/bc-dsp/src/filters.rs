//! Biquads, Linkwitz-Riley crossovers, the 3-band isolator EQ with kills, and
//! the one-knob LP<->HP filter. All allocation-free after construction.

use std::f64::consts::PI;

/// Direct-form-II-transposed biquad coefficients (a0 normalised to 1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coef {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl Coef {
    pub const IDENTITY: Coef = Coef { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    fn norm(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Coef {
        Coef { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// RBJ low-pass.
    pub fn lowpass(sr: f64, f: f64, q: f64) -> Coef {
        let f = f.clamp(5.0, sr * 0.499);
        let w = 2.0 * PI * f / sr;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        Coef::norm((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }

    /// RBJ high-pass.
    pub fn highpass(sr: f64, f: f64, q: f64) -> Coef {
        let f = f.clamp(5.0, sr * 0.499);
        let w = 2.0 * PI * f / sr;
        let (s, c) = w.sin_cos();
        let alpha = s / (2.0 * q);
        Coef::norm((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha)
    }
}

pub const BUTTERWORTH_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// Stereo biquad.
#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    pub c: Coef,
    z1: [f64; 2],
    z2: [f64; 2],
}

impl Biquad {
    pub fn new(c: Coef) -> Self {
        Self { c, z1: [0.0; 2], z2: [0.0; 2] }
    }
    pub fn set(&mut self, c: Coef) {
        self.c = c;
    }
    #[inline]
    pub fn process(&mut self, ch: usize, x: f64) -> f64 {
        let c = &self.c;
        let y = c.b0 * x + self.z1[ch];
        self.z1[ch] = c.b1 * x - c.a1 * y + self.z2[ch];
        self.z2[ch] = c.b2 * x - c.a2 * y;
        y
    }
    pub fn reset(&mut self) {
        self.z1 = [0.0; 2];
        self.z2 = [0.0; 2];
    }
}

/// Fourth-order Linkwitz-Riley low-pass or high-pass (two cascaded Butterworth).
#[derive(Debug, Clone, Copy)]
pub struct Lr4 {
    a: Biquad,
    b: Biquad,
}

impl Lr4 {
    pub fn low(sr: f64, f: f64) -> Self {
        let c = Coef::lowpass(sr, f, BUTTERWORTH_Q);
        Self { a: Biquad::new(c), b: Biquad::new(c) }
    }
    pub fn high(sr: f64, f: f64) -> Self {
        let c = Coef::highpass(sr, f, BUTTERWORTH_Q);
        Self { a: Biquad::new(c), b: Biquad::new(c) }
    }
    #[inline]
    pub fn process(&mut self, ch: usize, x: f64) -> f64 {
        let y = self.a.process(ch, x);
        self.b.process(ch, y)
    }
}

/// Crossover points of the isolator EQ.
pub const EQ_LOW_HZ: f64 = 250.0;
pub const EQ_HIGH_HZ: f64 = 2500.0;

/// Three-band isolator: LR4 splits at 250 Hz and 2.5 kHz. With every band
/// gain at 1 the sum is flat (allpass), because the low band passes through a
/// matching LR4 pair at the upper crossover. Gains of 0 are kills.
#[derive(Debug, Clone, Copy)]
pub struct Eq3 {
    lo1: Lr4,
    hi1: Lr4,
    lo2: Lr4,
    hi2: Lr4,
    // allpass compensation of the low band at the upper crossover
    lo_c_lo: Lr4,
    lo_c_hi: Lr4,
}

impl Eq3 {
    pub fn new(sr: f64) -> Self {
        Self {
            lo1: Lr4::low(sr, EQ_LOW_HZ),
            hi1: Lr4::high(sr, EQ_LOW_HZ),
            lo2: Lr4::low(sr, EQ_HIGH_HZ),
            hi2: Lr4::high(sr, EQ_HIGH_HZ),
            lo_c_lo: Lr4::low(sr, EQ_HIGH_HZ),
            lo_c_hi: Lr4::high(sr, EQ_HIGH_HZ),
        }
    }

    /// Split into (low, mid, high) of one sample on channel `ch`.
    #[inline]
    pub fn split(&mut self, ch: usize, x: f64) -> (f64, f64, f64) {
        let low = self.lo1.process(ch, x);
        let rest = self.hi1.process(ch, x);
        let low_c = self.lo_c_lo.process(ch, low) + self.lo_c_hi.process(ch, low);
        let mid = self.lo2.process(ch, rest);
        let high = self.hi2.process(ch, rest);
        (low_c, mid, high)
    }

    #[inline]
    pub fn process(&mut self, ch: usize, x: f64, gl: f64, gm: f64, gh: f64) -> f64 {
        let (l, m, h) = self.split(ch, x);
        l * gl + m * gm + h * gh
    }
}

/// dB to linear gain, with `-inf` below `-60` dB for kills.
pub fn db_to_gain(db: f64) -> f64 {
    if db <= -60.0 { 0.0 } else { 10f64.powf(db / 20.0) }
}

/// A one-pole smoother for control values (gains, knobs): `tau` seconds.
#[derive(Debug, Clone, Copy)]
pub struct Smooth {
    pub v: f64,
    k: f64,
}

impl Smooth {
    pub fn new(v: f64, sr: f64, tau_s: f64) -> Self {
        Self { v, k: 1.0 - (-1.0 / (sr * tau_s.max(1e-5))).exp() }
    }
    #[inline]
    pub fn step(&mut self, target: f64) -> f64 {
        self.v += (target - self.v) * self.k;
        self.v
    }
    pub fn set(&mut self, v: f64) {
        self.v = v;
    }
}

/// One-knob DJ filter: `k` in -1 (low-pass closed) .. 0 (open) .. +1 (high-pass
/// closed). Both sections are always running, parked at the band edges for
/// `k = 0` so crossing zero never clicks. Coefficients update every
/// [`KnobFilter::UPDATE`] frames.
#[derive(Debug, Clone, Copy)]
pub struct KnobFilter {
    sr: f64,
    lp: Biquad,
    hp: Biquad,
    k: f64,
    last_k: f64,
    count: u32,
}

impl KnobFilter {
    pub const UPDATE: u32 = 16;
    const LP_OPEN: f64 = 20_000.0;
    const LP_CLOSED: f64 = 150.0;
    const HP_OPEN: f64 = 20.0;
    const HP_CLOSED: f64 = 6_000.0;
    const Q: f64 = 0.9;

    pub fn new(sr: f64) -> Self {
        let mut f = Self {
            sr,
            lp: Biquad::new(Coef::IDENTITY),
            hp: Biquad::new(Coef::IDENTITY),
            k: 0.0,
            last_k: f64::NAN,
            count: 0,
        };
        f.update(0.0);
        f
    }

    fn update(&mut self, k: f64) {
        let lp_f = if k < 0.0 {
            Self::LP_OPEN * (Self::LP_CLOSED / Self::LP_OPEN).powf(-k)
        } else {
            Self::LP_OPEN
        };
        let hp_f = if k > 0.0 {
            Self::HP_OPEN * (Self::HP_CLOSED / Self::HP_OPEN).powf(k)
        } else {
            Self::HP_OPEN
        };
        // At the open end the filters sit at the band edges: transparent in practice.
        let lp_f = lp_f.min(self.sr * 0.45);
        self.lp.set(Coef::lowpass(self.sr, lp_f, Self::Q));
        self.hp.set(Coef::highpass(self.sr, hp_f, Self::Q));
        self.last_k = k;
    }

    /// Call once per frame with the current knob value (cheap: updates every 16 frames).
    #[inline]
    pub fn set_knob(&mut self, k: f64) {
        self.k = k.clamp(-1.0, 1.0);
        self.count += 1;
        if self.count >= Self::UPDATE {
            self.count = 0;
            if (self.k - self.last_k).abs() > 1e-4 || self.last_k.is_nan() {
                let k = self.k;
                self.update(k);
            }
        }
    }

    #[inline]
    pub fn process(&mut self, ch: usize, x: f64) -> f64 {
        let y = self.lp.process(ch, x);
        self.hp.process(ch, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(sr: f64, f: f64, n: usize) -> Vec<f64> {
        (0..n).map(|i| (2.0 * PI * f * i as f64 / sr).sin()).collect()
    }

    fn rms(v: &[f64]) -> f64 {
        (v.iter().map(|x| x * x).sum::<f64>() / v.len() as f64).sqrt()
    }

    fn run_eq(f: f64, gl: f64, gm: f64, gh: f64) -> f64 {
        let sr = 48_000.0;
        let mut eq = Eq3::new(sr);
        let x = tone(sr, f, 48_000);
        let y: Vec<f64> = x.iter().map(|&s| eq.process(0, s, gl, gm, gh)).collect();
        rms(&y[24_000..]) / rms(&x[24_000..])
    }

    #[test]
    fn eq_is_flat_with_unity_gains() {
        for f in [60.0, 200.0, 250.0, 1000.0, 2500.0, 8000.0] {
            let g = run_eq(f, 1.0, 1.0, 1.0);
            assert!((g - 1.0).abs() < 0.03, "{f} Hz: {g}");
        }
    }

    #[test]
    fn eq_kills_isolate_bands() {
        assert!(run_eq(60.0, 0.0, 1.0, 1.0) < 0.05);
        assert!(run_eq(1000.0, 1.0, 0.0, 1.0) < 0.05);
        assert!(run_eq(10_000.0, 1.0, 1.0, 0.0) < 0.05);
        // untouched bands pass
        assert!(run_eq(1000.0, 0.0, 1.0, 0.0) > 0.9);
    }

    #[test]
    fn knob_filter_is_transparent_open_and_cuts_when_closed() {
        let sr = 48_000.0;
        let gain = |k: f64, f: f64| {
            let mut kf = KnobFilter::new(sr);
            let x = tone(sr, f, 48_000);
            let y: Vec<f64> = x
                .iter()
                .map(|&s| {
                    kf.set_knob(k);
                    kf.process(0, s)
                })
                .collect();
            rms(&y[24_000..]) / rms(&x[24_000..])
        };
        assert!((gain(0.0, 1000.0) - 1.0).abs() < 0.02);
        assert!(gain(-1.0, 4000.0) < 0.05);
        assert!(gain(-1.0, 60.0) > 0.8);
        assert!(gain(1.0, 100.0) < 0.05);
        assert!(gain(1.0, 12_000.0) > 0.8);
    }

    #[test]
    fn db_to_gain_kill() {
        assert_eq!(db_to_gain(-80.0), 0.0);
        assert!((db_to_gain(6.0) - 1.9953).abs() < 1e-3);
    }
}
