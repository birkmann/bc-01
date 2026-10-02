//! `PlayerSession`: the server-side player. Owns the queue, history, shuffle,
//! repeat, sources, the DJ-mix trigger, auto-fill and the audio [`Engine`], and
//! survives UI reloads and window closes.
//!
//! Ports of the browser player's `store.ts` / `autoFill.ts` / `mixFrom.ts`
//! logic, plus the native upgrades: sample-accurate gapless advance (the next
//! track is pre-decoded on the idle deck and spliced inside the audio
//! callback), beat-quantised starts, true shuffle order (no repeats), real
//! transition types and a cue/preview deck.

use crate::autofill::{self, FILL_TO, FillJob, FillResult, RETRY_MS};
use crate::decode::{DecodeError, Opened, Source};
use crate::host::{DECK_A, Engine, OutputKind, PREVIEW, StreamInfo, list_output_devices, unix_ns};
use crate::mixpoints::{self, MixPoints, OUT_BEFORE_END_S};
use crate::plan;
use crate::ports::{PortError, Ports, Rng};
use crate::queue_ops::{self, QueueShape};
use bc_dsp::beatgrid::{BeatGrid, grid_absolute, grid_for, snap_bar_nearest, snap_nearest};
use bc_dsp::beatmatch::{BlendSpec, IncomingFacts, OutgoingFacts, cut_spec, plan_blend, short_blend, trigger_point, trigger_slack_s};
use bc_dsp::mixer::{Cmd, Event};
use bc_dsp::norm::trim_gain;
use bc_dsp::stretch::StretchQuality;
use bc_types::player::*;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Receives everything the session publishes (the service maps it onto the event bus).
pub trait Publisher: Send + Sync {
    fn state(&self, s: &PlayerState);
    fn clock(&self, c: &Clock);
    fn transition(&self, t: Option<&TransitionState>);
}

pub struct NullPublisher;
impl Publisher for NullPublisher {
    fn state(&self, _: &PlayerState) {}
    fn clock(&self, _: &Clock) {}
    fn transition(&self, _: Option<&TransitionState>) {}
}

#[derive(Debug, Clone)]
pub enum PlayerReply {
    Ok,
    Devices(DevicesInfo),
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum PlayerError {
    #[error("bad command: {0}")]
    BadCommand(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("audio unavailable: {0}")]
    Unavailable(String),
}

pub type Reply = tokio::sync::oneshot::Sender<Result<PlayerReply, PlayerError>>;

#[allow(clippy::large_enum_variant)]
pub enum SessionMsg {
    Cmd(PlayerCommand, Option<Reply>),
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handover {
    None,
    Short,
    Long,
}

/// What a deck holds.
#[derive(Debug, Clone)]
struct Loaded {
    uid: u64,
    dur_s: f64,
    bpm: Option<f64>,
    points: MixPoints,
}

#[derive(Debug, Clone, Default)]
struct TrackInfo {
    dur_s: f64,
    bpm: Option<f64>,
    grid: Option<BeatGrid>,
    lufs: Option<f64>,
    true_peak: Option<f64>,
    points: Option<MixPoints>,
}

enum Stage {
    Opening(Receiver<Result<Opened, DecodeError>>),
    Priming,
}

/// A deck being loaded for a start / blend / prime.
struct Loading {
    deck: usize,
    idx: usize,
    uid: u64,
    epoch: u32,
    start_s: f64,
    start_frame: u64,
    stage: Stage,
    ready: bool,
    cued: bool,
    spec: Option<BlendSpec>,
    handover: Handover,
    began: Instant,
    /// Cut to the current deck's audio when ready (vs. play from silence / blend).
    purpose: Purpose,
    autoplay: bool,
    /// a primed deck whose gapless arm has been sent
    armed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Purpose {
    /// Fire as soon as ready.
    Start,
    /// Pre-decode the next track (gapless / blend), fire later.
    Prime,
}

#[allow(clippy::large_enum_variant)]
enum BgJob {
    Fill(FillJob),
    Advance { run: u64, early: bool, source: QueueSource },
    StartSource { run: u64, source: QueueSource, shuffle: bool },
    Sweep { run: u64, source: QueueSource },
    RecordPlay { track_id: i64, ms: i64, completed: bool, skipped: bool },
    Persist { key: String, value: String },
}

enum BgResult {
    Fill(FillResult),
    Advance { run: u64, early: bool, items: Vec<QueueItem>, source: Option<QueueSource> },
    Started {
        run: u64,
        items: Vec<QueueItem>,
        source: Option<QueueSource>,
        shuffle: bool,
        error: Option<String>,
        /// A DJ set's planned cues: `(track_id, points)` pinned before the queue starts.
        pins: Vec<Pin>,
    },
    Sweep { run: u64, items: Vec<QueueItem>, source: QueueSource, done: bool },
}

/// A preview deck being loaded.
struct PreviewLoad {
    epoch: u32,
    rx: Receiver<Result<Opened, DecodeError>>,
    start_s: f64,
    cued: bool,
}

pub struct SessionConfig {
    pub output: OutputKind,
    pub quality: StretchQuality,
    pub base_url: String,
    /// Register the MPRIS endpoint (media keys, `playerctl`). Default: on unless `BC_MPRIS=0`.
    pub mpris: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            output: OutputKind::Cpal(OutputTarget::default()),
            quality: StretchQuality::Normal,
            base_url: "http://127.0.0.1:8420".into(),
            mpris: std::env::var("BC_MPRIS").map(|v| v != "0").unwrap_or(true),
        }
    }
}

/// Max consecutive undecodable tracks stepped over before stopping.
const MAX_LOAD_FAILS: u32 = 5;
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_HISTORY: usize = 500;
/// How far ahead an explore sweep keeps the queue.
const SWEEP_LOOKAHEAD: usize = 12;
/// The same for a shuffled sweep, counted in unplayed tracks: new tracks join the unplayed part
/// at random places, so a deep lookahead is what mixes a whole catalogue together (an artist's
/// releases all load in the background) instead of two albums at a time.
const SHUFFLE_SWEEP_LOOKAHEAD: usize = 400;
const MAX_QUEUE: usize = 5000;

pub struct Session {
    ports: Ports,
    publisher: Box<dyn Publisher>,
    cfg: SessionConfig,
    engine: Option<Engine>,
    pub st: PlayerState,
    next_uid: u64,
    shuffle_unplayed: Vec<usize>,
    rng: Rng,
    decks: [Option<Loaded>; 2],
    cur_deck: Option<usize>,
    loading: Option<Loading>,
    primed: Option<Loading>,
    infos: HashMap<i64, TrackInfo>,
    pinned: HashMap<i64, MixPoints>,
    /// `(queue_rev, queue_index, mix)` the published entry points were computed for.
    entry_key: Option<(u64, i64, bool)>,
    /// A set's per-slot tempo adjustment `(rate, key_lock)`, applied live exactly as the offline render does.
    adjust: HashMap<i64, (f64, bool)>,
    mix_fired: bool,
    early_run: i64,
    started_at_s: f64,
    counted_play: bool,
    load_fails: u32,
    // transition
    tr: Option<TransitionState>,
    tr_end_engine_s: f64,
    tr_spec: Option<BlendSpec>,
    // background
    bg_tx: Sender<BgJob>,
    bg_rx: Receiver<BgResult>,
    fill_inflight: bool,
    fill_run: u64,
    last_empty: Option<(String, Instant)>,
    advance_run: u64,
    sweep_inflight: bool,
    // publishing
    d_state: bool,
    d_queue: bool,
    d_prefs: bool,
    d_plan: bool,
    d_pos: bool,
    last_state_pub: Instant,
    last_clock_pub: Instant,
    last_phase_pub: Instant,
    last_persist_queue: Instant,
    last_persist_pos: Instant,
    queue_pub_rev: u64,
    resume_main_after_preview: bool,
    preview_loading: Option<PreviewLoad>,
    events_buf: Vec<Event>,
    pub paused_by_user: bool,
    pending_transition_ids: (Option<u64>, Option<u64>),
    /// The outgoing deck of a running blend: it still plays (fade, echo tail) until the mixer
    /// parks it, so it must not be primed for the next track before that.
    parking: Option<usize>,
    parking_since: Instant,
    /// The last engine events (bounded), for tests and diagnostics.
    pub event_log: Vec<Event>,
}

fn now_ms() -> u64 {
    unix_ns() / 1_000_000
}

fn is_remote(i: &QueueItem) -> bool {
    i.origin == ItemOrigin::Bandcamp
}

impl Session {
    pub fn new(ports: Ports, publisher: Box<dyn Publisher>, cfg: SessionConfig) -> Self {
        let (bg_tx, bg_job_rx) = unbounded::<BgJob>();
        let (bg_res_tx, bg_rx) = unbounded::<BgResult>();
        spawn_bg(ports.clone(), bg_job_rx, bg_res_tx);
        let st = PlayerState {
            queue_index: -1,
            history_pos: -1,
            volume: 0.8,
            ..Default::default()
        };
        let mut s = Session {
            ports,
            publisher,
            cfg,
            engine: None,
            st,
            next_uid: 1,
            shuffle_unplayed: vec![],
            rng: Rng::seeded(),
            decks: [None, None],
            cur_deck: None,
            loading: None,
            primed: None,
            infos: HashMap::new(),
            pinned: HashMap::new(),
            entry_key: None,
            adjust: HashMap::new(),
            mix_fired: false,
            early_run: -1,
            started_at_s: 0.0,
            counted_play: false,
            load_fails: 0,
            tr: None,
            tr_end_engine_s: 0.0,
            tr_spec: None,
            bg_tx,
            bg_rx,
            fill_inflight: false,
            fill_run: 0,
            last_empty: None,
            advance_run: 0,
            sweep_inflight: false,
            d_state: true,
            d_queue: true,
            d_prefs: false,
            d_plan: false,
            d_pos: false,
            last_state_pub: Instant::now() - Duration::from_secs(1),
            last_clock_pub: Instant::now(),
            last_phase_pub: Instant::now(),
            last_persist_queue: Instant::now(),
            last_persist_pos: Instant::now(),
            queue_pub_rev: 0,
            resume_main_after_preview: false,
            preview_loading: None,
            events_buf: Vec::new(),
            paused_by_user: false,
            pending_transition_ids: (None, None),
            parking: None,
            parking_since: Instant::now(),
            event_log: Vec::new(),
        };
        s.refresh_devices(false);
        s
    }

    // ------------------------------------------------------------------
    // main loop
    // ------------------------------------------------------------------

