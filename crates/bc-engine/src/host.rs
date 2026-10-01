//! The native audio host: cpal output (ALSA, which PipeWire serves on Arch)
//! with device selection and buffer-size setting, an optional second device
//! for cue/headphones, a null (software-paced) output for tests and the
//! offline renderer, and the control-side [`Engine`] handle.
//!
//! The audio callback owns the [`Mixer`] and only touches pre-allocated
//! buffers; the control side talks to it through `rtrb` command/event queues,
//! lock-free decoded-PCM rings and a seqlock snapshot.

use crate::decode::{DecodeError, DecodeWorker, Opened, Source};
use bc_dsp::deck::Chunk;
use bc_dsp::mixer::{Cmd, CueOut, Event, MAX_BLOCK, Mixer, MixerPorts};
use bc_dsp::shared::{SharedState, Snapshot};
use bc_dsp::stretch::StretchQuality;
use bc_types::player::{OutputDevice, OutputTarget};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::Receiver;
use parking_lot::Mutex;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DECK_A: usize = 0;
pub const DECK_B: usize = 1;
pub const PREVIEW: usize = 2;

/// Chunks per deck ring: 512 x 1024 frames, about 11 s at 48 kHz (rides out a stalled disk).
const RING_CHUNKS: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("no audio output device: {0}")]
    NoDevice(String),
    #[error("audio stream: {0}")]
    Stream(String),
}

/// What the engine renders into.
#[derive(Clone)]
pub enum OutputKind {
    /// cpal (ALSA / PipeWire) with the given target.
    Cpal(OutputTarget),
    /// A software-paced output: renders `block` frames at a time. `speed`
    /// above 1 renders faster than real time (0 = as fast as possible).
    Null { sample_rate: u32, block: usize, speed: f64, capture: Option<Arc<Mutex<Vec<f32>>>> },
}

#[derive(Debug, Clone, Default)]
pub struct StreamInfo {
    pub backend: String,
    pub device: String,
    pub sample_rate: u32,
    pub buffer_frames: u32,
    pub latency_ms: f64,
    pub cue_device: Option<String>,
}

pub fn unix_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

/// Output devices the host can open.
pub fn list_output_devices() -> Vec<OutputDevice> {
    let host = cpal::default_host();
    let default_id = host.default_output_device().and_then(|d| d.id().ok()).map(|i| i.to_string());
    let mut out = Vec::new();
    let Ok(devs) = host.output_devices() else { return out };
    for d in devs {
        let Ok(id) = d.id() else { continue };
        let id = id.to_string();
        let label = d.description().map(|x| x.name().to_string()).unwrap_or_else(|_| id.clone());
        let (rate, ch) = d
            .default_output_config()
            .map(|c| (c.sample_rate(), c.channels()))
            .unwrap_or((0, 0));
        out.push(OutputDevice {
            is_default: default_id.as_deref() == Some(id.as_str()),
            name: id,
            label,
            max_channels: ch,
            default_sample_rate: rate,
        });
    }
    out
}

fn find_device(host: &cpal::Host, name: &Option<String>) -> Result<cpal::Device, HostError> {
    match name {
        None => host.default_output_device().ok_or_else(|| HostError::NoDevice("no default output device".into())),
        Some(n) => {
            let devs = host.output_devices().map_err(|e| HostError::NoDevice(e.to_string()))?;
            for d in devs {
                let id = d.id().map(|i| i.to_string()).unwrap_or_default();
                let label = d.description().map(|x| x.name().to_string()).unwrap_or_default();
                if &id == n || &label == n {
                    return Ok(d);
                }
            }
            Err(HostError::NoDevice(format!("device {n:?} not found")))
        }
    }
}

/// Whether a (silent) stream can really be opened on `device` (ALSA reports "busy" or
/// "not available" only at open time, e.g. the raw card while PipeWire owns it).
fn probe(device: &cpal::Device, config: &cpal::StreamConfig, fmt: cpal::SampleFormat) -> Result<(), String> {
    macro_rules! silent {
        ($t:ty) => {
            device.build_output_stream(
                config.clone(),
                |data: &mut [$t], _: &cpal::OutputCallbackInfo| data.iter_mut().for_each(|s| *s = <$t as cpal::Sample>::EQUILIBRIUM),
                |_| {},
                None,
            )
        };
    }
    let stream = match fmt {
        cpal::SampleFormat::F32 => silent!(f32),
        cpal::SampleFormat::I16 => silent!(i16),
        cpal::SampleFormat::I32 => silent!(i32),
        cpal::SampleFormat::U16 => silent!(u16),
        f => return Err(format!("unsupported sample format {f}")),
    }
    .map_err(|e| e.to_string())?;
    drop(stream);
    Ok(())
}

