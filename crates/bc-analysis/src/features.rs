//! Streaming feature extractors fed from the decimated mono stream:
//! * [`OnsetExtractor`]: mel log-spectral flux (+ kick-band flux and 3-band energies) at ~172 fps;
//! * [`ChromaExtractor`]: HPCP-style peak-picked chroma (36 bins, bass and mid split);
//! * [`RmsMeter`]: RMS / zero-crossing statistics for energy.

use crate::dsp::{Fft, hann};

pub const ONSET_FFT: usize = 1024;
pub const ONSET_HOP: usize = 128;
const N_MELS: usize = 40;

fn hz_to_mel(f: f32) -> f32 {
    2595.0 * (1.0 + f / 700.0).log10()
}
fn mel_to_hz(m: f32) -> f32 {
    700.0 * (10f32.powf(m / 2595.0) - 1.0)
}

/// Per-frame series produced by the onset extractor. All vectors have equal length.
#[derive(Debug, Default, Clone)]
pub struct FrameSeries {
    pub fps: f32,
    /// Full-band log-mel spectral flux (half-wave rectified).
    pub onset: Vec<f32>,
    /// Flux restricted to the kick/bass bands (< ~200 Hz).
    pub low_flux: Vec<f32>,
    /// Flux of the 200 Hz - 2.5 kHz bands (claps, snares, chord stabs).
    pub mid_flux: Vec<f32>,
    /// log10 band energies (20-200, 200-2500, 2500+ Hz).
    pub e_low: Vec<f32>,
    pub e_mid: Vec<f32>,
    pub e_high: Vec<f32>,
    /// Spectral centroid in Hz per frame.
    pub centroid: Vec<f32>,
}

pub struct OnsetExtractor {
    sr: f32,
    fft: Fft,
    win: Vec<f32>,
    buf: Vec<f32>,
    // mel filters: (first_bin, weights)
    mel: Vec<(usize, Vec<f32>)>,
    low_bands: usize,
    mid_bands_end: usize,
    prev: Vec<f32>,
    bin_low_end: usize,
    bin_mid_end: usize,
    bin_start: usize,
    freqs: Vec<f32>,
    pub series: FrameSeries,
    scratch: Vec<f32>,
}

impl OnsetExtractor {
    pub fn new(sr_a: f32) -> Self {
        let n = ONSET_FFT;
        let bins = n / 2 + 1;
        let hz = |b: usize| b as f32 * sr_a / n as f32;
        let fmin = 30.0f32;
        let fmax = (sr_a / 2.0 - 200.0).min(10_000.0);
        let (m0, m1) = (hz_to_mel(fmin), hz_to_mel(fmax));
        let centers: Vec<f32> =
            (0..N_MELS + 2).map(|i| mel_to_hz(m0 + (m1 - m0) * i as f32 / (N_MELS + 1) as f32)).collect();
        let mut mel = Vec::with_capacity(N_MELS);
        for b in 0..N_MELS {
            let (lo, c, hi) = (centers[b], centers[b + 1], centers[b + 2]);
            let first = ((lo / sr_a * n as f32).floor() as usize).max(1);
            let last = ((hi / sr_a * n as f32).ceil() as usize).min(bins - 1);
            let mut w = Vec::new();
            for k in first..=last {
                let f = hz(k);
                let v = if f < c { (f - lo) / (c - lo) } else { (hi - f) / (hi - c) };
                w.push(v.max(0.0));
            }
            // area-normalise so wide high bands do not dominate
            let s: f32 = w.iter().sum();
            if s > 0.0 {
                for x in &mut w {
                    *x /= s;
                }
            }
            mel.push((first, w));
        }
        let low_bands = centers[1..=N_MELS].iter().filter(|c| **c < 200.0).count().max(1);
        let mid_bands_end = centers[1..=N_MELS].iter().filter(|c| **c < 2500.0).count();
        let freqs: Vec<f32> = (0..bins).map(hz).collect();
        let bin_of = |f: f32| ((f / sr_a * n as f32).round() as usize).min(bins - 1);
        let mut series = FrameSeries::default();
        series.fps = sr_a / ONSET_HOP as f32;
        Self {
            sr: sr_a,
            fft: Fft::new(n),
            win: hann(n),
            buf: Vec::with_capacity(n * 2),
            mel,
            low_bands,
            mid_bands_end,
            prev: vec![0.0; N_MELS],
            bin_start: bin_of(20.0).max(1),
            bin_low_end: bin_of(200.0),
            bin_mid_end: bin_of(2500.0),
            freqs,
            series,
            scratch: vec![0.0; n],
        }
    }

