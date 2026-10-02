//! Cue (headphone) device tests: a second software-paced output stands in for the headphones.
//! Previews must land on it -- at the right pitch even when it runs at another rate than the
//! main output -- while the main output keeps playing, and a cue device that fails to open
//! must fall back to previewing through the main output instead of going silent.

use bc_engine::host::{NullCue, OutputKind};
use bc_engine::ports::FilePorts;
use bc_engine::session::{NullPublisher, Session, SessionConfig};
use bc_types::player::*;
use parking_lot::Mutex;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SR: u32 = 48_000;
const CUE_SR: u32 = 44_100;

/// A stereo 16-bit WAV of a steady sine.
fn sine_wav(path: &Path, hz: f64, secs: f64, rate: u32) {
    let spec = hound::WavSpec { channels: 2, sample_rate: rate, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..(secs * rate as f64) as usize {
        let x = ((2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64).sin() * 0.5 * 32767.0) as i16;
        w.write_sample(x).unwrap();
        w.write_sample(x).unwrap();
    }
    w.finalize().unwrap();
}

struct Rig {
    session: Session,
    main: Arc<Mutex<Vec<f32>>>,
    cue: Arc<Mutex<Vec<f32>>>,
}

fn rig(files: Vec<PathBuf>, cue_rate: u32) -> Rig {
    let main = Arc::new(Mutex::new(Vec::new()));
    let cue = Arc::new(Mutex::new(Vec::new()));
    let cfg = SessionConfig {
        output: OutputKind::Null {
            sample_rate: SR,
            block: 256,
            speed: 4.0,
            capture: Some(main.clone()),
            cue: Some(NullCue { sample_rate: cue_rate, capture: Some(cue.clone()) }),
        },
        mpris: false,
        ..Default::default()
    };
    Rig { session: Session::new(FilePorts::new(files).into_ports(), Box::new(NullPublisher), cfg), main, cue }
}

impl Rig {
    fn item(&self, id: i64) -> QueueItem {
        QueueItem { track_id: id, title: format!("t{id}"), ..Default::default() }
    }
    fn cmd(&mut self, c: PlayerCommand) {
        self.session.handle(c).unwrap();
    }
    fn until(&mut self, what: &str, mut f: impl FnMut(&mut Rig) -> bool) {
        let t0 = Instant::now();
        while !f(self) {
            assert!(t0.elapsed() < Duration::from_secs(30), "timed out waiting for {what}: preview {:?}", self.session.st.preview);
            self.session.tick();
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn left(buf: &Mutex<Vec<f32>>) -> Vec<f32> {
    buf.lock().as_chunks::<2>().0.iter().map(|c| c[0]).collect()
}

fn loud(x: &[f32]) -> usize {
    x.iter().filter(|s| s.abs() > 0.1).count()
}

/// Frequency of the steady tone in `x` (rising zero crossings over the loud stretch).
fn tone_hz(x: &[f32], rate: u32) -> f64 {
    let first = x.iter().position(|s| s.abs() > 0.1).expect("no tone");
    let last = x.iter().rposition(|s| s.abs() > 0.1).expect("no tone");
    // skip the fade-in and stay clear of the end
    let (a, b) = (first + rate as usize / 10, last.saturating_sub(rate as usize / 10));
    assert!(b > a + rate as usize / 4, "tone too short: {} frames", last - first);
    let ups: Vec<usize> = (a + 1..b).filter(|&i| x[i - 1] < 0.0 && x[i] >= 0.0).collect();
    let span = (ups[ups.len() - 1] - ups[0]) as f64 / rate as f64;
    (ups.len() - 1) as f64 / span
}

#[test]
fn preview_plays_on_the_cue_device_at_its_own_rate() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.wav");
    sine_wav(&a, 1000.0, 3.0, 44_100);
    let mut r = rig(vec![a], CUE_SR);
    let item = r.item(1);
    r.cmd(PlayerCommand::PreviewStart { item, at_s: Some(0.0) });
    assert!(r.session.st.preview.on_cue_device, "preview should be routed to the cue device");
    r.until("a second of preview on the cue device", |r| loud(&left(&r.cue)) > CUE_SR as usize);
    let cue = left(&r.cue);
    let hz = tone_hz(&cue, CUE_SR);
    // Decoded at the main rate but played at the cue rate it would come out at ~919 Hz.
    assert!((hz - 1000.0).abs() < 5.0, "cue tone at {hz:.1} Hz, want 1000");
    assert_eq!(loud(&left(&r.main)), 0, "the preview leaked into the main output");
}

#[test]
fn main_output_keeps_playing_while_previewing_on_the_cue_device() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.wav"), dir.path().join("b.wav"));
    sine_wav(&a, 440.0, 20.0, 48_000);
    sine_wav(&b, 1000.0, 3.0, 44_100);
    let mut r = rig(vec![a, b], CUE_SR);
    let items = vec![r.item(1)];
    r.cmd(PlayerCommand::PlayQueue { items, start_index: 0, source: None });
    r.until("main playing", |r| r.session.st.status == PlayerStatus::Playing && loud(&left(&r.main)) > SR as usize / 2);
    let item = r.item(2);
    r.cmd(PlayerCommand::PreviewStart { item, at_s: Some(0.0) });
    r.until("preview on the cue device", |r| loud(&left(&r.cue)) > CUE_SR as usize / 2);
    assert_eq!(r.session.st.status, PlayerStatus::Playing, "a cue-device preview must not pause the main player");
    let main = left(&r.main);
    let tail = &main[main.len().saturating_sub(SR as usize / 2)..];
    assert!(loud(tail) > SR as usize / 4, "main output went quiet during the preview");
    assert!((tone_hz(&main, SR) - 440.0).abs() < 3.0, "main output should still carry only the 440 Hz track");
}

#[test]
fn a_cue_device_that_fails_to_open_falls_back_to_the_main_output() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.wav");
    sine_wav(&a, 1000.0, 3.0, 44_100);
    // rate 0: the stand-in device refuses to open
    let mut r = rig(vec![a], 0);
    let item = r.item(1);
    r.cmd(PlayerCommand::PreviewStart { item, at_s: Some(0.0) });
    assert!(!r.session.st.preview.on_cue_device, "a cue device that did not open cannot carry the preview");
    r.until("preview on the main output", |r| loud(&left(&r.main)) > SR as usize);
    let hz = tone_hz(&left(&r.main), SR);
    assert!((hz - 1000.0).abs() < 5.0, "main-output preview at {hz:.1} Hz, want 1000");
    assert!(r.cue.lock().is_empty());
}
