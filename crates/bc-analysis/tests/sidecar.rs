//! Essentia sidecar (port of the legacy backend accuracy cases). Skipped when the legacy venv
//! (or `BC_ESSENTIA_PYTHON`) with essentia is missing.

mod common;
use bc_analysis::sidecar::{EssentiaSidecar, available};
use common::*;

fn sidecar() -> Option<EssentiaSidecar> {
    if !available() {
        eprintln!("no essentia python, skipping");
        return None;
    }
    EssentiaSidecar::detect(2)
}

#[test]
fn detects_a_known_tempo_within_an_octave() {
    let Some(sc) = sidecar() else { return };
    for bpm in [90.0, 128.0] {
        let wav = tmp(&format!("sc_click{bpm}.wav"));
        write_wav(&wav, &click_samples(bpm, 30.0), SR);
        let r = sc.analyze(&wav).expect("sidecar result");
        let got = r.bpm.expect("bpm");
        assert!([1.0, 0.5, 2.0].iter().any(|m| (got - bpm * m).abs() / (bpm * m) < 0.03), "{got} vs {bpm}");
        assert!(!r.bpm_candidates.is_empty(), "octave candidates are kept");
    }
}

#[test]
fn detects_a_known_key_and_confidence_orders() {
    let Some(sc) = sidecar() else { return };
    let amin = tmp("sc_amin.wav");
    write_wav(&amin, &a_minor(30.0), SR);
    let click = tmp("sc_beat.wav");
    write_wav(&click, &click_samples(128.0, 30.0), SR);
    let tonal = sc.analyze(&amin).unwrap();
    assert!(matches!(tonal.camelot(), Some("8A") | Some("8B")), "{:?}", tonal.camelot());
    let beat = sc.analyze(&click).unwrap();
    if let (Some(t), Some(b)) = (tonal.bpm_confidence, beat.bpm_confidence) {
        assert!(t < b);
    }
}

#[test]
fn a_bad_file_is_an_error_and_the_worker_survives() {
    let Some(sc) = sidecar() else { return };
    let bad = tmp("sc_broken.mp3");
    std::fs::write(&bad, b"definitely not audio").unwrap();
    assert!(sc.analyze(&bad).is_err());
    let wav = tmp("sc_after.wav");
    write_wav(&wav, &click_samples(120.0, 20.0), SR);
    assert!(sc.analyze(&wav).is_ok(), "worker still serves requests after a failure");
}
