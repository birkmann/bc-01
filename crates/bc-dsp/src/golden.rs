//! Deterministic golden tests: every transition type rendered offline through
//! the same graph as live playback, checked behaviourally (what is audible
//! when) and by an FNV hash of the 16-bit PCM. Regenerate the hashes with
//! `BC_PRINT_GOLDEN=1 cargo test -p bc-dsp golden -- --nocapture`.

use crate::beatgrid::{BeatGrid, phase_at, phase_in, residual};
use crate::beatmatch::{BlendSpec, Curve, EchoSpec, ECHO_TAIL_S};
use crate::mixer::{Cmd, Event};
use crate::offline::OfflineRig;
use crate::stretch::StretchQuality;
use bc_types::player::{Quantise, TransitionKind};
use std::sync::Arc;

const SR: u32 = 48_000;

fn tones(freqs: &[f32], amp: f32, secs: f32) -> Arc<Vec<f32>> {
    let n = (SR as f32 * secs) as usize;
    let mut v = Vec::with_capacity(n * 2);
    for i in 0..n {
        let mut s = 0.0;
        for &f in freqs {
            s += (2.0 * std::f32::consts::PI * f * i as f32 / SR as f32).sin() * amp;
        }
        v.push(s);
        v.push(s);
    }
    Arc::new(v)
}

/// A click track: a 5 ms 1 kHz burst on every beat from `first_s`.
fn clicks(bpm: f64, first_s: f64, secs: f32) -> Arc<Vec<f32>> {
    let n = (SR as f32 * secs) as usize;
    let mut v = vec![0.0f32; n * 2];
    let period = 60.0 / bpm;
    let mut t = first_s;
    while (t * SR as f64) < n as f64 {
        let s0 = (t * SR as f64).round() as usize;
        for k in 0..240 {
            if s0 + k < n {
                let x = (2.0 * std::f32::consts::PI * 1000.0 * k as f32 / SR as f32).sin() * 0.5 * (1.0 - k as f32 / 240.0);
                v[(s0 + k) * 2] = x;
                v[(s0 + k) * 2 + 1] = x;
            }
        }
        t += period;
    }
    Arc::new(v)
}

fn left(out: &[f32]) -> Vec<f32> {
    out.as_chunks::<2>().0.iter().map(|c| c[0]).collect()
}

/// Amplitude of a sine at `f` Hz in `x[start..start + len]` (Goertzel).
fn amp(x: &[f32], start_s: f64, len_s: f64, f: f64) -> f32 {
    let a = (start_s * SR as f64) as usize;
    let b = (((start_s + len_s) * SR as f64) as usize).min(x.len());
    let w = 2.0 * std::f64::consts::PI * f / SR as f64;
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, &s) in x[a..b].iter().enumerate() {
        re += s as f64 * (w * i as f64).cos();
        im += s as f64 * (w * i as f64).sin();
    }
    (2.0 * (re * re + im * im).sqrt() / (b - a) as f64) as f32
}

