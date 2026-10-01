//! Key lock: a phase vocoder with identity phase locking (Laroche & Dolson),
//! stereo-linked (the phase advance is derived from the mid channel so the
//! inter-channel phase relation, hence the stereo image, survives).
//!
//! The vocoder plays an input at `rate` while keeping pitch. Output sample `m`
//! corresponds to input position `base + (m - frame_start) * rate`, so
//! [`Stretch::position`] has no latency; the price is that it needs `N`
//! input frames of look-ahead beyond that position ([`Stretch::needed_end`]).

use crate::fft::Fft;
use std::f32::consts::PI;

/// Where the vocoder reads its input: absolute frame positions, zeros outside.
pub trait FrameSource {
    /// Copy `l.len()` frames starting at absolute frame `start` (may be negative).
    fn read(&self, start: i64, l: &mut [f32], r: &mut [f32]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StretchQuality {
    Fast,
    Normal,
    High,
}

impl StretchQuality {
    pub fn size(self) -> usize {
        match self {
            StretchQuality::Fast => 1024,
            StretchQuality::Normal => 2048,
            StretchQuality::High => 4096,
        }
    }
}

pub struct Stretch {
    n: usize,
    hs: usize,
    fft: Fft,
    win: Vec<f32>,
    // scratch
    xl: Vec<f32>,
    xr: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    mag_m: Vec<f32>,
    ph_m: Vec<f32>,
    prev_ph_m: Vec<f32>,
    syn_ph_m: Vec<f32>,
    prev_syn_ph_m: Vec<f32>,
    peaks: Vec<u32>,
    // OLA
    ola_l: Vec<f32>,
    ola_r: Vec<f32>,
    wsum: Vec<f32>,
    ready_l: Vec<f32>,
    ready_r: Vec<f32>,
    ready_pos: usize,
    ready_len: usize,
    out_start: usize, // ring index of the next frame's start
    // timeline
    start_pos: f64,    // where output starts after a reset
    in_pos: f64,       // float input position of the NEXT frame
    prev_ip: i64,      // integer position used for the previous frame
    frame_base: f64,   // float input position matching ready[0]
    primed: bool,
    discard_hops: usize,
    has_prev: bool,
}

impl Stretch {
    pub fn new(q: StretchQuality) -> Self {
        let n = q.size();
        let hs = n / 4;
        let win = (0..n).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / n as f32).cos()).collect();
        let bins = n / 2 + 1;
        Self {
            n,
            hs,
            fft: Fft::new(n),
            win,
            xl: vec![0.0; n],
            xr: vec![0.0; n],
            re: vec![0.0; n],
            im: vec![0.0; n],
            mag_m: vec![0.0; bins],
            ph_m: vec![0.0; bins],
            prev_ph_m: vec![0.0; bins],
            syn_ph_m: vec![0.0; bins],
            prev_syn_ph_m: vec![0.0; bins],
            peaks: Vec::with_capacity(bins),
            ola_l: vec![0.0; n],
            ola_r: vec![0.0; n],
            wsum: vec![0.0; n],
            ready_l: vec![0.0; hs],
            ready_r: vec![0.0; hs],
            ready_pos: 0,
            ready_len: 0,
            out_start: 0,
            start_pos: 0.0,
            in_pos: 0.0,
            prev_ip: 0,
            frame_base: 0.0,
            primed: false,
            discard_hops: 0,
            has_prev: false,
        }
    }

    pub fn frame_size(&self) -> usize {
        self.n
    }

