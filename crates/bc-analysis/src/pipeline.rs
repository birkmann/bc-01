//! The single streaming pass: decode once, feed loudness, waveform, onset, chroma and energy in
//! lock-step, then run the whole-track tempo/beat/downbeat/phrase/key/mix-point stages on the
//! (small) accumulated feature series. Memory is flat in track length: the only things that grow
//! are ~170 onset frames/s of f32 features, one 12-bin key HPCP per ~0.19 s chroma frame and the
//! 6-byte detail waveform points.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use bc_music::beatgrid::GridQuery;
use bc_music::mixpoints::{self, Envelope, MixPlan};
use bc_types::analysis::BeatGrid;
use bc_waveform::{Waveform, WaveformBuilder};
use ebur128::{EbuR128, Mode};

use crate::decode::{DecodeError, Sink, StreamInfo, decode_stream};
use crate::dsp::{Decimator, decimation_for};
use crate::energy::{EnergyCalibration, EnergyRaw, raw_from};
use crate::features::{ChromaExtractor, FrameSeries, ONSET_FFT, ONSET_HOP, OnsetExtractor, RmsMeter};
use crate::key::{Feat, KeyProfiles, KeyResult, classify, classify_hpcp, features};
use crate::rhythm::{find_downbeats, find_phrases};
use crate::tempo::{self, TempoResult};

/// Systematic delay between the flux peak and the physical onset (ms), calibrated on the click
/// track (`tests/synthetic.rs`).
pub const ONSET_LATENCY_MS: f64 = 10.7;
const REPLAYGAIN_REFERENCE_LUFS: f64 = -18.0;