/// The device to use: the one asked for, or -- with no preference -- the first that
/// really opens among the host default, PipeWire, PulseAudio and the rest. (On a
/// PipeWire system the raw ALSA `default` card is often held by the server.)
fn pick_device(host: &cpal::Host, name: &Option<String>) -> Result<(cpal::Device, cpal::StreamConfig, cpal::SampleFormat), HostError> {
    let mut candidates: Vec<cpal::Device> = Vec::new();
    if name.is_some() {
        candidates.push(find_device(host, name)?);
    } else {
        if let Some(d) = host.default_output_device() {
            candidates.push(d);
        }
        for want in ["alsa:pipewire", "alsa:pulse", "pipewire", "pulse"] {
            if let Ok(d) = find_device(host, &Some(want.to_string())) {
                candidates.push(d);
            }
        }
        if let Ok(devs) = host.output_devices() {
            candidates.extend(devs);
        }
    }
    let mut last = String::from("no output devices");
    for d in candidates {
        let Ok(supported) = d.default_output_config() else { continue };
        let fmt = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        match probe(&d, &config, fmt) {
            Ok(()) => return Ok((d, config, fmt)),
            Err(e) => {
                last = format!("{}: {e}", d.id().map(|i| i.to_string()).unwrap_or_default());
                tracing::debug!("output device skipped ({last})");
            }
        }
    }
    Err(HostError::NoDevice(last))
}

struct Rings {
    prod: [Producer<Chunk>; 3],
    cons: [Consumer<Chunk>; 3],
}

fn rings() -> Rings {
    let (p0, c0) = RingBuffer::new(RING_CHUNKS);
    let (p1, c1) = RingBuffer::new(RING_CHUNKS);
    let (p2, c2) = RingBuffer::new(RING_CHUNKS);
    Rings { prod: [p0, p1, p2], cons: [c0, c1, c2] }
}

/// Handle to a running cue (headphone) output.
struct CueHandle {
    cmd: Mutex<Producer<Cmd>>,
    events: Mutex<Consumer<Event>>,
    shared: Arc<SharedState>,
}

/// Control-side handle to the audio engine.
pub struct Engine {
    pub sample_rate: u32,
    pub info: StreamInfo,
    cmd: Mutex<Producer<Cmd>>,
    events: Mutex<Consumer<Event>>,
    shared: Arc<SharedState>,
    workers: [DecodeWorker; 3],
    cue: Option<CueHandle>,
    epoch: AtomicU32,
    pub device_xruns: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    owner: Option<std::thread::JoinHandle<()>>,
}

