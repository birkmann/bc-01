//! End-to-end engine tests: generated click files are played through the whole
//! stack (decode worker -> rings -> mixer -> software-paced output) and the
//! captured PCM is checked against the beat grid: sample-accurate gapless
//! advance, seek timing against the published clock, and where a blend starts.

use bc_dsp::beatgrid::{BeatGrid, phase_in};
use bc_dsp::mixer::Event;
use bc_engine::host::OutputKind;
use bc_engine::ports::FilePorts;
use bc_engine::session::{NullPublisher, Session, SessionConfig};
use bc_types::player::*;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SR: u32 = 48_000;

/// A 44.1 kHz stereo WAV with a 1 kHz click burst on every beat of `bpm` from `first_s`.
fn click_wav(path: &Path, bpm: f64, first_s: f64, secs: f64, rate: u32) {
    let spec = hound::WavSpec { channels: 2, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    let n = (secs * rate as f64) as usize;
    let mut v = vec![0i16; n];
    let period = 60.0 / bpm;
    let mut t = first_s;
    while t * (rate as f64) < n as f64 {
        let s0 = (t * rate as f64).round() as usize;
        for k in 0..220 {
            if s0 + k < n {
                v[s0 + k] = ((2.0 * std::f64::consts::PI * 1000.0 * k as f64 / rate as f64).sin() * 0.6 * 32767.0 * (1.0 - k as f64 / 220.0)) as i16;
            }
        }
        t += period;
    }
    for x in v {
        w.write_sample(x).unwrap();
        w.write_sample(x).unwrap();
    }
    w.finalize().unwrap();
}

struct Rig {
    session: Session,
    capture: Arc<Mutex<Vec<f32>>>,
}

fn rig(files: Vec<PathBuf>, bpm: Option<f64>, speed: f64) -> Rig {
    let capture = Arc::new(Mutex::new(Vec::new()));
    let mut fp = FilePorts::new(files);
    fp.bpm = bpm;
    let cfg = SessionConfig {
        output: OutputKind::Null { sample_rate: SR, block: 256, speed, capture: Some(capture.clone()) },
        ..Default::default()
    };
    Rig { session: Session::new(fp.into_ports(), Box::new(NullPublisher), cfg), capture }
}

impl Rig {
    fn items(&self, n: usize) -> Vec<QueueItem> {
        (1..=n as i64).map(|i| QueueItem { track_id: i, title: format!("t{i}"), ..Default::default() }).collect()
    }
    fn cmd(&mut self, c: PlayerCommand) {
        self.session.handle(c).unwrap();
    }
    /// Tick the session until `f` is true (or 30 s pass).
    fn until(&mut self, what: &str, mut f: impl FnMut(&mut Session) -> bool) {
        let t0 = Instant::now();
        while !f(&mut self.session) {
            if t0.elapsed() >= Duration::from_secs(30) {
                let n = self.session.event_log.len();
                panic!(
                    "timed out waiting for {what}: status {:?}, index {}, clock {:?}, snapshot {:?}, last events {:#?}",
                    self.session.st.status,
                    self.session.st.queue_index,
                    self.session.clock_now(),
                    self.session.engine_snapshot(),
                    &self.session.event_log[n.saturating_sub(14)..]
                );
            }
            self.session.tick();
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn left(&self) -> Vec<f32> {
        self.capture.lock().as_chunks::<2>().0.iter().map(|c| c[0]).collect()
    }
}

/// Frame indices where a click starts (first sample above the threshold after quiet).
fn click_frames(x: &[f32], from: usize) -> Vec<usize> {
    let mut out = vec![];
    let mut quiet = 0usize;
    for (i, &s) in x.iter().enumerate().skip(from) {
        if s.abs() > 0.05 {
            if quiet > 4000 {
                out.push(i);
            }
            quiet = 0;
        } else {
            quiet += 1;
        }
    }
    out
}

/// Every click captured up to the latest snapshot must sit on a 0.5 s multiple of the track
/// time the engine's own clock implies for that output frame (limiter look-ahead included).
fn assert_clicks_on_grid(r: &mut Rig, from_frame: usize, min_clicks: usize) {
    let snap = r.session.engine_snapshot().unwrap();
    let (f0, p0) = (snap.frames_played as i64, snap.decks[snap.active as usize].pos_frames / SR as f64);
    let lat = 96i64;
    let x = r.left();
    let mut checked = 0;
    for c in click_frames(&x, from_frame) {
        if (c as i64) > f0 + 2000 {
            continue;
        }
        let t = p0 + ((c as i64 - lat) - f0) as f64 / SR as f64;
        let off = (t / 0.5 - (t / 0.5).round()).abs() * 0.5;
        assert!(off < 0.0015, "click at output frame {c}: implied track time {t} is {off} s off the beat grid");
        checked += 1;
    }
    assert!(checked >= min_clicks, "only {checked} clicks checked");
}

#[test]
fn plays_a_file_and_the_clock_follows_the_audio() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.wav");
    click_wav(&a, 120.0, 0.0, 20.0, 44_100);
    let mut r = rig(vec![a], None, 4.0);
    let items = r.items(1);
    r.cmd(PlayerCommand::PlayQueue { items, start_index: 0, source: None });
    r.until("playing past 3 s", |s| s.clock_now().position_s > 3.0 && s.st.status == PlayerStatus::Playing);
    let x = r.left();
    let clicks = click_frames(&x, 0);
    assert!(clicks.len() >= 4, "clicks {clicks:?}");
    // 120 BPM => 0.5 s between clicks, resampled 44.1k -> 48k exactly
    for w in clicks.windows(2) {
        assert!((w[1] as i64 - w[0] as i64 - 24_000).abs() <= 1, "spacing {:?}", w);
    }
    // the published clock and the audible clicks agree to within 1.5 ms
    assert_clicks_on_grid(&mut r, 0, 3);
    assert_eq!(r.session.st.status, PlayerStatus::Playing);
}

#[test]
fn seek_lands_where_the_clock_says() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.wav");
    click_wav(&a, 120.0, 0.0, 40.0, 44_100);
    let mut r = rig(vec![a], None, 4.0);
    let items = r.items(1);
    r.cmd(PlayerCommand::PlayQueue { items, start_index: 0, source: None });
    r.until("started", |s| s.clock_now().position_s > 1.0);
    r.cmd(PlayerCommand::Seek { seconds: 20.2 });
    // the seek lands (after a few ms of dip), then playback continues from there
    r.until("seek applied", |s| (20.0..21.0).contains(&s.clock_now().position_s));
    let applied = r.session.engine_snapshot().unwrap().frames_played as usize;
    r.until("after seek", |s| (22.4..30.0).contains(&s.clock_now().position_s));
    let snap = r.session.engine_snapshot().unwrap();
    let (f0, p0) = (snap.frames_played as i64, snap.decks[snap.active as usize].pos_frames / SR as f64);
    let lat = 96i64; // limiter look-ahead (2 ms at 48 kHz)
    let x = r.left();
    // every click after the seek must sit exactly on a 0.5 s multiple of the track time implied by the clock
    let clicks = click_frames(&x, applied.saturating_sub(100));
    let mut checked = 0;
    for c in clicks {
        if (c as i64) > f0 + 2000 {
            continue; // captured after the snapshot
        }
        let t = p0 + ((c as i64 - lat) - f0) as f64 / SR as f64;
        let off = (t / 0.5 - (t / 0.5).round()).abs() * 0.5;
        assert!(off < 0.0015, "click at output frame {c}: implied track time {t} is {off} s off the beat grid");
        checked += 1;
    }
    assert!(checked >= 3, "only {checked} clicks checked");
}

#[test]
fn gapless_advance_keeps_the_beat_across_the_splice() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
    // both exactly 3 s of 120 BPM clicks from 0: the splice must keep 0.5 s spacing
    click_wav(&a, 120.0, 0.0, 3.0, 48_000);
    click_wav(&b, 120.0, 0.0, 3.0, 48_000);
    let mut r = rig(vec![a, b], None, 4.0);
    let items = r.items(2);
    r.cmd(PlayerCommand::PlayQueue { items, start_index: 0, source: None });
    r.until("second track playing", |s| s.st.queue_index == 1 && s.clock_now().position_s > 1.5);
    assert!(
        r.session.event_log.iter().any(|e| matches!(e, Event::Advanced { .. })),
        "the hand-over should be spliced inside the audio callback: {:?}",
        r.session.event_log
    );
    let x = r.left();
    let clicks = click_frames(&x, 0);
    assert!(clicks.len() >= 8, "clicks {clicks:?}");
    for w in clicks.windows(2) {
        assert!((w[1] as i64 - w[0] as i64 - 24_000).abs() <= 1, "gap or overlap at the splice: {:?}", w);
    }
}

