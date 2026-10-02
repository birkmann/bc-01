//! `bc-worklet`: `bc-dsp` compiled to wasm for a browser AudioWorklet.
//!
//! A plain C ABI (no wasm-bindgen), so the module can be instantiated inside
//! `AudioWorkletGlobalScope`, which has no `TextDecoder`/`fetch` glue. The JS
//! side (`js/processor.js`, `js/player.js`) owns decoding (WebCodecs
//! `AudioDecoder` or `decodeAudioData`) and feeds PCM through
//! `bcw_scratch` + `bcw_push_pcm`; a SharedArrayBuffer ring carries it from
//! the main thread when cross-origin isolated, `postMessage` otherwise.
//!
//! Every function takes the `Host` pointer from `bcw_new`. Positions and frame
//! counts are `f64` (exact below 2^53). Events and the snapshot are read as
//! flat `f64` arrays so JS needs no struct layout knowledge.

use bc_dsp::beatgrid::BeatGrid;
use bc_dsp::beatmatch::{BlendSpec, Curve, EchoSpec, SyncSpec};
use bc_dsp::deck::{CHUNK_FRAMES, Chunk};
use bc_dsp::mixer::{Cmd, Event, MAX_BLOCK, Mixer, MixerPorts};
use bc_dsp::shared::{DECKS, SharedState};
use bc_dsp::stretch::StretchQuality;
use bc_types::player::{Quantise, TransitionKind};
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::Arc;

/// Frames of PCM the JS side may stage in one `bcw_push_pcm` call.
pub const SCRATCH_FRAMES: usize = 16_384;
const RING_CHUNKS: usize = 192;

pub struct Host {
    mixer: Mixer,
    cmd: Producer<Cmd>,
    ev: Consumer<Event>,
    prods: [Producer<Chunk>; DECKS],
    shared: Arc<SharedState>,
    scratch: Vec<f32>,
    out: Vec<f32>,
    snap: [f64; SNAP_LEN],
    sr: f32,
}

/// Length of the flat snapshot array: 8 header values + 7 per deck.
pub const SNAP_LEN: usize = 8 + DECKS * 7;

fn host<'a>(h: *mut Host) -> &'a mut Host {
    // SAFETY: the pointer comes from `bcw_new` and is used from a single thread (the worklet).
    unsafe { &mut *h }
}

fn quality_of(q: u32) -> StretchQuality {
    match q {
        0 => StretchQuality::Fast,
        2 => StretchQuality::High,
        _ => StretchQuality::Normal,
    }
}

fn kind_of(k: u32) -> TransitionKind {
    match k {
        1 => TransitionKind::BassSwap,
        2 => TransitionKind::Filter,
        3 => TransitionKind::EchoOut,
        4 => TransitionKind::Cut,
        5 => TransitionKind::EqBlend,
        _ => TransitionKind::Blend,
    }
}

fn quantise_of(q: u32) -> Quantise {
    match q {
        1 => Quantise::Beat,
        2 => Quantise::Bar,
        3 => Quantise::Phrase,
        _ => Quantise::Off,
    }
}

/// Create the mixer. `quality`: 0 fast, 1 normal, 2 high (key lock FFT size).
#[unsafe(no_mangle)]
pub extern "C" fn bcw_new(sample_rate: f32, quality: u32) -> *mut Host {
    let (p0, c0) = RingBuffer::<Chunk>::new(RING_CHUNKS);
    let (p1, c1) = RingBuffer::<Chunk>::new(RING_CHUNKS);
    let (p2, c2) = RingBuffer::<Chunk>::new(16);
    let (cmd, cmd_rx) = RingBuffer::<Cmd>::new(512);
    let (ev_tx, ev) = RingBuffer::<Event>::new(512);
    let shared = Arc::new(SharedState::new());
    let mixer = Mixer::new(
        sample_rate as f64,
        MixerPorts { rings: [c0, c1, c2], cmds: cmd_rx, events: ev_tx, shared: shared.clone() },
        quality_of(quality),
    );
    Box::into_raw(Box::new(Host {
        mixer,
        cmd,
        ev,
        prods: [p0, p1, p2],
        shared,
        scratch: vec![0.0; SCRATCH_FRAMES * 2],
        out: vec![0.0; MAX_BLOCK * 2],
        snap: [0.0; SNAP_LEN],
        sr: sample_rate,
    }))
}