impl Engine {
    /// Open an output and start the mixer.
    pub fn open(kind: OutputKind, quality: StretchQuality) -> Result<Engine, HostError> {
        let Rings { prod, cons } = rings();
        let [p0, p1, p2] = prod;
        let [c0, c1, c2] = cons;
        let (cmd_tx, cmd_rx) = RingBuffer::<Cmd>::new(512);
        let (ev_tx, ev_rx) = RingBuffer::<Event>::new(512);
        let shared = Arc::new(SharedState::new());
        let stop = Arc::new(AtomicBool::new(false));
        let device_xruns = Arc::new(AtomicU64::new(0));
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<StreamInfo, HostError>>(1);

        // cue-device rings/queues exist only when a cue device was asked for
        let want_cue = matches!(&kind, OutputKind::Cpal(t) if t.cue_device.is_some());
        let (cue_ring_c, cue_p2, cue_ctl) = if want_cue {
            let (cp, cc) = RingBuffer::<Chunk>::new(RING_CHUNKS);
            let (ctx, crx) = RingBuffer::<Cmd>::new(128);
            let (etx, erx) = RingBuffer::<Event>::new(128);
            let sh = Arc::new(SharedState::new());
            (Some(cc), Some(cp), Some((ctx, crx, etx, erx, sh)))
        } else {
            (None, None, None)
        };
        let (cue_cmd_rx, cue_ev_tx, cue_handle, cue_shared_for_stream) = match cue_ctl {
            Some((ctx, crx, etx, erx, sh)) => (
                Some(crx),
                Some(etx),
                Some(CueHandle { cmd: Mutex::new(ctx), events: Mutex::new(erx), shared: sh.clone() }),
                Some(sh),
            ),
            None => (None, None, None, None),
        };

        let stop2 = stop.clone();
        let dx = device_xruns.clone();
        let sh2 = shared.clone();
        let owner = std::thread::Builder::new()
            .name("bc-audio-owner".into())
            .spawn(move || {
                // dummy ring for the main mixer's preview slot when the cue device owns the preview
                let (main_preview_ring, ring_for_cue) = match cue_ring_c {
                    Some(cc) => {
                        let (_dp, dc) = RingBuffer::<Chunk>::new(2);
                        (dc, Some(cc))
                    }
                    None => (c2, None),
                };
                let ports = MixerPorts { rings: [c0, c1, main_preview_ring], cmds: cmd_rx, events: ev_tx, shared: sh2 };
                let started = match kind {
                    OutputKind::Cpal(target) => start_cpal(
                        &target,
                        ports,
                        quality,
                        dx,
                        ring_for_cue.zip(cue_cmd_rx).zip(cue_ev_tx).zip(cue_shared_for_stream),
                    ),
                    OutputKind::Null { sample_rate, block, speed, capture } => {
                        start_null(sample_rate, block, speed, capture, ports, quality, stop2.clone())
                    }
                };
                match started {
                    Ok((keep, info)) => {
                        let _ = ready_tx.send(Ok(info));
                        while !stop2.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(50));
                        }
                        drop(keep);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| HostError::Stream(e.to_string()))?;
        let info = ready_rx
            .recv_timeout(Duration::from_secs(15))
            .map_err(|_| HostError::Stream("audio thread did not start".into()))??;
        let sr = info.sample_rate;
        // preview decodes at the cue device's rate when it has its own device
        let cue_sr = info.cue_device.as_ref().map(|_| sr).unwrap_or(sr);
        let _ = cue_sr;
        let workers = [DecodeWorker::spawn("A", sr, p0), DecodeWorker::spawn("B", sr, p1), {
            match cue_p2 {
                Some(cp) => DecodeWorker::spawn("cue", sr, cp),
                None => DecodeWorker::spawn("P", sr, p2),
            }
        }];
        Ok(Engine {
            sample_rate: sr,
            info,
            cmd: Mutex::new(cmd_tx),
            events: Mutex::new(ev_rx),
            shared,
            workers,
            cue: cue_handle,
            epoch: AtomicU32::new(1),
            device_xruns,
            stop,
            owner: Some(owner),
        })
    }

    /// A null-output engine (tests, offline use): real-time paced when `speed` is 1.
    pub fn open_null(sample_rate: u32, speed: f64, capture: Option<Arc<Mutex<Vec<f32>>>>) -> Result<Engine, HostError> {
        Self::open(OutputKind::Null { sample_rate, block: 512, speed, capture }, StretchQuality::Normal)
    }

    pub fn next_epoch(&self) -> u32 {
        self.epoch.fetch_add(1, Ordering::Relaxed)
    }

    /// Send a command to the mixer. Returns false when the queue is full.
    pub fn send(&self, c: Cmd) -> bool {
        self.cmd.lock().push(c).is_ok()
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut s = self.shared.load();
        s.xruns += self.device_xruns.load(Ordering::Relaxed);
        s
    }

    /// Snapshot of the cue device's preview deck, when there is one.
    pub fn cue_snapshot(&self) -> Option<Snapshot> {
        self.cue.as_ref().map(|c| c.shared.load())
    }

    pub fn has_cue(&self) -> bool {
        self.cue.is_some()
    }

    pub fn poll_events(&self, out: &mut Vec<Event>) {
        let mut ev = self.events.lock();
        while let Ok(e) = ev.pop() {
            out.push(e);
        }
        if let Some(c) = &self.cue {
            let mut cev = c.events.lock();
            while let Ok(e) = cev.pop() {
                out.push(e);
            }
        }
    }

    /// Send a preview command to the cue device when present, else to the main mixer.
    pub fn send_preview(&self, c: Cmd) -> bool {
        match &self.cue {
            Some(h) => h.cmd.lock().push(c).is_ok(),
            None => self.send(c),
        }
    }

    /// Start decoding `source` into `deck`'s ring at `start_frame` (device
    /// rate). Returns the new epoch and a channel with the container facts.
    pub fn begin_load(&self, deck: usize, source: Source, start_frame: u64) -> (u32, Receiver<Result<Opened, DecodeError>>) {
        let epoch = self.next_epoch();
        let rx = self.workers[deck].load_async(epoch, source, start_frame);
        (epoch, rx)
    }

    /// Re-position a deck: new epoch, decoder seeks, mixer follows. Returns the epoch.
    pub fn seek_deck(&self, deck: usize, frame: u64) -> u32 {
        let epoch = self.next_epoch();
        self.workers[deck].seek(epoch, frame);
        if deck == PREVIEW {
            self.send_preview(Cmd::Seek { deck: 0, epoch, frame });
        } else {
            self.send(Cmd::Seek { deck: deck as u8, epoch, frame });
        }
        epoch
    }

    pub fn stop_decoder(&self, deck: usize) {
        self.workers[deck].stop();
    }

    pub fn stop_all(&self) {
        for w in &self.workers {
            w.stop();
        }
        self.send(Cmd::Stop);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.owner.take() {
            let _ = j.join();
        }
    }
}

type Keep = Box<dyn std::any::Any>;
type CueParts = Option<(((Consumer<Chunk>, Consumer<Cmd>), Producer<Event>), Arc<SharedState>)>;

fn stream_err(device_xruns: Arc<AtomicU64>) -> impl FnMut(cpal::Error) + Send + 'static {
    move |err| match err.kind() {
        cpal::ErrorKind::Xrun => {
            device_xruns.fetch_add(1, Ordering::Relaxed);
        }
        _ => tracing::warn!("audio stream error: {err}"),
    }
}