#[test]
fn blend_starts_on_the_outgoing_bar_and_stays_in_phase() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (dir.path().join("a.wav"), dir.path().join("b.wav"), dir.path().join("c.wav"));
    click_wav(&a, 120.0, 0.0, 100.0, 44_100);
    click_wav(&b, 120.0, 0.0, 100.0, 44_100);
    click_wav(&c, 120.0, 0.0, 100.0, 44_100);
    // a third queued track: while the A->B blend runs, the idle deck is still A (fade, echo tail),
    // so C must not be primed onto it until the mixer has parked it
    let mut r = rig(vec![a, b, c], Some(120.0), 8.0);
    r.cmd(PlayerCommand::SetMix { on: true });
    r.cmd(PlayerCommand::SetMixSettings {
        patch: MixSettingsPatch { length_beats: Some(16), echo: Some(false), ..Default::default() },
    });
    r.cmd(PlayerCommand::SetShuffle { on: false });
    let items = r.items(3);
    r.cmd(PlayerCommand::PlayQueue { items, start_index: 0, source: None });
    // the mix-out marker is where the blend will fire (out point 90 s, bounded so the blend fits)
    r.until("blend started", |s| s.event_log.iter().any(|e| matches!(e, Event::TransitionStarted { .. })));
    let a_started = r
        .session
        .event_log
        .iter()
        .find_map(|e| if let Event::Started { frame, .. } = e { Some(*frame) } else { None })
        .expect("A started");
    let (start_frame, length_s) = r
        .session
        .event_log
        .iter()
        .find_map(|e| if let Event::TransitionStarted { start_frame, length_s, .. } = e { Some((*start_frame, *length_s)) } else { None })
        .unwrap();
    assert!((length_s - 8.0).abs() < 0.01, "16 beats at 120 BPM: {length_s}");
    // quantised: the blend begins on a bar line of the outgoing grid (track A is played at rate 1).
    // The trigger fires at most a tick after the bar; a start within 30 ms past it begins at once
    // and lands the incoming on the beat by skipping ahead.
    let grid = BeatGrid { origin_s: 0.0, period_s: 0.5, confidence: 0.9, downbeat_beat: 0 };
    let t = (start_frame - a_started) as f64 / SR as f64; // A's own position at the blend start (rate 1)
    let ph = phase_in(&grid, t, 4);
    assert!(ph < 0.06 || ph > 2.0 - 1.5 / SR as f64, "blend start {t} s is {ph} s past the bar line");
    // while blending, the beat-phase loop reports a small error
    r.until("blend half way", |s| s.engine_snapshot().map(|x| x.transitioning && x.phase_err_ms.is_finite()).unwrap_or(false));
    let mut worst = 0f32;
    for _ in 0..40 {
        r.session.tick();
        std::thread::sleep(Duration::from_millis(5));
        if let Some(e) = r.session.engine_snapshot().map(|x| x.phase_err_ms).filter(|e| e.is_finite()) {
            worst = worst.max(e.abs());
        }
    }
    assert!(worst < 3.0, "beat-phase error during the blend: {worst} ms");
    r.until("fade done", |s| s.event_log.iter().any(|e| matches!(e, Event::FadeDone)));
    // the blend ran its whole planned length (a hijacked outgoing deck would end it at once)
    let frames_now = r.session.engine_snapshot().unwrap().frames_played;
    let ran = (frames_now - start_frame) as f64 / SR as f64;
    assert!((ran - 8.0).abs() < 0.5, "the blend ran {ran} s instead of 8 s");
    assert_eq!(r.session.st.queue_index, 1, "queue index {}", r.session.st.queue_index);
    // after the outgoing deck is parked, the next track (C) is primed onto it
    r.until("parked", |s| s.event_log.iter().any(|e| matches!(e, Event::Parked { .. })));
    r.until("C primed", |s| s.event_log.iter().filter(|e| matches!(e, Event::Ready { .. })).count() >= 3);
}

