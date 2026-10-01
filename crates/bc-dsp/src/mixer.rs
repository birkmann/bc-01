//! The real-time mixer graph: two decks with per-channel 3-band EQ + filter +
//! echo send, a preview deck, an equal-power / EQ / filter / echo transition
//! engine, the phase-lock loop, a master strip, echo bus and look-ahead
//! limiter. `render` is the single entry point called from the audio callback
//! (cpal), the AudioWorklet (wasm) or the offline renderer.
//!
//! Real-time rules: no allocation, no locks, no syscalls. Commands arrive on
//! an `rtrb` queue as plain `Copy` data, events leave the same way.

use crate::beatgrid::{BeatGrid, phase_at, phase_in, residual};
use crate::beatmatch::*;
use crate::deck::{Chunk, Deck, DeckState};
use crate::echo::{self, Echo};
use crate::envelope::*;
use crate::filters::*;
use crate::limiter::Limiter;
use crate::shared::{DECKS, DeckSnap, SharedState, Snapshot};
use crate::stretch::StretchQuality;
use bc_types::player::{PhaseState, Quantise, TransitionKind};
use rtrb::{Consumer, Producer};
use std::sync::Arc;

pub const MAX_SEG: usize = 64;
/// Longest block the scratch buffers hold (hosts may hand bigger ones; they are sliced).
pub const MAX_BLOCK: usize = 8192;
const PREVIEW: usize = 2;
const DIP_TAU_S: f64 = 0.0015;
const PAUSE_TAU_S: f64 = 0.004;
/// A quantised start never waits longer than this for the next boundary.
const MAX_QUANT_WAIT_S: f64 = 8.0;
/// A transition whose incoming deck has no data after this long starts anyway.
const READY_TIMEOUT_S: f64 = 3.0;
const PLL_PERIOD_FRAMES: u64 = 512;
const PLL_KP: f64 = 0.25;
const PLL_KI: f64 = 0.02;

/// Commands from the control thread. Plain data only.
#[derive(Debug, Clone, Copy)]
pub enum Cmd {
    /// Prime a deck at `frame` (decoder chunks of `epoch` follow).
    Cue { deck: u8, epoch: u32, frame: u64, len_frames: u64, rate: f64, keylock: bool, trim: f64, grid: Option<BeatGrid> },
    /// Make a cued deck the playing deck (a click-free cut); other decks are silenced.
    Play { deck: u8 },
    /// Key-lock vocoder size for decks cued from now on: 0 fast, 1 normal, 2 high.
    SetQuality(u8),
    Pause,
    Resume,
    /// Silence everything and empty both decks.
    Stop,
    Seek { deck: u8, epoch: u32, frame: u64 },
    SetRate { deck: u8, rate: f64, keylock: bool, glide_s: f64 },
    SetTrim { deck: u8, trim: f64 },
    SetGrid { deck: u8, grid: Option<BeatGrid> },
    /// Blend `inc` (cued) into `out` (playing).
    StartTransition { out: u8, inc: u8, spec: BlendSpec },
    Retime { remaining_s: f64 },
    CutNow,
    SetEcho { on: bool },
    SetSync { on: bool },
    Nudge { delta_s: f64 },
    /// When the active deck ends with no transition running, start `next` at once (gapless).
    ArmGapless { next: Option<u8> },
    SetVolume { gain: f64 },
    SetStrip { low: f64, mid: f64, high: f64, filter: f64, echo_send: f64 },
    SetCeilingDb { db: f64 },
    PreviewCue { epoch: u32, frame: u64, len_frames: u64, gain: f64 },
    PreviewPlay,
    PreviewStop,
    SetPreviewGain { gain: f64 },
}

/// Events back to the control thread.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// The cued deck now has enough decoded data to start.
    Ready { deck: u8, epoch: u32 },
    /// A deck began playing at engine frame `frame`.
    Started { deck: u8, frame: u64 },
    /// A deck reached the end of its track.
    Ended { deck: u8, epoch: u32 },
    /// Gapless hand-over happened inside the audio callback.
    Advanced { from: u8, to: u8, frame: u64 },
    /// A playing deck ran out of decoded data at engine frame `frame`.
    Underrun { deck: u8, frame: u64 },
    TransitionStarted { out: u8, inc: u8, start_frame: u64, length_s: f64, phase: u8 },
    TransitionRetimed { end_t: f64 },
    /// The fade is over (the strip comes down); the outgoing may still ring out.
    FadeDone,
    /// The outgoing deck was parked (emptied).
    Parked { deck: u8 },
    Paused,
    Resumed,
}