    pub fn push(&mut self, x: &[f32]) {
        self.buf.extend_from_slice(x);
        let n = ONSET_FFT;
        let mut start = 0;
        while start + n <= self.buf.len() {
            self.frame(start);
            start += ONSET_HOP;
        }
        if start > 0 {
            self.buf.drain(..start);
        }
    }

    fn frame(&mut self, start: usize) {
        let n = ONSET_FFT;
        for i in 0..n {
            self.scratch[i] = self.buf[start + i] * self.win[i];
        }
        self.fft.run(&mut self.scratch);
        let norm = 2.0 / n as f32;
        let sp = &self.fft.spectrum;
        let mag = |k: usize| (sp[k].re * sp[k].re + sp[k].im * sp[k].im).sqrt() * norm;

        let (mut flux, mut low, mut mid) = (0.0f32, 0.0f32, 0.0f32);
        for (b, (first, w)) in self.mel.iter().enumerate() {
            let mut m = 0.0;
            for (j, wt) in w.iter().enumerate() {
                m += wt * mag(first + j);
            }
            let l = (1.0 + 1000.0 * m).ln();
            let d = (l - self.prev[b]).max(0.0);
            self.prev[b] = l;
            flux += d;
            if b < self.low_bands {
                low += d;
            } else if b < self.mid_bands_end {
                mid += d;
            }
        }
        let (mut el, mut em, mut eh) = (0.0f32, 0.0f32, 0.0f32);
        let (mut csum, mut wsum) = (0.0f32, 0.0f32);
        for k in self.bin_start..sp.len() {
            let p = {
                let m = mag(k);
                m * m
            };
            if k < self.bin_low_end {
                el += p;
            } else if k < self.bin_mid_end {
                em += p;
            } else {
                eh += p;
            }
            csum += p * self.freqs[k];
            wsum += p;
        }
        let s = &mut self.series;
        s.onset.push(flux / N_MELS as f32);
        s.low_flux.push(low / self.low_bands as f32);
        s.mid_flux.push(mid / (self.mid_bands_end.saturating_sub(self.low_bands)).max(1) as f32);
        s.e_low.push((el + 1e-12).log10());
        s.e_mid.push((em + 1e-12).log10());
        s.e_high.push((eh + 1e-12).log10());
        s.centroid.push(if wsum > 1e-12 { csum / wsum } else { 0.0 });
        let _ = self.sr;
    }

    pub fn finish(self) -> FrameSeries {
        self.series
    }
}

// --------------------------------------------------------------------------------------------

pub const CHROMA_FFT: usize = 8192;
pub const CHROMA_HOP: usize = 4096;
pub const BINS: usize = 36;

/// HPCP-like chroma accumulator (36 bins per band), bass (< 250 Hz) and mid (250 Hz - 4 kHz).
pub struct ChromaExtractor {
    sr: f32,
    fft: Fft,
    win: Vec<f32>,
    buf: Vec<f32>,
    scratch: Vec<f32>,
    pub bass: [f64; BINS],
    pub mid: [f64; BINS],
    /// harmonic-summation chroma over 40 Hz - 3.5 kHz peaks (essentia-HPCP-like)
    pub harm: [f64; BINS],
    /// all-bin compressed spectrum folded to chroma, 60 Hz - 2 kHz
    pub spec: [f64; BINS],
    pub frames: u32,
    /// Per-block (BLOCK_FRAMES frames each) accumulators, so any time window can be re-summed.
    pub blocks: Vec<[[f64; BINS]; 4]>,
    cur: [[f64; BINS]; 4],
    frames_seen: u32,
}