#[test]
fn skipping_around_with_mix_on_never_underruns() {
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<PathBuf> = ["a", "b", "c"]
        .iter()
        .map(|n| {
            let p = dir.path().join(format!("{n}.wav"));
            click_wav(&p, 120.0, 0.0, 30.0, 44_100);
            p
        })
        .collect();
    let mut r = rig(files, Some(120.0), 2.0);
    r.cmd(PlayerCommand::SetMix { on: true });
    let items = r.items(3);
    r.cmd(PlayerCommand::PlayQueue { items, start_index: 0, source: None });
    r.until("A playing", |s| s.clock_now().position_s > 1.5);
    r.cmd(PlayerCommand::Next);
    r.until("B playing", |s| s.st.queue_index == 1 && s.clock_now().position_s > 1.5);
    // let the echo tail of the short blend park the outgoing deck, then skip again
    r.until("parked", |s| s.event_log.iter().filter(|e| matches!(e, Event::Parked { .. })).count() >= 1);
    r.cmd(PlayerCommand::Next);
    r.until("C playing", |s| s.st.queue_index == 2 && s.clock_now().position_s > 1.5);
    // a first Previous past 3 s only restarts the track; the second steps back
    r.cmd(PlayerCommand::Previous);
    r.until("restarted", |s| s.clock_now().position_s < 2.5);
    r.cmd(PlayerCommand::Previous);
    r.until("B again", |s| s.st.queue_index == 1 && s.clock_now().position_s > 1.5);
    let xruns = r.session.engine_snapshot().unwrap().xruns;
    assert_eq!(xruns, 0, "underruns while skipping; events: {:#?}", r.session.event_log);
}