    /// Start (or restart) so that the next emitted output sample corresponds
    /// to input position `pos`.
    pub fn reset(&mut self, pos: f64) {
        self.ola_l.iter_mut().for_each(|s| *s = 0.0);
        self.ola_r.iter_mut().for_each(|s| *s = 0.0);
        self.wsum.iter_mut().for_each(|s| *s = 0.0);
        self.prev_ph_m.iter_mut().for_each(|s| *s = 0.0);
        self.syn_ph_m.iter_mut().for_each(|s| *s = 0.0);
        self.prev_syn_ph_m.iter_mut().for_each(|s| *s = 0.0);
        self.ready_pos = 0;
        self.ready_len = 0;
        self.out_start = 0;
        self.start_pos = pos;
        self.in_pos = pos;
        self.frame_base = pos;
        self.prev_ip = pos.round() as i64;
        self.has_prev = false;
        self.primed = false;
        self.discard_hops = 3;
    }

    /// Input position of the next output sample.
    pub fn position(&self, rate: f64) -> f64 {
        if self.ready_pos < self.ready_len {
            self.frame_base + (self.ready_pos as f64) * rate
        } else if self.primed {
            self.in_pos
        } else {
            self.start_pos
        }
    }

    /// Absolute input frame (exclusive) the next analysis frame needs data up to.
    pub fn needed_end(&self, _rate: f64) -> i64 {
        let next = if self.primed { self.in_pos } else { self.start_pos };
        next.round() as i64 + self.n as i64 + 2
    }

    /// True when a whole analysis frame can be computed from data up to `avail_end`.
    pub fn can_frame(&self, rate: f64, avail_end: i64) -> bool {
        avail_end >= self.needed_end(rate)
    }

    /// Whether samples are buffered ready to emit.
    pub fn has_ready(&self) -> bool {
        self.ready_pos < self.ready_len
    }

    /// Pop one ready frame (non-blocking).
    #[inline]
    pub fn pop(&mut self) -> Option<[f32; 2]> {
        if self.ready_pos < self.ready_len {
            let i = self.ready_pos;
            self.ready_pos += 1;
            Some([self.ready_l[i], self.ready_r[i]])
        } else {
            None
        }
    }

    /// Compute frames until one hop of output is ready. Call only when
    /// `can_frame` holds. `rate` is the instantaneous playback rate.
    pub fn refill(&mut self, src: &impl FrameSource, rate: f64) {
        let rate = rate.clamp(0.25, 4.0);
        if !self.primed {
            // Pre-roll: the first frames start before `start_pos` so that the
            // first emitted hop is fully overlapped (4 frames contribute).
            self.in_pos = self.start_pos - 3.0 * self.hs as f64 * rate;
            for _ in 0..3 {
                self.frame(src, rate, false);
            }
            self.primed = true;
        }
        self.frame(src, rate, true);
    }