pub const BLOCK_FRAMES: u32 = 22; // ~4.1 s at hop 4096 / 22.05 kHz

impl ChromaExtractor {
    pub fn new(sr_a: f32) -> Self {
        Self {
            sr: sr_a,
            fft: Fft::new(CHROMA_FFT),
            win: hann(CHROMA_FFT),
            buf: Vec::with_capacity(CHROMA_FFT * 2),
            scratch: vec![0.0; CHROMA_FFT],
            bass: [0.0; BINS],
            mid: [0.0; BINS],
            harm: [0.0; BINS],
            spec: [0.0; BINS],
            frames: 0,
            blocks: Vec::new(),
            cur: [[0.0; BINS]; 4],
            frames_seen: 0,
        }
    }

    pub fn push(&mut self, x: &[f32]) {
        self.buf.extend_from_slice(x);
        let mut start = 0;
        while start + CHROMA_FFT <= self.buf.len() {
            self.frame(start);
            start += CHROMA_HOP;
        }
        if start > 0 {
            self.buf.drain(..start);
        }
    }

    /// Seconds (in decimated-stream time) at which frame `i` starts.
    pub fn frame_start_s(&self, i: u32) -> f64 {
        i as f64 * CHROMA_HOP as f64 / self.sr as f64
    }

    /// Sum of the blocks overlapping `[start_s, end_s)` (4 channels: bass, mid, harm, spec).
    pub fn window(&self, start_s: f64, end_s: f64) -> [[f64; BINS]; 4] {
        let mut acc = [[0.0; BINS]; 4];
        let block_s = BLOCK_FRAMES as f64 * CHROMA_HOP as f64 / self.sr as f64;
        let n_total = self.blocks.len();
        for (b, blk) in self.blocks.iter().enumerate() {
            let (a, z) = (b as f64 * block_s, (b + 1) as f64 * block_s);
            if z > start_s && a < end_s || (b + 1 == n_total && acc.iter().all(|c| c.iter().all(|v| *v == 0.0)) && a < end_s) {
                for c in 0..4 {
                    for i in 0..BINS {
                        acc[c][i] += blk[c][i];
                    }
                }
            }
        }
        acc
    }

    /// Close the trailing partial block (call once after the last `push`).
    pub fn finish(&mut self) {
        if self.frames_seen % BLOCK_FRAMES != 0 {
            self.blocks.push(self.cur);
            self.cur = [[0.0; BINS]; 4];
        }
    }