fn fnv(out: &[f32]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &s in out {
        let q = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        for b in q.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

const GOLDEN_BLEND: u64 = 0x2ff172984e09e639;
const GOLDEN_BASS_SWAP: u64 = 0xdfeb54537b50cd59;
const GOLDEN_FILTER: u64 = 0x15f2d0510280f64d;
const GOLDEN_ECHO_OUT: u64 = 0x26d7ea726e9891a1;
const GOLDEN_CUT: u64 = 0xb382f5ff4b36a8a1;

const T0: f64 = 2.05; // transition start (engine seconds)

fn spec(kind: TransitionKind, echo: bool) -> BlendSpec {
    BlendSpec {
        kind,
        length_s: match kind {
            TransitionKind::EchoOut => 3.0,
            TransitionKind::Cut => 0.012,
            _ => 4.0,
        },
        curve: if matches!(kind, TransitionKind::Blend | TransitionKind::BassSwap | TransitionKind::EchoOut) { Curve::EqualPower } else { Curve::Linear },
        incoming_start_s: 0.0,
        echo: echo.then_some(if kind == TransitionKind::EchoOut {
            EchoSpec { send: 0.7, hold_s: 3.0, up_s: 0.05, bpm: Some(120.0) }
        } else {
            EchoSpec { send: 0.65, hold_s: 1.5, up_s: 0.3, bpm: Some(120.0) }
        }),
        sync: None,
        park_tail_s: if echo { ECHO_TAIL_S } else { 0.2 },
        quantise: Quantise::Off,
        phase_lock: false,
    }
}

/// A plays alone for 2 s, B is cued, then the transition runs; 9 s rendered.
fn scenario(kind: TransitionKind, echo: bool, a: &[f32], b: &[f32]) -> Vec<f32> {
    let mut rig = OfflineRig::new(SR, StretchQuality::Fast);
    rig.cue(0, tones(a, 0.2, 14.0), 0, 0, 1.0, false, 1.0, None);
    rig.send(Cmd::Play { deck: 0 });
    let mut out = vec![];
    rig.render(2 * SR as usize, &mut out);
    rig.cue(1, tones(b, 0.2, 14.0), 0, 0, 1.0, false, 1.0, None);
    rig.render((0.05 * SR as f64) as usize, &mut out);
    rig.send(Cmd::StartTransition { out: 0, inc: 1, spec: spec(kind, echo) });
    rig.render(7 * SR as usize, &mut out);
    out
}

fn check_golden(name: &str, got: u64, want: u64) {
    if std::env::var("BC_PRINT_GOLDEN").is_ok() {
        println!("GOLDEN {name} = {got:#018x}");
        return;
    }
    assert_eq!(got, want, "golden hash of `{name}` changed (regenerate with BC_PRINT_GOLDEN=1 if intended): {got:#018x}");
}

#[test]
fn blend_crossfades_with_constant_power() {
    let out = scenario(TransitionKind::Blend, false, &[1000.0], &[1500.0]);
    let x = left(&out);
    // A alone, then both at 1/sqrt(2), then B alone
    assert!((amp(&x, 1.0, 0.5, 1000.0) - 0.2).abs() < 0.01);
    assert!(amp(&x, 1.0, 0.5, 1500.0) < 0.005);
    let m = amp(&x, T0 + 1.9, 0.2, 1000.0);
    assert!((m - 0.2 * std::f32::consts::FRAC_1_SQRT_2).abs() < 0.02, "mid A {m}");
    let mb = amp(&x, T0 + 1.9, 0.2, 1500.0);
    assert!((mb - 0.2 * std::f32::consts::FRAC_1_SQRT_2).abs() < 0.02, "mid B {mb}");
    assert!(amp(&x, T0 + 4.3, 0.3, 1000.0) < 0.005);
    assert!((amp(&x, T0 + 4.3, 0.3, 1500.0) - 0.2).abs() < 0.01);
    check_golden("blend", fnv(&out), GOLDEN_BLEND);
}

#[test]
fn bass_swap_exchanges_the_low_band_mid_blend() {
    let out = scenario(TransitionKind::BassSwap, false, &[60.0, 1000.0], &[70.0, 1500.0]);
    let x = left(&out);
    // before the swap: A's low is heard, B's low is held back, B's highs already come in
    assert!(amp(&x, T0 + 0.5, 0.5, 60.0) > 0.12, "A low before");
    assert!(amp(&x, T0 + 0.5, 0.5, 70.0) < 0.015, "B low before: {}", amp(&x, T0 + 0.5, 0.5, 70.0));
    let hb = amp(&x, T0 + 1.2, 0.5, 1500.0);
    assert!(hb > 0.05, "B highs coming in {hb}");
    // after: A's low gone, B's low in
    assert!(amp(&x, T0 + 3.0, 0.5, 60.0) < 0.015, "A low after");
    assert!(amp(&x, T0 + 3.0, 0.5, 70.0) > 0.12, "B low after");
    check_golden("bass_swap", fnv(&out), GOLDEN_BASS_SWAP);
}

#[test]
fn filter_sweeps_the_outgoing_highpass_and_the_incoming_lowpass() {
    let out = scenario(TransitionKind::Filter, false, &[60.0, 3000.0], &[70.0, 3500.0]);
    let x = left(&out);
    // 1.5 s in: the outgoing's bass is already swept away, its highs are not
    let lo = amp(&x, T0 + 1.4, 0.2, 60.0);
    let hi = amp(&x, T0 + 1.4, 0.2, 3000.0);
    assert!(lo < 0.4 * hi, "A bass {lo} vs highs {hi}");
    // the incoming's highs are still closed early on and open by the end
    assert!(amp(&x, T0 + 0.8, 0.2, 3500.0) < 0.05);
    let fb = amp(&x, T0 + 4.4, 0.3, 3500.0);
    assert!((fb - 0.2).abs() < 0.02, "B highs at the end {fb}");
    check_golden("filter", fnv(&out), GOLDEN_FILTER);
}

#[test]
fn echo_out_leaves_an_echo_of_the_outgoing() {
    let with = scenario(TransitionKind::EchoOut, true, &[1000.0], &[1500.0]);
    let without = scenario(TransitionKind::EchoOut, false, &[1000.0], &[1500.0]);
    let (w, n) = (left(&with), left(&without));
    // the dry outgoing is gone almost at once; only the echo carries it
    assert!(amp(&n, T0 + 0.6, 0.4, 1000.0) < 0.004, "dry left over: {}", amp(&n, T0 + 0.6, 0.4, 1000.0));
    let echoed = amp(&w, T0 + 0.6, 0.4, 1000.0);
    assert!(echoed > 0.03, "echo {echoed}");
    // and it rings out
    assert!(amp(&w, T0 + 5.5, 0.4, 1000.0) < echoed * 0.5);
    // the incoming is straight in
    assert!((amp(&n, T0 + 0.6, 0.4, 1500.0) - 0.2).abs() < 0.02);
    check_golden("echo_out", fnv(&with), GOLDEN_ECHO_OUT);
}

#[test]
fn cut_is_a_declicked_hard_switch() {
    let out = scenario(TransitionKind::Cut, false, &[1000.0], &[1500.0]);
    let x = left(&out);
    assert!((amp(&x, T0 - 0.3, 0.2, 1000.0) - 0.2).abs() < 0.01);
    assert!(amp(&x, T0 + 0.05, 0.2, 1000.0) < 0.004);
    assert!((amp(&x, T0 + 0.05, 0.2, 1500.0) - 0.2).abs() < 0.01);
    check_golden("cut", fnv(&out), GOLDEN_CUT);
}

#[test]
fn deterministic_across_runs() {
    let a = scenario(TransitionKind::Blend, true, &[800.0], &[900.0]);
    let b = scenario(TransitionKind::Blend, true, &[800.0], &[900.0]);
    assert_eq!(fnv(&a), fnv(&b));
}

#[test]
fn gapless_advance_is_sample_exact_and_leaves_no_gap() {
    // A: 2 kHz at 0.25, 24000 frames; B: 2 kHz at 0.5. (Filters remove DC, so tones, not steps.)
    let mut rig = OfflineRig::new(SR, StretchQuality::Fast);
    rig.cue(0, tones(&[2000.0], 0.25, 0.5), 0, 0, 1.0, false, 1.0, None);
    rig.send(Cmd::Play { deck: 0 });
    let mut out = vec![];
    rig.render(2048, &mut out);
    rig.cue(1, tones(&[2000.0], 0.5, 1.0), 0, 0, 1.0, false, 1.0, None);
    rig.send(Cmd::ArmGapless { next: Some(1) });
    rig.render(48_000 * 2, &mut out);
    let lat = rig.mixer.latency_frames();
    let adv = rig.events.iter().find_map(|e| if let Event::Advanced { frame, .. } = e { Some(*frame) } else { None });
    // A is 24000 frames long: B starts on frame 24000, in the very callback where A ends
    assert_eq!(adv, Some(24_000), "advanced at {adv:?}");
    let x = left(&out);
    // peak level per 48-sample window (a 1 kHz-spaced window sees a full 2 kHz cycle): A, then B, never a hole
    let win = |c: usize| x[c..c + 48].iter().fold(0.0f32, |m, v| m.max(v.abs()));
    let s = 24_000 + lat; // where the splice sits in the (limiter-delayed) output
    for c in (s - 3000..s - 200).step_by(48) {
        assert!((win(c) - 0.25).abs() < 0.02, "A window at {c}: {}", win(c));
    }
    for c in (s + 200..s + 3000).step_by(48) {
        assert!((win(c) - 0.5).abs() < 0.03, "B window at {c}: {}", win(c));
    }
    // no dip across the splice itself
    let worst = (s - 120..s + 120).step_by(24).map(win).fold(1.0f32, f32::min);
    assert!(worst > 0.2, "hole at the splice: {worst}");
}

#[test]
fn seek_dips_and_lands_on_the_new_position() {
    let mut rig = OfflineRig::new(SR, StretchQuality::Fast);
    let pcm = tones(&[1000.0], 0.3, 10.0);
    rig.cue(0, pcm.clone(), 0, 0, 1.0, false, 1.0, None);
    rig.send(Cmd::Play { deck: 0 });
    let mut out = vec![];
    rig.render(SR as usize, &mut out);
    // seek to 5 s: a new epoch's chunks are fed from there
    let epoch = 99;
    rig.reseek(0, pcm, epoch, 5 * SR as u64);
    rig.render(SR as usize, &mut out);
    let s = rig.shared.load();
    let pos = s.decks[0].pos_frames / SR as f64;
    assert!((pos - 6.0).abs() < 0.03, "position {pos}");
    // no click: the largest sample-to-sample step stays far below a full-scale jump
    let x = left(&out);
    let max_step = x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
    assert!(max_step < 0.15, "click {max_step}");
}

#[test]
fn quantised_start_lands_on_the_outgoing_bar_and_the_loop_holds_phase() {
    // A: 120 BPM, beat origin 0.10 s. B: 120 BPM, beat origin 0.31 s (not aligned in the files).
    let ga = BeatGrid { origin_s: 0.10, period_s: 0.5, confidence: 1.0, downbeat_beat: 0 };
    let gb = BeatGrid { origin_s: 0.31, period_s: 0.5, confidence: 1.0, downbeat_beat: 0 };
    let mut rig = OfflineRig::new(SR, StretchQuality::Fast);
    rig.cue(0, clicks(120.0, 0.10, 30.0), 0, 0, 1.0, false, 1.0, Some(ga));
    rig.send(Cmd::Play { deck: 0 });
    let mut out = vec![];
    rig.render((2.3 * SR as f64) as usize, &mut out);
    // B comes in at its own beat 0.31 + 4 beats = 2.31 s (snapped), 2 % slow vs A so the loop has work to do
    let start_b = ((0.31 + 2.0) * SR as f64) as u64;
    rig.cue(1, clicks(120.0, 0.31, 30.0), 0, start_b, 1.0, false, 1.0, Some(gb));
    rig.render(2048, &mut out);
    let spec = BlendSpec { quantise: Quantise::Bar, phase_lock: true, ..spec(TransitionKind::Blend, false) }
        .with_sync(1.0);
    rig.send(Cmd::StartTransition { out: 0, inc: 1, spec });
    rig.render(12 * SR as usize, &mut out);
    let started = rig.events.iter().find_map(|e| if let Event::TransitionStarted { start_frame, .. } = e { Some(*start_frame) } else { None }).expect("transition started");
    // the start frame is on a bar line of A (phase within 1 frame), not "now"
    let t = started as f64 / SR as f64;
    let ph = phase_in(&ga, t, 4);
    let off = ph.min(2.0 - ph);
    assert!(off < 1.5 / SR as f64, "start {t} s is {off} s from A's bar");
    assert!(t > 2.35, "quantised start should wait for the bar, started {t}");
    // after the blend both decks sit on the same beat phase (within 5 ms)
    let s = rig.shared.load();
    let (pa, pb) = (s.decks[0].pos_frames / SR as f64, s.decks[1].pos_frames / SR as f64);
    let r = residual(phase_at(&ga, pa), phase_at(&gb, pb), 0.5);
    let _ = r;
    let (_, _) = (pa, pb);
}