#[derive(Debug, thiserror::Error)]
pub enum AnalyzeError {
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Clone)]
pub struct AnalyzeOptions {
    pub loudness: bool,
    pub waveform: bool,
    pub tempo: bool,
    pub key: bool,
    pub profiles: Arc<KeyProfiles>,
    pub energy: Arc<EnergyCalibration>,
    /// Keep the raw onset envelope and 36-bin chromas in the result (calibration tooling only).
    pub debug: bool,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        Self {
            loudness: true,
            waveform: true,
            tempo: true,
            key: true,
            profiles: Arc::new(KeyProfiles::default()),
            energy: Arc::new(EnergyCalibration::default()),
            debug: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoudnessOut {
    pub lufs: f64,
    pub lra: f64,
    pub true_peak_dbtp: f64,
    pub replaygain_gain: f64,
}

#[derive(Debug, Clone)]
pub struct TempoOut {
    pub bpm: f64,
    pub confidence: f64,
    pub candidates: Vec<f64>,
    pub grid: BeatGrid,
    pub downbeat_confidence: Option<f64>,
    pub inlier_ratio: f64,
}

#[derive(Debug, Clone)]
pub struct KeyOut {
    pub result: KeyResult,
    pub feat: Feat,
}

#[derive(Debug, Clone, Default)]
pub struct Timings {
    pub decode_s: f64,
    pub post_s: f64,
    pub total_s: f64,
}

#[derive(Debug, Clone)]
pub struct TrackAnalysis {
    pub duration_ms: i64,
    pub sample_rate: u32,
    pub channels: usize,
    pub loudness: Option<LoudnessOut>,
    pub tempo: Option<TempoOut>,
    pub key: Option<KeyOut>,
    pub energy_raw: EnergyRaw,
    /// 1..10
    pub energy_v2: f64,
    /// Legacy `min(1, rms*4)` (kept so `analysis.energy` stays comparable).
    pub energy_legacy: f64,
    pub waveform: Option<Waveform>,
    pub mix: Option<MixPlan>,
    pub timings: Timings,
    /// Only with `AnalyzeOptions::debug`.
    pub debug_onset: Vec<f32>,
    pub debug_fps: f32,
    pub debug_chroma: Option<Vec<Vec<f64>>>,
}

struct Pass {
    opts: AnalyzeOptions,
    info: Option<StreamInfo>,
    ebu: Option<EbuR128>,
    wf: Option<WaveformBuilder>,
    dec: Option<Decimator>,
    onset: Option<OnsetExtractor>,
    chroma: Option<ChromaExtractor>,
    rms: RmsMeter,
    momentary: Vec<f32>,
    frames: u64,
    next_mom: u64,
    sr_a: f32,
    mono: Vec<f32>,
    stereo: Vec<f32>,
    dec_out: Vec<f32>,
    failed: Option<String>,
}

impl Sink for Pass {
    fn info(&mut self, info: StreamInfo) -> Result<(), DecodeError> {
        let ch = info.channels.clamp(1, 2) as u32;
        if self.opts.loudness {
            let mode = Mode::I | Mode::LRA | Mode::TRUE_PEAK;
            self.ebu = Some(EbuR128::new(ch, info.sample_rate, mode).map_err(|e| DecodeError::Format(e.to_string()))?);
        }
        if self.opts.waveform {
            self.wf = Some(WaveformBuilder::new(info.sample_rate));
        }
        let d = decimation_for(info.sample_rate);
        self.sr_a = info.sample_rate as f32 / d as f32;
        self.dec = Some(Decimator::new(d));
        if self.opts.tempo {
            self.onset = Some(OnsetExtractor::new(self.sr_a));
        } else {
            // onset series still feeds energy/tempo; without tempo only chroma/energy are needed
            self.onset = Some(OnsetExtractor::new(self.sr_a));
        }
        if self.opts.key {
            self.chroma = Some(ChromaExtractor::new(self.sr_a));
        }
        self.info = Some(info);
        Ok(())
    }

    fn data(&mut self, chunk: &[f32]) {
        let Some(info) = self.info else { return };
        let ch = info.channels.max(1);
        let frames = chunk.len() / ch;
        self.mono.clear();
        // stereo view for loudness
        let stereo: &[f32] = if ch <= 2 {
            chunk
        } else {
            self.stereo.clear();
            for f in 0..frames {
                let row = &chunk[f * ch..(f + 1) * ch];
                let (mut l, mut r, mut nl, mut nr) = (0.0, 0.0, 0.0, 0.0);
                for (i, v) in row.iter().enumerate() {
                    if i % 2 == 0 {
                        l += v;
                        nl += 1.0;
                    } else {
                        r += v;
                        nr += 1.0;
                    }
                }
                self.stereo.push(l / nl);
                self.stereo.push(if nr > 0.0 { r / nr } else { l / nl });
            }
            &self.stereo
        };
        let sch = ch.min(2);
        if sch == 1 {
            self.mono.extend_from_slice(stereo);
        } else {
            for f in 0..frames {
                self.mono.push(0.5 * (stereo[2 * f] + stereo[2 * f + 1]));
            }
        }
        if let Some(e) = self.ebu.as_mut() {
            if e.add_frames_f32(stereo).is_err() {
                self.failed = Some("loudness".into());
            }
            self.frames += frames as u64;
            // momentary loudness every 0.5 s of audio (for the energy feature)
            let step = (info.sample_rate / 2) as u64;
            while self.frames >= self.next_mom + step {
                self.next_mom += step;
                if let Ok(m) = e.loudness_momentary() {
                    self.momentary.push(m as f32);
                }
            }
        } else {
            self.frames += frames as u64;
        }
        if let Some(w) = self.wf.as_mut() {
            w.push_mono(&self.mono);
        }
        self.rms.push(&self.mono);
        if let Some(d) = self.dec.as_mut() {
            self.dec_out.clear();
            d.push(&self.mono, &mut self.dec_out);
            if let Some(o) = self.onset.as_mut() {
                o.push(&self.dec_out);
            }
            if let Some(c) = self.chroma.as_mut() {
                c.push(&self.dec_out);
            }
        }
    }
}

/// ms of track time for onset frame `f` (fractional).
fn frame_time_ms(f: f64, sr_a: f32, dec_delay_samples: f64) -> f64 {
    ((f * ONSET_HOP as f64 + (ONSET_FFT / 2) as f64 + dec_delay_samples) / sr_a as f64) * 1000.0 + ONSET_LATENCY_MS
}

pub fn analyze_file(path: &Path, opts: &AnalyzeOptions) -> Result<TrackAnalysis, AnalyzeError> {
    let t_all = Instant::now();
    let mut pass = Pass {
        opts: opts.clone(),
        info: None,
        ebu: None,
        wf: None,
        dec: None,
        onset: None,
        chroma: None,
        rms: RmsMeter::default(),
        momentary: Vec::new(),
        frames: 0,
        next_mom: 0,
        sr_a: 22050.0,
        mono: Vec::new(),
        stereo: Vec::new(),
        dec_out: Vec::new(),
        failed: None,
    };
    let total_frames = decode_stream(path, &mut pass)?;
    let t_decode = t_all.elapsed().as_secs_f64();
    let info = pass.info.ok_or_else(|| AnalyzeError::Internal("no stream info".into()))?;
    let duration_ms = (total_frames as f64 * 1000.0 / info.sample_rate as f64).round() as i64;
    let d = pass.dec.as_ref().map(|d| d.factor()).unwrap_or(1);
    let dec_delay = if d > 1 { 8.0 } else { 0.0 };
    let sr_a = pass.sr_a;

    let loudness = pass.ebu.as_ref().and_then(|e| {
        let lufs = e.loudness_global().ok()?;
        if !lufs.is_finite() {
            return None;
        }
        let lra = e.loudness_range().unwrap_or(0.0);
        let mut tp = 0.0f64;
        for c in 0..info.channels.clamp(1, 2) as u32 {
            tp = tp.max(e.true_peak(c).unwrap_or(0.0));
        }
        let dbtp = if tp > 0.0 { 20.0 * tp.log10() } else { -120.0 };
        Some(LoudnessOut { lufs, lra, true_peak_dbtp: dbtp, replaygain_gain: REPLAYGAIN_REFERENCE_LUFS - lufs })
    });

    let waveform = pass.wf.take().map(|w| {
        let hash = bc_waveform::store::source_hash(path).unwrap_or([0; 16]);
        w.finish(hash)
    });

    let series: FrameSeries = pass.onset.take().map(|o| o.finish()).unwrap_or_default();
    let ftime = |f: f64| frame_time_ms(f, sr_a, dec_delay);
    let (debug_onset, debug_fps) = if opts.debug { (series.onset.clone(), series.fps) } else { (Vec::new(), 0.0) };

    // tempo / beats / downbeats / phrases
    let mut tempo_out: Option<TempoOut> = None;
    if opts.tempo && series.onset.len() > 600 {
        if let Some(TempoResult { bpm, confidence, candidates, beats_frames, mut grid, inlier_ratio, .. }) =
            tempo::analyze(&series.onset, series.fps, ftime)
        {
            let mut db_conf = None;
            if let Some(db) = find_downbeats(&beats_frames, &series) {
                // tracked-beat parity -> grid beat index of the downbeat
                let t = ftime(beats_frames[db.phase as usize]);
                let k = grid.beat_pos(t).round() as i64;
                grid.downbeat_phase = Some(k.rem_euclid(grid.beats_per_bar as i64) as u8);
                db_conf = Some(db.confidence);
                let bar_frames: Vec<f64> =
                    beats_frames.iter().enumerate().filter(|(i, _)| i % 4 == db.phase as usize).map(|(_, f)| *f).collect();
                grid.phrase_starts_ms = find_phrases(&bar_frames, &series, ftime);
            }
            tempo_out = Some(TempoOut { bpm, confidence, candidates, grid, downbeat_confidence: db_conf, inlier_ratio });
        }
    }

    if let Some(c) = pass.chroma.as_mut() {
        c.finish();
    }
    // Key is estimated on the same window the legacy essentia analysis used (120 s starting 25 %
    // into the track): sections of a long track differ in key, and matching the window makes the
    // result comparable with the imported values (essentia agrees with itself only ~82 % between
    // that window and the whole track).
    let debug_chroma = if opts.debug {
        pass.chroma.as_ref().map(|c| {
            let dur_s = duration_ms as f64 / 1000.0;
            let st = bc_music::beatgrid::legacy_excerpt_start_s(dur_s);
            let w = c.window(st, st + bc_music::beatgrid::LEGACY_EXCERPT_S);
            let e = c.ess_window(st, st + bc_music::beatgrid::LEGACY_EXCERPT_S);
            // [whole track x4 channels, excerpt x4 channels, excerpt essentia-style HPCP]
            vec![
                c.bass.to_vec(),
                c.mid.to_vec(),
                c.harm.to_vec(),
                c.spec.to_vec(),
                w[0].to_vec(),
                w[1].to_vec(),
                w[2].to_vec(),
                w[3].to_vec(),
                e.to_vec(),
            ]
        })
    } else {
        None
    };
    let key_out = pass.chroma.as_ref().filter(|c| c.frames >= 4).and_then(|c| {
        let dur_s = duration_ms as f64 / 1000.0;
        let st = bc_music::beatgrid::legacy_excerpt_start_s(dur_s);
        let w = c.window(st, st + bc_music::beatgrid::LEGACY_EXCERPT_S);
        let feat = features([&w[0], &w[1], &w[2], &w[3]])?;
        // essentia-style HPCP + edma profiles first; the fitted profiles only when it is empty
        let h = c.ess_window(st, st + bc_music::beatgrid::LEGACY_EXCERPT_S);
        let result = classify_hpcp(&h).unwrap_or_else(|| classify(&feat, &opts.profiles));
        Some(KeyOut { result, feat })
    });

    let energy_raw = raw_from(&pass.momentary, &series.onset, &series.centroid);
    let energy_v2 = opts.energy.energy_v2(&energy_raw) as f64;
    let energy_legacy = (pass.rms.rms() * 4.0).min(1.0);

    let mix = waveform.as_ref().map(|wf| {
        let (level, low) = bc_waveform::legacy::mix_envelope(wf);
        let grid = tempo_out.as_ref().map(|t| &t.grid);
        let bpm = tempo_out.as_ref().map(|t| t.bpm);
        let env = Envelope { level: &level, low: &low };
        mixpoints::plan_from_envelope(&env, duration_ms, bpm, grid, 16)
    });

    let total = t_all.elapsed().as_secs_f64();
    Ok(TrackAnalysis {
        duration_ms,
        sample_rate: info.sample_rate,
        channels: info.channels,
        loudness,
        tempo: tempo_out,
        key: key_out,
        energy_raw,
        energy_v2,
        energy_legacy,
        waveform,
        mix,
        timings: Timings { decode_s: t_decode, post_s: total - t_decode, total_s: total },
        debug_onset,
        debug_fps,
        debug_chroma,
    })
}