#[derive(Debug, Clone, Copy)]
struct Ext {
    ch: usize,
    which: ExtKind,
    env: Env,
    /// Value the lane settles at (also what a retime lands on).
    fin: f64,
    /// Short step (bass swap): finishes instantly on retime instead of stretching.
    step: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtKind {
    Filter,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    Waiting { start_frame: u64, deadline: u64 },
    Running,
}

#[derive(Debug, Clone, Copy)]
struct Tr {
    spec: BlendSpec,
    out: usize,
    inc: usize,
    stage: Stage,
    t0: f64,
    t1: f64,
    park_t: f64,
    echo_on: bool,
    sync_on: bool,
    fade_done: bool,
    env_in: Env,
    env_out: Env,
    ext: [Option<Ext>; 4],
    phase: Option<PhaseState>,
    pll_i: f64,
    next_pll: u64,
    dismissed: bool,
}

struct Channel {
    deck: Deck,
    gain: Auto,
    send: Auto,
    low: Auto,
    mid: Auto,
    high: Auto,
    filter: Auto,
    trim: f64,
    eq: Eq3,
    flt: KnobFilter,
    dip: Smooth,
    dip_target: f64,
}

impl Channel {
    fn new(sr: f64, deck: Deck) -> Self {
        Channel {
            deck,
            gain: Auto::Const(0.0),
            send: Auto::Const(0.0),
            low: Auto::Const(1.0),
            mid: Auto::Const(1.0),
            high: Auto::Const(1.0),
            filter: Auto::Const(0.0),
            trim: 1.0,
            eq: Eq3::new(sr),
            flt: KnobFilter::new(sr),
            dip: Smooth::new(1.0, sr, DIP_TAU_S),
            dip_target: 1.0,
        }
    }
    fn reset_lanes(&mut self, gain: f64) {
        self.gain = Auto::Const(gain);
        self.send = Auto::Const(0.0);
        self.low = Auto::Const(1.0);
        self.mid = Auto::Const(1.0);
        self.high = Auto::Const(1.0);
        self.filter = Auto::Const(0.0);
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingSeek {
    deck: usize,
    epoch: u32,
    frame: u64,
}

pub struct MixerPorts {
    pub rings: [Consumer<Chunk>; DECKS],
    pub cmds: Consumer<Cmd>,
    pub events: Producer<Event>,
    pub shared: Arc<SharedState>,
}

pub struct Mixer {
    sr: f64,
    frames: u64,
    ch: [Channel; 2],
    preview: Deck,
    preview_gain: f64,
    quality: StretchQuality,
    echo: Echo,
    master_eq: Eq3,
    master_flt: KnobFilter,
    strip: [Smooth; 4], // low, mid, high gains, echo_send
    strip_filter: Smooth,
    vol: Smooth,
    play_gain: Smooth,
    limiter: Limiter,
    cmds: Consumer<Cmd>,
    events: Producer<Event>,
    shared: Arc<SharedState>,
    active: usize,
    tr: Option<Tr>,
    paused: bool,
    armed_next: Option<usize>,
    pending_seek: Option<PendingSeek>,
    /// A deck waiting to become the playing deck once it has data.
    pending_play: Option<(usize, u64)>,
    tmp: [Vec<f32>; DECKS],
    xruns: u64,
    peak: [f32; 2],
    ready_sent: [bool; DECKS],
    output_ts_ns: u64,
    phase_err_ms: f32,
    under: [bool; 2],
    pending_clear: Option<(usize, u64)>,
    vol_target: f64,
    strip_target: [f64; 4],
    strip_filter_target: f64,
}

impl Mixer {
    pub fn new(sr: f64, ports: MixerPorts, quality: StretchQuality) -> Self {
        let [r0, r1, r2] = ports.rings;
        let mut strip = [Smooth::new(1.0, sr, 0.01), Smooth::new(1.0, sr, 0.01), Smooth::new(1.0, sr, 0.01), Smooth::new(0.0, sr, 0.02)];
        strip[3].set(0.0);
        Mixer {
            sr,
            frames: 0,
            ch: [Channel::new(sr, Deck::new(sr, r0, quality)), Channel::new(sr, Deck::new(sr, r1, quality))],
            preview: Deck::new(sr, r2, quality),
            preview_gain: 1.0,
            quality,
            echo: Echo::new(sr),
            master_eq: Eq3::new(sr),
            master_flt: KnobFilter::new(sr),
            strip,
            strip_filter: Smooth::new(0.0, sr, 0.02),
            vol: Smooth::new(1.0, sr, 0.02),
            play_gain: Smooth::new(1.0, sr, PAUSE_TAU_S),
            limiter: Limiter::new(sr, -1.0, 2.0, 80.0),
            cmds: ports.cmds,
            events: ports.events,
            shared: ports.shared,
            active: 0,
            tr: None,
            paused: false,
            armed_next: None,
            pending_seek: None,
            pending_play: None,
            tmp: std::array::from_fn(|_| vec![0.0; MAX_SEG * 2]),
            xruns: 0,
            peak: [0.0; 2],
            ready_sent: [false; DECKS],
            output_ts_ns: 0,
            phase_err_ms: f32::NAN,
            under: [false; 2],
            pending_clear: None,
            vol_target: 1.0,
            strip_target: [1.0, 1.0, 1.0, 0.0],
            strip_filter_target: 0.0,
        }
    }

    pub fn sample_rate(&self) -> f64 {
        self.sr
    }
    pub fn frames(&self) -> u64 {
        self.frames
    }
    pub fn latency_frames(&self) -> usize {
        self.limiter.latency_frames()
    }
    pub fn xruns(&self) -> u64 {
        self.xruns
    }
    /// Count a device-level xrun (late callback / stream error) from the host.
    pub fn note_xrun(&mut self) {
        self.xruns += 1;
    }

    #[inline]
    fn now(&self) -> f64 {
        self.frames as f64 / self.sr
    }

    fn emit(&mut self, e: Event) {
        let _ = self.events.push(e);
    }

    // ------------------------------------------------------------------
    // commands
    // ------------------------------------------------------------------

    fn drain_cmds(&mut self) {
        while let Ok(c) = self.cmds.pop() {
            self.handle(c);
        }
    }

    fn handle(&mut self, c: Cmd) {
        let now = self.now();
        match c {
            Cmd::Cue { deck, epoch, frame, len_frames, rate, keylock, trim, grid } => {
                let d = deck as usize & 1;
                // a cue on a deck that is part of a running blend would break it
                if self.tr.map(|t| t.out == d || t.inc == d).unwrap_or(false)
                    && let Some(t) = self.tr
                        && t.out == d {
                            // outgoing replaced mid-blend: end the blend as a cut first
                            self.finish_transition(true);
                        }
                self.ch[d].deck.set_quality(self.quality);
                self.ch[d].deck.cue(epoch, frame, len_frames);
                self.ch[d].deck.set_rate(rate, keylock);
                self.ch[d].deck.grid = grid;
                self.ch[d].trim = trim;
                self.ch[d].reset_lanes(if d == self.active && self.ch[d].deck.state() == DeckState::Playing { 1.0 } else { 0.0 });
                self.ready_sent[d] = false;
                if self.pending_play.map(|p| p.0 == d).unwrap_or(false) {
                    self.pending_play = None;
                }
            }
            Cmd::SetQuality(q) => {
                self.quality = match q {
                    0 => StretchQuality::Fast,
                    2 => StretchQuality::High,
                    _ => StretchQuality::Normal,
                };
            }
            Cmd::Play { deck } => {
                let d = deck as usize & 1;
                if self.ch[d].deck.state() == DeckState::Cued && self.ch[d].deck.ready() {
                    // data already here: the deck starts on the very next frame
                    self.start_play(d);
                } else {
                    self.pending_play = Some((d, self.frames + (READY_TIMEOUT_S * self.sr) as u64));
                }
            }
            Cmd::Pause => {
                self.paused = true;
                self.echo.set_wet(0.0);
                self.emit(Event::Paused);
            }
            Cmd::Resume => {
                self.paused = false;
                self.echo.set_wet(echo::WET);
                self.emit(Event::Resumed);
            }
            Cmd::Stop => {
                self.finish_transition(true);
                for i in 0..2 {
                    self.ch[i].deck.clear();
                    self.ch[i].reset_lanes(0.0);
                    self.ready_sent[i] = false;
                }
                self.pending_play = None;
                self.pending_seek = None;
                self.armed_next = None;
                self.echo.set_wet(0.0);
            }
            Cmd::Seek { deck, epoch, frame } => {
                let d = deck as usize & 1;
                if self.ch[d].deck.state() == DeckState::Playing && !self.paused_silent() {
                    // dip, then jump (click-free seek)
                    self.ch[d].dip_target = 0.0;
                    self.pending_seek = Some(PendingSeek { deck: d, epoch, frame });
                } else {
                    self.ch[d].deck.seek(epoch, frame);
                    self.ready_sent[d] = false;
                }
            }
            Cmd::SetRate { deck, rate, keylock, glide_s } => {
                let d = deck as usize & 1;
                if glide_s > 0.0 {
                    self.ch[d].deck.set_keylock(keylock);
                    self.ch[d].deck.glide_to(rate, glide_s);
                } else {
                    self.ch[d].deck.set_rate(rate, keylock);
                }
            }
            Cmd::SetTrim { deck, trim } => self.ch[deck as usize & 1].trim = trim,
            Cmd::SetGrid { deck, grid } => self.ch[deck as usize & 1].deck.grid = grid,
            Cmd::StartTransition { out, inc, spec } => {
                let (out, inc) = (out as usize & 1, inc as usize & 1);
                // the previous blend (its echo tail may still be ringing) ends now; the deck that was
                // its outgoing is very likely this one's incoming, freshly cued: keep it
                self.finish_transition_keeping(true, Some(inc));
                self.ch[inc].deck.cancel_glide();
                let rate = spec.sync.map(|s| s.rate).unwrap_or(1.0);
                // key lock only where a tempo match asks for it and the rate is not unity (vinyl at 1.0 is bit-exact)
                let kl = spec.sync.map(|s| s.key_lock && (s.rate - 1.0).abs() > 1e-4).unwrap_or(false);
                if self.ch[inc].deck.state() != DeckState::Empty {
                    self.ch[inc].deck.set_rate(rate, kl);
                }
                self.ch[out].deck.cancel_glide();
                self.ch[inc].reset_lanes(0.0);
                let start_frame = self.quantised_start(out, &spec);
                let timeout = start_frame + (READY_TIMEOUT_S * self.sr) as u64;
                self.tr = Some(Tr {
                    spec,
                    out,
                    inc,
                    stage: Stage::Waiting { start_frame, deadline: timeout },
                    t0: now,
                    t1: now,
                    park_t: now,
                    echo_on: spec.echo.is_some(),
                    sync_on: spec.sync.is_some(),
                    fade_done: false,
                    env_in: Env::new(0.0, 1.0, now, now, Shape::Linear),
                    env_out: Env::new(1.0, 0.0, now, now, Shape::Linear),
                    ext: [None; 4],
                    phase: None,
                    pll_i: 0.0,
                    next_pll: 0,
                    dismissed: false,
                });
            }
            Cmd::Retime { remaining_s } => self.retime(remaining_s),
            Cmd::CutNow => self.cut_now(),
            Cmd::SetEcho { on } => self.set_echo_live(on),
            Cmd::SetSync { on } => self.set_sync_live(on),
            Cmd::Nudge { delta_s } => {
                if let Some(t) = self.tr
                    && matches!(t.stage, Stage::Running) {
                        self.bend_by(t.inc, delta_s);
                    }
            }
            Cmd::ArmGapless { next } => self.armed_next = next.map(|n| n as usize & 1),
            Cmd::SetVolume { gain } => self.vol_target = gain.clamp(0.0, 2.0),
            Cmd::SetStrip { low, mid, high, filter, echo_send } => {
                self.strip_target = [low, mid, high, echo_send];
                self.strip_filter_target = filter;
            }
            Cmd::SetCeilingDb { db } => self.limiter.set_ceiling_db(db),
            Cmd::PreviewCue { epoch, frame, len_frames, gain } => {
                self.preview.cue(epoch, frame, len_frames);
                self.preview.set_rate(1.0, false);
                self.preview_gain = gain;
                self.ready_sent[PREVIEW] = false;
            }
            Cmd::PreviewPlay => self.preview.start(),
            Cmd::PreviewStop => self.preview.clear(),
            Cmd::SetPreviewGain { gain } => self.preview_gain = gain,
        }
    }

    /// Make a cued deck the playing deck (a click-free cut), silencing the other.
    fn start_play(&mut self, d: usize) {
        self.pending_play = None;
        let other = d ^ 1;
        self.ch[d].deck.start();
        self.echo.set_wet(echo::WET);
        let now = self.now();
        self.ch[d].reset_lanes(1.0);
        self.ch[d].gain = Auto::Env(Env::new(0.0, 1.0, now, now + 0.008, Shape::Linear));
        if self.ch[other].deck.state() == DeckState::Playing {
            self.ch[other].gain = self.ch[other].gain.ramp_to(now, 0.0, 0.02);
            // the replaced deck is emptied once its 20 ms fade-out is over
            self.pending_clear = Some((other, self.frames + (0.03 * self.sr) as u64));
        }
        self.active = d;
        self.ready_sent[d] = true;
        self.emit(Event::Started { deck: d as u8, frame: self.frames });
    }

    fn paused_silent(&self) -> bool {
        self.paused && self.play_gain.v < 1e-3
    }

    /// The frame at which a transition into the incoming deck should begin.
    fn quantised_start(&self, out: usize, spec: &BlendSpec) -> u64 {
        let now = self.frames;
        let d = &self.ch[out].deck;
        let Some(g) = d.grid else { return now };
        if spec.quantise == Quantise::Off || d.state() != DeckState::Playing {
            return now;
        }
        let beats = match spec.quantise {
            Quantise::Off => return now,
            Quantise::Beat => 1,
            Quantise::Bar => 4,
            Quantise::Phrase => 16,
        };
        let len = g.period_s * beats as f64;
        let phase = phase_in(&g, d.position_s(), beats);
        // just past a boundary: start now, the loop absorbs the few ms
        if phase < 0.03 {
            return now;
        }
        let wait_src_s = len - phase;
        let rate = d.rate_eff().max(0.1);
        let wait_s = wait_src_s / rate;
        // never let the quantise push the blend past the end of the outgoing track
        let end = d.end_frame().unwrap_or(d.len_frames);
        if end > 0 {
            let left_s = (end as f64 - d.position_frames()) / self.sr / rate;
            if wait_s > left_s - spec.length_s - 0.5 {
                return now;
            }
        }
        if wait_s > MAX_QUANT_WAIT_S {
            return now;
        }
        now + (wait_s * self.sr).round() as u64
    }

    // ------------------------------------------------------------------
    // transitions
    // ------------------------------------------------------------------

    fn begin_running(&mut self) {
        let now = self.now();
        let Some(mut t) = self.tr else { return };
        let (o, i) = (t.out, t.inc);
        // Port of the browser's "a track that is nearly over cannot host a long blend".
        let mut length_s = t.spec.length_s;
        {
            let d = &self.ch[o].deck;
            if let Some(end) = d.end_frame().or((d.len_frames > 0).then_some(d.len_frames)) {
                let left = (end as f64 - d.position_frames()) / self.sr / d.rate_eff().max(0.1);
                if left > 0.5 && t.spec.kind != TransitionKind::Cut {
                    length_s = length_s.min((left - 0.2).max(0.5));
                }
            }
        }
        t.t0 = now;
        t.t1 = now + length_s;
        t.stage = Stage::Running;
        let power = t.spec.curve == Curve::EqualPower;
        let (sh_in, sh_out) = if power { (Shape::In, Shape::Out) } else { (Shape::Linear, Shape::Linear) };
        t.ext = [None; 4];

        // Start the incoming deck now (sample-exact: this runs at a segment edge).
        self.ch[i].deck.start();
        self.active = i;
        self.ready_sent[i] = true;

        match t.spec.kind {
            TransitionKind::Blend => {
                t.env_in = Env::new(0.0, 1.0, now, t.t1, sh_in);
                t.env_out = Env::new(1.0, 0.0, now, t.t1, sh_out);
            }
            TransitionKind::BassSwap => {
                t.env_in = Env::new(0.0, 1.0, now, t.t1, sh_in);
                t.env_out = Env::new(1.0, 0.0, now, t.t1, sh_out);
                let swap = self.swap_time(&t, o, now, length_s);
                let ramp = 0.03;
                t.ext[0] = Some(Ext { ch: i, which: ExtKind::Low, env: Env::new(0.0, 1.0, swap - ramp / 2.0, swap + ramp / 2.0, Shape::Linear), fin: 1.0, step: true });
                t.ext[1] = Some(Ext { ch: o, which: ExtKind::Low, env: Env::new(1.0, 0.0, swap - ramp / 2.0, swap + ramp / 2.0, Shape::Linear), fin: 0.0, step: true });
                // incoming enters with its low band out of the way from the first sample
                self.ch[i].low = Auto::Env(t.ext[0].map(|e| e.env).unwrap_or(Env::new(0.0, 0.0, now, now, Shape::Linear)));
                self.ch[o].low = Auto::Env(t.ext[1].map(|e| e.env).unwrap_or(Env::new(1.0, 1.0, now, now, Shape::Linear)));
            }
            TransitionKind::Filter => {
                // incoming opens from a closed low-pass; outgoing high-pass-sweeps out
                t.env_in = Env::new(0.0, 1.0, now, now + length_s * 0.65, Shape::In);
                t.env_out = Env::new(1.0, 0.0, now + length_s * 0.35, t.t1, Shape::Out);
                t.ext[0] = Some(Ext { ch: o, which: ExtKind::Filter, env: Env::new(0.0, 1.0, now, t.t1, Shape::Linear), fin: 1.0, step: false });
                t.ext[1] = Some(Ext { ch: i, which: ExtKind::Filter, env: Env::new(-1.0, 0.0, now, t.t1, Shape::Linear), fin: 0.0, step: false });
            }
            TransitionKind::EchoOut => {
                t.env_in = Env::new(0.0, 1.0, now, now + 0.12, Shape::Linear);
                t.env_out = Env::new(1.0, 0.0, now, now + 0.15, Shape::Linear);
            }
            TransitionKind::Cut => {
                t.env_in = Env::new(0.0, 1.0, now, t.t1.max(now + 0.004), Shape::Linear);
                t.env_out = Env::new(1.0, 0.0, now, t.t1.max(now + 0.004), Shape::Linear);
            }
        }
        self.ch[i].gain = Auto::Env(t.env_in);
        self.ch[o].gain = Auto::Env(t.env_out);
        for e in t.ext.iter().flatten() {
            match e.which {
                ExtKind::Filter => self.ch[e.ch].filter = Auto::Env(e.env),
                ExtKind::Low => self.ch[e.ch].low = Auto::Env(e.env),
            }
        }
        // the echo repeats at a dotted eighth of the outgoing tune -- set before the send opens
        if let Some(e) = t.spec.echo {
            self.echo.set_delay_s(echo::delay_for_bpm(e.bpm));
        }
        self.echo.set_wet(echo::WET);
        let end_t = match t.spec.kind {
            TransitionKind::EchoOut => t.t1,
            _ => t.t1,
        };
        if let (Some(e), true) = (t.spec.echo, t.echo_on) {
            let cur = self.ch[o].send.value(now);
            let steps = send_steps(end_t, &e, now);
            self.ch[o].send = Auto::Pw(Piecewise::from_steps(now, cur, steps));
        }
        t.park_t = t.t1 + t.spec.park_tail_s;

        // Beat phase: both grids trusted and the tempos matched -> bend onto the grid once.
        let (og, ig) = (self.ch[o].deck.grid, self.ch[i].deck.grid);
        if t.spec.sync.is_some() && t.sync_on
            && let (Some(og), Some(ig)) = (og, ig)
                && let Some(r) = self.phase_residual(o, i, &og, &ig) {
                    let rate = self.ch[i].deck.rate_base.max(0.1);
                    if r > 0.0 && r < 0.05 {
                        // started a few ms late: the incoming is silent now, so jump it onto the beat
                        let frames = r * self.ch[i].deck.rate_eff().max(0.1) * self.sr;
                        self.ch[i].deck.skip_forward(frames);
                    } else {
                        // otherwise bend onto the grid (+-4 %, at most 2 s)
                        self.bend_by(i, r * rate);
                    }
                    t.phase = Some(PhaseState::Est);
                }
        t.next_pll = self.frames + PLL_PERIOD_FRAMES;
        self.tr = Some(t);
        let phase = t.phase.map(|p| if p == PhaseState::Est { 1 } else { 2 }).unwrap_or(0);
        self.emit(Event::TransitionStarted { out: o as u8, inc: i as u8, start_frame: self.frames, length_s, phase });
        self.emit(Event::Started { deck: i as u8, frame: self.frames });
    }

    /// The time (engine seconds) of the bass swap: the middle of the blend,
    /// snapped to the nearest bar boundary of the outgoing grid when known.
    fn swap_time(&self, t: &Tr, out: usize, now: f64, length_s: f64) -> f64 {
        let mid = now + length_s * 0.5;
        let d = &self.ch[out].deck;
        let Some(g) = d.grid else { return mid };
        let rate = d.rate_eff().max(0.1);
        let bar = g.period_s * 4.0;
        let phase = phase_in(&g, d.position_s(), 4);
        let mut best = mid;
        let mut best_d = f64::MAX;
        let mut k = 0.0;
        while k < 40.0 {
            let at = now + (k * bar - phase) / rate;
            if at >= now + length_s * 0.25 && at <= now + length_s * 0.75 && (at - mid).abs() < best_d {
                best_d = (at - mid).abs();
                best = at;
            }
            k += 1.0;
        }
        let _ = t;
        best
    }

    /// Residual of the incoming's beat phase against the outgoing's, in wall
    /// seconds (positive: incoming late). `None` when the beat periods do not match.
    fn phase_residual(&self, o: usize, i: usize, og: &BeatGrid, ig: &BeatGrid) -> Option<f64> {
        let (od, id) = (&self.ch[o].deck, &self.ch[i].deck);
        let orate = od.rate_eff().max(0.1);
        let irate = id.rate_eff().max(0.1);
        let op_wall = og.period_s / orate;
        let ip_wall = ig.period_s / irate;
        if (op_wall / ip_wall - 1.0).abs() > 0.12 {
            return None;
        }
        let out_phase = phase_at(og, od.position_s()) / orate;
        let in_phase = phase_at(ig, id.position_s()) / irate;
        Some(residual(out_phase, in_phase, ip_wall))
    }

    /// Push a deck forward (+) or back (-) by `delta_s` of media time, as a
    /// few-percent rate bend (port of `bendBy`).
    fn bend_by(&mut self, deck: usize, delta_s: f64) {
        let d = &mut self.ch[deck].deck;
        if d.is_bending() || delta_s.abs() < 0.001 {
            return;
        }
        let secs = (delta_s.abs() / BEND_PCT).min(BEND_MAX_S);
        d.bend(delta_s.signum() * BEND_PCT, secs);
    }

    fn retime(&mut self, remaining_s: f64) {
        let now = self.now();
        let Some(mut t) = self.tr else { return };
        if !matches!(t.stage, Stage::Running) {
            return;
        }
        let mut r = remaining_s.max(CUT_S);
        {
            let d = &self.ch[t.out].deck;
            if let Some(end) = d.end_frame().or((d.len_frames > 0).then_some(d.len_frames)) {
                let left = (end as f64 - d.position_frames()) / self.sr / d.rate_eff().max(0.1);
                r = r.min((left - 0.2).max(CUT_S));
            }
        }
        t.env_in = retimed(&t.env_in, now, now + r);
        t.env_out = retimed(&t.env_out, now, now + r);
        t.t1 = now + r;
        for e in t.ext.iter_mut().flatten() {
            if e.step {
                let cur = e.env.value_at(now);
                e.env = Env::new(cur, e.fin, now, now + 0.03, Shape::Linear);
            } else {
                e.env = retimed(&e.env, now, now + r);
            }
        }
        self.ch[t.inc].gain = Auto::Env(t.env_in);
        self.ch[t.out].gain = Auto::Env(t.env_out);
        for e in t.ext.iter().flatten() {
            match e.which {
                ExtKind::Filter => self.ch[e.ch].filter = Auto::Env(e.env),
                ExtKind::Low => self.ch[e.ch].low = Auto::Env(e.env),
            }
        }
        let cur = self.ch[t.out].send.value(now);
        if let (true, Some(e)) = (t.echo_on, t.spec.echo) {
            self.ch[t.out].send = Auto::Pw(Piecewise::from_steps(now, cur, send_steps(t.t1, &e, now)));
        } else {
            self.ch[t.out].send = self.ch[t.out].send.ramp_to(now, 0.0, 0.1);
        }
        t.park_t = t.t1 + t.spec.park_tail_s;
        t.fade_done = false;
        self.tr = Some(t);
        self.emit(Event::TransitionRetimed { end_t: now + r });
    }

    fn cut_now(&mut self) {
        let Some(mut t) = self.tr else { return };
        if !matches!(t.stage, Stage::Running) {
            return;
        }
        if t.echo_on
            && let Some(e) = t.spec.echo.as_mut() {
                e.hold_s = 0.6;
                e.up_s = 0.05;
                e.send = 0.7;
            }
        self.tr = Some(t);
        self.retime(CUT_S);
        if let Some(t) = self.tr.as_mut() {
            t.dismissed = true;
        }
    }

    fn set_echo_live(&mut self, on: bool) {
        let now = self.now();
        let Some(mut t) = self.tr else { return };
        if !matches!(t.stage, Stage::Running) {
            return;
        }
        t.echo_on = on;
        if on {
            let e = t.spec.echo.unwrap_or(EchoSpec { send: 0.65, hold_s: 3.0, up_s: 0.3, bpm: None });
            t.spec.echo = Some(e);
            t.spec.park_tail_s = ECHO_TAIL_S;
            t.park_t = t.t1 + t.spec.park_tail_s;
            let cur = self.ch[t.out].send.value(now);
            self.ch[t.out].send = Auto::Pw(Piecewise::from_steps(now, cur, send_steps(t.t1, &e, now)));
        } else {
            self.ch[t.out].send = self.ch[t.out].send.ramp_to(now, 0.0, 0.1);
        }
        self.tr = Some(t);
    }

    fn set_sync_live(&mut self, on: bool) {
        let Some(mut t) = self.tr else { return };
        let Some(s) = t.spec.sync else { return };
        t.sync_on = on;
        self.ch[t.inc].deck.glide_to(if on { s.rate } else { 1.0 }, 2.0);
        if !on {
            self.ch[t.inc].deck.set_pll_trim(0.0);
        }
        self.tr = Some(t);
    }

    /// End the running blend immediately (park the outgoing, incoming at unity).
    fn finish_transition(&mut self, instant: bool) {
        self.finish_transition_keeping(instant, None);
    }

    /// Like [`finish_transition`](Self::finish_transition), but a deck that a *new* transition
    /// is about to use (`keep`) is not emptied: it was just re-cued.
    fn finish_transition_keeping(&mut self, instant: bool, keep: Option<usize>) {
        let now = self.now();
        let Some(t) = self.tr.take() else { return };
        let (o, i) = (t.out, t.inc);
        if matches!(t.stage, Stage::Waiting { .. }) {
            // never started: the incoming deck simply stays cued
            return;
        }
        if instant {
            self.ch[o].gain = self.ch[o].gain.ramp_to(now, 0.0, 0.02);
            self.ch[o].send = self.ch[o].send.ramp_to(now, 0.0, 0.02);
            self.ch[i].gain = self.ch[i].gain.ramp_to(now, 1.0, 0.02);
            self.ch[i].filter = self.ch[i].filter.ramp_to(now, 0.0, 0.02);
            self.ch[i].low = self.ch[i].low.ramp_to(now, 1.0, 0.02);
        }
        self.ch[i].deck.set_pll_trim(0.0);
        if keep != Some(o) {
            self.ch[o].deck.clear();
            self.ch[o].reset_lanes(0.0);
        }
        self.emit(Event::FadeDone);
        self.emit(Event::Parked { deck: o as u8 });
        let _ = t;
    }

    /// Per-segment transition bookkeeping.
    fn tick_transition(&mut self) {
        let now_f = self.frames;
        let now = self.now();
        let Some(mut t) = self.tr else { return };
        match t.stage {
            Stage::Waiting { start_frame, deadline } => {
                let out_ended = self.ch[t.out].deck.state() == DeckState::Ended;
                let inc_ready = self.ch[t.inc].deck.ready();
                if self.ch[t.inc].deck.state() == DeckState::Empty && now_f >= deadline {
                    // nothing to blend into: abandon
                    self.tr = None;
                    return;
                }
                if (now_f >= start_frame && inc_ready) || out_ended || now_f >= deadline {
                    self.tr = Some(t);
                    self.begin_running();
                }
            }
            Stage::Running => {
                // PLL
                if now_f >= t.next_pll {
                    t.next_pll = now_f + PLL_PERIOD_FRAMES;
                    if t.spec.phase_lock && t.sync_on && t.spec.sync.is_some() && !self.ch[t.inc].deck.is_bending() && now < t.t1 + 2.0
                        && let (Some(og), Some(ig)) = (self.ch[t.out].deck.grid, self.ch[t.inc].deck.grid)
                            && let Some(r) = self.phase_residual(t.out, t.inc, &og, &ig) {
                                self.phase_err_ms = (r * 1000.0) as f32;
                                let dt = PLL_PERIOD_FRAMES as f64 / self.sr;
                                t.pll_i = (t.pll_i + r * PLL_KI * dt).clamp(-PLL_MAX_TRIM, PLL_MAX_TRIM);
                                let trim = (r * PLL_KP + t.pll_i).clamp(-PLL_MAX_TRIM, PLL_MAX_TRIM);
                                self.ch[t.inc].deck.set_pll_trim(trim);
                                t.phase = Some(PhaseState::Locked);
                            }
                }
                if !t.fade_done && now >= t.t1 {
                    t.fade_done = true;
                    // the incoming is the track now
                    self.ch[t.inc].gain = Auto::Const(1.0);
                    self.ch[t.inc].filter = Auto::Const(0.0);
                    self.ch[t.inc].low = Auto::Const(1.0);
                    self.ch[t.inc].deck.set_pll_trim(0.0);
                    self.ch[t.out].gain = Auto::Const(0.0);
                    self.emit(Event::FadeDone);
                }
                let out_ended = self.ch[t.out].deck.state() == DeckState::Ended;
                if now >= t.park_t || (out_ended && t.fade_done) {
                    // park the outgoing; the echo bus keeps ringing
                    let (o, i) = (t.out, t.inc);
                    self.ch[o].deck.clear();
                    self.ch[o].reset_lanes(0.0);
                    if let Some(s) = t.spec.sync {
                        let d = &mut self.ch[i].deck;
                        if !s.hold && (d.rate_base - 1.0).abs() > 1e-4 {
                            d.glide_to(1.0, GLIDE_BACK_S);
                        }
                    }
                    self.tr = None;
                    self.phase_err_ms = f32::NAN;
                    self.emit(Event::Parked { deck: o as u8 });
                    return;
                }
                self.tr = Some(t);
            }
        }
    }

    // ------------------------------------------------------------------
    // rendering
    // ------------------------------------------------------------------

    /// Render `frames` interleaved stereo frames into `out`. `output_ts_ns` is
    /// the unix time at which the first frame will leave the device (0 = unknown).
    pub fn render(&mut self, out: &mut [f32], frames: usize, output_ts_ns: u64) {
        self.drain_cmds();
        // a command that asked for "now" begins at this block's first frame
        self.tick_transition();
        self.output_ts_ns = output_ts_ns;
        self.publish();
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_SEG).min(self.until_next_event());
            let n = n.max(1);
            self.segment(&mut out[done * 2..(done + n) * 2], n);
            done += n;
            self.frames += n as u64;
            self.tick();
        }
        // decay-held peak meter
    }