fn start_cpal(
    target: &OutputTarget,
    ports: MixerPorts,
    quality: StretchQuality,
    device_xruns: Arc<AtomicU64>,
    cue: CueParts,
) -> Result<(Keep, StreamInfo), HostError> {
    let host = cpal::default_host();
    let (device, mut config, sample_format) = pick_device(&host, &target.device)?;
    if let Some(n) = target.buffer_frames {
        config.buffer_size = cpal::BufferSize::Fixed(n);
    }
    let sr = config.sample_rate;
    let channels = config.channels as usize;
    let buffer_frames = match config.buffer_size {
        cpal::BufferSize::Fixed(n) => n,
        cpal::BufferSize::Default => 1024,
    };
    let mixer = Mixer::new(sr as f64, ports, quality);
    let label = device.description().map(|d| d.name().to_string()).unwrap_or_default();

    macro_rules! build {
        ($t:ty) => {
            build_stream::<$t>(&device, config.clone(), mixer, channels, device_xruns.clone())?
        };
    }
    let stream = match sample_format {
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::I32 => build!(i32),
        cpal::SampleFormat::U16 => build!(u16),
        f => return Err(HostError::Stream(format!("unsupported sample format {f}"))),
    };
    stream.play().map_err(|e| HostError::Stream(e.to_string()))?;
    let mut keep: Vec<Box<dyn std::any::Any>> = vec![Box::new(stream)];
    let mut cue_label = None;

    if let (Some((((ring, cmds), events), shared)), Some(name)) = (cue, target.cue_device.clone()) {
        match start_cue(&name, ring, cmds, events, shared, device_xruns.clone()) {
            Ok((s, l)) => {
                keep.push(Box::new(s));
                cue_label = Some(l);
            }
            Err(e) => tracing::warn!("cue device unavailable: {e}"),
        }
    }
    let info = StreamInfo {
        backend: format!("cpal/{}", host.id().name()),
        device: label,
        sample_rate: sr,
        buffer_frames,
        latency_ms: buffer_frames as f64 * 1000.0 / sr as f64,
        cue_device: cue_label,
    };
    Ok((Box::new(keep), info))
}

fn out_ts(info: &cpal::OutputCallbackInfo) -> u64 {
    let ts = info.timestamp();
    let lat = ts.playback.saturating_duration_since(ts.callback);
    unix_ns() + lat.as_nanos() as u64
}

/// Copy stereo frames to the device layout (any channel count) and format.
fn write_frames<T: cpal::SizedSample + cpal::FromSample<f32>>(data: &mut [T], stereo: &[f32], channels: usize) {
    let frames = data.len() / channels;
    for i in 0..frames {
        let (l, r) = (stereo[i * 2], stereo[i * 2 + 1]);
        let f = &mut data[i * channels..(i + 1) * channels];
        match channels {
            1 => f[0] = T::from_sample(0.5 * (l + r)),
            _ => {
                f[0] = T::from_sample(l);
                f[1] = T::from_sample(r);
                for s in &mut f[2..] {
                    *s = T::from_sample(0.0);
                }
            }
        }
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut mixer: Mixer,
    channels: usize,
    device_xruns: Arc<AtomicU64>,
) -> Result<cpal::Stream, HostError>
where
    T: cpal::SizedSample + cpal::FromSample<f32> + Send + 'static,
{
    let mut scratch = vec![0.0f32; MAX_BLOCK * 2];
    device
        .build_output_stream(
            config,
            move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
                let total = data.len() / channels;
                let ts = out_ts(info);
                let mut done = 0;
                while done < total {
                    let n = (total - done).min(MAX_BLOCK);
                    mixer.render(&mut scratch[..n * 2], n, ts + (done as u64 * 1_000_000_000) / mixer.sample_rate() as u64);
                    write_frames(&mut data[done * channels..(done + n) * channels], &scratch[..n * 2], channels);
                    done += n;
                }
            },
            stream_err(device_xruns),
            None,
        )
        .map_err(|e| HostError::Stream(e.to_string()))
}