    /// Run until `Shutdown` (call on a dedicated thread).
    pub fn run(mut self, rx: Receiver<SessionMsg>) {
        self.restore();
        loop {
            match rx.recv_timeout(Duration::from_millis(8)) {
                Ok(SessionMsg::Shutdown) => break,
                Ok(SessionMsg::Cmd(cmd, reply)) => {
                    let r = self.handle(cmd);
                    self.publish_state_now();
                    if let Some(tx) = reply {
                        let _ = tx.send(r);
                    }
                    // drain a burst without ticking between commands
                    while let Ok(m) = rx.try_recv() {
                        match m {
                            SessionMsg::Shutdown => {
                                self.flush_persist(true);
                                return;
                            }
                            SessionMsg::Cmd(cmd, reply) => {
                                let r = self.handle(cmd);
                                self.publish_state_now();
                                if let Some(tx) = reply {
                                    let _ = tx.send(r);
                                }
                            }
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
            self.tick();
        }
        self.flush_persist(true);
    }

    /// One scheduler step (also callable directly from tests).
    pub fn tick(&mut self) {
        self.process_bg();
        self.process_engine_events();
        self.tick_loading();
        self.tick_parking();
        self.tick_preview();
        self.tick_position();
        self.maybe_fill();
        self.maybe_sweep();
        self.publish_clock_if_due();
        self.publish_state_if_due();
        self.flush_persist(false);
    }

    // ------------------------------------------------------------------
    // engine
    // ------------------------------------------------------------------

    fn engine(&mut self) -> Result<&Engine, String> {
        if self.engine.is_none() {
            let e = Engine::open(self.cfg.output.clone(), self.cfg.quality).map_err(|e| e.to_string())?;
            tracing::info!(
                "audio: {} on {} @ {} Hz, buffer {} frames (~{:.1} ms)",
                e.info.backend, e.info.device, e.info.sample_rate, e.info.buffer_frames, e.info.latency_ms
            );
            self.apply_levels(&e);
            self.engine = Some(e);
            self.refresh_devices(true);
        }
        self.engine.as_ref().ok_or_else(|| "no engine".to_string())
    }

    fn sr(&self) -> f64 {
        self.engine.as_ref().map(|e| e.sample_rate as f64).unwrap_or(48_000.0)
    }

    /// Rate of the preview deck: the cue device's when it has its own, else the main output's.
    fn preview_sr(&self) -> f64 {
        self.engine.as_ref().map(|e| e.preview_rate as f64).unwrap_or(48_000.0)
    }

    fn send(&self, c: Cmd) {
        if let Some(e) = &self.engine
            && !e.send(c) {
                tracing::warn!("audio command queue full: {c:?}");
            }
    }

    fn apply_levels(&self, e: &Engine) {
        e.send(Cmd::SetQuality(quality_code(self.st.mix_settings.key_lock_quality)));
        e.send(Cmd::SetVolume { gain: if self.st.muted { 0.0 } else { self.st.volume } });
        self.send_strip(e);
        e.send(Cmd::SetCeilingDb { db: -1.0 });
    }

    fn send_strip(&self, e: &Engine) {
        let s = &self.st.strip;
        let g = |db: f64, kill: bool| if kill { 0.0 } else { bc_dsp::filters::db_to_gain(db) };
        e.send(Cmd::SetStrip {
            low: g(s.low_db, s.kill_low),
            mid: g(s.mid_db, s.kill_mid),
            high: g(s.high_db, s.kill_high),
            filter: s.filter,
            echo_send: s.echo_send,
        });
    }

    fn refresh_devices(&mut self, opened: bool) {
        let target = match &self.cfg.output {
            OutputKind::Cpal(t) => t.clone(),
            OutputKind::Null { .. } => OutputTarget::default(),
        };
        let outputs = if opened || matches!(self.cfg.output, OutputKind::Null { .. }) { vec![] } else { list_output_devices() };
        let info: StreamInfo = self.engine.as_ref().map(|e| e.info.clone()).unwrap_or_default();
        let xr = self.engine.as_ref().map(|e| e.snapshot().xruns).unwrap_or(0);
        let prev = std::mem::take(&mut self.st.devices.outputs);
        self.st.devices = DevicesInfo {
            outputs: if outputs.is_empty() { prev } else { outputs },
            target,
            backend: if info.backend.is_empty() { "closed".into() } else { info.backend },
            sample_rate: info.sample_rate,
            buffer_frames: info.buffer_frames,
            latency_ms: info.latency_ms,
            xruns: xr,
            cue_device: info.cue_device,
        };
        self.d_state = true;
    }

    // ------------------------------------------------------------------
    // track info
    // ------------------------------------------------------------------

    fn assign_uid(&mut self, mut it: QueueItem) -> QueueItem {
        it.uid = self.next_uid;
        self.next_uid += 1;
        it
    }

    fn info_for(&mut self, item: &QueueItem) -> TrackInfo {
        if let Some(i) = self.infos.get(&item.track_id) {
            let mut i = i.clone();
            if item.duration_ms.is_some() && i.dur_s <= 0.0 {
                i.dur_s = item.duration_s().unwrap_or(0.0);
            }
            return i;
        }
        let mut info = TrackInfo { dur_s: item.duration_s().unwrap_or(0.0), bpm: item.bpm, ..Default::default() };
        if item.is_library()
            && let Ok(Some(f)) = self.ports.library.facts(item.track_id) {
                if info.dur_s <= 0.0 {
                    info.dur_s = f.duration_ms.map(|d| d as f64 / 1000.0).unwrap_or(0.0);
                }
                info.bpm = f.bpm.or(info.bpm);
                info.lufs = f.loudness_lufs;
                info.true_peak = f.true_peak_dbtp;
                info.grid = match (f.grid_origin_s, f.bpm) {
                    (Some(o), Some(b)) => grid_absolute(o, b, f.downbeat_beat.unwrap_or(0), f.bpm_confidence.unwrap_or(1.0)),
                    _ => grid_for(f.bpm, f.beat_offset_ms, f.bpm_confidence, info.dur_s),
                };
            }
        let base = mixpoints::heuristic(info.dur_s, info.bpm);
        let mut pts = base;
        if item.is_library()
            && let Ok(Some(f)) = self.ports.library.facts(item.track_id) {
                pts = mixpoints::with_cues(pts, f.mix_in_ms, f.mix_out_ms);
            }
        if item.is_library() && base.out_s.is_some()
            && let Ok(Some(peaks)) = self.ports.library.peaks(item.track_id, 200) {
                pts = mixpoints::from_peaks(&peaks, info.dur_s, base);
            }
        if let Some(p) = self.pinned.get(&item.track_id) {
            pts = *p;
        }
        info.points = Some(pts);
        if self.infos.len() > 400 {
            self.infos.clear();
        }
        self.infos.insert(item.track_id, info.clone());
        info
    }

    /// Pin a track's mix points to what a plan decided (a DJ set's cue in/out).
    pub fn pin_mix_points(&mut self, track_id: i64, p: MixPoints) {
        self.pinned.insert(track_id, p);
        self.entry_key = None;
        self.infos.remove(&track_id);
    }

    fn points_of(&mut self, item: &QueueItem) -> MixPoints {
        let info = self.info_for(item);
        info.points.unwrap_or_else(|| mixpoints::heuristic(info.dur_s, info.bpm))
    }

    fn source_for(&self, item: &QueueItem) -> Result<Source, String> {
        if is_remote(item) {
            let url = self.ports.bandcamp.resolve_stream(item).map_err(|e| e.to_string())?;
            let url = if url.starts_with('/') { format!("{}{}", self.ports.base_url.trim_end_matches('/'), url) } else { url };
            return Ok(Source::Http { url });
        }
        match self.ports.library.file_path(item.track_id) {
            Ok(Some(p)) => Ok(Source::File(p)),
            Ok(None) => Err("file missing".to_string()),
            Err(e) => Err(e.to_string()),
        }
    }

    // ------------------------------------------------------------------
    // state helpers
    // ------------------------------------------------------------------

    fn touch(&mut self) {
        self.d_state = true;
    }

    fn queue_changed(&mut self) {
        self.st.queue_rev += 1;
        self.d_queue = true;
        self.d_state = true;
        self.last_persist_queue = Instant::now() - Duration::from_secs(10); // flush soon
    }

    fn shape(&self) -> QueueShape<QueueItem> {
        QueueShape {
            queue: self.st.queue.clone(),
            queue_index: self.st.queue_index,
            history: self.st.history.clone(),
            history_pos: self.st.history_pos,
        }
    }

    fn set_current_from_index(&mut self) {
        self.st.current = usize::try_from(self.st.queue_index).ok().and_then(|i| self.st.queue.get(i)).cloned();
    }

    fn upcoming(&self) -> &[QueueItem] {
        let from = (self.st.queue_index + 1).max(0) as usize;
        self.st.queue.get(from..).unwrap_or(&[])
    }

    fn set_error(&mut self, msg: String) {
        tracing::warn!("player error: {msg}");
        self.st.error = Some(msg);
        self.st.status = PlayerStatus::Error;
        self.touch();
    }

    // ------------------------------------------------------------------
    // commands
    // ------------------------------------------------------------------

    pub fn handle(&mut self, cmd: PlayerCommand) -> Result<PlayerReply, PlayerError> {
        use PlayerCommand as C;
        match cmd {
            C::Play => self.cmd_play()?,
            C::Pause => self.cmd_pause(),
            C::Toggle => {
                if self.st.status == PlayerStatus::Playing || self.st.status == PlayerStatus::Loading {
                    self.cmd_pause();
                } else {
                    self.cmd_play()?;
                }
            }
            C::Stop => self.cmd_stop(),
            C::Seek { seconds } => self.cmd_seek(seconds),
            C::SeekRelative { delta_s } => {
                if let Some((pos, _, _)) = self.cur_pos() {
                    self.cmd_seek(pos + delta_s);
                }
            }
            C::Next => self.next(false, false),
            C::Previous => self.previous(),
            C::JumpTo { index } => {
                if index >= self.st.queue.len() {
                    return Err(PlayerError::NotFound(format!("queue index {index}")));
                }
                self.jump_to(index);
            }
            C::PlayTrack { item, queue } => {
                let list = queue.unwrap_or_else(|| vec![item.clone()]);
                let idx = list.iter().position(|t| t.track_id == item.track_id).unwrap_or(0);
                self.start_queue(list, idx, None, Handover::Short);
            }
            C::PlayQueue { items, start_index, source } => {
                if items.is_empty() {
                    return Err(PlayerError::BadCommand("empty queue".into()));
                }
                let idx = start_index.min(items.len() - 1);
                self.start_queue(items, idx, source, Handover::Short);
            }
            C::StartSource { source, shuffle } => self.cmd_start_source(source, shuffle),
            C::AddToQueue { items } => self.add_to_queue(items),
            C::InsertAt { index, items } => self.edit_queue(|s| queue_ops::insert_ops(s, index, &items)),
            C::PlayNext { items } => {
                let at = (self.st.queue_index + 1).max(0) as usize;
                self.edit_queue(|s| queue_ops::insert_ops(s, at, &items))
            }
            C::MoveInQueue { from, to } => self.edit_queue(|s| queue_ops::move_ops(s, from, to)),
            C::RemoveAt { index } => self.edit_queue(|s| queue_ops::remove_ops(s, index, index + 1)),
            C::RemoveRange { from, to } => self.edit_queue(|s| queue_ops::remove_ops(s, from, to)),
            C::ReplaceAt { index, item } => self.edit_queue(|s| queue_ops::replace_ops(s, index, item.clone())),
            C::SetVolume { volume } => {
                self.st.volume = volume.clamp(0.0, 1.0);
                self.st.muted = self.st.volume == 0.0;
                self.send(Cmd::SetVolume { gain: if self.st.muted { 0.0 } else { self.st.volume } });
                self.d_prefs = true;
                self.touch();
            }
            C::SetMuted { muted } => self.set_muted(muted),
            C::ToggleMute => {
                let m = !self.st.muted;
                self.set_muted(m)
            }
            C::SetRepeat { mode } => {
                self.st.repeat = mode;
                self.d_prefs = true;
                self.touch();
            }
            C::CycleRepeat => {
                self.st.repeat = match self.st.repeat {
                    RepeatMode::Off => RepeatMode::All,
                    RepeatMode::All => RepeatMode::One,
                    RepeatMode::One => RepeatMode::Off,
                };
                self.d_prefs = true;
                self.touch();
                self.after_order_change();
            }
            C::SetShuffle { on } => self.set_shuffle(on),
            C::ToggleShuffle => {
                let on = !self.st.shuffle;
                self.set_shuffle(on)
            }
            C::SetMix { on } => self.set_mix(on),
            C::ToggleMix => {
                let on = !self.st.mix;
                self.set_mix(on)
            }
            C::SetMixSettings { patch } => {
                self.st.mix_settings.apply(&patch);
                self.send(Cmd::SetQuality(quality_code(self.st.mix_settings.key_lock_quality)));
                self.d_prefs = true;
                self.touch();
                // a different plan primes the next track at a different point
                self.invalidate_prime();
                self.prime_next();
            }
            C::CutNow => self.send(Cmd::CutNow),
            C::Retime { factor } => self.retime(factor),
            C::SetTransitionEcho { on } => {
                self.send(Cmd::SetEcho { on });
                if let Some(t) = self.tr.as_mut() {
                    t.echo = on;
                    self.publish_transition();
                }
            }
            C::SetTransitionSync { on } => {
                self.send(Cmd::SetSync { on });
                if let Some(t) = self.tr.as_mut() {
                    if let Some(s) = t.sync.as_mut() {
                        s.on = on;
                    }
                    self.publish_transition();
                }
            }
            C::Nudge { delta_s } => self.send(Cmd::Nudge { delta_s }),
            C::SetMixOutOverride { seconds } => {
                self.st.mix_out_override_s = seconds;
                if seconds.is_some() {
                    self.mix_fired = false;
                }
                self.touch();
            }
            C::MixNow => {
                if self.st.status == PlayerStatus::Playing && self.st.current.is_some() {
                    self.mix_fired = true;
                    self.next(true, true);
                } else {
                    return Err(PlayerError::Conflict("nothing is playing".into()));
                }
            }
            C::SetStrip { patch } => {
                self.st.strip.apply(&patch);
                if let Some(e) = &self.engine {
                    self.send_strip(e);
                }
                self.d_prefs = true;
                self.touch();
            }
            C::Plan { op } => {
                if plan::apply(&mut self.st.plan, op.clone(), now_ms()) {
                    if matches!(op, PlanOp::SetPools { .. } | PlanOp::MixFrom { .. } | PlanOp::ChainPool { .. } | PlanOp::RemovePool { .. }) {
                        self.st.plan.spent_pool = None;
                        self.last_empty = None;
                    }
                    if matches!(op, PlanOp::SetAutoFill { on: true }) {
                        self.last_empty = None;
                    }
                    self.d_plan = true;
                    self.touch();
                }
            }
            C::SetPlan { plan } => {
                self.st.plan = plan;
                self.st.plan.spent_pool = None;
                self.last_empty = None;
                self.d_plan = true;
                self.touch();
            }
            C::StartMixFrom { pool } => self.start_mix_from(pool),
            C::SetOutput { target } => self.cmd_set_output(target)?,
            C::ListDevices => {
                self.st.devices.outputs = list_output_devices();
                return Ok(PlayerReply::Devices(self.st.devices.clone()));
            }
            C::PreviewStart { item, at_s } => self.preview_start(item, at_s),
            C::PreviewStop => self.preview_stop(true),
            C::MarkLoved { track_id, loved } => {
                for t in self.st.queue.iter_mut().filter(|t| t.track_id == track_id && t.is_library()) {
                    t.loved = loved;
                }
                if let Some(c) = self.st.current.as_mut().filter(|c| c.track_id == track_id && c.is_library()) {
                    c.loved = loved;
                }
                self.queue_changed();
            }
            C::ClearError => {
                self.st.error = None;
                if self.st.status == PlayerStatus::Error {
                    self.st.status = if self.st.current.is_some() { PlayerStatus::Paused } else { PlayerStatus::Idle };
                }
                self.touch();
            }
        }
        Ok(PlayerReply::Ok)
    }

    fn set_muted(&mut self, muted: bool) {
        self.st.muted = muted;
        self.send(Cmd::SetVolume { gain: if muted { 0.0 } else { self.st.volume } });
        self.d_prefs = true;
        self.touch();
    }

    fn cmd_play(&mut self) -> Result<(), PlayerError> {
        match self.st.status {
            PlayerStatus::Playing | PlayerStatus::Loading => {}
            PlayerStatus::Paused if self.cur_deck.is_some() && self.loading.is_none() => {
                if let (Some(e), Some(d)) = (self.engine.as_ref(), self.cur_deck) {
                    if e.snapshot().decks[d].state == 1 {
                        // restored but never started: begin playing the cued deck
                        e.send(Cmd::Play { deck: d as u8 });
                    } else {
                        e.send(Cmd::Resume);
                    }
                }
                self.paused_by_user = false;
                self.st.status = PlayerStatus::Playing;
                self.touch();
            }
            _ => {
                // idle / paused-after-restore / error: (re)start the current row
                if let Some(idx) = usize::try_from(self.st.queue_index).ok().filter(|i| *i < self.st.queue.len()) {
                    let pos = self.st.mix_out_override_s.map(|_| 0.0);
                    let _ = pos;
                    self.load_track(idx, Handover::None, true, None);
                } else if !self.st.queue.is_empty() {
                    self.st.queue_index = 0;
                    self.set_current_from_index();
                    self.st.history = vec![0];
                    self.st.history_pos = 0;
                    self.load_track(0, Handover::None, true, None);
                } else {
                    return Err(PlayerError::Conflict("the queue is empty".into()));
                }
            }
        }
        Ok(())
    }

    fn cmd_pause(&mut self) {
        // Pause means silence: the outgoing deck of a blend stops too, and the
        // echo tail is ducked rather than left ringing.
        if self.engine.is_some() && self.cur_deck.is_some() {
            self.send(Cmd::Pause);
        }
        if self.loading.as_ref().map(|l| l.purpose == Purpose::Start).unwrap_or(false)
            && let Some(l) = self.loading.as_mut() {
                l.autoplay = false;
            }
        if self.st.current.is_some() {
            self.st.status = PlayerStatus::Paused;
        }
        self.paused_by_user = true;
        self.d_pos = true;
        self.touch();
    }

    fn cmd_stop(&mut self) {
        if let Some(e) = &self.engine {
            e.stop_all();
        }
        self.loading = None;
        self.primed = None;
        self.decks = [None, None];
        self.cur_deck = None;
        self.parking = None;
        self.st.status = PlayerStatus::Idle;
        self.clear_transition();
        self.touch();
    }

    fn cmd_seek(&mut self, seconds: f64) {
        let Some(deck) = self.cur_deck else {
            return;
        };
        let dur = self.cur_pos().map(|(_, d, _)| d).unwrap_or(0.0);
        let mut s = seconds.max(0.0);
        if dur > 0.0 {
            s = s.min((dur - 0.05).max(0.0));
        }
        let frame = (s * self.sr()).round() as u64;
        if let Some(e) = &self.engine {
            e.seek_deck(deck, frame);
        }
        self.d_pos = true;
        self.touch();
    }

    // ------------------------------------------------------------------
    // queue
    // ------------------------------------------------------------------

    fn start_queue(&mut self, items: Vec<QueueItem>, start: usize, source: Option<QueueSource>, handover: Handover) {
        if items.is_empty() {
            return;
        }
        self.advance_run += 1;
        let mut items: Vec<QueueItem> = items.into_iter().map(|i| self.assign_uid(i)).collect();
        items.truncate(MAX_QUEUE);
        let start = start.min(items.len() - 1);
        self.st.queue = items;
        self.st.queue_index = start as i64;
        self.st.history = vec![start];
        self.st.history_pos = 0;
        self.st.source = source;
        self.st.error = None;
        self.set_current_from_index();
        self.rebuild_shuffle();
        self.queue_changed();
        self.invalidate_prime();
        self.load_track(start, handover, true, None);
    }

    fn add_to_queue(&mut self, items: Vec<QueueItem>) {
        if items.is_empty() {
            return;
        }
        let first_new = self.st.queue.len();
        for it in items {
            if self.st.queue.len() >= MAX_QUEUE {
                break;
            }
            let it = self.assign_uid(it);
            self.st.queue.push(it);
        }
        // new rows join the not-yet-played part of a shuffle at random places
        if self.st.shuffle {
            for i in first_new..self.st.queue.len() {
                let at = self.rng.below(self.shuffle_unplayed.len() + 1);
                self.shuffle_unplayed.insert(at, i);
            }
        }
        self.queue_changed();
        if first_new == (self.st.queue_index + 1).max(0) as usize {
            self.prime_next();
        }
    }

    fn edit_queue(&mut self, op: impl FnOnce(&QueueShape<QueueItem>) -> Option<QueueShape<QueueItem>>) {
        let before = self.shape();
        let was_next = before.queue.get((before.queue_index + 1).max(0) as usize).map(|t| t.uid);
        let Some(mut after) = op(&before) else { return };
        // rows coming from the client get fresh uids
        for t in after.queue.iter_mut() {
            if t.uid == 0 {
                t.uid = self.next_uid;
                self.next_uid += 1;
            }
        }
        self.st.queue = after.queue;
        self.st.history = after.history;
        self.st.history_pos = after.history_pos;
        // planning an order and shuffling per track contradict each other
        self.st.shuffle = false;
        self.shuffle_unplayed.clear();
        self.queue_changed();
        let is_next = self.st.queue.get((self.st.queue_index + 1).max(0) as usize).map(|t| t.uid);
        if was_next != is_next {
            self.invalidate_prime();
            self.prime_next();
        }
    }

    // ------------------------------------------------------------------
    // shuffle (true shuffled order, no repeats)
    // ------------------------------------------------------------------

    fn rebuild_shuffle(&mut self) {
        self.shuffle_unplayed.clear();
        if !self.st.shuffle {
            return;
        }
        let played: std::collections::HashSet<usize> = self.st.history.iter().copied().collect();
        let cur = usize::try_from(self.st.queue_index).ok();
        let mut v: Vec<usize> =
            (0..self.st.queue.len()).filter(|i| !played.contains(i) && Some(*i) != cur).collect();
        self.rng.shuffle(&mut v);
        self.shuffle_unplayed = v;
    }

    fn set_shuffle(&mut self, on: bool) {
        if self.st.shuffle == on {
            return;
        }
        self.st.shuffle = on;
        self.rebuild_shuffle();
        self.d_prefs = true;
        self.touch();
        self.after_order_change();
    }

    fn after_order_change(&mut self) {
        self.invalidate_prime();
        self.prime_next();
    }

    /// The index `next()` would play without side effects, when it is already known.
    fn peek_next_index(&self) -> Option<usize> {
        let q = self.st.queue.len();
        if q == 0 || self.st.queue_index < 0 {
            return None;
        }
        if self.st.repeat == RepeatMode::One {
            return None;
        }
        if self.st.shuffle {
            let pos = self.st.history_pos;
            if pos >= 0 && (pos as usize) + 1 < self.st.history.len() {
                return self.st.history.get(pos as usize + 1).copied();
            }
            if let Some(&i) = self.shuffle_unplayed.last() {
                return Some(i);
            }
            if self.st.repeat == RepeatMode::All && q > 1 {
                return None; // reshuffled at the time
            }
            return None;
        }
        let n = self.st.queue_index as usize + 1;
        if n < q {
            Some(n)
        } else if self.st.repeat == RepeatMode::All {
            Some(0)
        } else {
            None
        }
    }

    // ------------------------------------------------------------------
    // navigation
    // ------------------------------------------------------------------

    /// `auto` is the track running out on its own (or the mix trigger firing),
    /// as opposed to the listener pressing next. `early` is the mix trigger:
    /// the track is still audible, so nothing may go quiet or report paused.
    pub fn next(&mut self, auto: bool, early: bool) {
        if self.st.queue.is_empty() {
            return;
        }
        let handover = if auto { Handover::Long } else { Handover::Short };
        if auto && self.st.repeat == RepeatMode::One {
            self.cmd_seek(0.0);
            self.counted_play = false;
            self.mix_fired = false;
            if self.st.status == PlayerStatus::Paused {
                self.send(Cmd::Resume);
                self.st.status = PlayerStatus::Playing;
            }
            return;
        }

        // After stepping back, replay the chain forward instead of shuffling a
        // new track; shuffle only resumes at the newest end of the chain.
        if self.st.shuffle && self.st.history_pos >= 0 && (self.st.history_pos as usize) + 1 < self.st.history.len() {
            let pos = self.st.history_pos as usize + 1;
            let idx = self.st.history[pos];
            if idx < self.st.queue.len() {
                self.st.history_pos = pos as i64;
                self.enter_index(idx, handover);
                return;
            }
        }

        let next_index: usize;
        if self.st.shuffle {
            if let Some(i) = self.shuffle_unplayed.pop() {
                next_index = i;
            } else if self.st.repeat == RepeatMode::All && self.st.queue.len() > 1 {
                // every track heard once: deal again, never starting on the one that just played
                let cur = self.st.queue_index as usize;
                let mut v: Vec<usize> = (0..self.st.queue.len()).filter(|i| *i != cur).collect();
                self.rng.shuffle(&mut v);
                next_index = v.pop().unwrap_or(0);
                self.shuffle_unplayed = v;
            } else if self.st.repeat == RepeatMode::All {
                next_index = 0;
            } else {
                // played out: the listing the queue came from takes over
                if !self.advance_source(early) && !early {
                    self.finish_queue();
                }
                return;
            }
        } else {
            let n = (self.st.queue_index + 1) as usize;
            if n >= self.st.queue.len() {
                if self.st.repeat == RepeatMode::All {
                    next_index = 0;
                } else {
                    if !self.advance_source(early) && !early {
                        self.finish_queue();
                    }
                    return;
                }
            } else {
                next_index = n;
            }
        }

        // a genuinely new track truncates any forward branch and extends the chain
        let keep = (self.st.history_pos + 1).max(0) as usize;
        self.st.history.truncate(keep);
        self.st.history.push(next_index);
        if self.st.history.len() > MAX_HISTORY {
            let drop = self.st.history.len() - MAX_HISTORY;
            self.st.history.drain(..drop);
        }
        self.st.history_pos = self.st.history.len() as i64 - 1;
        self.enter_index(next_index, handover);
    }

    fn enter_index(&mut self, idx: usize, handover: Handover) {
        self.st.queue_index = idx as i64;
        self.set_current_from_index();
        self.st.error = None;
        self.load_track(idx, handover, true, None);
        self.touch();
    }

    fn finish_queue(&mut self) {
        // out of tracks and no listing behind the queue: this is the end
        if let Some(e) = &self.engine {
            let _ = e;
        }
        self.st.status = PlayerStatus::Paused;
        self.d_pos = true;
        self.touch();
    }

    pub fn jump_to(&mut self, index: usize) {
        if index >= self.st.queue.len() {
            return;
        }
        let keep = (self.st.history_pos + 1).max(0) as usize;
        self.st.history.truncate(keep);
        self.st.history.push(index);
        if self.st.history.len() > MAX_HISTORY {
            let drop = self.st.history.len() - MAX_HISTORY;
            self.st.history.drain(..drop);
        }
        self.st.history_pos = self.st.history.len() as i64 - 1;
        if self.st.shuffle {
            self.shuffle_unplayed.retain(|i| *i != index);
        }
        self.enter_index(index, Handover::Short);
    }

    pub fn previous(&mut self) {
        // restart the track first, like every other player, before stepping back
        if self.cur_pos().map(|(p, _, _)| p > 3.0).unwrap_or(false) {
            self.cmd_seek(0.0);
            self.counted_play = false;
            return;
        }
        // walk back through what was actually played (matters in shuffle)
        if self.st.history_pos > 0 {
            let pos = self.st.history_pos as usize - 1;
            if let Some(&idx) = self.st.history.get(pos)
                && idx < self.st.queue.len() {
                    self.st.history_pos = pos as i64;
                    self.st.queue_index = idx as i64;
                    self.set_current_from_index();
                    self.st.error = None;
                    self.load_track(idx, Handover::None, true, None);
                    self.touch();
                    return;
                }
        }
        // no played history left: in queue order step back and grow the chain at its start
        let prev = self.st.queue_index - 1;
        if self.st.shuffle || prev < 0 || prev as usize >= self.st.queue.len() {
            self.cmd_seek(0.0);
            return;
        }
        let prev = prev as usize;
        self.st.history.insert(0, prev);
        self.st.history_pos = 0;
        self.st.queue_index = prev as i64;
        self.set_current_from_index();
        self.st.error = None;
        self.load_track(prev, Handover::None, true, None);
        self.touch();
    }

    // ------------------------------------------------------------------
    // loading / starting
    // ------------------------------------------------------------------

    /// Whether a deck is audibly playing the current track right now.
    fn playing_now(&self) -> bool {
        match (&self.engine, self.cur_deck) {
            (Some(e), Some(d)) => {
                let s = e.snapshot();
                s.decks[d].state == 2 && !s.paused && self.st.status == PlayerStatus::Playing
            }
            _ => false,
        }
    }

    /// Plan the end-of-track blend from the playing deck into `incoming`.
    fn plan_for(&mut self, incoming: &QueueItem, handover: Handover) -> Option<BlendSpec> {
        let settings = self.st.mix_settings.clone();
        let out_deck = self.cur_deck?;
        let out = self.decks[out_deck].clone()?;
        let snap = self.engine.as_ref()?.snapshot();
        let in_points = self.points_of(incoming);
        let in_info = self.info_for(incoming);
        let in_bpm = in_info.bpm;
        if handover == Handover::Short {
            let bpm = out.bpm.map(|b| b * snap.decks[out_deck].rate.max(0.1));
            return Some(short_blend(bpm, settings.echo, in_points.in_s));
        }
        let out_dur = if snap.decks[out_deck].len_frames > 0 {
            snap.decks[out_deck].len_frames as f64 / self.sr()
        } else {
            out.dur_s
        };
        let mut spec = plan_blend(
            &OutgoingFacts { bpm: out.bpm, rate: snap.decks[out_deck].rate, dur_s: out_dur, out_s: out.points.out_s },
            &IncomingFacts { bpm: in_bpm, in_s: in_points.in_s },
            &settings,
        );
        // a set slot that is tempo-adjusted plays at the planned rate, held after the blend: the
        // Arrange view's tempo is authoritative over the live beat-match
        if let (Some(&(rate, kl)), Some(sy)) = (self.adjust.get(&incoming.track_id), spec.sync.as_mut()) {
            sy.rate = rate;
            sy.key_lock = kl && (rate - 1.0).abs() > 1e-4;
            sy.hold = true;
            sy.out_rate = 0.0;
            sy.to_bpm = in_bpm.map(|b| b * rate).unwrap_or(sy.to_bpm);
        }
        // A DJ set pinned this transition: its overlap is the blend's length and
        // the incoming enters at its cue-in, whatever the general entry setting.
        if let Some(b) = in_points.blend_s.filter(|b| *b > 0.5) {
            let remaining = match out.points.out_s {
                Some(o) if out_dur > 0.0 => (out_dur - o - 0.5).max(0.5),
                _ => b,
            };
            return Some(BlendSpec { length_s: b.min(remaining), incoming_start_s: in_points.in_s, swap_s: None, ..spec });
        }
        Some(spec)
    }

    fn trim_for(&mut self, item: &QueueItem) -> f64 {
        if !self.st.mix_settings.normalise {
            return 1.0;
        }
        let info = self.info_for(item);
        trim_gain(info.lufs, info.true_peak, self.st.mix_settings.target_lufs, -1.0)
    }

    /// Every start of a track goes through here.
    fn load_track(&mut self, idx: usize, handover: Handover, autoplay: bool, resume_pos: Option<f64>) {
        let Some(item) = self.st.queue.get(idx).cloned() else { return };
        self.mix_fired = false;
        self.early_run = -1;
        self.counted_play = false;
        self.st.mix_out_override_s = None;
        self.st.mix_out_s = None;
        self.started_at_s = 0.0;
        // a pending prime for exactly this row is promoted instead of re-decoding
        if let Some(p) = self.primed.take() {
            if p.uid == item.uid && resume_pos.is_none() && p.handover == handover && p.cued {
                let mut p = p;
                p.autoplay = autoplay;
                p.purpose = Purpose::Start;
                self.fire_loading(p);
                return;
            }
            // wrong prime: leave its deck decoding; it is replaced below
            self.primed = Some(p);
            self.invalidate_prime();
        }
        if self.engine().is_err() {
            let err = self.engine().err().unwrap_or_default();
            self.set_error(format!("No audio output: {err}"));
            return;
        }
        let info = self.info_for(&item);
        // handover: blend only when something is audibly playing
        let blending = handover != Handover::None && self.st.mix && self.playing_now();
        let cutting = self.playing_now() && !blending; // declick cut under the old audio
        let mut spec = if blending { self.plan_for(&item, handover) } else if cutting { Some(cut_spec(0.0)) } else { None };
        let mut start_s = match (&spec, blending) {
            (Some(s), true) => s.incoming_start_s,
            _ => resume_pos.unwrap_or(0.0),
        };
        // beat-quantised start: land the incoming on a beat (a bar) of its own grid
        if blending
            && let (Some(g), Some(s)) = (info.grid, spec.as_mut())
                && s.sync.is_some() && s.quantise != Quantise::Off {
                    start_s = snap_start(&g, start_s, s);
                }
        let deck = match self.cur_deck {
            Some(d) if self.decks[d].is_some() && (blending || cutting) => d ^ 1,
            Some(d) => d,
            None => DECK_A,
        };
        let deck = if blending || cutting { deck } else { self.cur_deck.map(|d| d ^ 1).unwrap_or(DECK_A) };
        let source = match self.source_for(&item) {
            Ok(s) => s,
            Err(msg) => {
                self.track_failed(idx, &msg, handover);
                return;
            }
        };
        let sr = self.sr();
        let start_frame = (start_s * sr).round() as u64;
        let Some(e) = self.engine.as_ref() else { return };
        let (epoch, rx) = e.begin_load(deck, source, start_frame);
        self.loading = Some(Loading {
            deck,
            idx,
            uid: item.uid,
            epoch,
            start_s,
            start_frame,
            stage: Stage::Opening(rx),
            ready: false,
            cued: false,
            spec,
            handover,
            began: Instant::now(),
            purpose: Purpose::Start,
            autoplay,
            armed: false,
        });
        self.st.status = PlayerStatus::Loading;
        self.started_at_s = start_s;
        self.touch();
    }

    fn track_failed(&mut self, idx: usize, msg: &str, handover: Handover) {
        let title = self.st.queue.get(idx).map(|t| t.title.clone()).unwrap_or_default();
        self.load_fails += 1;
        tracing::warn!("cannot play {title:?}: {msg}");
        self.loading = None;
        if self.load_fails <= MAX_LOAD_FAILS && handover != Handover::None && idx + 1 < self.st.queue.len() {
            // step over it, like the browser player's file-missing handling
            self.st.error = Some(format!("Skipped {title}: {msg}"));
            self.next(true, false);
        } else {
            self.set_error(format!("{title}: {msg}"));
        }
    }

    fn tick_loading(&mut self) {
        use crossbeam_channel::TryRecvError;
        // --- the track being started
        if let Some(mut l) = self.loading.take() {
            let mut keep = true;
            let polled = if let Stage::Opening(rx) = &l.stage { Some(rx.try_recv()) } else { None };
            match polled {
                Some(Ok(Ok(opened))) => self.cue_loading(&mut l, &opened),
                Some(Ok(Err(e))) => {
                    let (idx, h) = (l.idx, l.handover);
                    self.track_failed(idx, &e.to_string(), h);
                    keep = false;
                }
                Some(Err(TryRecvError::Empty)) | None => {
                    if l.began.elapsed() > LOAD_TIMEOUT {
                        let (idx, h) = (l.idx, l.handover);
                        self.track_failed(idx, "timed out loading the source", h);
                        keep = false;
                    }
                }
                Some(Err(TryRecvError::Disconnected)) => {
                    let (idx, h) = (l.idx, l.handover);
                    self.track_failed(idx, "decoder stopped", h);
                    keep = false;
                }
            }
            if keep {
                if l.ready && l.cued {
                    self.fire_loading(l);
                } else {
                    self.loading = Some(l);
                }
            }
        }
        // --- the primed next track
        if let Some(mut p) = self.primed.take() {
            let mut keep = true;
            let polled = if let Stage::Opening(rx) = &p.stage { Some(rx.try_recv()) } else { None };
            match polled {
                Some(Ok(Ok(opened))) => self.cue_loading(&mut p, &opened),
                Some(Ok(Err(e))) => {
                    tracing::warn!("prime failed: {e}");
                    keep = false;
                }
                Some(Err(TryRecvError::Empty)) | None => {
                    if p.began.elapsed() > LOAD_TIMEOUT {
                        keep = false;
                    }
                }
                Some(Err(TryRecvError::Disconnected)) => keep = false,
            }
            if keep {
                if p.ready && p.cued && !p.armed {
                    p.armed = true;
                    self.arm_gapless(&p);
                }
                self.primed = Some(p);
            }
        }
    }

    /// The container is open: cue the deck.
    fn cue_loading(&mut self, l: &mut Loading, opened: &Opened) {
        let Some(item) = self.st.queue.get(l.idx).cloned() else { return };
        let mut info = self.info_for(&item);
        // the container knows the real length: learn it when the library did not (streams, plain files)
        if info.dur_s <= 0.0 && opened.len_frames > 0 {
            let d = opened.len_frames as f64 / self.sr();
            if let Some(i) = self.infos.get_mut(&item.track_id) {
                i.dur_s = d;
                if !self.pinned.contains_key(&item.track_id) {
                    i.points = Some(mixpoints::heuristic(d, i.bpm));
                }
                info = i.clone();
            }
        }
        let trim = self.trim_for(&item);
        let (rate, keylock) = match &l.spec {
            Some(s) if l.handover != Handover::None => (
                s.sync.map(|x| x.rate).unwrap_or(1.0),
                // key lock (the phase vocoder) only for a matched blend that asks for it at a rate other than 1
                s.sync.map(|x| x.key_lock && (x.rate - 1.0).abs() > 1e-4).unwrap_or(false),
            ),
            _ => (1.0, false),
        };
        // the browser sets the rate at blend start; a cut / first start plays at the file's own tempo
        let (rate, keylock) = if l.spec.as_ref().map(|s| s.kind == TransitionKind::Cut).unwrap_or(false) && l.handover == Handover::None {
            (1.0, false)
        } else {
            (rate, keylock)
        };
        // a set's tempo-adjusted slot starts at its planned rate, key-locked when the slot says so
        let (rate, keylock) = match self.adjust.get(&item.track_id) {
            Some(&(r, kl)) => (r, kl && (r - 1.0).abs() > 1e-4),
            None => (rate, keylock),
        };
        let points = self.points_of(&item);
        self.decks[l.deck] = Some(Loaded {
            uid: item.uid,
            dur_s: if opened.len_frames > 0 { opened.len_frames as f64 / self.sr() } else { info.dur_s },
            bpm: info.bpm,
            points,
        });
        self.send(Cmd::Cue {
            deck: l.deck as u8,
            epoch: l.epoch,
            frame: l.start_frame,
            len_frames: opened.len_frames,
            rate,
            keylock,
            trim,
            grid: info.grid,
        });
        l.cued = true;
        l.stage = Stage::Priming;
    }

    /// The incoming deck holds data: start it (cut, blend or plain play).
    fn fire_loading(&mut self, l: Loading) {
        let idx = l.idx;
        if self.st.queue.get(idx).map(|t| t.uid) != Some(l.uid) {
            return; // the row moved away while loading
        }
        let out = self.cur_deck;
        let deck = l.deck;
        // is the outgoing deck itself audibly playing? (not `st.status`: that already reads Loading)
        let out_playing = match (out, self.engine.as_ref()) {
            (Some(o), Some(e)) => {
                let s = e.snapshot();
                s.decks[o].state == 2 && !s.paused
            }
            _ => false,
        };
        if l.autoplay {
            // pressing a track while paused plays it (the browser's behaviour)
            if self.paused_by_user {
                self.send(Cmd::Resume);
                self.paused_by_user = false;
            }
            match (&l.spec, out, out_playing) {
                (Some(spec), Some(o), true) if o != deck => {
                    self.tr_spec = Some(*spec);
                    // `o` keeps playing (fade, echo tail) until the mixer parks it: no priming onto it
                    self.parking = Some(o);
                    self.parking_since = Instant::now();
                    self.send(Cmd::StartTransition { out: o as u8, inc: deck as u8, spec: *spec });
                }
                _ => {
                    self.send(Cmd::Play { deck: deck as u8 });
                    // a deck that was only paused / idle is replaced: stop its decoder
                    if let (Some(o), Some(e)) = (out, self.engine.as_ref())
                        && o != deck {
                            e.stop_decoder(o);
                            self.decks[o] = None;
                        }
                }
            }
        }
        // the clock, waveform and OS overlay follow the incoming track from this moment
        let prev_uid = out.and_then(|o| self.decks[o].as_ref().map(|d| d.uid));
        self.pending_transition_ids = (prev_uid, Some(l.uid));
        self.cur_deck = Some(deck);
        self.set_current_from_index_checked(idx);
        self.started_at_s = l.start_s;
        self.load_fails = 0;
        // becomes Playing on the engine's Started event; a restore / device switch that was
        // paused leaves the deck cued and the player paused until Play
        self.st.status = if l.autoplay { PlayerStatus::Loading } else { PlayerStatus::Paused };
        self.loading = None;
        self.primed = None;
        self.touch();
        self.d_pos = true;
    }

    fn set_current_from_index_checked(&mut self, idx: usize) {
        self.st.queue_index = idx as i64;
        self.set_current_from_index();
    }

    // ------------------------------------------------------------------
    // priming (pre-decode the next track) and gapless
    // ------------------------------------------------------------------

    fn invalidate_prime(&mut self) {
        if let Some(p) = self.primed.take() {
            if let Some(e) = &self.engine {
                e.stop_decoder(p.deck);
            }
            self.send(Cmd::ArmGapless { next: None });
        }
    }

    fn arm_gapless(&mut self, p: &Loading) {
        // Only a primed deck that starts from the top may be spliced at the natural end.
        if p.start_frame == 0 || p.handover == Handover::None {
            self.send(Cmd::ArmGapless { next: Some(p.deck as u8) });
        }
    }

    /// Pre-decode the row after the current one on the idle deck so the next
    /// start (gapless end, blend or skip) needs no load.
    pub fn prime_next(&mut self) {
        if self.engine.is_none() || self.cur_deck.is_none() || self.st.preview.playing && !self.engine.as_ref().map(|e| e.has_cue()).unwrap_or(false) {
            return;
        }
        // never while a track is loading: `cur_deck` is still the outgoing one, and the idle deck is the load target
        if self.primed.is_some() || self.parking.is_some() || self.loading.is_some() {
            return;
        }
        let Some(idx) = self.peek_next_index() else {
            return;
        };
        let Some(item) = self.st.queue.get(idx).cloned() else { return };
        let Some(cur) = self.cur_deck else { return };
        let deck = cur ^ 1;
        // Where it will come in: at the drop for a blend (when the current track
        // will actually blend out), from the top for a natural end.
        let will_blend = self.st.mix && self.decks[cur].as_ref().map(|d| d.points.out_s.is_some()).unwrap_or(false);
        let (spec, start_s, handover) = if will_blend {
            match self.plan_for(&item, Handover::Long) {
                Some(mut s) => {
                    let info = self.info_for(&item);
                    let mut st = s.incoming_start_s;
                    if let (Some(g), true) = (info.grid, s.sync.is_some() && s.quantise != Quantise::Off) {
                        st = snap_start(&g, st, &mut s);
                    }
                    (Some(s), st, Handover::Long)
                }
                None => (None, 0.0, Handover::None),
            }
        } else {
            (None, 0.0, Handover::None)
        };
        let source = match self.source_for(&item) {
            Ok(s) => s,
            Err(_) => return,
        };
        let sr = self.sr();
        let start_frame = (start_s * sr).round() as u64;
        let Some(e) = self.engine.as_ref() else { return };
        let (epoch, rx) = e.begin_load(deck, source, start_frame);
        self.primed = Some(Loading {
            deck,
            idx,
            uid: item.uid,
            epoch,
            start_s,
            start_frame,
            stage: Stage::Opening(rx),
            ready: false,
            cued: false,
            spec,
            handover,
            began: Instant::now(),
            purpose: Purpose::Prime,
            autoplay: false,
            armed: false,
        });
    }

    // ------------------------------------------------------------------
    // engine events
    // ------------------------------------------------------------------

    fn process_engine_events(&mut self) {
        let Some(e) = &self.engine else { return };
        let mut evs = std::mem::take(&mut self.events_buf);
        evs.clear();
        e.poll_events(&mut evs);
        let list: Vec<Event> = std::mem::take(&mut evs);
        self.events_buf = evs;
        for ev in list {
            if self.event_log.len() >= 512 {
                self.event_log.drain(..256);
            }
            self.event_log.push(ev);
            match ev {
                Event::Ready { deck, epoch } => {
                    let d = deck as usize;
                    if d == PREVIEW {
                        self.on_preview_ready(epoch);
                    } else {
                        if let Some(l) = self.loading.as_mut()
                            && l.deck == d && l.epoch == epoch {
                                l.ready = true;
                            }
                        if let Some(p) = self.primed.as_mut()
                            && p.deck == d && p.epoch == epoch {
                                p.ready = true;
                            }
                    }
                }
                Event::Started { deck, .. } => {
                    if Some(deck as usize) == self.cur_deck && !self.paused_by_user {
                        self.st.status = PlayerStatus::Playing;
                        self.touch();
                        self.prime_next();
                    }
                }
                Event::Advanced { from: _, to, frame: _ } => self.on_gapless(to as usize),
                Event::Ended { deck, epoch: _ } => {
                    let d = deck as usize;
                    if d == PREVIEW {
                        self.preview_stop(false);
                    } else if Some(d) == self.cur_deck && self.loading.is_none() {
                        // no spliced successor was armed: advance the ordinary way
                        self.decks[d] = None;
                        self.next(true, false);
                    }
                }
                Event::Underrun { deck, frame } => {
                    tracing::debug!(deck, frame, "audio underrun");
                }
                Event::TransitionStarted { out, inc, start_frame, length_s, phase } => {
                    self.parking = Some(out as usize);
                    self.parking_since = Instant::now();
                    let spec = self.tr_spec;
                    let now = now_ms();
                    let sr = self.sr();
                    self.tr_end_engine_s = start_frame as f64 / sr + length_s;
                    let (ou, iu) = self.pending_transition_ids;
                    let (ou, iu) = (self.decks[out as usize].as_ref().map(|d| d.uid).or(ou), self.decks[inc as usize].as_ref().map(|d| d.uid).or(iu));
                    if let Some(s) = spec {
                        self.tr = Some(TransitionState {
                            kind: s.kind,
                            started_at_ms: now,
                            ends_at_ms: now + (length_s * 1000.0) as u64,
                            echo: s.echo.is_some(),
                            sync: s.sync.map(|y| TransitionSync { rate: y.rate, from_bpm: y.from_bpm, to_bpm: y.to_bpm, on: true }),
                            phase: match phase {
                                1 => Some(PhaseState::Est),
                                2 => Some(PhaseState::Locked),
                                _ => None,
                            },
                            outgoing_uid: ou,
                            incoming_uid: iu,
                            phase_error_ms: None,
                        });
                        // a plain cut has nothing to show
                        if s.kind == TransitionKind::Cut && s.length_s < 0.1 && s.echo.is_none() {
                            self.tr = None;
                        }
                        self.publish_transition();
                    }
                }
                Event::TransitionRetimed { end_t } => {
                    self.tr_end_engine_s = end_t;
                    let sr = self.sr();
                    let _ = sr;
                    if let (Some(t), Some(e)) = (self.tr.as_mut(), self.engine.as_ref()) {
                        let snap = e.snapshot();
                        let now_engine = snap.frames_played as f64 / snap.sample_rate.max(1) as f64;
                        t.ends_at_ms = now_ms() + (((end_t - now_engine).max(0.0)) * 1000.0) as u64;
                    }
                    self.publish_transition();
                }
                Event::FadeDone => self.clear_transition(),
                Event::Parked { deck } => {
                    let d = deck as usize;
                    if Some(d) != self.cur_deck {
                        self.decks[d] = None;
                    }
                    // the outgoing deck is free again: pre-decode the next track onto it
                    if self.parking == Some(d) {
                        self.parking = None;
                        self.prime_next();
                    }
                }
                Event::Paused | Event::Resumed => {}
            }
        }
    }

    fn clear_transition(&mut self) {
        if self.tr.take().is_some() {
            self.publish_transition();
        }
        self.tr_spec = None;
    }

    /// The spliced gapless hand-over happened inside the audio callback.
    fn on_gapless(&mut self, to: usize) {
        let Some(p) = self.primed.take() else {
            return;
        };
        if p.deck != to {
            return;
        }
        // the same bookkeeping as next(auto) without loading anything
        let idx = p.idx;
        if self.st.shuffle {
            self.shuffle_unplayed.retain(|i| *i != idx);
        }
        let in_chain_forward =
            self.st.history_pos >= 0 && (self.st.history_pos as usize) + 1 < self.st.history.len() && self.st.history.get(self.st.history_pos as usize + 1) == Some(&idx);
        if in_chain_forward {
            self.st.history_pos += 1;
        } else {
            let keep = (self.st.history_pos + 1).max(0) as usize;
            self.st.history.truncate(keep);
            self.st.history.push(idx);
            if self.st.history.len() > MAX_HISTORY {
                let drop = self.st.history.len() - MAX_HISTORY;
                self.st.history.drain(..drop);
            }
            self.st.history_pos = self.st.history.len() as i64 - 1;
        }
        self.cur_deck = Some(to);
        self.set_current_from_index_checked(idx);
        self.mix_fired = false;
        self.early_run = -1;
        self.counted_play = false;
        self.started_at_s = p.start_s;
        self.st.mix_out_override_s = None;
        self.st.error = None;
        self.st.status = PlayerStatus::Playing;
        self.load_fails = 0;
        self.touch();
        self.d_pos = true;
        self.prime_next();
    }

    // ------------------------------------------------------------------
    // position-dependent logic: mix trigger, play counting
    // ------------------------------------------------------------------

    /// (position_s, duration_s, buffered_s) of the playing track.
    fn cur_pos(&self) -> Option<(f64, f64, f64)> {
        let e = self.engine.as_ref()?;
        let d = self.cur_deck?;
        let s = e.snapshot();
        let sr = s.sample_rate.max(1) as f64;
        let dk = &s.decks[d];
        if dk.state == 0 {
            return None;
        }
        let l = self.decks[d].as_ref()?;
        let dur = if dk.len_frames > 0 { dk.len_frames as f64 / sr } else { l.dur_s };
        Some((dk.pos_frames / sr, dur, dk.buffered_s))
    }

    /// A transition that never started (the incoming deck had nothing) is never parked by the
    /// mixer: do not hold priming back for ever.
    fn tick_parking(&mut self) {
        if self.parking.is_none() || self.parking_since.elapsed() < Duration::from_secs(3) {
            return;
        }
        let running = self.engine.as_ref().map(|e| e.snapshot().transitioning).unwrap_or(false);
        if !running {
            self.parking = None;
            self.prime_next();
        }
    }

    /// Keep the strip's beat-phase readout fresh (a few times a second, only when it moved).
    fn tick_phase_error(&mut self) {
        if self.tr.is_none() {
            return;
        }
        let Some(e) = &self.engine else { return };
        let err = e.snapshot().phase_err_ms;
        let v = if err.is_finite() { Some(err as f64) } else { None };
        let changed = match (self.tr.as_ref().and_then(|t| t.phase_error_ms), v) {
            (Some(a), Some(b)) => (a - b).abs() > 0.5,
            (a, b) => a.is_some() != b.is_some(),
        };
        if changed && self.last_phase_pub.elapsed() > Duration::from_millis(250) {
            self.last_phase_pub = Instant::now();
            if let Some(t) = self.tr.as_mut() {
                t.phase_error_ms = v;
                if v.is_some() && t.phase.is_some() {
                    t.phase = Some(PhaseState::Locked);
                }
            }
            self.publish_transition();
        }
    }

    fn tick_position(&mut self) {
        self.tick_phase_error();
        let Some((pos, dur, _)) = self.cur_pos() else { return };
        if self.st.status == PlayerStatus::Loading && self.loading.is_none() && self.engine.is_some() {
            // started but no Started event yet: the clock moving means it is playing
        }
        self.maybe_count_play(pos, dur);
        self.maybe_start_mix(pos, dur);
        if self.st.status == PlayerStatus::Playing && self.last_persist_pos.elapsed() > Duration::from_secs(5) {
            self.d_pos = true;
        }
    }

    fn maybe_count_play(&mut self, pos: f64, dur: f64) {
        // Last.fm's rule: a play counts at 50 % or 4 minutes, whichever is sooner.
        // Bandcamp streams are excluded: play history is a statement about the library.
        if self.counted_play || self.st.status != PlayerStatus::Playing {
            return;
        }
        let Some(cur) = self.st.current.as_ref() else { return };
        if !cur.is_library() || dur <= 0.0 {
            return;
        }
        let threshold = (dur * 0.5).min(240.0);
        if pos >= threshold {
            self.counted_play = true;
            let _ = self.bg_tx.send(BgJob::RecordPlay { track_id: cur.track_id, ms: (pos * 1000.0).round() as i64, completed: true, skipped: false });
        }
    }

    /// Fires the blend into the next track once the playing one reaches its
    /// mix-out point. Checked live every tick, so flipping mix or repeat
    /// mid-track needs no bookkeeping. A seek back to before the mix-out point
    /// re-arms the trigger and drops a continuation the early trigger set going.
    fn maybe_start_mix(&mut self, pos: f64, dur: f64) {
        let Some(cur) = self.st.current.clone() else { return };
        if dur <= 0.0 || self.loading.is_some() {
            return;
        }
        let points = self.points_of(&cur);
        let Some(out_point) = points.out_s else {
            self.st.mix_out_s = None;
            return;
        };
        let outro_s = out_point.min(dur - OUT_BEFORE_END_S);
        // A matched blend is longer than the outro heuristic allows for: start it
        // early enough to fit, when the next track is known.
        let next_idx = self.peek_next_index();
        let length_s = match next_idx.and_then(|i| self.st.queue.get(i).cloned()) {
            Some(n) if !self.st.shuffle || self.primed.is_some() => self.plan_for(&n, Handover::Long).map(|s| s.length_s).unwrap_or(0.0),
            _ => 0.0,
        };
        // leave the incoming time to load and the start time to wait for its bar / phrase
        let slack = trigger_slack_s(self.st.mix_settings.quantise, self.decks[self.cur_deck.unwrap_or(0)].as_ref().and_then(|d| d.bpm));
        let outro_trigger = if length_s > 0.0 { trigger_point(outro_s, dur, length_s, slack) } else { outro_s };
        let max_play = self.st.mix_settings.max_play_s;
        let limit_s = max_play.map(|m| self.started_at_s + m).unwrap_or(f64::INFINITY);
        let over = self.st.mix_out_override_s;
        let out_s = over.unwrap_or(outro_trigger.min(limit_s));
        let acts = self.st.mix || max_play.is_some() || over.is_some();
        let new_mark = if acts && self.st.repeat != RepeatMode::One { Some(out_s) } else { None };
        let changed = match (self.st.mix_out_s, new_mark) {
            (Some(a), Some(b)) => (a - b).abs() >= 0.05,
            (a, b) => a != b,
        };
        if changed {
            self.st.mix_out_s = new_mark;
            self.touch();
        }

        if self.mix_fired {
            if pos < out_s - 1.0 {
                self.mix_fired = false;
                if self.early_run == self.advance_run as i64 {
                    self.advance_run += 1; // abandon the early continuation
                }
                self.early_run = -1;
            }
            return;
        }
        if self.st.status != PlayerStatus::Playing || self.st.repeat == RepeatMode::One {
            return;
        }
        // Without DJ mix the outro is not ours to act on -- the track simply
        // ends -- but a play-time limit or a dragged point still moves things on, with a cut.
        if !self.st.mix {
            let cut_s = over.unwrap_or(limit_s);
            if pos < cut_s || dur - pos < 3.0 {
                return;
            }
            self.mix_fired = true;
            self.next(true, false);
            return;
        }
        // Too close to the end for a blend to be anything but a stutter; let the
        // track end and the ordinary next() take it.
        if pos < out_s || dur - pos < 3.0 {
            return;
        }
        self.mix_fired = true;
        self.next(true, true);
    }

    fn retime(&mut self, factor: f64) {
        let Some(e) = &self.engine else { return };
        let snap = e.snapshot();
        let now_engine = snap.frames_played as f64 / snap.sample_rate.max(1) as f64;
        let left = self.tr_end_engine_s - now_engine;
        if left > 0.0 && self.tr.is_some() {
            self.send(Cmd::Retime { remaining_s: left * factor });
        }
    }

    // ------------------------------------------------------------------
    // mix toggles
    // ------------------------------------------------------------------

    fn set_mix(&mut self, on: bool) {
        self.st.mix = on;
        self.d_prefs = true;
        self.touch();
        // mid-track switch-on: learn where this track goes out and where the next comes in
        self.invalidate_prime();
        self.prime_next();
    }

    fn start_mix_from(&mut self, pool: Pool) {
        plan::apply(&mut self.st.plan, PlanOp::MixFrom { pool: pool.clone() }, now_ms());
        plan::apply(&mut self.st.plan, PlanOp::SetAutoFill { on: true }, now_ms());
        self.st.plan.spent_pool = None;
        self.last_empty = None;
        self.d_plan = true;
        self.st.mix = true;
        self.d_prefs = true;
        let playing = matches!(self.st.status, PlayerStatus::Playing | PlayerStatus::Loading);
        if playing {
            // keep the floor; the next track and everything after come from the new pool
            let from = (self.st.queue_index + 1).max(0) as usize;
            let to = self.st.queue.len();
            self.edit_queue(|s| queue_ops::remove_ops(s, from, to));
            self.st.shuffle = false;
            self.shuffle_unplayed.clear();
        } else {
            match self.ports.library.random_tracks(Some(&pool), 1) {
                Ok(mut v) if !v.is_empty() => {
                    let first = v.remove(0);
                    self.start_queue(vec![first], 0, None, Handover::Short);
                }
                _ => {}
            }
        }
        self.touch();
        self.invalidate_prime();
    }

    // ------------------------------------------------------------------
    // sources: starting a source and continuing past the queue's end
    // ------------------------------------------------------------------

    fn cmd_start_source(&mut self, source: QueueSource, shuffle: bool) {
        self.advance_run += 1;
        let run = self.advance_run;
        self.st.status = PlayerStatus::Loading;
        self.touch();
        let _ = self.bg_tx.send(BgJob::StartSource { run, source, shuffle });
    }

    /// Plays on into whatever follows the queue that just ended. Returns whether
    /// it took the job on (false: the queue had no continuation, so this is the end).
    fn advance_source(&mut self, early: bool) -> bool {
        let Some(source) = self.st.source.clone() else { return false };
        if !source.continues() {
            return false;
        }
        self.advance_run += 1;
        let run = self.advance_run;
        // an early run happens while the track is still audible: the bar must keep
        // reading as playing, and a failure is nobody's business until the track ends
        if early {
            self.early_run = run as i64;
        } else {
            self.st.status = PlayerStatus::Loading;
            self.touch();
        }
        let _ = self.bg_tx.send(BgJob::Advance { run, early, source });
        true
    }

    fn process_bg(&mut self) {
        while let Ok(r) = self.bg_rx.try_recv() {
            match r {
                BgResult::Fill(f) => self.on_fill(f),
                BgResult::Advance { run, early, items, source } => {
                    if run != self.advance_run {
                        continue; // abandoned: whatever replaced it owns the player
                    }
                    match (items.is_empty(), source) {
                        (false, Some(src)) => {
                            let handover = if early { Handover::Long } else { Handover::None };
                            self.start_queue(items, 0, Some(src), handover);
                        }
                        _ => {
                            if !early {
                                self.finish_queue();
                            } else {
                                self.mix_fired = false;
                            }
                        }
                    }
                }
                BgResult::Started { run, items, source, shuffle, error, pins } => {
                    if run != self.advance_run {
                        continue;
                    }
                    if let Some(e) = error {
                        self.set_error(e);
                        continue;
                    }
                    if items.is_empty() {
                        self.st.status = if self.st.current.is_some() { PlayerStatus::Paused } else { PlayerStatus::Idle };
                        self.touch();
                        continue;
                    }
                    self.st.shuffle = shuffle;
                    self.d_prefs = true;
                    // a new queue starts from a clean plan: nothing of a previous set lingers
                    self.pinned.clear();
                    self.adjust.clear();
                    self.infos.clear();
                    for (id, p, adj) in pins {
                        self.pin_mix_points(id, p);
                        if let Some(a) = adj {
                            self.adjust.insert(id, a);
                        }
                    }
                    self.start_queue(items, 0, source, Handover::Short);
                }
                BgResult::Sweep { run, items, source, done } => {
                    self.sweep_inflight = false;
                    if run != self.advance_run {
                        continue;
                    }
                    if !items.is_empty() {
                        self.st.source = Some(source);
                        self.add_to_queue(items);
                    } else if done {
                        // nothing more to add: the source's end
                        if let Some(QueueSource::Explore { next, cards, .. }) = self.st.source.as_mut() {
                            *next = cards.len();
                        }
                    } else if let QueueSource::Explore { .. } = &source {
                        self.st.source = Some(source);
                    }
                }
            }
        }
    }

    /// Keep an explore sweep's queue ~12 tracks ahead of the needle.
    fn maybe_sweep(&mut self) {
        if self.sweep_inflight {
            return;
        }
        let Some(QueueSource::Explore { cards, next, .. }) = self.st.source.as_ref() else { return };
        // Shuffled, the queue's order says nothing about what plays next; the unplayed pool does.
        let ahead = if self.st.shuffle { (self.shuffle_unplayed.len(), SHUFFLE_SWEEP_LOOKAHEAD) } else { (self.upcoming().len(), SWEEP_LOOKAHEAD) };
        if *next >= cards.len() || ahead.0 >= ahead.1 {
            return;
        }
        let src = self.st.source.clone().unwrap_or(QueueSource::Explore { cards: vec![], shuffle: false, next: 0 });
        self.sweep_inflight = true;
        let _ = self.bg_tx.send(BgJob::Sweep { run: self.advance_run, source: src });
    }

    // ------------------------------------------------------------------
    // auto-fill
    // ------------------------------------------------------------------

    fn maybe_fill(&mut self) {
        if self.fill_inflight {
            return;
        }
        let plan = &self.st.plan;
        if !plan.auto_fill || !self.st.mix || self.st.current.is_none() || self.st.repeat == RepeatMode::One {
            return;
        }
        let upcoming = self.upcoming().to_vec();
        if upcoming.len() >= FILL_TO {
            return;
        }
        let Some(cur) = self.st.current.clone() else { return };
        // the seed is the last planned track, or the playing one
        let seed = upcoming.last().cloned().unwrap_or(cur.clone());
        let key = format!("{}:{}", seed.track_id, self.st.queue.len());
        if let Some((k, t)) = &self.last_empty
            && *k == key && t.elapsed() < Duration::from_millis(RETRY_MS) {
                return;
            }
        let exclude = autofill::exclude_ids(Some(&cur), &upcoming, &self.st.history, &self.st.queue);
        let want = autofill::BATCH.min(FILL_TO - upcoming.len());
        self.fill_inflight = true;
        self.fill_run += 1;
        let _ = self.bg_tx.send(BgJob::Fill(FillJob { run: self.fill_run, seed, exclude, want, plan: self.st.plan.clone() }));
    }

    fn on_fill(&mut self, f: FillResult) {
        self.fill_inflight = false;
        let seed_key = format!("{}:{}", f.seed_id, self.st.queue.len());
        let still_short = self.upcoming().len() < FILL_TO;
        if f.found.is_empty() {
            // the active pool has nothing left: the next in the chain takes over, straight
            // away. The last one is kept -- an empty chain would mean the whole library,
            // which is not what a DJ who chose a playlist meant.
            if self.st.plan.pools.len() > 1 {
                plan::apply(&mut self.st.plan, PlanOp::AdvancePool, now_ms());
                self.last_empty = None;
                self.d_plan = true;
            } else {
                self.st.plan.spent_pool = self.st.plan.pools.first().map(|p| p.key());
                self.last_empty = Some((seed_key, Instant::now()));
            }
            self.touch();
        } else if still_short && self.st.mix && self.st.plan.auto_fill {
            self.st.plan.spent_pool = None;
            let have: std::collections::HashSet<i64> = self.st.queue.iter().map(|t| t.track_id).collect();
            let fresh: Vec<QueueItem> = f.found.into_iter().filter(|t| !have.contains(&t.track_id)).collect();
            if !fresh.is_empty() {
                self.add_to_queue(fresh);
            }
            self.touch();
        }
        // next batch, until the horizon is full (or nothing more can be found) happens on the next tick
    }

    // ------------------------------------------------------------------
    // preview (cue deck)
    // ------------------------------------------------------------------

    fn preview_start(&mut self, item: QueueItem, at_s: Option<f64>) {
        if self.st.preview.playing && self.st.preview.track_id == Some(item.track_id) {
            self.preview_stop(true);
            return;
        }
        if self.engine().is_err() {
            return;
        }
        self.preview_stop(false);
        let has_cue = self.engine.as_ref().map(|e| e.has_cue()).unwrap_or(false);
        // Without a cue device the preview shares the main output, so the main
        // player makes way for it and resumes afterwards (the browser's behaviour).
        if !has_cue && self.st.status == PlayerStatus::Playing {
            self.send(Cmd::Pause);
            self.st.status = PlayerStatus::Paused;
            self.resume_main_after_preview = true;
        }
        let start_s = at_s.unwrap_or_else(|| self.points_of(&item).in_s);
        let Ok(source) = self.source_for(&item) else {
            return;
        };
        let frame = (start_s * self.preview_sr()).round() as u64;
        let Some(e) = self.engine.as_ref() else { return };
        let (epoch, rx) = e.begin_load(PREVIEW, source, frame);
        self.preview_loading = Some(PreviewLoad { epoch, rx, start_s, cued: false });
        self.st.preview = PreviewState { track_id: Some(item.track_id), playing: true, position_s: start_s, on_cue_device: has_cue };
        self.touch();
    }

    fn tick_preview(&mut self) {
        let Some(p) = self.preview_loading.take() else { return };
        if p.cued {
            self.preview_loading = Some(p);
            return;
        }
        let PreviewLoad { epoch, rx, start_s, cued } = p;
        match rx.try_recv() {
            Ok(Ok(opened)) => {
                let frame = (start_s * self.preview_sr()).round() as u64;
                if let Some(e) = &self.engine {
                    e.send_preview(Cmd::PreviewCue { epoch, frame, len_frames: opened.len_frames, gain: 0.9 });
                }
                self.preview_loading = Some(PreviewLoad { epoch, rx, start_s, cued: true });
            }
            Ok(Err(_)) | Err(crossbeam_channel::TryRecvError::Disconnected) => {
                self.preview_stop(true);
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {
                self.preview_loading = Some(PreviewLoad { epoch, rx, start_s, cued });
            }
        }
    }

    fn on_preview_ready(&mut self, epoch: u32) {
        if self.preview_loading.as_ref().map(|p| p.epoch) == Some(epoch)
            && let Some(e) = &self.engine {
                e.send_preview(Cmd::PreviewPlay);
            }
    }

    fn preview_stop(&mut self, resume: bool) {
        self.preview_loading = None;
        if let Some(e) = &self.engine {
            e.send_preview(Cmd::PreviewStop);
            e.stop_decoder(PREVIEW);
        }
        let was = self.st.preview.playing;
        self.st.preview = PreviewState::default();
        if resume && self.resume_main_after_preview {
            self.send(Cmd::Resume);
            self.st.status = PlayerStatus::Playing;
        }
        if resume || was {
            self.resume_main_after_preview = false;
        }
        self.touch();
    }

    // ------------------------------------------------------------------
    // devices
    // ------------------------------------------------------------------

    fn cmd_set_output(&mut self, target: OutputTarget) -> Result<(), PlayerError> {
        let OutputKind::Cpal(_) = &self.cfg.output else {
            return Err(PlayerError::Conflict("a null output cannot change device".into()));
        };
        let pos = self.cur_pos().map(|(p, _, _)| p);
        let was_playing = self.st.status == PlayerStatus::Playing;
        self.cfg.output = OutputKind::Cpal(target);
        // rebuild the engine; the decks start over at the same position
        self.loading = None;
        self.primed = None;
        self.decks = [None, None];
        self.cur_deck = None;
        self.clear_transition();
        self.engine = None;
        if let Err(e) = self.engine() {
            self.refresh_devices(false);
            return Err(PlayerError::Unavailable(e));
        }
        self.refresh_devices(true);
        if let Some(idx) = usize::try_from(self.st.queue_index).ok().filter(|i| *i < self.st.queue.len()) {
            self.paused_by_user = !was_playing;
            self.load_track(idx, Handover::None, was_playing, pos);
        }
        self.d_prefs = true;
        Ok(())
    }

    // ------------------------------------------------------------------
    // persistence (incremental: separate keys, written when their part changed)
    // ------------------------------------------------------------------

    fn flush_persist(&mut self, force: bool) {
        let ps = |s: &Session, key: &str, v: String| {
            let _ = s.bg_tx.send(BgJob::Persist { key: key.into(), value: v });
        };
        if self.d_prefs {
            self.d_prefs = false;
            let v = serde_json::json!({
                "volume": self.st.volume, "muted": self.st.muted, "repeat": self.st.repeat, "shuffle": self.st.shuffle,
                "mix": self.st.mix, "mix_settings": self.st.mix_settings, "strip": self.st.strip,
                "output": self.st.devices.target,
            });
            ps(self, "player.prefs.v1", v.to_string());
        }
        if self.d_plan {
            self.d_plan = false;
            if let Ok(v) = serde_json::to_string(&self.st.plan) {
                ps(self, "player.plan.v1", v);
            }
        }
        if (self.d_queue && self.last_persist_queue.elapsed() > Duration::from_millis(1500)) || (force && self.d_queue) {
            self.d_queue = false;
            self.last_persist_queue = Instant::now();
            let v = serde_json::json!({
                "queue": self.st.queue, "queue_index": self.st.queue_index, "history": self.st.history,
                "history_pos": self.st.history_pos, "source": self.st.source,
            });
            ps(self, "player.queue.v1", v.to_string());
        }
        if self.d_pos && (self.last_persist_pos.elapsed() > Duration::from_secs(5) || force) {
            self.d_pos = false;
            self.last_persist_pos = Instant::now();
            if let (Some(c), Some((pos, _, _))) = (self.st.current.as_ref(), self.cur_pos()) {
                ps(self, "player.pos.v1", serde_json::json!({ "uid": c.uid, "track_id": c.track_id, "pos": pos }).to_string());
            }
        }
    }

    fn restore(&mut self) {
        if let Some(v) = self.ports.state.get("player.prefs.v1").and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
            let g = |k: &str| v.get(k).cloned();
            if let Some(x) = g("volume").and_then(|x| x.as_f64()) {
                self.st.volume = x;
            }
            if let Some(x) = g("muted").and_then(|x| x.as_bool()) {
                self.st.muted = x;
            }
            if let Some(x) = g("repeat").and_then(|x| serde_json::from_value(x).ok()) {
                self.st.repeat = x;
            }
            if let Some(x) = g("shuffle").and_then(|x| x.as_bool()) {
                self.st.shuffle = x;
            }
            if let Some(x) = g("mix").and_then(|x| x.as_bool()) {
                self.st.mix = x;
            }
            if let Some(x) = g("mix_settings").and_then(|x| serde_json::from_value::<MixSettings>(x).ok()) {
                self.st.mix_settings = x;
                if self.st.mix_settings.rev < bc_types::player::MIX_SETTINGS_REV {
                    self.st.mix_settings.migrate();
                    self.d_prefs = true;
                }
            }
            if let Some(x) = g("strip").and_then(|x| serde_json::from_value(x).ok()) {
                self.st.strip = x;
            }
            if let Some(x) = g("output").and_then(|x| serde_json::from_value::<OutputTarget>(x).ok())
                && let OutputKind::Cpal(t) = &mut self.cfg.output {
                    *t = x;
                }
        }
        if let Some(p) = self.ports.state.get("player.plan.v1").and_then(|s| serde_json::from_str::<PlanState>(&s).ok()) {
            self.st.plan = PlanState { spent_pool: None, ..p };
        }
        if let Some(v) = self.ports.state.get("player.queue.v1").and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()) {
            let queue: Vec<QueueItem> = v.get("queue").and_then(|x| serde_json::from_value(x.clone()).ok()).unwrap_or_default();
            if !queue.is_empty() {
                self.st.queue = queue.into_iter().map(|i| self.assign_uid(i)).collect();
                self.st.queue_index = v.get("queue_index").and_then(|x| x.as_i64()).unwrap_or(-1).min(self.st.queue.len() as i64 - 1);
                self.st.history = v.get("history").and_then(|x| serde_json::from_value(x.clone()).ok()).unwrap_or_default();
                self.st.history.retain(|h| *h < self.st.queue.len());
                self.st.history_pos = v.get("history_pos").and_then(|x| x.as_i64()).unwrap_or(-1).min(self.st.history.len() as i64 - 1);
                self.st.source = v.get("source").and_then(|x| serde_json::from_value(x.clone()).ok());
                self.set_current_from_index();
                self.rebuild_shuffle();
                if self.st.current.is_some() {
                    self.st.status = PlayerStatus::Paused;
                    self.paused_by_user = true;
                }
                self.queue_changed();
            }
        }
        self.d_queue = false;
        self.refresh_devices(false);
        self.touch();
    }

    // ------------------------------------------------------------------
    // publishing
    // ------------------------------------------------------------------

    fn publish_transition(&mut self) {
        self.st.transition = self.tr.clone();
        self.publisher.transition(self.tr.as_ref());
        self.d_state = true;
    }

    /// The full state (the queue always included), for `GET /player/state`.
    pub fn snapshot_state(&mut self) -> PlayerState {
        self.refresh_entry_points();
        let mut s = self.st.clone();
        s.queue_included = true;
        s
    }

    /// Publish right away (a command was just handled and its caller will read the state).
    pub fn publish_state_now(&mut self) {
        self.last_state_pub = Instant::now() - Duration::from_secs(1);
        self.publish_state_if_due();
    }

    fn publish_state_if_due(&mut self) {
        if !self.d_state || self.last_state_pub.elapsed() < Duration::from_millis(50) {
            return;
        }
        self.d_state = false;
        self.last_state_pub = Instant::now();
        self.st.rev += 1;
        if let Some(e) = &self.engine {
            let snap = e.snapshot();
            self.st.devices.xruns = snap.xruns;
        }
        self.refresh_entry_points();
        let include = self.d_queue_pub();
        let mut out = if include {
            let mut s = self.st.clone();
            s.queue_included = true;
            s
        } else {
            let mut s = PlayerState { queue: vec![], ..self.st.clone() };
            s.queue_included = false;
            s
        };
        out.preview.position_s = self.preview_pos();
        self.publisher.state(&out);
    }

    fn preview_pos(&self) -> f64 {
        let Some(e) = &self.engine else { return 0.0 };
        if !self.st.preview.playing {
            return 0.0;
        }
        let s = if e.has_cue() { e.cue_snapshot().unwrap_or_default() } else { e.snapshot() };
        s.decks[PREVIEW].pos_frames / e.preview_rate.max(1) as f64
    }

    /// Entry / exit points of the current track and the next rows, for the UI planner.
    fn refresh_entry_points(&mut self) {
        self.st.max_play_s = self.st.mix_settings.max_play_s;
        let key = (self.st.queue_rev, self.st.queue_index, self.st.mix);
        if self.entry_key == Some(key) {
            return;
        }
        self.entry_key = Some(key);
        self.st.entry_points.clear();
        if !self.st.mix || self.st.queue_index < 0 {
            return;
        }
        let from = self.st.queue_index as usize;
        let rows: Vec<QueueItem> = self.st.queue.iter().skip(from).take(ENTRY_POINT_ROWS).cloned().collect();
        for it in rows {
            let p = self.points_of(&it);
            self.st.entry_points.push(EntryPoint { uid: it.uid, drop_s: p.in_s, out_s: p.out_s });
        }
    }

    /// Whether the queue changed since the last published state.
    fn d_queue_pub(&mut self) -> bool {
        if self.queue_pub_rev != self.st.queue_rev {
            self.queue_pub_rev = self.st.queue_rev;
            true
        } else {
            false
        }
    }

    /// The audio thread's latest snapshot (tests, diagnostics).
    pub fn engine_snapshot(&self) -> Option<bc_dsp::shared::Snapshot> {
        self.engine.as_ref().map(|e| e.snapshot())
    }

    pub fn clock_now(&self) -> Clock {
        let Some(e) = &self.engine else {
            return Clock { rate: 1.0, ..Default::default() };
        };
        let snap = e.snapshot();
        let sr = snap.sample_rate.max(1) as f64;
        let (mut pos, mut dur, mut buffered, mut rate) = (0.0, 0.0, 0.0, 1.0);
        if let Some(d) = self.cur_deck {
            let dk = &snap.decks[d];
            pos = dk.pos_frames / sr;
            rate = dk.rate.max(0.0);
            buffered = dk.buffered_s;
            dur = if dk.len_frames > 0 { dk.len_frames as f64 / sr } else { self.decks[d].as_ref().map(|l| l.dur_s).unwrap_or(0.0) };
        }
        Clock {
            frames_played: snap.frames_played,
            sample_rate: snap.sample_rate,
            output_timestamp_ns: snap.output_ts_ns,
            server_time_ns: unix_ns(),
            rate,
            playing: self.st.status == PlayerStatus::Playing && !snap.paused,
            position_s: pos,
            duration_s: dur,
            buffered_s: buffered,
            track_uid: self.st.current.as_ref().map(|c| c.uid),
            mix_out_s: self.st.mix_out_s,
            peak_l: snap.peak_l,
            peak_r: snap.peak_r,
            xruns: snap.xruns,
        }
    }

    fn publish_clock_if_due(&mut self) {
        let every = if self.st.status == PlayerStatus::Playing { 33 } else { 500 };
        if self.last_clock_pub.elapsed() < Duration::from_millis(every) {
            return;
        }
        self.last_clock_pub = Instant::now();
        if self.engine.is_none() && self.st.status == PlayerStatus::Idle {
            return;
        }
        let c = self.clock_now();
        self.publisher.clock(&c);
    }
}

// ---------------------------------------------------------------------------
// background worker: everything that may block on the DB or the network
// ---------------------------------------------------------------------------

fn spawn_bg(ports: Ports, rx: Receiver<BgJob>, tx: Sender<BgResult>) {
    let _ = std::thread::Builder::new().name("bc-player-bg".into()).spawn(move || {
        while let Ok(job) = rx.recv() {
            match job {
                BgJob::Fill(job) => {
                    let found = autofill::fetch_fill(&ports, &job);
                    let _ = tx.send(BgResult::Fill(FillResult { run: job.run, seed_id: job.seed.track_id, found }));
                }
                BgJob::RecordPlay { track_id, ms, completed, skipped } => {
                    if let Err(e) = ports.library.record_play(track_id, ms, completed, skipped) {
                        tracing::debug!("recording a play failed: {e}");
                    }
                }
                BgJob::Persist { key, value } => ports.state.set(&key, &value),
                BgJob::Advance { run, early, source } => {
                    let (items, src) = advance(&ports, source);
                    let _ = tx.send(BgResult::Advance { run, early, items, source: src });
                }
                BgJob::StartSource { run, source, shuffle } => {
                    let (items, src, error, pins) = match start_source(&ports, source.clone(), shuffle) {
                        Ok((i, s, p)) => (i, s, None, p),
                        Err(e) => (vec![], None, Some(e.to_string()), vec![]),
                    };
                    let _ = tx.send(BgResult::Started { run, items, source: src, shuffle, error, pins });
                }
                BgJob::Sweep { run, source } => {
                    let (items, src, done) = sweep_step(&ports, source);
                    let _ = tx.send(BgResult::Sweep { run, items, source: src, done });
                }
            }
        }
    });
}

const MAX_EMPTY_HOPS: usize = 10;

/// The next queue of a source that just played out.
fn advance(ports: &Ports, source: QueueSource) -> (Vec<QueueItem>, Option<QueueSource>) {
    match source {
        QueueSource::Release { release_id, listing } => {
            let mut at = release_id;
            for _ in 0..MAX_EMPTY_HOPS {
                let Ok(Some(next)) = ports.library.next_release(at, &listing) else { return (vec![], None) };
                if let Ok(items) = ports.library.release_tracks(next)
                    && !items.is_empty() {
                        return (items, Some(QueueSource::Release { release_id: next, listing }));
                    }
                at = next;
            }
            (vec![], None)
        }
        QueueSource::Label { label_id, listing, mode } => {
            let mut at = label_id;
            for _ in 0..MAX_EMPTY_HOPS {
                let next = if mode == LabelMode::Shuffle {
                    ports.library.random_label(&listing, at)
                } else {
                    ports.library.next_label(at, &listing)
                };
                let Ok(Some(next)) = next else { return (vec![], None) };
                if let Ok(items) = ports.library.label_tracks(next, mode)
                    && !items.is_empty() {
                        return (items, Some(QueueSource::Label { label_id: next, listing, mode }));
                    }
                at = next;
            }
            (vec![], None)
        }
        QueueSource::Labels { listing } => match ports.library.shuffle_labels(&listing, 500) {
            Ok(items) if !items.is_empty() => (items, Some(QueueSource::Labels { listing })),
            _ => (vec![], None),
        },
        QueueSource::Fan(cursor) => match fan_batch(ports, &cursor) {
            Some((items, last)) => (items, Some(QueueSource::Fan(FanCursor { item_id: Some(last), ..cursor }))),
            None => (vec![], None),
        },
        QueueSource::Explore { .. } | QueueSource::Playlist { .. } | QueueSource::Set { .. } => (vec![], None),
    }
}

const FAN_BATCH: usize = 3;

/// The next step of a wishlist queue: the following record whole (in order),
/// or a few more records a track each (shuffled). Records with nothing playable
/// are stepped over, up to a cap.
fn fan_batch(ports: &Ports, cursor: &FanCursor) -> Option<(Vec<QueueItem>, i64)> {
    let mut after = cursor.item_id;
    let mut tracks: Vec<QueueItem> = vec![];
    let mut last: Option<i64> = None;
    let want = if cursor.order == FanOrder::Shuffle { FAN_BATCH } else { 1 };
    let mut hops = 0;
    let mut rng = Rng::seeded();
    while tracks.len() < want && hops < MAX_EMPTY_HOPS {
        let page = ports.bandcamp.fan_next(cursor, after, want).ok()?;
        if page.items.is_empty() {
            break;
        }
        for item in &page.items {
            after = Some(item.item_id);
            let mut found: Vec<QueueItem> = vec![];
            if let Some(rid) = item.release_id
                && let Ok(v) = ports.library.release_tracks(rid) {
                    found = v;
                }
            if found.is_empty() {
                found = ports.bandcamp.release_tracks(&item.url).unwrap_or_default();
            }
            let take: Vec<QueueItem> = if cursor.order == FanOrder::Shuffle {
                if found.is_empty() { vec![] } else { vec![found[rng.below(found.len())].clone()] }
            } else {
                found
            };
            if take.is_empty() {
                hops += 1;
                if hops >= MAX_EMPTY_HOPS {
                    break;
                }
                continue;
            }
            tracks.extend(take);
            last = Some(item.item_id);
            if tracks.len() >= want {
                break;
            }
        }
        if page.exhausted {
            break;
        }
    }
    if tracks.is_empty() { None } else { last.map(|l| (tracks, l)) }
}

/// A set slot's plan for one track: mix points and, when the set tempo-adjusts it, `(rate, key_lock)`.
/// Where a matched incoming starts: on a beat of its own grid, on a bar when the
/// blend is bar- or phrase-aligned, so its bars run with the outgoing's. A planned
/// bass swap moves with the start, staying on the same place in the incoming.
fn snap_start(g: &BeatGrid, start_s: f64, spec: &mut BlendSpec) -> f64 {
    let st = match spec.quantise {
        Quantise::Bar | Quantise::Phrase => snap_bar_nearest(g, start_s),
        _ => snap_nearest(g, start_s),
    };
    if let (Some(sw), Some(sy)) = (spec.swap_s.as_mut(), spec.sync) {
        *sw = (*sw - (st - start_s) / sy.rate.max(0.1)).max(0.0);
    }
    spec.incoming_start_s = st;
    st
}

fn quality_code(q: KeyLockQuality) -> u8 {
    match q {
        KeyLockQuality::Fast => 0,
        KeyLockQuality::Balanced => 1,
        KeyLockQuality::High => 2,
    }
}

/// How many rows (from the current one) get published entry points.
const ENTRY_POINT_ROWS: usize = 12;

type Pin = (i64, MixPoints, Option<(f64, bool)>);
type Started = (Vec<QueueItem>, Option<QueueSource>, Vec<Pin>);

fn start_source(ports: &Ports, source: QueueSource, shuffle: bool) -> Result<Started, PortError> {
    match &source {
        QueueSource::Release { release_id, .. } => {
            let items = ports.library.release_tracks(*release_id)?;
            Ok((items, Some(source), vec![]))
        }
        QueueSource::Label { label_id, mode, .. } => Ok((ports.library.label_tracks(*label_id, *mode)?, Some(source), vec![])),
        QueueSource::Labels { listing } => Ok((ports.library.shuffle_labels(listing, 500)?, Some(source), vec![])),
        QueueSource::Playlist { id, .. } => {
            let mut items = ports.library.playlist_items(*id)?;
            if shuffle {
                Rng::seeded().shuffle(&mut items);
            }
            Ok((items, Some(source), vec![]))
        }
        QueueSource::Set { id, .. } => {
            // A DJ set plays its plan: the cues and the planned blend lengths are pinned as the
            // tracks' mix points, so the live blend is the one the Arrange view drew.
            let slots = ports.library.set_plan(*id)?;
            let pins = slots
                .iter()
                .map(|sl| {
                    let dur = sl.item.duration_s();
                    let pts = MixPoints {
                        in_s: sl.cue_in_ms.map(|v| v as f64 / 1000.0).unwrap_or(0.0),
                        out_s: sl.cue_out_ms.map(|v| v as f64 / 1000.0).or(dur),
                        blend_s: sl.blend_in_ms.map(|v| v as f64 / 1000.0),
                    };
                    let rate = 1.0 + sl.tempo_adjust_pct / 100.0;
                    let adj = ((rate - 1.0).abs() > 1e-6).then_some((rate, sl.key_lock));
                    (sl.item.track_id, pts, adj)
                })
                .collect();
            Ok((slots.into_iter().map(|sl| sl.item).collect(), Some(source), pins))
        }
        QueueSource::Fan(c) => {
            let (items, last) = fan_batch(ports, c).ok_or(PortError::NotFound)?;
            Ok((items, Some(QueueSource::Fan(FanCursor { item_id: Some(last), ..c.clone() })), vec![]))
        }
        QueueSource::Explore { cards, shuffle: sh, .. } => {
            let mut cards = cards.clone();
            if *sh {
                Rng::seeded().shuffle(&mut cards);
            }
            let src = QueueSource::Explore { cards, shuffle: *sh, next: 0 };
            let (mut items, src2, _) = sweep_step(ports, src);
            if items.is_empty() {
                return Err(PortError::NotFound);
            }
            if *sh {
                Rng::seeded().shuffle(&mut items);
            }
            Ok((items, Some(src2), vec![]))
        }
    }
}

/// Fetch the next card(s) of an explore sweep until some tracks arrive.
fn sweep_step(ports: &Ports, source: QueueSource) -> (Vec<QueueItem>, QueueSource, bool) {
    let QueueSource::Explore { cards, shuffle, mut next } = source else {
        return (vec![], source, true);
    };
    let mut out = vec![];
    while next < cards.len() && out.is_empty() {
        let card = &cards[next];
        next += 1;
        let items = if let Some(rid) = card.library_release_id {
            ports.library.release_tracks(rid).unwrap_or_default()
        } else {
            ports.bandcamp.release_tracks(&card.url).unwrap_or_default()
        };
        out = items;
    }
    if shuffle {
        Rng::seeded().shuffle(&mut out);
    }
    let done = next >= cards.len();
    (out, QueueSource::Explore { cards, shuffle, next }, done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{FanItem, FanPage, FilePorts, LibraryPort};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn wav(dir: &std::path::Path, name: &str, secs: f32) -> PathBuf {
        let p = dir.join(name);
        let spec = hound::WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut w = hound::WavWriter::create(&p, spec).unwrap();
        for i in 0..(48_000.0 * secs) as usize {
            let v = ((i as f32 * 0.05).sin() * 8000.0) as i16;
            w.write_sample(v).unwrap();
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
        p
    }

    fn session(n: usize) -> (Session, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<PathBuf> = (0..n).map(|i| wav(dir.path(), &format!("t{i}.wav"), 1.0)).collect();
        let fp = FilePorts::new(files);
        let cfg = SessionConfig { output: OutputKind::Null { sample_rate: 48_000, block: 256, speed: 0.0, capture: None, cue: None }, ..Default::default() };
        (Session::new(fp.into_ports(), Box::new(NullPublisher), cfg), dir)
    }

    fn items(n: usize) -> Vec<QueueItem> {
        (1..=n as i64).map(|i| QueueItem { track_id: i, title: format!("t{i}"), ..Default::default() }).collect()
    }

    fn play(s: &mut Session, n: usize) {
        s.handle(PlayerCommand::PlayQueue { items: items(n), start_index: 0, source: None }).unwrap();
    }

    #[test]
    fn true_shuffle_visits_every_track_once() {
        let (mut s, _d) = session(8);
        s.handle(PlayerCommand::SetShuffle { on: true }).unwrap();
        play(&mut s, 8);
        let mut seen = vec![s.st.queue_index as usize];
        for _ in 0..7 {
            s.next(false, false);
            seen.push(s.st.queue_index as usize);
        }
        let mut sorted = seen.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 8, "every row exactly once, no repeats: {seen:?}");
        assert_eq!(s.st.history.len(), 8);
    }

    #[test]
    fn previous_and_next_retrace_the_played_chain_in_shuffle() {
        let (mut s, _d) = session(8);
        s.handle(PlayerCommand::SetShuffle { on: true }).unwrap();
        play(&mut s, 8);
        let mut chain = vec![s.st.queue_index];
        for _ in 0..3 {
            s.next(false, false);
            chain.push(s.st.queue_index);
        }
        for expect in chain.iter().rev().skip(1) {
            s.previous();
            assert_eq!(s.st.queue_index, *expect, "walks back through what was played");
        }
        for expect in chain.iter().skip(1) {
            s.next(false, false);
            assert_eq!(s.st.queue_index, *expect, "walks forward along the same chain");
        }
    }

    #[test]
    fn repeat_all_wraps_and_off_ends_the_queue() {
        let (mut s, _d) = session(3);
        play(&mut s, 3);
        s.handle(PlayerCommand::SetRepeat { mode: RepeatMode::All }).unwrap();
        s.next(false, false);
        s.next(false, false);
        s.next(false, false);
        assert_eq!(s.st.queue_index, 0);
        s.handle(PlayerCommand::SetRepeat { mode: RepeatMode::Off }).unwrap();
        s.next(false, false);
        s.next(false, false);
        assert_eq!(s.st.queue_index, 2);
        s.next(false, false);
        assert_eq!(s.st.status, PlayerStatus::Paused, "out of tracks with no source is the end");
        assert_eq!(s.st.queue_index, 2);
    }

    #[test]
    fn queue_edits_remap_history_and_turn_shuffle_off() {
        let (mut s, _d) = session(6);
        s.handle(PlayerCommand::SetShuffle { on: true }).unwrap();
        play(&mut s, 6);
        assert!(s.st.shuffle);
        let cur = s.st.queue_index as usize;
        let last = s.st.queue.len();
        s.handle(PlayerCommand::MoveInQueue { from: last - 1, to: (cur + 1).min(last - 1) }).unwrap();
        assert!(!s.st.shuffle, "planning an order and shuffling per track contradict each other");
        s.handle(PlayerCommand::InsertAt { index: 0, items: items(1) }).unwrap();
        // the current row and the played chain stay where they were
        assert_eq!(s.st.history.first().copied(), Some(cur));
        assert_eq!(s.st.queue_index as usize, cur);
        assert_eq!(s.st.queue.len(), last + 1);
        s.handle(PlayerCommand::RemoveRange { from: cur + 1, to: s.st.queue.len() }).unwrap();
        assert_eq!(s.st.queue.len(), cur + 1);
    }

    #[test]
    fn plan_ops_flow_through_the_command_surface() {
        let (mut s, _d) = session(1);
        s.handle(PlayerCommand::Plan { op: PlanOp::SetAutoFill { on: false } }).unwrap();
        assert!(!s.st.plan.auto_fill);
        s.handle(PlayerCommand::Plan { op: PlanOp::MixFrom { pool: Pool::Loved } }).unwrap();
        assert_eq!(s.st.plan.pools, vec![Pool::Loved]);
        s.handle(PlayerCommand::SetMixSettings { patch: MixSettingsPatch { max_play_s: Some(Some(180.0)), ..Default::default() } }).unwrap();
        assert_eq!(s.st.mix_settings.max_play_s, Some(180.0));
    }

    /// A tiny fan walk: items 1 (two tracks), 2 (nothing playable), 3 (one track).
    struct FanPorts;
    impl crate::ports::BandcampPort for FanPorts {
        fn resolve_stream(&self, _: &QueueItem) -> crate::ports::PortResult<String> {
            Err(PortError::NotFound)
        }
        fn fan_next(&self, c: &FanCursor, after: Option<i64>, limit: usize) -> crate::ports::PortResult<FanPage> {
            let all = [1i64, 2, 3];
            let items: Vec<FanItem> =
                all.iter().filter(|i| after.map(|a| **i > a).unwrap_or(true)).take(limit).map(|&i| FanItem { item_id: i, url: format!("u{i}"), release_id: None }).collect();
            let exhausted = items.last().map(|l| l.item_id >= 3).unwrap_or(true);
            let _ = c;
            Ok(FanPage { items, exhausted })
        }
        fn release_tracks(&self, url: &str) -> crate::ports::PortResult<Vec<QueueItem>> {
            let t = |n: &str| QueueItem { track_id: -1, title: n.into(), origin: ItemOrigin::Bandcamp, ..Default::default() };
            Ok(match url {
                "u1" => vec![t("a1"), t("a2")],
                "u3" => vec![t("c1")],
                _ => vec![],
            })
        }
    }

    fn fan_ports() -> Ports {
        let base = FilePorts::new(vec![]).into_ports();
        Ports { bandcamp: Arc::new(FanPorts), ..base }
    }

    #[test]
    fn fan_walk_in_order_takes_whole_records_and_steps_over_empty_ones() {
        let ports = fan_ports();
        let cur = FanCursor { fan_id: 1, item_id: None, order: FanOrder::Seq, seed: 1, states: vec![], tab: None, fan_name: "x".into(), shelf: "x".into() };
        let (items, last) = fan_batch(&ports, &cur).unwrap();
        assert_eq!(items.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), vec!["a1", "a2"]);
        assert_eq!(last, 1);
        let (items, last) = fan_batch(&ports, &FanCursor { item_id: Some(1), ..cur.clone() }).unwrap();
        assert_eq!(items.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(), vec!["c1"], "the empty record 2 is stepped over");
        assert_eq!(last, 3);
        assert!(fan_batch(&ports, &FanCursor { item_id: Some(3), ..cur }).is_none(), "played out");
    }

    #[test]
    fn fan_walk_shuffled_takes_one_track_per_record() {
        let ports = fan_ports();
        let cur = FanCursor { fan_id: 1, item_id: None, order: FanOrder::Shuffle, seed: 1, states: vec![], tab: None, fan_name: "x".into(), shelf: "x".into() };
        let (items, last) = fan_batch(&ports, &cur).unwrap();
        // records 1 and 3 have tracks; one each, record 2 is empty
        assert_eq!(items.len(), 2);
        assert_eq!(last, 3);
    }

    #[test]
    fn a_dj_set_plays_its_plan_with_pinned_cues_and_blends() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<PathBuf> = (0..3).map(|i| wav(dir.path(), &format!("s{i}.wav"), 1.0)).collect();
        let mut fp = FilePorts::new(files);
        fp.duration_ms = vec![Some(200_000); 3];
        let cfg = SessionConfig { output: OutputKind::Null { sample_rate: 48_000, block: 256, speed: 0.0, capture: None, cue: None }, mpris: false, ..Default::default() };
        let mut s = Session::new(fp.into_ports(), Box::new(NullPublisher), cfg);
        s.handle(PlayerCommand::StartSource { source: QueueSource::Set { id: 1, name: "x".into() }, shuffle: false }).unwrap();
        let t0 = Instant::now();
        while s.st.queue.is_empty() {
            assert!(t0.elapsed() < Duration::from_secs(10), "the set never started");
            s.tick();
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(s.st.queue.len(), 3);
        assert!(matches!(s.st.source, Some(QueueSource::Set { .. })));
        assert!(!s.st.source.as_ref().unwrap().continues(), "a set stops where it stops");
        let (first, second) = (s.st.queue[0].clone(), s.st.queue[1].clone());
        let p0 = s.points_of(&first);
        assert_eq!((p0.in_s, p0.out_s, p0.blend_s), (2.0, Some(197.0), None));
        let p1 = s.points_of(&second);
        assert_eq!((p1.in_s, p1.out_s, p1.blend_s), (2.0, Some(197.0), Some(4.0)), "the planned blend INTO this slot");
        // the planner fields: entry points follow the pinned cues
        s.set_mix(true);
        let st = s.snapshot_state();
        assert_eq!(st.entry_points.len(), 3);
        assert_eq!((st.entry_points[1].drop_s, st.entry_points[1].out_s), (2.0, Some(197.0)));
        assert_eq!(st.max_play_s, None);
        // the slot's tempo adjustment and key lock are the live plan too, as in the offline render
        assert_eq!(s.adjust.get(&first.track_id), None);
        assert_eq!(s.adjust.get(&second.track_id), Some(&(1.04, true)));
        s.handle(PlayerCommand::Next).unwrap();
        let t1 = Instant::now();
        loop {
            assert!(t1.elapsed() < Duration::from_secs(10), "slot 2 never played");
            s.tick();
            if s.st.queue_index == 1
                && let Some(snap) = s.engine_snapshot()
            {
                let d = &snap.decks[s.cur_deck.unwrap_or(0)];
                if d.state == 2 {
                    assert!((d.rate - 1.04).abs() < 1e-3, "slot plays at its adjusted tempo: {}", d.rate);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn explore_sweep_steps_cards_until_tracks_arrive() {
        let ports = FilePorts::new(vec![]).into_ports();
        let src = QueueSource::Explore { cards: vec![ExploreCard { url: "x".into(), library_release_id: None }; 3], shuffle: false, next: 0 };
        let (items, src2, done) = sweep_step(&ports, src);
        assert!(items.is_empty(), "no bandcamp port: nothing playable");
        assert!(done);
        assert!(matches!(src2, QueueSource::Explore { next: 3, .. }));
        let _ = LibraryPort::next_release(&*ports.library, 0, &serde_json::Value::Null);
    }
}