    fn until_next_event(&self) -> usize {
        let mut m = MAX_SEG as u64;
        if let Some(Tr { stage: Stage::Waiting { start_frame, .. }, .. }) = self.tr
            && start_frame > self.frames {
                m = m.min(start_frame - self.frames);
            }
        m as usize
    }

    fn tick(&mut self) {
        // underruns and readiness
        for d in 0..2 {
            let under = self.ch[d].deck.take_underrun() && self.ch[d].deck.state() == DeckState::Playing && !self.paused_silent();
            if under && !self.under[d] {
                // one xrun per underrun episode, not per 64-frame segment
                self.xruns += 1;
                self.emit(Event::Underrun { deck: d as u8, frame: self.frames });
            }
            self.under[d] = under;
            if !self.ready_sent[d] && self.ch[d].deck.state() == DeckState::Cued && self.ch[d].deck.ready() {
                self.ready_sent[d] = true;
                let epoch = self.ch[d].deck.epoch();
                self.emit(Event::Ready { deck: d as u8, epoch });
            }
        }
        if !self.ready_sent[PREVIEW] && self.preview.state() == DeckState::Cued && self.preview.ready() {
            self.ready_sent[PREVIEW] = true;
            let epoch = self.preview.epoch();
            self.emit(Event::Ready { deck: PREVIEW as u8, epoch });
        }
        if let Some((d, at)) = self.pending_clear
            && self.frames >= at {
                self.pending_clear = None;
                if d != self.active {
                    self.ch[d].deck.clear();
                    self.ch[d].reset_lanes(0.0);
                }
            }
        // a pending seek completes once the dip has faded to silence
        if let Some(p) = self.pending_seek
            && self.ch[p.deck].dip.v < 0.002 {
                self.ch[p.deck].deck.seek(p.epoch, p.frame);
                self.ch[p.deck].dip_target = 1.0;
                self.ready_sent[p.deck] = false;
                self.pending_seek = None;
            }
        // a pending play starts when its deck has data (or the wait times out)
        if let Some((d, deadline)) = self.pending_play {
            let st = self.ch[d].deck.state();
            if st == DeckState::Cued && (self.ch[d].deck.ready() || self.frames >= deadline) {
                self.pending_play = None;
                self.start_play(d);
            } else if st == DeckState::Empty {
                self.pending_play = None;
            }
        }
        self.tick_transition();
        // dips for non-seek channels return to 1
        for c in 0..2 {
            if self.pending_seek.map(|p| p.deck) != Some(c) {
                self.ch[c].dip_target = 1.0;
            }
        }
    }