fn start_cue(
    name: &str,
    ring: Consumer<Chunk>,
    cmds: Consumer<Cmd>,
    events: Producer<Event>,
    shared: Arc<SharedState>,
    device_xruns: Arc<AtomicU64>,
) -> Result<(cpal::Stream, String), HostError> {
    let host = cpal::default_host();
    let device = find_device(&host, &Some(name.to_string()))?;
    let supported = device.default_output_config().map_err(|e| HostError::Stream(e.to_string()))?;
    let fmt = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = config.channels as usize;
    let sr = config.sample_rate;
    let label = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| name.to_string());
    let mut cue = CueOut::new(sr as f64, ring, cmds, events, shared);
    let mut scratch = vec![0.0f32; MAX_BLOCK * 2];
    macro_rules! cue_stream {
        ($t:ty) => {
            device.build_output_stream(
                config.clone(),
                move |data: &mut [$t], info: &cpal::OutputCallbackInfo| {
                    let total = data.len() / channels;
                    let ts = out_ts(info);
                    let mut done = 0;
                    while done < total {
                        let n = (total - done).min(MAX_BLOCK);
                        cue.render(&mut scratch[..n * 2], n, ts);
                        write_frames(&mut data[done * channels..(done + n) * channels], &scratch[..n * 2], channels);
                        done += n;
                    }
                },
                stream_err(device_xruns.clone()),
                None,
            )
        };
    }
    let stream = match fmt {
        cpal::SampleFormat::F32 => cue_stream!(f32),
        cpal::SampleFormat::I16 => cue_stream!(i16),
        cpal::SampleFormat::I32 => cue_stream!(i32),
        cpal::SampleFormat::U16 => cue_stream!(u16),
        f => return Err(HostError::Stream(format!("unsupported sample format {f}"))),
    }
    .map_err(|e| HostError::Stream(e.to_string()))?;
    stream.play().map_err(|e| HostError::Stream(e.to_string()))?;
    Ok((stream, label))
}

fn start_null(
    sample_rate: u32,
    block: usize,
    speed: f64,
    capture: Option<Arc<Mutex<Vec<f32>>>>,
    ports: MixerPorts,
    quality: StretchQuality,
    stop: Arc<AtomicBool>,
) -> Result<(Keep, StreamInfo), HostError> {
    let mut mixer = Mixer::new(sample_rate as f64, ports, quality);
    let block = block.clamp(32, MAX_BLOCK);
    let handle = std::thread::Builder::new()
        .name("bc-null-out".into())
        .spawn(move || {
            let mut buf = vec![0.0f32; block * 2];
            let start = std::time::Instant::now();
            let mut rendered: u64 = 0;
            while !stop.load(Ordering::Relaxed) {
                if speed > 0.0 {
                    // pace to wall clock * speed
                    let due = rendered as f64 / sample_rate as f64 / speed;
                    let now = start.elapsed().as_secs_f64();
                    if due > now + 0.001 {
                        std::thread::sleep(Duration::from_secs_f64((due - now).min(0.01)));
                        continue;
                    }
                } else {
                    std::thread::sleep(Duration::from_micros(200));
                }
                mixer.render(&mut buf, block, unix_ns());
                if let Some(c) = &capture {
                    c.lock().extend_from_slice(&buf);
                }
                rendered += block as u64;
            }
        })
        .map_err(|e| HostError::Stream(e.to_string()))?;
    let info = StreamInfo {
        backend: "null".into(),
        device: "null".into(),
        sample_rate,
        buffer_frames: block as u32,
        latency_ms: block as f64 * 1000.0 / sample_rate as f64,
        cue_device: None,
    };
    struct JoinOnDrop(Option<std::thread::JoinHandle<()>>);
    impl Drop for JoinOnDrop {
        fn drop(&mut self) {
            if let Some(h) = self.0.take() {
                let _ = h.join();
            }
        }
    }
    Ok((Box::new(JoinOnDrop(Some(handle))), info))
}