    fn frame(&mut self, src: &impl FrameSource, rate: f64, emit: bool) {
        let n = self.n;
        let hs = self.hs;
        let ip = self.in_pos.round() as i64;
        src.read(ip, &mut self.xl, &mut self.xr);
        // z = (L + jR) * window
        for i in 0..n {
            self.re[i] = self.xl[i] * self.win[i];
            self.im[i] = self.xr[i] * self.win[i];
        }
        self.fft.run(&mut self.re, &mut self.im, false);
        let ha = if self.has_prev { (ip - self.prev_ip).max(1) as f32 } else { (hs as f32 * rate as f32).max(1.0) };
        let bins = n / 2 + 1;

        // Mid-channel spectrum phase drives the phase advance. Separate L/R:
        // X_L[k] = (Z[k] + conj Z[N-k]) / 2 ; X_R[k] = (Z[k] - conj Z[N-k]) / 2j.
        for k in 0..bins {
            let k2 = (n - k) % n;
            let (zr, zi) = (self.re[k], self.im[k]);
            let (cr, ci) = (self.re[k2], -self.im[k2]);
            let lr = 0.5 * (zr + cr);
            let li = 0.5 * (zi + ci);
            let rr = 0.5 * (zi - ci);
            let ri = -0.5 * (zr - cr);
            let mr = 0.5 * (lr + rr);
            let mi = 0.5 * (li + ri);
            self.mag_m[k] = (mr * mr + mi * mi).sqrt();
            self.ph_m[k] = mi.atan2(mr);
        }

        // Peaks of the mid magnitude spectrum.
        self.peaks.clear();
        for k in 0..bins {
            let m = self.mag_m[k];
            let l1 = if k >= 1 { self.mag_m[k - 1] } else { 0.0 };
            let l2 = if k >= 2 { self.mag_m[k - 2] } else { 0.0 };
            let r1 = if k + 1 < bins { self.mag_m[k + 1] } else { 0.0 };
            let r2 = if k + 2 < bins { self.mag_m[k + 2] } else { 0.0 };
            if m > l1 && m > l2 && m >= r1 && m >= r2 && m > 1e-9 {
                self.peaks.push(k as u32);
            }
        }
        if self.peaks.is_empty() {
            self.peaks.push(0);
        }

        // Synthesis phase of the mid channel.
        let two_pi = 2.0 * PI;
        let hs_f = hs as f32;
        for pi in 0..self.peaks.len() {
            let p = self.peaks[pi] as usize;
            let omega = two_pi * p as f32 / n as f32;
            let mut dphi = self.ph_m[p] - self.prev_ph_m[p] - omega * ha;
            dphi -= two_pi * (dphi / two_pi).round();
            let freq = omega + dphi / ha;
            self.syn_ph_m[p] =
                if self.has_prev { self.prev_syn_ph_m[p] + freq * hs_f } else { self.ph_m[p] };
        }
        // Peaks first computed above; now lock the others to their peak.
        let mut owner_idx = 0usize;
        for k in 0..bins {
            while owner_idx + 1 < self.peaks.len() {
                let a = self.peaks[owner_idx] as usize;
                let b = self.peaks[owner_idx + 1] as usize;
                if k > (a + b) / 2 {
                    owner_idx += 1;
                } else {
                    break;
                }
            }
            let p = self.peaks[owner_idx] as usize;
            if k != p {
                self.syn_ph_m[k] = self.syn_ph_m[p] + (self.ph_m[k] - self.ph_m[p]);
            }
        }

        // Rebuild L and R spectra with the new mid phase, preserving each
        // channel's phase relative to the mid.
        // Output Z'[k] = Y_L[k] + j Y_R[k] with Y conjugate-symmetric.
        for k in 0..bins {
            let k2 = (n - k) % n;
            let (zr, zi) = (self.re[k], self.im[k]);
            let (cr, ci) = (self.re[k2], -self.im[k2]);
            let lr = 0.5 * (zr + cr);
            let li = 0.5 * (zi + ci);
            let rr = 0.5 * (zi - ci);
            let ri = -0.5 * (zr - cr);
            let rot = if k == 0 || k == n / 2 { 0.0 } else { self.syn_ph_m[k] - self.ph_m[k] };
            let (s, c) = rot.sin_cos();
            // rotate both channels by the same angle (keeps their mutual phase)
            let ylr = lr * c - li * s;
            let yli = lr * s + li * c;
            let yrr = rr * c - ri * s;
            let yri = rr * s + ri * c;
            // Z[k] = Y_L[k] + j Y_R[k]
            self.re[k] = ylr - yri;
            self.im[k] = yli + yrr;
            if k != 0 && k != n / 2 {
                // mirror bin N-k: Y[N-k] = conj(Y[k])
                self.re[k2] = ylr + yri;
                self.im[k2] = -yli + yrr;
            }
        }
        self.fft.run(&mut self.re, &mut self.im, true);

        // Window, normalise (1/N) and overlap-add.
        let inv_n = 1.0 / n as f32;
        for i in 0..n {
            let pos = (self.out_start + i) % n;
            let w = self.win[i];
            self.ola_l[pos] += self.re[i] * inv_n * w;
            self.ola_r[pos] += self.im[i] * inv_n * w;
            self.wsum[pos] += w * w;
        }

        // bookkeeping for the next frame
        self.prev_ph_m[..bins].copy_from_slice(&self.ph_m[..bins]);
        self.prev_syn_ph_m[..bins].copy_from_slice(&self.syn_ph_m[..bins]);
        self.prev_ip = ip;
        self.has_prev = true;
        let frame_pos = self.in_pos;
        self.in_pos += hs as f64 * rate;

        // First hop of this frame's range is complete: emit or discard it.
        for i in 0..hs {
            let pos = (self.out_start + i) % n;
            let w = self.wsum[pos].max(1e-3);
            if emit {
                self.ready_l[i] = self.ola_l[pos] / w;
                self.ready_r[i] = self.ola_r[pos] / w;
            }
            self.ola_l[pos] = 0.0;
            self.ola_r[pos] = 0.0;
            self.wsum[pos] = 0.0;
        }
        self.out_start = (self.out_start + hs) % n;
        if emit {
            self.ready_pos = 0;
            self.ready_len = hs;
            // ready[0] corresponds to the input position of the frame that was
            // 3 hops ago in output terms: `frame_pos` aligned by the pre-roll.
            self.frame_base = frame_pos;
        } else if self.discard_hops > 0 {
            self.discard_hops -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Buf {
        l: Vec<f32>,
        r: Vec<f32>,
    }
    impl FrameSource for Buf {
        fn read(&self, start: i64, l: &mut [f32], r: &mut [f32]) {
            for i in 0..l.len() {
                let p = start + i as i64;
                if p >= 0 && (p as usize) < self.l.len() {
                    l[i] = self.l[p as usize];
                    r[i] = self.r[p as usize];
                } else {
                    l[i] = 0.0;
                    r[i] = 0.0;
                }
            }
        }
    }

    fn sine(sr: f32, f: f32, n: usize) -> Buf {
        let l: Vec<f32> = (0..n).map(|i| (2.0 * PI * f * i as f32 / sr).sin() * 0.5).collect();
        Buf { r: l.clone(), l }
    }

    fn render(st: &mut Stretch, b: &Buf, rate: f64, frames: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames);
        while out.len() < frames {
            if let Some(f) = st.pop() {
                out.push(f[0]);
            } else {
                st.refill(b, rate);
            }
        }
        out
    }

    /// Dominant frequency via zero-crossing count over a settled region.
    fn freq_of(x: &[f32], sr: f32) -> f32 {
        let mut crossings = 0;
        for w in x.windows(2) {
            if w[0] <= 0.0 && w[1] > 0.0 {
                crossings += 1;
            }
        }
        crossings as f32 * sr / x.len() as f32
    }

    #[test]
    fn unity_rate_reproduces_the_input() {
        let sr = 48_000.0;
        let b = sine(sr, 440.0, 96_000);
        let mut st = Stretch::new(StretchQuality::Normal);
        st.reset(10_000.0);
        let out = render(&mut st, &b, 1.0, 20_000);
        let mut err = 0.0f32;
        let mut sig = 0.0f32;
        for i in 4000..20_000 {
            err += (out[i] - b.l[10_000 + i]).powi(2);
            sig += b.l[10_000 + i].powi(2);
        }
        assert!(err / sig < 0.01, "relative error {}", err / sig);
    }

    #[test]
    fn keeps_pitch_at_other_rates() {
        let sr = 48_000.0;
        let b = sine(sr, 1000.0, 400_000);
        for rate in [0.94, 1.06] {
            let mut st = Stretch::new(StretchQuality::Normal);
            st.reset(20_000.0);
            let out = render(&mut st, &b, rate, 48_000);
            let f = freq_of(&out[8000..], sr);
            assert!((f - 1000.0).abs() < 8.0, "rate {rate}: pitch {f}");
            // and the input advanced at `rate`
            let adv = st.position(rate) - 20_000.0;
            assert!((adv / 48_000.0 - rate).abs() < 0.02, "advanced {adv} for rate {rate}");
        }
    }
}