    fn publish(&mut self) {
        let mut s = Snapshot {
            frames_played: self.frames,
            sample_rate: self.sr as u32,
            output_ts_ns: self.output_ts_ns,
            active: self.active as u8,
            paused: self.paused,
            transitioning: self.tr.map(|t| matches!(t.stage, Stage::Running)).unwrap_or(false),
            xruns: self.xruns,
            peak_l: self.peak[0],
            peak_r: self.peak[1],
            limiter_gr_db: (20.0 * self.limiter.min_gain.max(1e-6).log10()) as f32,
            phase_err_ms: self.phase_err_ms,
            decks: [DeckSnap::default(); DECKS],
        };
        self.limiter.min_gain = 1.0;
        self.peak = [0.0; 2];
        for i in 0..2 {
            let d = &mut self.ch[i].deck;
            let ready = d.state() != DeckState::Empty && d.ready();
            s.decks[i] = DeckSnap {
                pos_frames: d.position_frames(),
                rate: d.rate_eff(),
                state: d.state() as u8,
                ready,
                epoch: d.epoch(),
                buffered_s: d.buffered_s(),
                len_frames: d.end_frame().unwrap_or(d.len_frames),
            };
        }
        let p = &mut self.preview;
        let ready = p.state() != DeckState::Empty && p.ready();
        s.decks[PREVIEW] = DeckSnap {
            pos_frames: p.position_frames(),
            rate: p.rate_eff(),
            state: p.state() as u8,
            ready,
            epoch: p.epoch(),
            buffered_s: p.buffered_s(),
            len_frames: p.end_frame().unwrap_or(p.len_frames),
        };
        self.shared.store(&s);
    }

