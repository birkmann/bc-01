//! Tempo-synced stereo echo: dotted-eighth feedback delay with a darkening
//! filter in the loop (low-pass 3 kHz, high-pass 120 Hz), as in the browser graph.

use crate::filters::{Biquad, Coef, BUTTERWORTH_Q};

pub const FEEDBACK: f64 = 0.6;
pub const LOWPASS_HZ: f64 = 3000.0;
pub const HIGHPASS_HZ: f64 = 120.0;
pub const WET: f64 = 0.8;
pub const MIN_S: f64 = 0.2;
pub const MAX_S: f64 = 0.7;
pub const DEFAULT_S: f64 = 0.375;

/// Delay time for a tempo: a dotted eighth (0.75 of a beat), bounded.
pub fn delay_for_bpm(bpm: Option<f64>) -> f64 {
    match bpm {
        Some(b) if b > 0.0 => (60.0 / b * 0.75).clamp(MIN_S, MAX_S),
        _ => DEFAULT_S,
    }
}

pub struct Echo {
    sr: f64,
    buf: Vec<f32>, // interleaved stereo, `cap` frames
    cap: usize,
    w: usize,
    delay_frames: f64,
    lp: Biquad,
    hp: Biquad,
    wet: f64,
    wet_target: f64,
    wet_k: f64,
    feedback: f64,
}

impl Echo {
    pub fn new(sr: f64) -> Self {
        let cap = (sr * 2.0) as usize + 8;
        Self {
            sr,
            buf: vec![0.0; cap * 2],
            cap,
            w: 0,
            delay_frames: DEFAULT_S * sr,
            lp: Biquad::new(Coef::lowpass(sr, LOWPASS_HZ, BUTTERWORTH_Q)),
            hp: Biquad::new(Coef::highpass(sr, HIGHPASS_HZ, BUTTERWORTH_Q)),
            wet: WET,
            wet_target: WET,
            wet_k: 1.0 - (-1.0 / (sr * 0.02)).exp(),
            feedback: FEEDBACK,
        }
    }

    pub fn set_delay_s(&mut self, s: f64) {
        self.delay_frames = (s * self.sr).clamp(1.0, (self.cap - 4) as f64);
    }

    /// Duck or restore the wet return (smoothed, click-free).
    pub fn set_wet(&mut self, w: f64) {
        self.wet_target = w;
    }

    /// Clear the tail (used on stop).
    pub fn flush(&mut self) {
        self.buf.iter_mut().for_each(|s| *s = 0.0);
        self.lp.reset();
        self.hp.reset();
    }

    #[inline]
    fn read(&self, ch: usize) -> f64 {
        let pos = self.w as f64 - self.delay_frames;
        let pos = if pos < 0.0 { pos + self.cap as f64 } else { pos };
        let i0 = pos.floor() as usize % self.cap;
        let i1 = (i0 + 1) % self.cap;
        let fr = pos - pos.floor();
        self.buf[i0 * 2 + ch] as f64 * (1.0 - fr) + self.buf[i1 * 2 + ch] as f64 * fr
    }

    /// One frame: `send` is the summed send bus; returns the wet return.
    #[inline]
    pub fn process(&mut self, send: [f64; 2]) -> [f64; 2] {
        self.wet += (self.wet_target - self.wet) * self.wet_k;
        let mut out = [0.0; 2];
        for ch in 0..2 {
            let d = self.read(ch);
            let fb = self.hp.process(ch, self.lp.process(ch, d * self.feedback));
            self.buf[self.w * 2 + ch] = (send[ch] + fb) as f32;
            out[ch] = d * self.wet;
        }
        self.w = (self.w + 1) % self.cap;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_times_follow_dotted_eighth() {
        assert!((delay_for_bpm(Some(128.0)) - 0.3515625).abs() < 1e-9);
        assert_eq!(delay_for_bpm(None), DEFAULT_S);
        assert_eq!(delay_for_bpm(Some(300.0)), MIN_S);
        assert_eq!(delay_for_bpm(Some(90.0)), 0.5);
        assert_eq!(delay_for_bpm(Some(60.0)), MAX_S);
        assert_eq!(delay_for_bpm(Some(30.0)), MAX_S);
    }

    #[test]
    fn echo_repeats_decay() {
        let sr = 48_000.0;
        let mut e = Echo::new(sr);
        e.set_delay_s(0.25);
        let n = (sr * 3.0) as usize;
        let mut peaks = [0f64; 8];
        for i in 0..n {
            let x = if i == 0 { [1.0, 1.0] } else { [0.0, 0.0] };
            let y = e.process(x);
            let slot = (i as f64 / (sr * 0.25)) as usize;
            if slot < 8 {
                peaks[slot] = peaks[slot].max(y[0].abs());
            }
        }
        assert!(peaks[0] < 1e-6);
        assert!(peaks[1] > 0.05);
        assert!(peaks[2] < peaks[1]);
        assert!(peaks[3] < peaks[2]);
    }
}
