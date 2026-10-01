//! Accuracy gate, part 1: the legacy synthetic ground truth (web/PLAN.md 9f table):
//! 128 BPM click within 0.1 %, 90 BPM click exact, A-minor triad 8A, C-major triad 8B.

mod common;
use bc_analysis::{AnalyzeOptions, analyze_file};
use bc_music::beatgrid::GridQuery;
use common::*;

fn analyse(name: &str, samples: &[f32]) -> bc_analysis::TrackAnalysis {
    let wav = tmp(&format!("{name}.wav"));
    write_wav(&wav, samples, SR);
    analyze_file(&wav, &AnalyzeOptions::default()).expect("analysis")
}

#[test]
fn click_128_within_a_tenth_of_a_percent() {
    let a = analyse("click128", &click_samples(128.0, 30.0));
    let t = a.tempo.expect("tempo");
    let err = (t.bpm - 128.0).abs() / 128.0;
    eprintln!("128 -> {:.4} (err {:.4} %) conf {:.2} cands {:?}", t.bpm, err * 100.0, t.confidence, t.candidates);
    assert!(err < 0.001, "{}", t.bpm);
    assert!(t.confidence > 0.5, "{}", t.confidence);
    assert!(!t.candidates.is_empty(), "octave candidates are kept");
    // beat phase: the click attacks at exactly t = k * 468.75 ms
    let origin = t.grid.segments[0].origin_ms;
    let raw = origin.rem_euclid(468.75);
    let phase = if raw > 234.375 { raw - 468.75 } else { raw };
    eprintln!("signed grid phase {phase:.2} ms (positive = late)");
    let phase = phase.abs();
    eprintln!("grid phase error {phase:.2} ms");
    assert!(phase < 15.0, "{phase}");
}

#[test]
fn click_90_exact() {
    let a = analyse("click90", &click_samples(90.0, 30.0));
    let t = a.tempo.expect("tempo");
    eprintln!("90 -> {:.4} conf {:.2}", t.bpm, t.confidence);
    assert!((t.bpm - 90.0).abs() < 0.05, "{}", t.bpm);
}

#[test]
fn a_minor_triad_is_8a() {
    let a = analyse("aminor", &a_minor(30.0));
    let k = a.key.expect("key");
    eprintln!("A minor -> {} strength {:.2}", k.result.camelot, k.result.strength);
    assert_eq!(k.result.camelot, "8A");
}

#[test]
fn c_major_triad_is_8b() {
    let a = analyse("cmajor", &c_major(30.0));
    let k = a.key.expect("key");
    eprintln!("C major -> {} strength {:.2}", k.result.camelot, k.result.strength);
    assert_eq!(k.result.camelot, "8B");
}

#[test]
fn sustained_chord_has_lower_tempo_confidence_than_a_beat() {
    let tonal = analyse("tonal", &a_minor(30.0));
    let beat = analyse("beat", &click_samples(128.0, 30.0));
    let tc = tonal.tempo.map(|t| t.confidence).unwrap_or(0.0);
    let bc = beat.tempo.map(|t| t.confidence).unwrap_or(0.0);
    eprintln!("tonal {tc:.2} vs beat {bc:.2}");
    assert!(tc < bc);
    assert!(tc < 0.3, "{tc}");
}

#[test]
fn loudness_is_real_bs1770() {
    // 1 kHz full-scale-ish sine at -20 dBFS peak: integrated loudness ~ -23.0 - ... use known
    // reference: a 997 Hz sine at -23.0 dBFS RMS-ish => about -23 LUFS (+/- 0.3) for mono 2x.
    let amp = 10f64.powf(-20.0 / 20.0);
    let n = (20.0 * SR as f64) as usize;
    let s: Vec<f32> = (0..n).map(|i| (amp * (2.0 * std::f64::consts::PI * 997.0 * i as f64 / SR as f64).sin()) as f32).collect();
    let a = analyse("sine997", &s);
    let l = a.loudness.expect("loudness");
    eprintln!("lufs {:.2} tp {:.2} lra {:.2} rg {:.2}", l.lufs, l.true_peak_dbtp, l.lra, l.replaygain_gain);
    // K-weighting gains +0.69 dB at 997 Hz, cancelling the -0.691 LUFS offset: -20 dBFS peak
    // mono sine = -23.01 LUFS (BS.1770 / EBU tech 3341 reference level)
    assert!((l.lufs - (-23.01)).abs() < 0.15, "{}", l.lufs);
    assert!((l.true_peak_dbtp - (-20.0)).abs() < 0.5, "{}", l.true_peak_dbtp);
    assert!((l.replaygain_gain - (-18.0 - l.lufs)).abs() < 0.01);
}

#[test]
fn waveform_and_mix_points_come_out_of_the_same_pass() {
    let a = analyse("wf", &click_samples(120.0, 20.0));
    let wf = a.waveform.expect("waveform");
    assert_eq!(wf.overview.n, 2048);
    assert!(wf.detail.as_ref().unwrap().n > 3000);
    assert!(a.mix.is_some());
    assert!((a.duration_ms - 20_000).abs() < 50, "{}", a.duration_ms);
}

#[test]
fn corrupt_and_missing_files_fail_cleanly() {
    let bad = tmp("broken.mp3");
    std::fs::write(&bad, b"definitely not audio").unwrap();
    assert!(analyze_file(&bad, &AnalyzeOptions::default()).is_err());
    assert!(analyze_file(&tmp("nope.mp3"), &AnalyzeOptions::default()).is_err());
}

#[test]
fn mp3_roundtrip_keeps_the_tempo() {
    let wav = tmp("click128m.wav");
    write_wav(&wav, &click_samples(128.0, 30.0), SR);
    let Some(mp3) = to_mp3(&wav) else {
        eprintln!("ffmpeg missing, skipping");
        return;
    };
    let a = analyze_file(&mp3, &AnalyzeOptions::default()).unwrap();
    let t = a.tempo.unwrap();
    eprintln!("mp3 128 -> {:.4}", t.bpm);
    assert!((t.bpm - 128.0).abs() / 128.0 < 0.001, "{}", t.bpm);
    let _ = t.grid.bpm();
}