#[unsafe(no_mangle)]
/// # Safety
/// `h` must come from `bcw_new` and not be used afterwards.
pub unsafe extern "C" fn bcw_free(h: *mut Host) {
    if !h.is_null() {
        // SAFETY: allocated by `bcw_new`.
        drop(unsafe { Box::from_raw(h) });
    }
}

/// Pointer to the interleaved-f32 staging buffer JS writes decoded PCM into.
#[unsafe(no_mangle)]
pub extern "C" fn bcw_scratch(h: *mut Host) -> *mut f32 {
    host(h).scratch.as_mut_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn bcw_scratch_frames() -> u32 {
    SCRATCH_FRAMES as u32
}

/// Pointer to the render output buffer (`bcw_render` fills it, interleaved).
#[unsafe(no_mangle)]
pub extern "C" fn bcw_out(h: *mut Host) -> *mut f32 {
    host(h).out.as_mut_ptr()
}

/// Frames the deck's ring can accept right now.
#[unsafe(no_mangle)]
pub extern "C" fn bcw_ring_free_frames(h: *mut Host, deck: u32) -> u32 {
    let p = &host(h).prods[(deck as usize).min(DECKS - 1)];
    (p.slots() * CHUNK_FRAMES) as u32
}

/// Move `frames` interleaved frames from the scratch buffer into the deck's
/// ring, tagged with `epoch` and starting at track frame `start_frame`.
/// Returns the number of frames accepted (less than `frames` when the ring is
/// full; the caller keeps the rest and retries). `last` marks end of track on
/// the final chunk of a track.
#[unsafe(no_mangle)]
pub extern "C" fn bcw_push_pcm(h: *mut Host, deck: u32, epoch: u32, start_frame: f64, frames: u32, last: u32) -> u32 {
    let ho = host(h);
    let d = (deck as usize).min(DECKS - 1);
    let frames = (frames as usize).min(SCRATCH_FRAMES);
    let mut pushed = 0usize;
    while pushed < frames || (frames == 0 && last != 0) {
        let n = (frames - pushed).min(CHUNK_FRAMES);
        if ho.prods[d].is_full() {
            break;
        }
        let mut c = Chunk::empty();
        c.epoch = epoch;
        c.start_frame = start_frame as u64 + pushed as u64;
        c.frames = n as u32;
        c.data[..n * 2].copy_from_slice(&ho.scratch[pushed * 2..(pushed + n) * 2]);
        pushed += n;
        c.last = last != 0 && pushed >= frames;
        let done = c.last;
        if ho.prods[d].push(c).is_err() {
            pushed -= n;
            break;
        }
        if frames == 0 || done {
            break;
        }
    }
    pushed as u32
}

/// Render `frames` (<= 8192) interleaved stereo frames into `bcw_out`.
#[unsafe(no_mangle)]
pub extern "C" fn bcw_render(h: *mut Host, frames: u32, output_ts_ns: f64) {
    let ho = host(h);
    let n = (frames as usize).min(MAX_BLOCK);
    ho.mixer.render(&mut ho.out[..n * 2], n, output_ts_ns as u64);
}

fn send(h: *mut Host, c: Cmd) -> u32 {
    host(h).cmd.push(c).is_ok() as u32
}

fn grid_of(origin_s: f64, period_s: f64, confidence: f64, downbeat: u32) -> Option<BeatGrid> {
    (period_s > 0.0).then_some(BeatGrid { origin_s, period_s, confidence, downbeat_beat: downbeat })
}

/// Prime `deck` at `frame` (decoder PCM of `epoch` follows). `period_s <= 0`: no beat grid.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn bcw_cue(
    h: *mut Host, deck: u32, epoch: u32, frame: f64, len_frames: f64, rate: f64, keylock: u32, trim: f64,
    grid_origin_s: f64, grid_period_s: f64, grid_confidence: f64, downbeat: u32,
) -> u32 {
    send(
        h,
        Cmd::Cue {
            deck: deck as u8,
            epoch,
            frame: frame as u64,
            len_frames: len_frames as u64,
            rate,
            keylock: keylock != 0,
            trim,
            grid: grid_of(grid_origin_s, grid_period_s, grid_confidence, downbeat),
        },
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn bcw_play(h: *mut Host, deck: u32) -> u32 {
    send(h, Cmd::Play { deck: deck as u8 })
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_pause(h: *mut Host) -> u32 {
    send(h, Cmd::Pause)
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_resume(h: *mut Host) -> u32 {
    send(h, Cmd::Resume)
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_stop(h: *mut Host) -> u32 {
    send(h, Cmd::Stop)
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_seek(h: *mut Host, deck: u32, epoch: u32, frame: f64) -> u32 {
    send(h, Cmd::Seek { deck: deck as u8, epoch, frame: frame as u64 })
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_set_rate(h: *mut Host, deck: u32, rate: f64, keylock: u32, glide_s: f64) -> u32 {
    send(h, Cmd::SetRate { deck: deck as u8, rate, keylock: keylock != 0, glide_s })
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_volume(h: *mut Host, gain: f64) -> u32 {
    send(h, Cmd::SetVolume { gain })
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_set_quality(h: *mut Host, quality: u32) -> u32 {
    send(h, Cmd::SetQuality(quality.min(2) as u8))
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_gapless(h: *mut Host, next_deck: i32) -> u32 {
    send(h, Cmd::ArmGapless { next: (next_deck >= 0).then_some(next_deck as u8) })
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_strip(h: *mut Host, low: f64, mid: f64, high: f64, filter: f64, echo_send: f64) -> u32 {
    send(h, Cmd::SetStrip { low, mid, high, filter, echo_send })
}

/// Blend `inc` into `out`. `kind`: 0 blend, 1 bass swap, 2 filter, 3 echo-out, 4 cut, 5 EQ blend.
/// `quantise`: 0 off, 1 beat, 2 bar, 3 phrase. `sync_rate <= 0`: no tempo match.
/// `echo_send <= 0`: no echo.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn bcw_transition(
    h: *mut Host, out: u32, inc: u32, kind: u32, length_s: f64, equal_power: u32, echo_send: f64, echo_hold_s: f64,
    echo_up_s: f64, echo_bpm: f64, sync_rate: f64, sync_keylock: u32, from_bpm: f64, to_bpm: f64, park_tail_s: f64,
    quantise: u32, phase_lock: u32,
) -> u32 {
    let spec = BlendSpec {
        kind: kind_of(kind),
        length_s,
        curve: if equal_power != 0 { Curve::EqualPower } else { Curve::Linear },
        incoming_start_s: 0.0,
        echo: (echo_send > 0.0).then_some(EchoSpec {
            send: echo_send,
            hold_s: echo_hold_s,
            up_s: echo_up_s,
            bpm: (echo_bpm > 0.0).then_some(echo_bpm),
        }),
        sync: (sync_rate > 0.0).then_some(SyncSpec {
            rate: sync_rate,
            key_lock: sync_keylock != 0,
            from_bpm,
            to_bpm,
            hold: false,
            out_rate: 0.0,
            glide_s: bc_dsp::beatmatch::GLIDE_BACK_S,
        }),
        park_tail_s,
        quantise: quantise_of(quantise),
        phase_lock: phase_lock != 0,
        swap_s: None,
    };
    send(h, Cmd::StartTransition { out: out as u8, inc: inc as u8, spec })
}

#[unsafe(no_mangle)]
pub extern "C" fn bcw_retime(h: *mut Host, remaining_s: f64) -> u32 {
    send(h, Cmd::Retime { remaining_s })
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_cut_now(h: *mut Host) -> u32 {
    send(h, Cmd::CutNow)
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_set_echo(h: *mut Host, on: u32) -> u32 {
    send(h, Cmd::SetEcho { on: on != 0 })
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_set_sync(h: *mut Host, on: u32) -> u32 {
    send(h, Cmd::SetSync { on: on != 0 })
}
#[unsafe(no_mangle)]
pub extern "C" fn bcw_nudge(h: *mut Host, delta_s: f64) -> u32 {
    send(h, Cmd::Nudge { delta_s })
}

/// Pop one event into `out[0..6]` as `[code, a, b, c, d, e]`; returns 0 when none.
/// Codes: 1 Ready{deck,epoch}, 2 Started{deck,frame}, 3 Ended{deck,epoch}, 4 Advanced{from,to,frame},
/// 5 Underrun{deck,frame}, 6 TransitionStarted{out,inc,start_frame,length_s,phase}, 7 TransitionRetimed{end_t},
/// 8 FadeDone, 9 Parked{deck}, 10 Paused, 11 Resumed.
#[unsafe(no_mangle)]
/// # Safety
/// `out` must point to at least 6 writable `f64` in wasm memory.
pub unsafe extern "C" fn bcw_poll_event(h: *mut Host, out: *mut f64) -> u32 {
    let Ok(e) = host(h).ev.pop() else { return 0 };
    let v: [f64; 6] = match e {
        Event::Ready { deck, epoch } => [1.0, deck as f64, epoch as f64, 0.0, 0.0, 0.0],
        Event::Started { deck, frame } => [2.0, deck as f64, frame as f64, 0.0, 0.0, 0.0],
        Event::Ended { deck, epoch } => [3.0, deck as f64, epoch as f64, 0.0, 0.0, 0.0],
        Event::Advanced { from, to, frame } => [4.0, from as f64, to as f64, frame as f64, 0.0, 0.0],
        Event::Underrun { deck, frame } => [5.0, deck as f64, frame as f64, 0.0, 0.0, 0.0],
        Event::TransitionStarted { out, inc, start_frame, length_s, phase } => {
            [6.0, out as f64, inc as f64, start_frame as f64, length_s, phase as f64]
        }
        Event::TransitionRetimed { end_t } => [7.0, end_t, 0.0, 0.0, 0.0, 0.0],
        Event::FadeDone => [8.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        Event::Parked { deck } => [9.0, deck as f64, 0.0, 0.0, 0.0, 0.0],
        Event::Paused => [10.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        Event::Resumed => [11.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    };
    // SAFETY: JS passes a pointer to at least 6 f64 in wasm memory.
    unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), out, 6) };
    1
}

/// Flat snapshot: `[frames_played, sample_rate, active, paused, transitioning, xruns, peak_l, peak_r,
/// then per deck (A, B, preview): pos_frames, rate, state, ready, epoch, buffered_s, len_frames]`.
#[unsafe(no_mangle)]
pub extern "C" fn bcw_snapshot(h: *mut Host) -> *const f64 {
    let ho = host(h);
    let s = ho.shared.load();
    let a = &mut ho.snap;
    a[0] = s.frames_played as f64;
    a[1] = ho.sr as f64;
    a[2] = s.active as f64;
    a[3] = s.paused as u32 as f64;
    a[4] = s.transitioning as u32 as f64;
    a[5] = s.xruns as f64;
    a[6] = s.peak_l as f64;
    a[7] = s.peak_r as f64;
    for (i, d) in s.decks.iter().enumerate() {
        let b = 8 + i * 7;
        a[b] = d.pos_frames;
        a[b + 1] = d.rate;
        a[b + 2] = d.state as f64;
        a[b + 3] = d.ready as u32 as f64;
        a[b + 4] = d.epoch as f64;
        a[b + 5] = d.buffered_s;
        a[b + 6] = d.len_frames as f64;
    }
    ho.snap.as_ptr()
}

/// Allocate `n` bytes in wasm memory (for JS-side staging); free with `bcw_dealloc`.
#[unsafe(no_mangle)]
pub extern "C" fn bcw_alloc(n: usize) -> *mut u8 {
    let mut v = Vec::<u8>::with_capacity(n);
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p
}

#[unsafe(no_mangle)]
/// # Safety
/// `p`/`n` must be exactly what `bcw_alloc(n)` returned.
pub unsafe extern "C" fn bcw_dealloc(p: *mut u8, n: usize) {
    // SAFETY: matches `bcw_alloc`.
    drop(unsafe { Vec::from_raw_parts(p, 0, n) });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_renders_a_pushed_tone() {
        let h = bcw_new(48_000.0, 1);
        let sc = bcw_scratch(h);
        let frames = 8192usize;
        // SAFETY: scratch has SCRATCH_FRAMES * 2 floats.
        let buf = unsafe { std::slice::from_raw_parts_mut(sc, frames * 2) };
        for i in 0..frames {
            let v = (i as f32 * 0.05).sin() * 0.5;
            buf[i * 2] = v;
            buf[i * 2 + 1] = v;
        }
        assert_eq!(bcw_cue(h, 0, 1, 0.0, frames as f64, 1.0, 0, 1.0, 0.0, 0.0, 0.0, 0), 1);
        assert_eq!(bcw_push_pcm(h, 0, 1, 0.0, frames as u32, 1), frames as u32);
        assert_eq!(bcw_play(h, 0), 1);
        let mut peak = 0f32;
        for _ in 0..40 {
            bcw_render(h, 128, 0.0);
            let out = unsafe { std::slice::from_raw_parts(bcw_out(h), 256) };
            peak = out.iter().fold(peak, |p, x| p.max(x.abs()));
        }
        assert!(peak > 0.3, "peak {peak}");
        let snap = unsafe { std::slice::from_raw_parts(bcw_snapshot(h), SNAP_LEN) };
        // published at the start of the last callback: 39 blocks of 128 had been rendered
        assert!(snap[0] >= 4992.0, "frames_played {}", snap[0]);
        unsafe { bcw_free(h) };
    }
}
