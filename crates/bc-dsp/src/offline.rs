//! A deterministic, synchronous driver for the mixer graph: decoded PCM is
//! fed into the deck rings between render blocks, so the graph runs faster
//! than real time and never underruns. Used by the offline set renderer, the
//! golden tests and the wasm host's self-test.

use crate::beatgrid::BeatGrid;
use crate::deck::{CHUNK_FRAMES, Chunk};
use crate::mixer::{Cmd, Event, Mixer, MixerPorts};
use crate::shared::{DECKS, SharedState};
use crate::stretch::StretchQuality;
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::Arc;

struct Feed {
    pcm: Arc<Vec<f32>>,
    /// Track-timeline frame of `pcm[0]`.
    base: u64,
    /// Next frame (relative to `base`) to push.
    next: usize,
    epoch: u32,
    done: bool,
}

pub struct OfflineRig {
    pub sr: u32,
    pub mixer: Mixer,
    cmd: Producer<Cmd>,
    ev: Consumer<Event>,
    prods: [Producer<Chunk>; DECKS],
    feeds: [Option<Feed>; DECKS],
    epoch: u32,
    pub shared: Arc<SharedState>,
    pub events: Vec<Event>,
    pub rendered: u64,
}

impl OfflineRig {
    pub fn new(sr: u32, quality: StretchQuality) -> Self {
        let (p0, c0) = RingBuffer::<Chunk>::new(64);
        let (p1, c1) = RingBuffer::<Chunk>::new(64);
        let (p2, c2) = RingBuffer::<Chunk>::new(8);
        let (cmd, cmd_rx) = RingBuffer::<Cmd>::new(256);
        let (ev_tx, ev) = RingBuffer::<Event>::new(1024);
        let shared = Arc::new(SharedState::new());
        let mixer = Mixer::new(
            sr as f64,
            MixerPorts { rings: [c0, c1, c2], cmds: cmd_rx, events: ev_tx, shared: shared.clone() },
            quality,
        );
        Self { sr, mixer, cmd, ev, prods: [p0, p1, p2], feeds: [None, None, None], epoch: 1, shared, events: vec![], rendered: 0 }
    }

    pub fn send(&mut self, c: Cmd) {
        let _ = self.cmd.push(c);
    }

    /// Cue `deck` on `pcm` (interleaved stereo at the engine rate; `base` is the
    /// track-timeline frame of `pcm[0]`), starting at track frame `start`.
    #[allow(clippy::too_many_arguments)]
    pub fn cue(&mut self, deck: usize, pcm: Arc<Vec<f32>>, base: u64, start: u64, rate: f64, keylock: bool, trim: f64, grid: Option<BeatGrid>) {
        self.epoch += 1;
        let total = (pcm.len() / 2) as u64;
        self.feeds[deck] = Some(Feed { pcm, base, next: start.saturating_sub(base) as usize, epoch: self.epoch, done: false });
        self.send(Cmd::Cue {
            deck: deck as u8,
            epoch: self.epoch,
            frame: start,
            len_frames: base + total,
            rate,
            keylock,
            trim,
            grid,
        });
    }

    /// Re-position a deck on the same PCM (a seek): a new epoch's chunks follow.
    pub fn reseek(&mut self, deck: usize, pcm: Arc<Vec<f32>>, epoch: u32, frame: u64) {
        self.feeds[deck] = Some(Feed { pcm, base: 0, next: frame as usize, epoch, done: false });
        self.send(Cmd::Seek { deck: deck as u8, epoch, frame });
    }

    fn top_up(&mut self) {
        for d in 0..DECKS {
            let Some(f) = self.feeds[d].as_mut() else { continue };
            while !f.done && !self.prods[d].is_full() {
                let total = f.pcm.len() / 2;
                let n = (total - f.next.min(total)).min(CHUNK_FRAMES);
                let mut c = Chunk::empty();
                c.epoch = f.epoch;
                c.start_frame = f.base + f.next as u64;
                c.frames = n as u32;
                c.data[..n * 2].copy_from_slice(&f.pcm[f.next * 2..(f.next + n) * 2]);
                f.next += n;
                if f.next >= total {
                    c.last = true;
                    f.done = true;
                }
                if self.prods[d].push(c).is_err() {
                    break;
                }
            }
        }
    }

    /// Render `frames` frames, appending interleaved stereo to `out`.
    pub fn render(&mut self, frames: usize, out: &mut Vec<f32>) {
        const BLOCK: usize = 256;
        let mut buf = vec![0.0f32; BLOCK * 2];
        let mut left = frames;
        while left > 0 {
            let n = left.min(BLOCK);
            self.top_up();
            self.mixer.render(&mut buf[..n * 2], n, 0);
            out.extend_from_slice(&buf[..n * 2]);
            self.rendered += n as u64;
            left -= n;
            while let Ok(e) = self.ev.pop() {
                self.events.push(e);
            }
        }
    }

    pub fn frames(&self) -> u64 {
        self.mixer.frames()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stereo tone buffer, `secs` long.
    pub fn tone(sr: u32, freq: f32, secs: f32, amp: f32) -> Arc<Vec<f32>> {
        let n = (sr as f32 * secs) as usize;
        let mut v = Vec::with_capacity(n * 2);
        for i in 0..n {
            let s = (2.0 * std::f32::consts::PI * freq * i as f32 / sr as f32).sin() * amp;
            v.push(s);
            v.push(s);
        }
        Arc::new(v)
    }

    #[test]
    fn plays_a_deck_to_its_exact_end() {
        let mut rig = OfflineRig::new(48_000, StretchQuality::Normal);
        let pcm = tone(48_000, 440.0, 1.0, 0.5);
        rig.cue(0, pcm, 0, 0, 1.0, false, 1.0, None);
        rig.send(Cmd::Play { deck: 0 });
        let mut out = vec![];
        rig.render(48_000 + 4000, &mut out);
        let lat = rig.mixer.latency_frames();
        // the deck starts within a few blocks of the Play command and ends exactly
        // one second of source later; the limiter delays everything by `lat`
        let last = (0..out.len() / 2).rev().find(|&i| out[i * 2].abs() > 0.05).unwrap_or(0);
        let first = (0..out.len() / 2).find(|&i| out[i * 2].abs() > 0.05).unwrap_or(0);
        let len = last - first;
        assert!((len as i64 - 48_000).abs() < 600, "played {len} frames (first {first}, last {last}, lat {lat})");
        assert!(first < 1200, "started late: {first}");
        assert!(rig.events.iter().any(|e| matches!(e, Event::Ended { .. })));
    }
}
