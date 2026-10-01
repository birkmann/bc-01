//! Streaming waveform builder: one detail point per hop (peak+/peak-/rms and 4th-order
//! Linkwitz-Riley band RMS at 200 Hz / 2.5 kHz), then a 2048-point max-pooled overview.

use crate::format::{Levels, OVERVIEW_POINTS, Waveform, hop_for_rate};
use crate::scale::lin_to_u8;

/// Low/mid crossover (Hz).
pub const XOVER_LOW_HZ: f64 = 200.0;
/// Mid/high crossover (Hz).
pub const XOVER_HIGH_HZ: f64 = 2500.0;

#[derive(Clone, Copy)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// 2nd-order Butterworth (Q = 1/sqrt 2), RBJ cookbook.
    fn butterworth(sr: f64, fc: f64, high: bool) -> Self {
        let fc = fc.min(sr * 0.45);
        let w0 = std::f64::consts::TAU * fc / sr;
        let (s, c) = w0.sin_cos();
        let alpha = s / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let (b0, b1, b2) = if high {
            ((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0)
        } else {
            ((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0)
        };
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: -2.0 * c / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }
    #[inline(always)]
    fn tick(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// LR4 = two cascaded Butterworth sections.
#[derive(Clone, Copy)]
struct Lr4([Biquad; 2]);

impl Lr4 {
    fn new(sr: f64, fc: f64, high: bool) -> Self {
        Self([Biquad::butterworth(sr, fc, high); 2])
    }
    #[inline(always)]
    fn tick(&mut self, x: f64) -> f64 {
        let y = self.0[0].tick(x);
        self.0[1].tick(y)
    }
}

/// Streaming builder. Feed mono samples at the native rate in any chunk size.
pub struct WaveformBuilder {
    sample_rate: u32,
    hop: u32,
    lp_low: Lr4,
    hp_mid: Lr4,
    lp_mid: Lr4,
    hp_high: Lr4,
    // accumulators for the current hop
    count: u32,
    pk_pos: f32,
    pk_neg: f32,
    ss: f64,
    ss_low: f64,
    ss_mid: f64,
    ss_high: f64,
    total: u64,
    detail: Levels,
}

impl WaveformBuilder {
    pub fn new(sample_rate: u32) -> Self {
        let sr = sample_rate.max(1);
        let srf = sr as f64;
        Self {
            sample_rate: sr,
            hop: hop_for_rate(sr),
            lp_low: Lr4::new(srf, XOVER_LOW_HZ, false),
            hp_mid: Lr4::new(srf, XOVER_LOW_HZ, true),
            lp_mid: Lr4::new(srf, XOVER_HIGH_HZ, false),
            hp_high: Lr4::new(srf, XOVER_HIGH_HZ, true),
            count: 0,
            pk_pos: 0.0,
            pk_neg: 0.0,
            ss: 0.0,
            ss_low: 0.0,
            ss_mid: 0.0,
            ss_high: 0.0,
            total: 0,
            detail: Levels::default(),
        }
    }

    pub fn hop_samples(&self) -> u32 {
        self.hop
    }

    /// Samples consumed so far.
    pub fn samples_seen(&self) -> u64 {
        self.total
    }

    /// Detail points completed so far (a partial hop is not counted); useful for progressive
    /// display while a stream loads.
    pub fn detail_so_far(&self) -> &Levels {
        &self.detail
    }

    pub fn push_mono(&mut self, samples: &[f32]) {
        for &x in samples {
            let x = if x.is_finite() { x } else { 0.0 };
            let xd = x as f64;
            let lo = self.lp_low.tick(xd);
            let mi = self.lp_mid.tick(self.hp_mid.tick(xd));
            let hi = self.hp_high.tick(xd);
            if x > self.pk_pos {
                self.pk_pos = x;
            }
            if -x > self.pk_neg {
                self.pk_neg = -x;
            }
            self.ss += xd * xd;
            self.ss_low += lo * lo;
            self.ss_mid += mi * mi;
            self.ss_high += hi * hi;
            self.count += 1;
            if self.count == self.hop {
                self.flush_point();
            }
        }
        self.total += samples.len() as u64;
    }

    fn flush_point(&mut self) {
        if self.count == 0 {
            return;
        }
        let n = self.count as f64;
        let rms = |ss: f64| lin_to_u8((ss / n).sqrt() as f32);
        let v = [
            lin_to_u8(self.pk_pos),
            lin_to_u8(self.pk_neg),
            rms(self.ss),
            rms(self.ss_low),
            rms(self.ss_mid),
            rms(self.ss_high),
        ];
        for (p, x) in v.iter().enumerate() {
            self.detail.planes[p].push(*x);
        }
        self.detail.n += 1;
        self.count = 0;
        self.pk_pos = 0.0;
        self.pk_neg = 0.0;
        self.ss = 0.0;
        self.ss_low = 0.0;
        self.ss_mid = 0.0;
        self.ss_high = 0.0;
    }

    /// Finish: flush the last partial hop and build the overview.
    pub fn finish(mut self, source_hash: [u8; 16]) -> Waveform {
        self.flush_point();
        let overview = self.detail.resample_overview(OVERVIEW_POINTS);
        Waveform {
            sample_rate: self.sample_rate,
            hop_samples: self.hop,
            total_samples: self.total,
            source_hash,
            overview,
            detail: Some(self.detail),
        }
    }
}

/// Convenience: build from one in-memory buffer.
pub fn build_from_mono(sample_rate: u32, samples: &[f32], source_hash: [u8; 16]) -> Waveform {
    let mut b = WaveformBuilder::new(sample_rate);
    b.push_mono(samples);
    b.finish(source_hash)
}
