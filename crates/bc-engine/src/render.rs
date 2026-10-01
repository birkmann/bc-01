//! Offline render of a planned DJ set: the same `bc-dsp` graph as live
//! playback, driven faster than real time through [`OfflineRig`].
//!
//! Honours each slot's cue range, tempo (vinyl rate, or key lock when asked),
//! loudness alignment (a trim knob, never more than +-6 dB toward the set's
//! median) and the transition type/overlap into the next slot. Output is WAV
//! (hound) or MP3 (the `ffmpeg` binary, when present).
//!
//! Also keeps the legacy `render.py` overlap-add math (`equal_power_curves`,
//! `gain_for`, `declick`, `overlap_add`) with its tests, as the reference for
//! what the graph must reproduce.

use crate::decode::{DecodeError, Source, decode_range};
use bc_dsp::beatmatch::{Curve, BlendSpec, CUT_TAIL_S, ECHO_TAIL_S, EchoSpec};
use bc_dsp::mixer::Cmd;
use bc_dsp::offline::OfflineRig;
use bc_dsp::stretch::StretchQuality;
use bc_types::player::{Quantise, TransitionKind};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: usize = 2;
pub const MP3_BITRATE: &str = "320k";
/// A DJ's trim knob, not a mastering chain: nudge each track toward the set's
/// median loudness, never by more than this.
pub const MAX_GAIN_DB: f64 = 6.0;
/// Micro-fade at hard cuts and the mix's edges, so a cut never clicks.
pub const DECLICK_MS: u32 = 15;

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("decode: {0}")]
    Decode(#[from] DecodeError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("wav: {0}")]
    Wav(#[from] hound::Error),
    #[error("encoder: {0}")]
    Encoder(String),
}

/// One slot as the renderer needs it (`RenderSlot` of `render.py`, plus the transition type).
#[derive(Debug, Clone)]
pub struct RenderSlot {
    pub path: PathBuf,
    pub cue_in_ms: i64,
    pub cue_out_ms: i64,
    pub tempo_adjust_pct: f64,
    pub key_lock: bool,
    /// Overlap of the blend INTO the next slot, in (played) ms; 0 = cut.
    pub overlap_out_ms: i64,
    pub loudness_lufs: Option<f64>,
    /// Transition into the next slot.
    pub transition: TransitionKind,
    /// Echo the outgoing out over the blend (only meaningful for blends).
    pub echo: bool,
}

impl RenderSlot {
    pub fn new(path: impl Into<PathBuf>, cue_in_ms: i64, cue_out_ms: i64) -> Self {
        Self {
            path: path.into(),
            cue_in_ms,
            cue_out_ms,
            tempo_adjust_pct: 0.0,
            key_lock: true,
            overlap_out_ms: 0,
            loudness_lufs: None,
            transition: TransitionKind::Blend,
            echo: false,
        }
    }
}

// ---------------------------------------------------------------------------
// the legacy math, kept as the reference
// ---------------------------------------------------------------------------

/// `(fade_in, fade_out)` over `n` samples, constant combined power.
pub fn equal_power_curves(n: usize) -> (Vec<f32>, Vec<f32>) {
    let step = if n > 1 { std::f32::consts::FRAC_PI_2 / (n - 1) as f32 } else { 0.0 };
    let t: Vec<f32> = (0..n).map(|i| i as f32 * step).collect();
    (t.iter().map(|x| x.sin()).collect(), t.iter().map(|x| x.cos()).collect())
}

/// Linear gain aligning a track toward the set's target loudness.
pub fn gain_for(lufs: Option<f64>, target: Option<f64>) -> f64 {
    let (Some(l), Some(t)) = (lufs, target) else { return 1.0 };
    10f64.powf((t - l).clamp(-MAX_GAIN_DB, MAX_GAIN_DB) / 20.0)
}

/// Micro-fades on a segment's raw edges (frames x 2, in place).
pub fn declick(seg: &mut [f32], sr: u32, head: bool, tail: bool) {
    let frames = seg.len() / 2;
    let n = frames.min((sr as usize * DECLICK_MS as usize) / 1000);
    if n <= 1 {
        return;
    }
    for i in 0..n {
        let g = i as f32 / (n - 1) as f32;
        if head {
            seg[i * 2] *= g;
            seg[i * 2 + 1] *= g;
        }
        if tail {
            let j = frames - n + i;
            seg[j * 2] *= 1.0 - g;
            seg[j * 2 + 1] *= 1.0 - g;
        }
    }
}

/// Mix `(segment, overlap_out_samples)` pairs into one stream. Each segment's
/// tail (its overlap into the next) is faded out and held back; the next
/// segment's head is faded in and summed with it. A zero overlap is a butt joint.
pub fn overlap_add(segments: Vec<(Vec<f32>, usize)>) -> Vec<f32> {
    let mut out: Vec<f32> = Vec::new();
    let mut pending: Option<Vec<f32>> = None;
    for (mut seg, overlap_out) in segments {
        if let Some(p) = pending.take() {
            let n = (p.len() / 2).min(seg.len() / 2);
            if n > 0 {
                let (fi, _) = equal_power_curves(n);
                for i in 0..n {
                    seg[i * 2] = seg[i * 2] * fi[i] + p[i * 2];
                    seg[i * 2 + 1] = seg[i * 2 + 1] * fi[i] + p[i * 2 + 1];
                }
            }
            if p.len() / 2 > n {
                // the incoming ran out under the blend: emit the rest of the outgoing tail
                seg.extend_from_slice(&p[n * 2..]);
            }
        }
        let frames = seg.len() / 2;
        let overlap = overlap_out.min(frames);
        let body_end = frames - overlap;
        out.extend_from_slice(&seg[..body_end * 2]);
        if overlap > 0 {
            let (_, fo) = equal_power_curves(overlap);
            let mut tail = seg[body_end * 2..].to_vec();
            for i in 0..overlap {
                tail[i * 2] *= fo[i];
                tail[i * 2 + 1] *= fo[i];
            }
            pending = Some(tail);
        }
    }
    if let Some(p) = pending {
        out.extend_from_slice(&p);
    }
    out
}

// ---------------------------------------------------------------------------
// the graph-driven render
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RenderOptions {
    pub sample_rate: u32,
    pub quality: StretchQuality,
    /// Align loudness toward the median of the slots' LUFS.
    pub align_loudness: bool,
    pub ffmpeg: String,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self { sample_rate: SAMPLE_RATE, quality: StretchQuality::Normal, align_loudness: true, ffmpeg: "ffmpeg".into() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RenderReport {
    pub frames: u64,
    pub seconds: f64,
    pub slots: usize,
    /// Wall-clock speed-up over real time.
    pub realtime_factor: f64,
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let m = v.len() / 2;
    Some(if v.len() % 2 == 1 { v[m] } else { 0.5 * (v[m - 1] + v[m]) })
}

/// The blend spec the renderer runs for the transition out of `slot` (length is
/// the overlap; tempo is already applied on the incoming deck).
fn blend_for(slot: &RenderSlot, overlap_s: f64) -> BlendSpec {
    let echo = slot.echo.then_some(EchoSpec { send: 0.65, hold_s: (overlap_s * 0.5).clamp(0.5, 3.0), up_s: 0.3, bpm: None });
    BlendSpec {
        kind: slot.transition,
        length_s: overlap_s,
        curve: if slot.transition == TransitionKind::Blend || slot.transition == TransitionKind::BassSwap { Curve::EqualPower } else { Curve::Linear },
        incoming_start_s: 0.0,
        echo,
        sync: None,
        park_tail_s: if echo.is_some() { ECHO_TAIL_S } else { CUT_TAIL_S },
        quantise: Quantise::Off,
        phase_lock: false,
    }
}

/// Render the set, handing PCM blocks (interleaved f32 stereo) to `sink`.
///
/// Slot `i` plays on deck `i % 2`. The blend into slot `i + 1` starts
/// `overlap_out_ms` before slot `i` ends (a cut: `DECLICK_MS` before), so the
/// total is the sum of the played lengths minus the overlaps, exactly as the
/// legacy renderer's overlap-add.
pub fn render_pcm(
    slots: &[RenderSlot],
    opts: &RenderOptions,
    sink: &mut dyn FnMut(&[f32]) -> Result<(), RenderError>,
) -> Result<RenderReport, RenderError> {
    let t0 = std::time::Instant::now();
    let sr = opts.sample_rate;
    let mut lufs: Vec<f64> = slots.iter().filter_map(|s| s.loudness_lufs).collect();
    let target = if opts.align_loudness { median(&mut lufs) } else { None };
    let mut rig = OfflineRig::new(sr, opts.quality);
    let mut total_frames = 0u64;
    let mut out: Vec<f32> = Vec::new();
    if slots.is_empty() {
        return Ok(RenderReport::default());
    }

    let load = |s: &RenderSlot| -> Result<(Arc<Vec<f32>>, u64), RenderError> {
        let (pcm, base) =
            decode_range(&Source::File(s.path.clone()), sr, s.cue_in_ms as f64 / 1000.0, Some(s.cue_out_ms as f64 / 1000.0))?;
        Ok((Arc::new(pcm), base))
    };
    let rate_of = |s: &RenderSlot| 1.0 + s.tempo_adjust_pct / 100.0;
    // Frames a slot plays for: its source range read at its rate.
    let played = |s: &RenderSlot, pcm: &Arc<Vec<f32>>| -> u64 { ((pcm.len() / 2) as f64 / rate_of(s)).round() as u64 };
    let cue_slot = |rig: &mut OfflineRig, deck: usize, s: &RenderSlot, pcm: &Arc<Vec<f32>>, base: u64| {
        let r = rate_of(s);
        rig.cue(deck, pcm.clone(), base, base, r, s.key_lock && (r - 1.0).abs() > 1e-6, gain_for(s.loudness_lufs, target), None);
    };

    // slot 0 starts the mix
    let (mut pcm, mut base) = load(&slots[0])?;
    cue_slot(&mut rig, 0, &slots[0], &pcm, base);
    rig.send(Cmd::Play { deck: 0 });
    let mut slot_start = 0u64; // engine frame the current slot started at

    for i in 0..slots.len() {
        let deck = i & 1;
        let s = &slots[i];
        let len = played(s, &pcm);
        if i + 1 < slots.len() {
            let blend_s = if s.overlap_out_ms > 0 { s.overlap_out_ms as f64 / 1000.0 } else { DECLICK_MS as f64 / 1000.0 };
            let blend_frames = ((blend_s * sr as f64).round() as u64).min(len);
            let blend_t = slot_start + len - blend_frames;
            // decode and cue the next slot on the other deck a little before the blend
            let cue_t = blend_t.saturating_sub(2048).max(rig.frames());
            rig.render((cue_t - rig.frames()) as usize, &mut out);
            flush(&mut out, &mut total_frames, sink)?;
            let n = &slots[i + 1];
            let (npcm, nbase) = load(n)?;
            cue_slot(&mut rig, 1 - deck, n, &npcm, nbase);
            rig.render((blend_t - rig.frames()) as usize, &mut out);
            flush(&mut out, &mut total_frames, sink)?;
            let mut spec = blend_for(s, blend_s);
            if s.overlap_out_ms == 0 {
                spec.kind = TransitionKind::Cut;
                spec.echo = None;
                spec.curve = Curve::Linear;
            }
            rig.send(Cmd::StartTransition { out: deck as u8, inc: (1 - deck) as u8, spec });
            slot_start = blend_t;
            pcm = npcm;
            base = nbase;
        } else {
            // the last slot plays out; the echo tail of a final blend rings a moment longer
            let end_t = slot_start + len + (CUT_TAIL_S * sr as f64) as u64;
            rig.render(end_t.saturating_sub(rig.frames()) as usize, &mut out);
            flush(&mut out, &mut total_frames, sink)?;
        }
    }
    let _ = base;
    let secs = total_frames as f64 / sr as f64;
    Ok(RenderReport {
        frames: total_frames,
        seconds: secs,
        slots: slots.len(),
        realtime_factor: secs / t0.elapsed().as_secs_f64().max(1e-9),
    })
}

fn flush(out: &mut Vec<f32>, total: &mut u64, sink: &mut dyn FnMut(&[f32]) -> Result<(), RenderError>) -> Result<(), RenderError> {
    if !out.is_empty() {
        *total += (out.len() / 2) as u64;
        sink(out)?;
        out.clear();
    }
    Ok(())
}

/// Render to a 16-bit WAV file.
pub fn render_wav(slots: &[RenderSlot], opts: &RenderOptions, path: &Path) -> Result<RenderReport, RenderError> {
    let spec = hound::WavSpec { channels: 2, sample_rate: opts.sample_rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec)?;
    let rep = render_pcm(slots, opts, &mut |pcm| {
        for &x in pcm {
            w.write_sample((x.clamp(-1.0, 1.0) * 32767.0).round() as i16)?;
        }
        Ok(())
    })?;
    w.finalize()?;
    Ok(rep)
}

/// Render to MP3 through the `ffmpeg` binary (320 kbps), streaming PCM as it is produced.
pub fn render_mp3(slots: &[RenderSlot], opts: &RenderOptions, path: &Path) -> Result<RenderReport, RenderError> {
    let mut child = std::process::Command::new(&opts.ffmpeg)
        .args(["-nostdin", "-v", "error", "-y", "-f", "f32le", "-ac", "2", "-ar"])
        .arg(opts.sample_rate.to_string())
        .args(["-i", "pipe:0", "-f", "mp3", "-b:a", MP3_BITRATE])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| RenderError::Encoder(format!("cannot start {}: {e}", opts.ffmpeg)))?;
    let mut stdin = child.stdin.take().ok_or_else(|| RenderError::Encoder("no encoder stdin".into()))?;
    let rep = render_pcm(slots, opts, &mut |pcm| {
        let mut bytes = Vec::with_capacity(pcm.len() * 4);
        for &x in pcm {
            bytes.extend_from_slice(&x.clamp(-1.0, 1.0).to_le_bytes());
        }
        stdin.write_all(&bytes)?;
        Ok(())
    });
    drop(stdin);
    let status = child.wait()?;
    let rep = rep?;
    if !status.success() {
        return Err(RenderError::Encoder(format!("encoder exited with {status}")));
    }
    Ok(rep)
}

/// Output container of a streamed render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Wav,
    Mp3,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        match s.to_ascii_lowercase().as_str() {
            "wav" => Some(Format::Wav),
            "mp3" => Some(Format::Mp3),
            _ => None,
        }
    }
    pub fn content_type(self) -> &'static str {
        match self {
            Format::Wav => "audio/wav",
            Format::Mp3 => "audio/mpeg",
        }
    }
    pub fn ext(self) -> &'static str {
        match self {
            Format::Wav => "wav",
            Format::Mp3 => "mp3",
        }
    }
}