    fn segment(&mut self, out: &mut [f32], n: usize) {
        let play_silent = self.paused && self.play_gain.v < 1e-4;
        let mut produced = [0usize; 2];
        let mut retire: Option<usize> = None;
        if !play_silent {
            for d in 0..2 {
                let mut buf = std::mem::take(&mut self.tmp[d]);
                produced[d] = self.ch[d].deck.render(&mut buf, n);
                self.tmp[d] = buf;
            }
            // gapless: the active deck ended mid-segment with no blend in flight
            let a = self.active;
            if produced[a] < n && self.ch[a].deck.state() == DeckState::Ended {
                let epoch = self.ch[a].deck.epoch();
                let in_blend = self.tr.is_some();
                if !in_blend {
                    if let Some(next) = self.armed_next.filter(|&x| x != a) {
                        if self.ch[next].deck.state() == DeckState::Cued {
                            let k = produced[a];
                            self.ch[next].deck.start();
                            self.ch[next].reset_lanes(1.0);
                            retire = Some(a);
                            let mut buf = std::mem::take(&mut self.tmp[next]);
                            let _got = self.ch[next].deck.render(&mut buf[k * 2..], n - k);
                            self.tmp[next] = buf;
                            self.active = next;
                            self.armed_next = None;
                            self.ch[a].deck.clear();
                            let frame = self.frames + k as u64;
                            self.emit(Event::Advanced { from: a as u8, to: next as u8, frame });
                            self.emit(Event::Started { deck: next as u8, frame });
                        }
                    } else {
                        self.ch[a].deck.clear();
                        self.emit(Event::Ended { deck: a as u8, epoch });
                    }
                }
            } else if self.tr.is_some() {
                for d in 0..2 {
                    if produced[d] < n && self.ch[d].deck.state() == DeckState::Ended {
                        // an outgoing/incoming deck that ran dry stays Ended until parked
                    }
                }
            }
        } else {
            for d in 0..2 {
                self.tmp[d][..n * 2].iter_mut().for_each(|s| *s = 0.0);
            }
        }
        let mut pbuf = std::mem::take(&mut self.tmp[PREVIEW]);
        let pgot = if !play_silent || true { self.preview.render(&mut pbuf, n) } else { 0 };
        self.tmp[PREVIEW] = pbuf;
        let preview_on = pgot > 0;
        if self.preview.state() == DeckState::Ended {
            self.preview.clear();
            let epoch = self.preview.epoch();
            self.emit(Event::Ended { deck: PREVIEW as u8, epoch });
        }

        let paused = self.paused;
        for i in 0..n {
            let t = (self.frames + i as u64) as f64 / self.sr;
            let mut dry = [0.0f64; 2];
            let mut send = [0.0f64; 2];
            for c in 0..2 {
                let ch = &mut self.ch[c];
                let x = [self.tmp[c][i * 2] as f64, self.tmp[c][i * 2 + 1] as f64];
                let (lo, mi, hi) = (ch.low.value(t), ch.mid.value(t), ch.high.value(t));
                ch.flt.set_knob(ch.filter.value(t));
                let dip = ch.dip.step(ch.dip_target);
                let g = ch.gain.value(t) * ch.trim * dip;
                let s = ch.send.value(t);
                for k in 0..2 {
                    let v = ch.eq.process(k, x[k], lo, mi, hi);
                    let v = ch.flt.process(k, v);
                    dry[k] += v * g;
                    send[k] += v * s;
                }
            }
            if preview_on {
                dry[0] += self.tmp[PREVIEW][i * 2] as f64 * self.preview_gain;
                dry[1] += self.tmp[PREVIEW][i * 2 + 1] as f64 * self.preview_gain;
            }
            // manual echo throw from the strip
            let es = self.strip[3].step(self.strip_target[3]);
            if es > 1e-6 {
                send[0] += dry[0] * es;
                send[1] += dry[1] * es;
            }
            let wet = self.echo.process(send);
            let mut m = [dry[0] + wet[0], dry[1] + wet[1]];
            // master strip
            let gl = self.strip[0].step(self.strip_target[0]);
            let gm = self.strip[1].step(self.strip_target[1]);
            let gh = self.strip[2].step(self.strip_target[2]);
            let kf = self.strip_filter.step(self.strip_filter_target);
            self.master_flt.set_knob(kf);
            for k in 0..2 {
                m[k] = self.master_eq.process(k, m[k], gl, gm, gh);
                m[k] = self.master_flt.process(k, m[k]);
            }
            let vol = self.vol.step(self.vol_target);
            let pg = self.play_gain.step(if paused { 0.0 } else { 1.0 });
            let g = vol * pg;
            let y = self.limiter.process([(m[0] * g) as f32, (m[1] * g) as f32]);
            out[i * 2] = y[0];
            out[i * 2 + 1] = y[1];
            self.peak[0] = self.peak[0].max(y[0].abs());
            self.peak[1] = self.peak[1].max(y[1].abs());
        }
        if let Some(a) = retire {
            self.ch[a].reset_lanes(0.0);
        }
    }
}

/// A standalone cue (headphone) output: one deck, a gain and a limiter, for a
/// second audio device. Same command/event vocabulary as the preview deck.
pub struct CueOut {
    sr: f64,
    deck: Deck,
    gain: Smooth,
    gain_target: f64,
    limiter: Limiter,
    cmds: Consumer<Cmd>,
    events: Producer<Event>,
    shared: Arc<SharedState>,
    frames: u64,
    ready_sent: bool,
    buf: Vec<f32>,
}

impl CueOut {
    pub fn new(sr: f64, ring: Consumer<Chunk>, cmds: Consumer<Cmd>, events: Producer<Event>, shared: Arc<SharedState>) -> Self {
        Self {
            sr,
            deck: Deck::new(sr, ring, StretchQuality::Fast),
            gain: Smooth::new(1.0, sr, 0.01),
            gain_target: 1.0,
            limiter: Limiter::new(sr, -1.0, 2.0, 80.0),
            cmds,
            events,
            shared,
            frames: 0,
            ready_sent: false,
            buf: vec![0.0; MAX_BLOCK * 2],
        }
    }

