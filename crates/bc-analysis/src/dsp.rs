//! Small DSP building blocks: decimator, windows, FFT wrapper.

use std::f32::consts::PI;
use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

/// Analysis rate target (Hz). 44.1/48 kHz sources decimate by 2, 88.2/96 kHz by 4, ...
pub const TARGET_RATE: f32 = 22_050.0;

pub fn decimation_for(sample_rate: u32) -> usize {
    ((sample_rate as f32 / TARGET_RATE).round() as usize).max(1)
}

/// Streaming FIR low-pass + decimate-by-D (windowed sinc, Blackman). Pass-through for D = 1.
pub struct Decimator {
    d: usize,
    taps: Vec<f32>,
    carry: Vec<f32>,
    /// Input index (relative to `carry` start) of the next output sample's *window start*.
    next: usize,
}

impl Decimator {
    pub fn new(d: usize) -> Self {
        let d = d.max(1);
        let n_taps = if d == 1 { 1 } else { 16 * d + 1 };
        let mut taps = vec![1.0f32; n_taps];
        if d > 1 {
            let fc = 0.45 / d as f32; // cycles/sample at the input rate
            let m = (n_taps - 1) as f32 / 2.0;
            let mut sum = 0.0;
            for (i, t) in taps.iter_mut().enumerate() {
                let x = i as f32 - m;
                let sinc = if x.abs() < 1e-6 { 2.0 * fc } else { (2.0 * PI * fc * x).sin() / (PI * x) };
                let w = 0.42 - 0.5 * (2.0 * PI * i as f32 / (n_taps - 1) as f32).cos()
                    + 0.08 * (4.0 * PI * i as f32 / (n_taps - 1) as f32).cos();
                *t = sinc * w;
                sum += *t;
            }
            for t in &mut taps {
                *t /= sum;
            }
        }
        Self { d, taps, carry: Vec::new(), next: 0 }
    }

    pub fn factor(&self) -> usize {
        self.d
    }

    pub fn push(&mut self, x: &[f32], out: &mut Vec<f32>) {
        if self.d == 1 {
            out.extend_from_slice(x);
            return;
        }
        self.carry.extend_from_slice(x);
        let n = self.taps.len();
        while self.next + n <= self.carry.len() {
            let w = &self.carry[self.next..self.next + n];
            let mut acc = 0.0f32;
            for (a, b) in w.iter().zip(&self.taps) {
                acc += a * b;
            }
            out.push(acc);
            self.next += self.d;
        }
        // drop consumed history
        if self.next > 0 {
            let drop = self.next.min(self.carry.len());
            self.carry.drain(..drop);
            self.next -= drop;
        }
    }
}

pub fn hann(n: usize) -> Vec<f32> {
    (0..n).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / n as f32).cos()).collect()
}

/// Reusable real FFT of fixed size.
pub struct Fft {
    pub n: usize,
    plan: Arc<dyn RealToComplex<f32>>,
    scratch: Vec<Complex<f32>>,
    pub spectrum: Vec<Complex<f32>>,
}

impl Fft {
    pub fn new(n: usize) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let plan = planner.plan_fft_forward(n);
        let scratch = plan.make_scratch_vec();
        let spectrum = plan.make_output_vec();
        Self { n, plan, scratch, spectrum }
    }
    /// Transform `input` (length n, clobbered) into `self.spectrum`.
    pub fn run(&mut self, input: &mut [f32]) {
        self.plan
            .process_with_scratch(input, &mut self.spectrum, &mut self.scratch)
            .expect("fft sizes are fixed at construction");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimator_preserves_a_low_tone_and_kills_an_alias() {
        let sr = 44_100.0f32;
        let n = 44_100;
        let low: Vec<f32> = (0..n).map(|i| (2.0 * PI * 1000.0 * i as f32 / sr).sin()).collect();
        let high: Vec<f32> = (0..n).map(|i| (2.0 * PI * 15_000.0 * i as f32 / sr).sin()).collect();
        let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
        let mut d = Decimator::new(2);
        let mut o = Vec::new();
        d.push(&low, &mut o);
        assert!((rms(&o[200..]) - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.02, "{}", rms(&o[200..]));
        let mut d = Decimator::new(2);
        let mut o = Vec::new();
        d.push(&high, &mut o);
        assert!(rms(&o[200..]) < 0.02, "{}", rms(&o[200..]));
    }

    #[test]
    fn decimator_is_chunk_invariant() {
        let x: Vec<f32> = (0..10_000).map(|i| ((i * 37 % 101) as f32 / 50.0) - 1.0).collect();
        let mut a = Vec::new();
        Decimator::new(2).push(&x, &mut a);
        let mut d = Decimator::new(2);
        let mut b = Vec::new();
        for c in x.chunks(77) {
            d.push(c, &mut b);
        }
        assert_eq!(a.len(), b.len());
        for (p, q) in a.iter().zip(&b) {
            assert!((p - q).abs() < 1e-6);
        }
    }
}