    fn frame(&mut self, start: usize) {
        let n = CHROMA_FFT;
        for i in 0..n {
            self.scratch[i] = self.buf[start + i] * self.win[i];
        }
        self.fft.run(&mut self.scratch);
        let sp = &self.fft.spectrum;
        let mags: Vec<f32> = sp.iter().map(|c| (c.re * c.re + c.im * c.im).sqrt()).collect();
        let max = mags.iter().cloned().fold(0.0f32, f32::max);
        self.frames_seen += 1;
        if max < 1e-3 {
            self.close_block_if_full();
            return;
        }
        let bin_hz = self.sr / n as f32;
        let (lo_bin, hi_bin) = ((55.0 / bin_hz) as usize, ((4000.0 / bin_hz) as usize).min(mags.len() - 2));
        let thresh = max * 0.01;
        let mut fb = [0.0f64; BINS];
        let mut fm = [0.0f64; BINS];
        let mut fh = [0.0f64; BINS];
        let mut fs = [0.0f64; BINS];
        let add36 = |dst: &mut [f64; BINS], f: f32, w: f64| {
            let x = 12.0 * (f / 261.6256).log2();
            let pc = x.rem_euclid(12.0) * 3.0;
            let lo = pc.floor();
            let frac = (pc - lo) as f64;
            let i0 = lo as usize % BINS;
            dst[i0] += w * (1.0 - frac);
            dst[(i0 + 1) % BINS] += w * frac;
        };
        for k in ((60.0 / bin_hz) as usize).max(1)..((2000.0 / bin_hz) as usize).min(mags.len()) {
            add36(&mut fs, k as f32 * bin_hz, (mags[k] / max).sqrt() as f64);
        }
        for k in lo_bin.max(1)..hi_bin {
            let m = mags[k];
            if m < thresh || m < mags[k - 1] || m <= mags[k + 1] {
                continue;
            }
            // parabolic interpolation of the peak position
            let (a, b, c) = (mags[k - 1].max(1e-12).ln(), m.max(1e-12).ln(), mags[k + 1].max(1e-12).ln());
            let denom = a - 2.0 * b + c;
            let off = if denom.abs() > 1e-9 { 0.5 * (a - c) / denom } else { 0.0 };
            let f = (k as f32 + off.clamp(-0.5, 0.5)) * bin_hz;
            let x = 12.0 * (f / 261.6256).log2(); // semitones from C4
            let pc = x.rem_euclid(12.0) * 3.0; // 0..36
            let lo = pc.floor();
            let frac = (pc - lo) as f64;
            let i0 = lo as usize % BINS;
            let i1 = (i0 + 1) % BINS;
            let w = (m / max).sqrt() as f64;
            let dst = if f < 250.0 { &mut fb } else { &mut fm };
            dst[i0] += w * (1.0 - frac);
            dst[i1] += w * frac;
            // harmonic summation: this peak may be the h-th harmonic of a lower fundamental
            if f < 3500.0 {
                for h in 1..=4u32 {
                    let f0 = f / h as f32;
                    if f0 >= 40.0 {
                        add36(&mut fh, f0, w * 0.6f64.powi(h as i32 - 1));
                    }
                }
            }
        }
        // frame weight: loudness-ish, each band normalised by its own frame total so one noisy
        // frame cannot dominate
        let wgt = (max as f64).sqrt();
        for (ci, (acc, fr)) in [(&mut self.bass, &fb), (&mut self.mid, &fm), (&mut self.harm, &fh), (&mut self.spec, &fs)].into_iter().enumerate() {
            let s: f64 = fr.iter().sum();
            if s > 0.0 {
                for i in 0..BINS {
                    let v = wgt * fr[i] / s;
                    acc[i] += v;
                    self.cur[ci][i] += v;
                }
            }
        }
        self.frames += 1;
        self.close_block_if_full();
    }

    fn close_block_if_full(&mut self) {
        if self.frames_seen % BLOCK_FRAMES == 0 {
            self.blocks.push(self.cur);
            self.cur = [[0.0; BINS]; 4];
        }
    }
}

/// Fold 36-bin chroma to 12 bins after estimating the tuning offset (0..2 thirds of a semitone).
/// Returns (12-bin chroma, tuning offset in bins).
pub fn fold_with_tuning(c36: &[f64; BINS]) -> ([f64; 12], usize) {
    // class centres are bins 0,3,6,... for offset 0; offset t shifts the centre by t bins
    let mut best = (0usize, -1.0f64);
    for t in 0..3 {
        let s: f64 = (0..12).map(|p| c36[(p * 3 + t) % BINS]).sum();
        if s > best.1 {
            best = (t, s);
        }
    }
    let t = best.0;
    let mut out = [0.0f64; 12];
    for p in 0..12 {
        // centre bin + its two neighbours (triangular 0.5/1/0.5)
        let c = p * 3 + t;
        out[p] = c36[c % BINS] + 0.5 * c36[(c + 1) % BINS] + 0.5 * c36[(c + BINS - 1) % BINS];
    }
    (out, t)
}

#[derive(Default)]
pub struct RmsMeter {
    pub sum_sq: f64,
    pub n: u64,
}

impl RmsMeter {
    pub fn push(&mut self, x: &[f32]) {
        for v in x {
            self.sum_sq += (*v as f64) * (*v as f64);
        }
        self.n += x.len() as u64;
    }
    pub fn rms(&self) -> f64 {
        if self.n == 0 { 0.0 } else { (self.sum_sq / self.n as f64).sqrt() }
    }
}
