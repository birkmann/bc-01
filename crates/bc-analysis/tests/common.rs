//! Synthetic audio with known ground truth (ported from `tests/integration/test_analysis.py`).
#![allow(dead_code)]

use std::f64::consts::PI;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const SR: u32 = 44_100;

pub fn write_wav(path: &Path, samples: &[f32], sr: u32) {
    let mut f = std::fs::File::create(path).unwrap();
    let n = samples.len() as u32;
    let data_len = n * 2;
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&sr.to_le_bytes()).unwrap();
    f.write_all(&(sr * 2).to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_len.to_le_bytes()).unwrap();
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        f.write_all(&v.to_le_bytes()).unwrap();
    }
}

/// `0.9*exp(-25*mod(t,P))*sin(2pi*1800 t) + 0.5*exp(-6*mod(t,P))*sin(2pi*55 t)`.
pub fn click_samples(bpm: f64, seconds: f64) -> Vec<f32> {
    let period = 60.0 / bpm;
    (0..(seconds * SR as f64) as usize)
        .map(|i| {
            let t = i as f64 / SR as f64;
            let m = t % period;
            (0.9 * (-25.0 * m).exp() * (2.0 * PI * 1800.0 * t).sin() + 0.5 * (-6.0 * m).exp() * (2.0 * PI * 55.0 * t).sin()) as f32
        })
        .collect()
}

pub fn chord(freqs_amps: &[(f64, f64)], seconds: f64) -> Vec<f32> {
    (0..(seconds * SR as f64) as usize)
        .map(|i| {
            let t = i as f64 / SR as f64;
            freqs_amps.iter().map(|(f, a)| a * (2.0 * PI * f * t).sin()).sum::<f64>() as f32
        })
        .collect()
}

/// A2 + C4 + E4 + A4: pitch class 9, minor = Camelot 8A.
pub fn a_minor(seconds: f64) -> Vec<f32> {
    chord(&[(110.0, 0.3), (261.63, 0.25), (329.63, 0.25), (440.0, 0.2)], seconds)
}

/// C3 + C4 + E4 + G4: C major = Camelot 8B.
pub fn c_major(seconds: f64) -> Vec<f32> {
    chord(&[(130.81, 0.3), (261.63, 0.25), (329.63, 0.25), (392.0, 0.2)], seconds)
}

pub fn tmp(name: &str) -> PathBuf {
    let dir = std::env::var_os("BC_TMP").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("bc-synth"));
    std::fs::create_dir_all(&dir).unwrap();
    PathBuf::from(dir).join(name)
}

/// Encode via ffmpeg to MP3 (the legacy fixtures were MP3); `None` when ffmpeg is absent.
pub fn to_mp3(wav: &Path) -> Option<PathBuf> {
    let out = wav.with_extension("mp3");
    let st = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(wav)
        .args(["-c:a", "libmp3lame", "-b:a", "192k"])
        .arg(&out)
        .status()
        .ok()?;
    st.success().then_some(out)
}
