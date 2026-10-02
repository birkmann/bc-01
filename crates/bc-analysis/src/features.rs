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

/// HPCP-like chroma accumulator (36 bins per band), bass (< 250 Hz) and mid (250 Hz - 4 kHz),
/// plus the per-frame essentia-style HPCP of [`EssHpcp`].
pub struct ChromaExtractor {
    sr: f32,
    fft: Fft,
    win: Vec<f32>,
    buf: Vec<f32>,
    scratch: Vec<f32>,
    ess: EssHpcp,
    /// One 12-bin essentia-style HPCP per frame (index 0 = C), see [`Self::ess_window`].
    pub ess_frames: Vec<[f32; 12]>,
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
            ess: EssHpcp::new(sr_a),
            ess_frames: Vec::new(),
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

    /// Sum of the essentia-style HPCP frames centred in `[start_s, end_s)` (all frames when
    /// none is).
    pub fn ess_window(&self, start_s: f64, end_s: f64) -> [f64; 12] {
        let centre = |i: usize| (i * CHROMA_HOP + CHROMA_FFT / 2) as f64 / self.sr as f64;
        let mut acc = [0.0f64; 12];
        let mut any = false;
        for (i, f) in self.ess_frames.iter().enumerate() {
            let t = centre(i);
            if t >= start_s && t < end_s {
                any = true;
                for p in 0..12 {
                    acc[p] += f[p] as f64;
                }
            }
        }
        if !any {
            for f in &self.ess_frames {
                for p in 0..12 {
                    acc[p] += f[p] as f64;
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
        // essentia-style HPCP on the centred ESS_FFT samples (same hop, same frame centres)
        let off = start + (CHROMA_FFT - ESS_FFT) / 2;
        let h = self.ess.frame(&self.buf[off..off + ESS_FFT]);
        self.ess_frames.push(h);
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

// --------------------------------------------------------------------------------------------

/// Frame size of the essentia-style HPCP (essentia `KeyExtractor` default, hop = frame).
pub const ESS_FFT: usize = 4096;
// The legacy app fed 22.05 kHz audio to an essentia `KeyExtractor` configured for 44.1 kHz, so
// every frequency constant of essentia's defaults acted at half its nominal value on the real
// signal: peaks 25 -> 12.5 Hz .. 3.5 -> 1.75 kHz, whitening grid 100 -> 50 Hz. Pitch classes
// are unaffected (everything moved by exactly one octave).
const ESS_MIN_HZ: f32 = 12.5;
const ESS_MAX_HZ: f32 = 1750.0;
const ESS_MAX_PEAKS: usize = 60;
const ESS_PEAK_THRESHOLD: f32 = 1e-4;
/// HPCP harmonic hypotheses (semitones below the peak, weight) for `harmonics = 4`: the peak as
/// fundamental, 2nd and 4th harmonic (weight 1 + 1 + 1), 3rd harmonic (19.02 st) and 5th
/// harmonic (27.86 st, weight 1 / 1.161).
const ESS_HARMONICS: [(f32, f32); 3] = [(0.0, 3.0), (7.019_55, 1.0), (3.863_137, 0.861_353)];

/// Port of essentia's `KeyExtractor` front end (the reference the imported keys came from):
/// Hann frame normalised to area 2, `SpectralPeaks` (60 strongest, parabolic interpolation),
/// `SpectralWhitening`, and a 12-bin `HPCP` with cosine weighting over one semitone.
pub struct EssHpcp {
    sr: f32,
    fft: Fft,
    win: Vec<f32>,
    scratch: Vec<f32>,
    mags: Vec<f32>,
}

impl EssHpcp {
    pub fn new(sr: f32) -> Self {
        let mut win = hann(ESS_FFT);
        let s: f32 = win.iter().sum();
        for w in &mut win {
            *w *= 2.0 / s;
        }
        Self { sr, fft: Fft::new(ESS_FFT), win, scratch: vec![0.0; ESS_FFT], mags: vec![0.0; ESS_FFT / 2 + 1] }
    }

    /// HPCP of one frame of `ESS_FFT` samples (index 0 = C, not normalised).
    pub fn frame(&mut self, x: &[f32]) -> [f32; 12] {
        for i in 0..ESS_FFT {
            self.scratch[i] = x[i] * self.win[i];
        }
        self.fft.run(&mut self.scratch);
        for (m, c) in self.mags.iter_mut().zip(&self.fft.spectrum) {
            *m = (c.re * c.re + c.im * c.im).sqrt();
        }
        let peaks = self.peaks();
        let white = self.whiten(&peaks);
        let mut h = [0.0f32; 12];
        for (&(f, _), &m) in peaks.iter().zip(&white) {
            if !(ESS_MIN_HZ..=ESS_MAX_HZ).contains(&f) {
                continue;
            }
            for (st, hw) in ESS_HARMONICS {
                let x = 12.0 * (f * (-st / 12.0).exp2() / 261.625_6).log2();
                for i in (x - 0.5).ceil() as i32..=(x + 0.5).floor() as i32 {
                    let w = (std::f32::consts::PI * (x - i as f32).abs()).cos();
                    h[i.rem_euclid(12) as usize] += w * m * m * hw * hw;
                }
            }
        }
        h
    }

    /// essentia `PeakDetection` (interpolated, ordered by magnitude): (Hz, magnitude).
    fn peaks(&self) -> Vec<(f32, f32)> {
        let a = &self.mags;
        let size = a.len();
        let scale = self.sr / 2.0 / (size - 1) as f32;
        let mut out = Vec::new();
        let mut i = (ESS_MIN_HZ / scale).ceil() as usize;
        if i + 1 < size && a[i] > a[i + 1] && a[i] > ESS_PEAK_THRESHOLD {
            out.push((i as f32 * scale, a[i]));
        }
        loop {
            while i + 1 < size - 1 && a[i] >= a[i + 1] {
                i += 1;
            }
            while i + 1 < size - 1 && a[i] < a[i + 1] {
                i += 1;
            }
            let mut j = i;
            while j + 1 < size - 1 && a[j] == a[j + 1] {
                j += 1;
            }
            if j + 1 < size - 1 && a[j + 1] < a[j] && a[j] > ESS_PEAK_THRESHOLD {
                let (bin, val) = if j != i {
                    ((i + j) as f32 * 0.5, a[i])
                } else {
                    let (l, m, r) = (a[j - 1], a[j], a[j + 1]);
                    let d = 0.5 * (l - r) / (l - 2.0 * m + r);
                    (j as f32 + d, m - 0.25 * (l - r) * d)
                };
                if bin * scale > ESS_MAX_HZ {
                    break;
                }
                out.push((bin * scale, val));
            }
            i = j;
            if i + 1 >= size - 1 {
                break;
            }
        }
        out.sort_by(|x, y| y.1.total_cmp(&x.1));
        out.truncate(ESS_MAX_PEAKS);
        out
    }

    /// essentia `SpectralWhitening`: each peak relative to a smoothed spectral envelope (dB),
    /// with a high-frequency tilt; returns linear magnitudes.
    fn whiten(&self, peaks: &[(f32, f32)]) -> Vec<f32> {
        let s = &self.mags;
        let size = s.len();
        let range = self.sr / 2.0;
        let max_f = ESS_MAX_HZ * 1.2;
        let incr = 50.0f32;
        let db = |v: f32| if v < 1e-10 { -100.0 } else { 10.0 * v.log10() };
        let (mut xs, mut ys) = (Vec::new(), Vec::new());
        let mut f = 0.0f32;
        while f <= max_f && f <= range {
            let bf = f - (0.34 * f).max(25.0);
            let ef = f + (0.58 * f).max(25.0);
            let b = ((bf / range * (size as f32 - 1.0) + 0.5) as i64).clamp(0, size as i64 - 1) as usize;
            let e = ((ef / range * (size as f32 - 1.0) + 0.5) as i64).max(b as i64 + 1).min(size as i64) as usize;
            let c = b as f32 / 2.0 + e as f32 / 2.0;
            let hl = e as f32 - c;
            let (mut n, mut wavg) = (0.0f64, 0.0f64);
            for (i, v) in s.iter().enumerate().take(e).skip(b) {
                let mut w = 1.0 - (i as f32 - c).abs() / hl;
                w *= w;
                w *= w;
                let en = (*v as f64) * (*v as f64);
                let w = w as f64 * en;
                wavg += en * w;
                n += w;
            }
            if n != 0.0 {
                wavg /= n;
            }
            xs.push(f);
            ys.push(wavg as f32);
            f += incr;
        }
        if ys.len() >= 2 {
            let l = ys.len();
            ys[l - 1] = ys[l - 2];
        }
        for y in &mut ys {
            *y = 2.0 * db(y.sqrt());
        }
        let env = |x: f32| -> f32 {
            let k = ((x / incr) as usize).min(xs.len().saturating_sub(2));
            let t = ((x - xs[k]) / incr).clamp(0.0, 1.0);
            ys[k] * (1.0 - t) + ys[k + 1] * t
        };
        peaks
            .iter()
            .map(|&(f, m)| {
                let amp = 2.0 * db(m);
                if xs.len() < 2 || f > max_f - incr {
                    return 10f32.powf(amp / 20.0);
                }
                let e = env(f);
                let w = if amp > e {
                    0.0
                } else if amp > e - 30.0 {
                    amp - e
                } else {
                    -200.0
                };
                // essentia: -20 dB per 4 kHz (nominal, i.e. per 2 kHz real)
                10f32.powf((w - f / 100.0) / 20.0)
            })
            .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freqs: &[f32], sr: f32) -> Vec<f32> {
        (0..ESS_FFT).map(|i| freqs.iter().map(|f| 0.2 * (2.0 * std::f32::consts::PI * f * i as f32 / sr).sin()).sum()).collect()
    }

    #[test]
    fn ess_hpcp_puts_a_tone_on_its_pitch_class() {
        let sr = 22_050.0;
        let mut e = EssHpcp::new(sr);
        // A3: pitch class 9 dominates (its fifth-below / third-below hypotheses get less)
        let h = e.frame(&tone(&[220.0], sr));
        let best = (0..12).max_by(|a, b| h[*a].total_cmp(&h[*b])).unwrap();
        assert_eq!(best, 9, "{h:?}");
        assert!(h[9] > 2.0 * h[2] && h[9] > 2.0 * h[5], "{h:?}");
        // above the (halved) 1.75 kHz limit nothing is counted
        let h = e.frame(&tone(&[3000.0], sr));
        assert!(h.iter().all(|v| *v == 0.0), "{h:?}");
        // silence
        assert!(e.frame(&[0.0; ESS_FFT]).iter().all(|v| *v == 0.0));
    }
}