    pub fn render(&mut self, out: &mut [f32], frames: usize, output_ts_ns: u64) {
        while let Ok(c) = self.cmds.pop() {
            match c {
                Cmd::PreviewCue { epoch, frame, len_frames, gain } => {
                    self.deck.cue(epoch, frame, len_frames);
                    self.gain_target = gain;
                    self.ready_sent = false;
                }
                Cmd::PreviewPlay => self.deck.start(),
                Cmd::PreviewStop => self.deck.clear(),
                Cmd::SetPreviewGain { gain } => self.gain_target = gain,
                Cmd::Seek { epoch, frame, .. } => {
                    self.deck.seek(epoch, frame);
                    self.ready_sent = false;
                }
                _ => {}
            }
        }
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK);
            let got = self.deck.render(&mut self.buf, n);
            for i in 0..n {
                let g = self.gain.step(self.gain_target) as f32;
                let y = self.limiter.process([self.buf[i * 2] * g, self.buf[i * 2 + 1] * g]);
                out[(done + i) * 2] = y[0];
                out[(done + i) * 2 + 1] = y[1];
            }
            if got < n && self.deck.state() == DeckState::Ended {
                let epoch = self.deck.epoch();
                self.deck.clear();
                let _ = self.events.push(Event::Ended { deck: PREVIEW as u8, epoch });
            }
            done += n;
        }
        if !self.ready_sent && self.deck.state() == DeckState::Cued && self.deck.ready() {
            self.ready_sent = true;
            let epoch = self.deck.epoch();
            let _ = self.events.push(Event::Ready { deck: PREVIEW as u8, epoch });
        }
        let mut s = Snapshot { frames_played: self.frames, sample_rate: self.sr as u32, output_ts_ns, ..Default::default() };
        s.decks[PREVIEW] = DeckSnap {
            pos_frames: self.deck.position_frames(),
            rate: self.deck.rate_eff(),
            state: self.deck.state() as u8,
            ready: self.deck.state() != DeckState::Empty,
            epoch: self.deck.epoch(),
            buffered_s: self.deck.buffered_s(),
            len_frames: self.deck.end_frame().unwrap_or(self.deck.len_frames),
        };
        self.shared.store(&s);
        self.frames += frames as u64;
    }
}