/// A WAV header for a stream of unknown length (sizes `0xFFFFFFFF`), 16-bit stereo.
pub fn streaming_wav_header(sample_rate: u32) -> Vec<u8> {
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes()); // PCM
    h.extend_from_slice(&2u16.to_le_bytes()); // channels
    h.extend_from_slice(&sample_rate.to_le_bytes());
    h.extend_from_slice(&(sample_rate * 4).to_le_bytes()); // byte rate
    h.extend_from_slice(&4u16.to_le_bytes()); // block align
    h.extend_from_slice(&16u16.to_le_bytes()); // bits
    h.extend_from_slice(b"data");
    h.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    h
}

pub type ByteSender = tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>;

fn send_bytes(tx: &ByteSender, b: Vec<u8>) -> Result<(), RenderError> {
    tx.blocking_send(Ok(bytes::Bytes::from(b)))
        .map_err(|_| RenderError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client went away")))
}

/// Render the set and push the encoded bytes into `tx` as they are produced, so a
/// long mix downloads while it renders. Blocking: run on a blocking thread. When
/// the receiver is dropped (client disconnected) the render stops.
pub fn render_stream(slots: &[RenderSlot], opts: &RenderOptions, format: Format, tx: &ByteSender) -> Result<RenderReport, RenderError> {
    match format {
        Format::Wav => {
            send_bytes(tx, streaming_wav_header(opts.sample_rate))?;
            render_pcm(slots, opts, &mut |pcm| {
                let mut b = Vec::with_capacity(pcm.len() * 2);
                for &x in pcm {
                    b.extend_from_slice(&((x.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
                }
                send_bytes(tx, b)
            })
        }
        Format::Mp3 => {
            let mut child = std::process::Command::new(&opts.ffmpeg)
                .args(["-nostdin", "-v", "error", "-f", "f32le", "-ac", "2", "-ar"])
                .arg(opts.sample_rate.to_string())
                .args(["-i", "pipe:0", "-f", "mp3", "-b:a", MP3_BITRATE, "pipe:1"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| RenderError::Encoder(format!("cannot start {}: {e}", opts.ffmpeg)))?;
            let mut stdin = child.stdin.take().ok_or_else(|| RenderError::Encoder("no encoder stdin".into()))?;
            let mut stdout = child.stdout.take().ok_or_else(|| RenderError::Encoder("no encoder stdout".into()))?;
            let tx2 = tx.clone();
            let reader = std::thread::spawn(move || {
                use std::io::Read;
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(n) = stdout.read(&mut buf) {
                    if n == 0 || tx2.blocking_send(Ok(bytes::Bytes::copy_from_slice(&buf[..n]))).is_err() {
                        break;
                    }
                }
            });
            let rep = render_pcm(slots, opts, &mut |pcm| {
                let mut b = Vec::with_capacity(pcm.len() * 4);
                for &x in pcm {
                    b.extend_from_slice(&x.clamp(-1.0, 1.0).to_le_bytes());
                }
                stdin.write_all(&b)?;
                Ok(())
            });
            drop(stdin);
            let _ = reader.join();
            let status = child.wait()?;
            let rep = rep?;
            if !status.success() {
                return Err(RenderError::Encoder(format!("encoder exited with {status}")));
            }
            Ok(rep)
        }
    }
}

// ---------------------------------------------------------------------------
// a DJ set from the library DB
// ---------------------------------------------------------------------------

/// Milliseconds spanned by `beats` at `bpm` (`setmath.overlap_ms`).
pub fn overlap_ms(beats: Option<i64>, bpm: Option<f64>) -> i64 {
    match (beats, bpm) {
        (Some(b), Some(bpm)) if b > 0 && bpm > 0.0 => (b as f64 * 60_000.0 / bpm) as i64,
        _ => 0,
    }
}

fn transition_of(kind: Option<&str>) -> (TransitionKind, bool) {
    let k = kind.unwrap_or("").to_ascii_lowercase();
    if k.contains("cut") {
        (TransitionKind::Cut, false)
    } else if k.contains("echo") {
        (TransitionKind::EchoOut, true)
    } else if k.contains("bass") {
        (TransitionKind::BassSwap, false)
    } else if k.contains("filter") || k.contains("sweep") {
        (TransitionKind::Filter, false)
    } else {
        (TransitionKind::Blend, false)
    }
}

/// The renderer's slots for a DJ set (`slots_from_plan` of `render.py`): slots without a
/// file are dropped and their neighbours join with a cut; the overlap into a slot comes
/// from the *incoming* slot's beats at its effective tempo.
pub fn slots_from_set(db: &bc_db::Db, set_id: i64) -> Result<(String, Vec<RenderSlot>), RenderError> {
    struct Row {
        track_id: Option<i64>,
        cue_in: Option<i64>,
        cue_out: Option<i64>,
        pct: f64,
        key_lock: bool,
        ttype: Option<String>,
        beats: Option<i64>,
        dur: Option<i64>,
        bpm: Option<f64>,
        lufs: Option<f64>,
        path: Option<String>,
    }
    let (name, rows) = db
        .read(|c| {
            use bc_db::rusqlite::OptionalExtension;
            let name: Option<String> = c.query_row("SELECT name FROM dj_sets WHERE id = ?1", [set_id], |r| r.get(0)).optional()?;
            let Some(name) = name else { return Ok((String::new(), None)) };
            let mut st = c.prepare(
                "SELECT i.track_id, i.cue_in_ms, i.cue_out_ms, i.tempo_adjust_pct, i.key_lock, i.transition_type, i.transition_beats, \
                        t.duration_ms, a.bpm, a.loudness_lufs, \
                        (SELECT f.path FROM files f WHERE f.track_id = i.track_id AND f.missing_since IS NULL ORDER BY f.id LIMIT 1) \
                 FROM dj_set_items i LEFT JOIN tracks t ON t.id = i.track_id LEFT JOIN analysis a ON a.track_id = i.track_id \
                 WHERE i.set_id = ?1 ORDER BY i.position, i.id",
            )?;
            let rows = st
                .query_map([set_id], |r| {
                    Ok(Row {
                        track_id: r.get(0)?,
                        cue_in: r.get(1)?,
                        cue_out: r.get(2)?,
                        pct: r.get(3)?,
                        key_lock: r.get::<_, i64>(4)? != 0,
                        ttype: r.get(5)?,
                        beats: r.get(6)?,
                        dur: r.get(7)?,
                        bpm: r.get(8)?,
                        lufs: r.get(9)?,
                        path: r.get(10)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((name, Some(rows)))
        })
        .map_err(|e| RenderError::Encoder(e.to_string()))?;
    let Some(rows) = rows else { return Err(RenderError::Encoder(format!("set {set_id} not found"))) };
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let (Some(path), Some(dur), Some(_)) = (row.path.as_ref(), row.dur, row.track_id) else { continue };
        let next = rows.get(i + 1);
        let next_has_file = next.map(|n| n.path.is_some()).unwrap_or(false);
        let overlap = match next {
            Some(n) if next_has_file => {
                let eff = n.bpm.map(|b| ((b * (1.0 + n.pct / 100.0)) * 100.0).round() / 100.0);
                overlap_ms(n.beats, eff)
            }
            _ => 0,
        };
        let (kind, echo) = transition_of(next.and_then(|n| n.ttype.as_deref()));
        out.push(RenderSlot {
            path: PathBuf::from(path),
            cue_in_ms: row.cue_in.unwrap_or(0),
            cue_out_ms: row.cue_out.unwrap_or(dur),
            tempo_adjust_pct: row.pct,
            key_lock: row.key_lock,
            overlap_out_ms: overlap,
            loudness_lufs: row.lufs,
            transition: kind,
            echo,
        });
    }
    Ok((name, out))
}

/// Render to `path`, choosing the encoder by extension (`.mp3` or `.wav`).
pub fn render_to_file(slots: &[RenderSlot], opts: &RenderOptions, path: &Path) -> Result<RenderReport, RenderError> {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("mp3") => render_mp3(slots, opts, path),
        _ => render_wav(slots, opts, path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(frames: usize, v: f32) -> Vec<f32> {
        vec![v; frames * 2]
    }

    #[test]
    fn overlap_add_shortens_by_the_overlap() {
        let out = overlap_add(vec![(seg(1000, 0.5), 200), (seg(800, 0.5), 0)]);
        assert_eq!(out.len() / 2, 1000 + 800 - 200);
    }

    #[test]
    fn a_cut_is_a_butt_joint() {
        let out = overlap_add(vec![(seg(500, 0.25), 0), (seg(300, 0.75), 0)]);
        assert_eq!(out.len() / 2, 800);
        assert!((out[499 * 2] - 0.25).abs() < 1e-6);
        assert!((out[500 * 2] - 0.75).abs() < 1e-6);
    }

    #[test]
    fn the_crossfade_conserves_power_at_its_midpoint() {
        let out = overlap_add(vec![(seg(1000, 1.0), 400), (seg(1000, 1.0), 0)]);
        let mid = out[(600 + 200) * 2];
        // sin(45 deg) + cos(45 deg) = sqrt(2): the equal-power sum of two full-scale tracks
        assert!((mid - 2f32.sqrt()).abs() < 0.01, "{mid}");
        // outside the blend both tracks stand alone
        assert!((out[100 * 2] - 1.0).abs() < 1e-6);
        assert!((out[out.len() - 200] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn an_overlap_longer_than_the_next_segment_keeps_the_tail() {
        let out = overlap_add(vec![(seg(1000, 0.5), 600), (seg(300, 0.5), 0)]);
        assert_eq!(out.len() / 2, 1000 + 300 - 300); // bounded by the shorter side
    }

    #[test]
    fn gain_alignment_is_bounded() {
        assert_eq!(gain_for(None, Some(-10.0)), 1.0);
        assert_eq!(gain_for(Some(-10.0), None), 1.0);
        assert!((gain_for(Some(-10.0), Some(-10.0)) - 1.0).abs() < 1e-12);
        // 20 dB quiet: nudged up by at most the cap
        assert!((gain_for(Some(-30.0), Some(-10.0)) - 10f64.powf(6.0 / 20.0)).abs() < 1e-9);
        assert!((gain_for(Some(-4.0), Some(-10.0)) - 10f64.powf(-6.0 / 20.0)).abs() < 1e-9);
    }

    #[test]
    fn declick_fades_the_raw_edges() {
        let mut s = seg(4000, 1.0);
        declick(&mut s, 44_100, true, true);
        assert_eq!(s[0], 0.0);
        assert!((s[2000 * 2] - 1.0).abs() < 1e-6);
        assert_eq!(s[3999 * 2 + 1], 0.0);
    }

    fn tone_file(path: &Path, freq: f32, secs: f32, sr: u32) {
        let spec = hound::WavSpec { channels: 2, sample_rate: sr, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..(sr as f32 * secs) as usize {
            let v = ((2.0 * std::f32::consts::PI * freq * i as f32 / sr as f32).sin() * 0.5 * 32767.0) as i16;
            w.write_sample(v).unwrap();
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
    }

    /// Two 4 s tones, cues trimming to 3 s each, a 1 s blend: 5 s of wav out.
    #[test]
    fn render_produces_the_planned_duration() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
        tone_file(&a, 220.0, 4.0, 44_100);
        tone_file(&b, 330.0, 4.0, 44_100);
        let mut s0 = RenderSlot::new(&a, 500, 3500);
        s0.overlap_out_ms = 1000;
        let s1 = RenderSlot::new(&b, 500, 3500);
        let out = dir.path().join("mix.wav");
        let rep = render_wav(&[s0, s1], &RenderOptions::default(), &out).unwrap();
        let r = hound::WavReader::open(&out).unwrap();
        assert_eq!(r.spec().sample_rate, 44_100);
        let secs = r.duration() as f64 / 44_100.0;
        assert!((secs - 5.0).abs() < 0.3, "duration {secs}");
        assert!((rep.seconds - secs).abs() < 1e-6);
        // faster than real time
        assert!(rep.realtime_factor > 5.0, "{}", rep.realtime_factor);
    }
}
