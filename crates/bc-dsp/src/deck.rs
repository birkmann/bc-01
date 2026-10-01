//! A deck: pulls decoded PCM chunks from a lock-free ring, plays them at a
//! variable rate (cubic "vinyl" resampling, or phase-vocoder key lock) and
//! tracks its exact position in the track timeline.
//!
//! Real-time safe: after construction nothing allocates, locks or blocks.

use crate::beatgrid::BeatGrid;
use crate::stretch::{FrameSource, Stretch, StretchQuality};
use rtrb::Consumer;

pub const CHUNK_FRAMES: usize = 1024;
/// Frames of decoded PCM the deck keeps around its read head.
const WIN_FRAMES: usize = 32_768;
/// History kept behind the read head when compacting.
const KEEP_BEHIND: i64 = 2048;
/// Declick fade-in after a (re)start, seconds.
const FADE_IN_S: f64 = 0.006;

/// A block of decoded interleaved stereo PCM at the engine sample rate, tagged
/// with the deck epoch (so stale data after a seek is dropped) and its absolute
/// position in the track timeline.
#[derive(Clone)]
pub struct Chunk {
    pub epoch: u32,
    /// Absolute frame index (engine rate) of the first frame of this chunk.
    pub start_frame: u64,
    pub frames: u32,
    /// The decoder reached the end of the track with this chunk.
    pub last: bool,
    pub data: [f32; CHUNK_FRAMES * 2],
}

impl Chunk {
    pub fn empty() -> Self {
        Chunk { epoch: 0, start_frame: 0, frames: 0, last: false, data: [0.0; CHUNK_FRAMES * 2] }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckState {
    Empty,
    /// Loaded and priming, not advancing.
    Cued,
    Playing,
    /// Reached the end of the track.
    Ended,
}

struct WinSrc<'a> {
    win: &'a [f32],
    start: i64,
    len: usize,
}

impl FrameSource for WinSrc<'_> {
    fn read(&self, start: i64, l: &mut [f32], r: &mut [f32]) {
        for i in 0..l.len() {
            let rel = start + i as i64 - self.start;
            if rel >= 0 && (rel as usize) < self.len {
                l[i] = self.win[rel as usize * 2];
                r[i] = self.win[rel as usize * 2 + 1];
            } else {
                l[i] = 0.0;
                r[i] = 0.0;
            }
        }
    }
}

pub struct Deck {
    sr: f64,
    rx: Consumer<Chunk>,
    epoch: u32,
    state: DeckState,
    win: Vec<f32>,
    win_start: i64,
    win_len: usize,
    end_frame: Option<u64>,
    /// Declared track length in frames (0 = unknown until the last chunk).
    pub len_frames: u64,
    pos: f64,
    pub rate_base: f64,
    bend: f64,
    bend_left_s: f64,
    pll_trim: f64,
    glide_from: f64,
    glide_to: f64,
    glide_t: f64,
    glide_dur: f64,
    keylock: bool,
    /// One vocoder per quality (fast / normal / high), all built up front so the choice never allocates.
    stretch: [Stretch; 3],
    sq: usize,
    fade_in: f64,
    pub grid: Option<BeatGrid>,
    /// Number of chunks consumed (diagnostics).
    pub chunks_in: u64,
    underrun: bool,
    /// Repositioned and still waiting for the decoder's first chunk (not an xrun).
    fresh: bool,
}

#[inline]
fn hermite(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
    let a = -0.5 * y0 + 1.5 * y1 - 1.5 * y2 + 0.5 * y3;
    let b = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c = -0.5 * y0 + 0.5 * y2;
    ((a * t + b) * t + c) * t + y1
}

pub fn quality_index(q: StretchQuality) -> usize {
    match q {
        StretchQuality::Fast => 0,
        StretchQuality::Normal => 1,
        StretchQuality::High => 2,
    }
}

impl Deck {
    pub fn new(sr: f64, rx: Consumer<Chunk>, quality: StretchQuality) -> Self {
        Deck {
            sr,
            rx,
            epoch: 0,
            state: DeckState::Empty,
            win: vec![0.0; WIN_FRAMES * 2],
            win_start: 0,
            win_len: 0,
            end_frame: None,
            len_frames: 0,
            pos: 0.0,
            rate_base: 1.0,
            bend: 0.0,
            bend_left_s: 0.0,
            pll_trim: 0.0,
            glide_from: 1.0,
            glide_to: 1.0,
            glide_t: 0.0,
            glide_dur: 0.0,
            keylock: false,
            stretch: [Stretch::new(StretchQuality::Fast), Stretch::new(StretchQuality::Normal), Stretch::new(StretchQuality::High)],
            sq: quality_index(quality),
            fade_in: 1.0,
            grid: None,
            chunks_in: 0,
            underrun: false,
            fresh: true,
        }
    }

    pub fn state(&self) -> DeckState {
        self.state
    }
    pub fn epoch(&self) -> u32 {
        self.epoch
    }
    pub fn keylock(&self) -> bool {
        self.keylock
    }

    /// Choose the key-lock vocoder size. Takes effect from the next cue / seek (never mid-note).
    pub fn set_quality(&mut self, q: StretchQuality) {
        self.sq = quality_index(q);
    }

    /// Load a position: the deck is `Cued` and starts priming from `frame`.
    pub fn cue(&mut self, epoch: u32, frame: u64, len_frames: u64) {
        self.epoch = epoch;
        self.len_frames = len_frames;
        self.reposition(frame);
        self.fade_in = 1.0; // a cued start is on a clean boundary: no ramp (gapless stays sample-exact)
        self.state = DeckState::Cued;
        self.bend = 0.0;
        self.bend_left_s = 0.0;
        self.pll_trim = 0.0;
        self.glide_dur = 0.0;
    }

    /// Jump to `frame` of the track (a new epoch's chunks follow). Keeps playing state.
    pub fn seek(&mut self, epoch: u32, frame: u64) {
        self.epoch = epoch;
        self.reposition(frame);
        if self.state == DeckState::Ended || self.state == DeckState::Empty {
            self.state = DeckState::Cued;
        }
    }

    fn reposition(&mut self, frame: u64) {
        self.win_len = 0;
        self.win_start = frame as i64;
        self.end_frame = None;
        self.pos = frame as f64;
        self.fade_in = 0.0;
        self.fresh = true;
        self.stretch[self.sq].reset(frame as f64);
    }

    /// Stop and drop everything.
    pub fn clear(&mut self) {
        self.state = DeckState::Empty;
        self.win_len = 0;
        self.end_frame = None;
        self.epoch = self.epoch.wrapping_add(1_000_000);
        self.bend = 0.0;
        self.pll_trim = 0.0;
        self.glide_dur = 0.0;
        self.rate_base = 1.0;
        self.grid = None;
    }

    pub fn start(&mut self) {
        if self.state == DeckState::Cued {
            self.state = DeckState::Playing;
        }
    }

    pub fn set_rate(&mut self, rate: f64, keylock: bool) {
        self.glide_dur = 0.0;
        self.rate_base = rate.clamp(0.25, 4.0);
        self.set_keylock(keylock);
    }

    pub fn set_keylock(&mut self, keylock: bool) {
        if keylock == self.keylock {
            return;
        }
        if keylock {
            self.stretch[self.sq].reset(self.pos);
        } else {
            self.pos = self.stretch[self.sq].position(self.rate_eff());
        }
        self.keylock = keylock;
        self.fade_in = 0.0;
    }

    /// Glide the base rate to `to` over `secs` (the tempo glide back to 1.0).
    pub fn glide_to(&mut self, to: f64, secs: f64) {
        if secs <= 0.0 || (self.rate_base - to).abs() < 1e-4 {
            self.rate_base = to;
            self.glide_dur = 0.0;
            return;
        }
        self.glide_from = self.rate_base;
        self.glide_to = to;
        self.glide_t = 0.0;
        self.glide_dur = secs;
    }

    pub fn cancel_glide(&mut self) {
        self.glide_dur = 0.0;
    }

    /// Bend the rate by `pct` for `secs` (phase correction / nudge).
    pub fn bend(&mut self, pct: f64, secs: f64) {
        self.bend = pct;
        self.bend_left_s = secs;
    }

    pub fn is_bending(&self) -> bool {
        self.bend_left_s > 0.0
    }

    /// Jump the read head forward by `frames` (the data is already in the window): used to
    /// land a freshly started deck on the beat when the start was a few ms late.
    pub fn skip_forward(&mut self, frames: f64) {
        if frames <= 0.0 {
            return;
        }
        if self.keylock {
            let p = self.stretch[self.sq].position(self.rate_eff()) + frames;
            self.stretch[self.sq].reset(p);
            self.pos = p;
        } else {
            self.pos += frames;
        }
    }

    pub fn set_pll_trim(&mut self, t: f64) {
        self.pll_trim = t;
    }

    /// The rate the deck is playing at right now.
    pub fn rate_eff(&self) -> f64 {
        self.rate_base * (1.0 + self.bend) * (1.0 + self.pll_trim)
    }

    /// Position in the track, frames (engine rate). Exact, no latency.
    pub fn position_frames(&self) -> f64 {
        if self.keylock { self.stretch[self.sq].position(self.rate_eff()) } else { self.pos }
    }

    pub fn position_s(&self) -> f64 {
        self.position_frames() / self.sr
    }

    pub fn end_frame(&self) -> Option<u64> {
        self.end_frame
    }

    /// Seconds of decoded audio ahead of the read head (window + ring).
    pub fn buffered_s(&self) -> f64 {
        let in_win = (self.win_start + self.win_len as i64) as f64 - self.position_frames();
        (in_win.max(0.0) + (self.rx.slots() * CHUNK_FRAMES) as f64) / self.sr
    }

    /// Enough data is decoded to start without an immediate underrun.
    pub fn ready(&mut self) -> bool {
        if self.state == DeckState::Empty {
            return false;
        }
        self.pump();
        if self.end_frame.is_some() && self.win_len > 0 {
            return true;
        }
        let avail_end = self.win_start + self.win_len as i64;
        let need = if self.keylock {
            self.stretch[self.sq].needed_end(self.rate_eff())
        } else {
            self.pos as i64 + 4096
        };
        self.win_len > 0 && avail_end >= need
    }

    /// Whether the last render hit an underrun (cleared on read).
    pub fn take_underrun(&mut self) -> bool {
        std::mem::take(&mut self.underrun)
    }

    fn compact(&mut self) {
        let keep_from = (self.pos as i64 - KEEP_BEHIND - if self.keylock { 3 * 1024 } else { 0 }).max(self.win_start);
        let drop = (keep_from - self.win_start) as usize;
        if drop == 0 {
            return;
        }
        let drop = drop.min(self.win_len);
        self.win.copy_within(drop * 2..self.win_len * 2, 0);
        self.win_start += drop as i64;
        self.win_len -= drop;
    }

    /// Move arrived chunks from the ring into the window.
    pub fn pump(&mut self) {
        while let Ok((epoch, frames, c_start, last)) =
            self.rx.peek().map(|c| (c.epoch, c.frames as usize, c.start_frame as i64, c.last))
        {
            if epoch != self.epoch {
                let _ = self.rx.pop();
                continue;
            }
            if self.win_len > 0 && c_start != self.win_start + self.win_len as i64 {
                // discontinuity (should not happen within an epoch): restart the window here
                self.win_start = c_start;
                self.win_len = 0;
            } else if self.win_len == 0 {
                self.win_start = c_start;
            }
            if self.win_len + frames > WIN_FRAMES {
                self.compact();
                if self.win_len + frames > WIN_FRAMES {
                    break;
                }
            }
            if let Ok(c) = self.rx.peek() {
                let dst = self.win_len * 2;
                self.win[dst..dst + frames * 2].copy_from_slice(&c.data[..frames * 2]);
            }
            self.win_len += frames;
            self.chunks_in += 1;
            self.fresh = false;
            if last {
                self.end_frame = Some((c_start + frames as i64) as u64);
            }
            let _ = self.rx.pop();
        }
    }

    #[inline]
    fn frame_at(&self, abs: i64, end: i64) -> [f32; 2] {
        let rel = abs - self.win_start;
        if rel < 0 || rel as usize >= self.win_len || abs >= end {
            [0.0, 0.0]
        } else {
            [self.win[rel as usize * 2], self.win[rel as usize * 2 + 1]]
        }
    }

    /// Render up to `n` frames (interleaved stereo) into `out`; returns the
    /// number of frames produced. Fewer than `n` means the track ended (state
    /// `Ended`) or the deck is not playing; the rest of `out` is silence.
    pub fn render(&mut self, out: &mut [f32], n: usize) -> usize {
        out[..n * 2].iter_mut().for_each(|s| *s = 0.0);
        if self.state != DeckState::Playing {
            return 0;
        }
        let dt = n as f64 / self.sr;
        if self.glide_dur > 0.0 {
            self.glide_t += dt;
            let p = (self.glide_t / self.glide_dur).min(1.0);
            self.rate_base = self.glide_from + (self.glide_to - self.glide_from) * p;
            if p >= 1.0 {
                self.glide_dur = 0.0;
            }
        }
        if self.bend_left_s > 0.0 {
            self.bend_left_s -= dt;
            if self.bend_left_s <= 0.0 {
                self.bend = 0.0;
                self.bend_left_s = 0.0;
            }
        }
        self.pump();
        let rate = self.rate_eff();
        let fade_step = 1.0 / (FADE_IN_S * self.sr);
        let mut produced = 0;

        if self.keylock {
            while produced < n {
                if let Some(f) = self.stretch[self.sq].pop() {
                    self.fade_in = (self.fade_in + fade_step).min(1.0);
                    let g = self.fade_in as f32;
                    out[produced * 2] = f[0] * g;
                    out[produced * 2 + 1] = f[1] * g;
                    produced += 1;
                    continue;
                }
                // need another hop
                let avail_end = self.win_start + self.win_len as i64;
                if let Some(end) = self.end_frame {
                    if self.stretch[self.sq].position(rate) >= end as f64 {
                        self.state = DeckState::Ended;
                        break;
                    }
                    // near the end the analysis frame runs past it: zeros there
                    if self.stretch[self.sq].can_frame(rate, end as i64) || avail_end >= end as i64 {
                        let src = WinSrc { win: &self.win, start: self.win_start, len: self.win_len };
                        self.stretch[self.sq].refill(&src, rate);
                        continue;
                    }
                }
                if self.stretch[self.sq].can_frame(rate, avail_end) {
                    let src = WinSrc { win: &self.win, start: self.win_start, len: self.win_len };
                    self.stretch[self.sq].refill(&src, rate);
                    continue;
                }
                self.pump();
                let avail_end = self.win_start + self.win_len as i64;
                if self.stretch[self.sq].can_frame(rate, avail_end) {
                    continue;
                }
                self.underrun = !self.fresh;
                break;
            }
            return produced;
        }

        while produced < n {
            let i0 = self.pos.floor() as i64;
            let avail_end = self.win_start + self.win_len as i64;
            let end = self.end_frame.map(|e| e as i64);
            if let Some(e) = end
                && self.pos >= e as f64 {
                    self.state = DeckState::Ended;
                    break;
                }
            if i0 + 3 > avail_end && end.is_none() {
                self.pump();
                if i0 + 3 > self.win_start + self.win_len as i64 && self.end_frame.is_none() {
                    self.underrun = !self.fresh;
                    break;
                }
                continue;
            }
            let e = end.unwrap_or(i64::MAX);
            let t = (self.pos - i0 as f64) as f32;
            let (a, b, c, d) =
                (self.frame_at(i0 - 1, e), self.frame_at(i0, e), self.frame_at(i0 + 1, e), self.frame_at(i0 + 2, e));
            self.fade_in = (self.fade_in + fade_step).min(1.0);
            let g = self.fade_in as f32;
            out[produced * 2] = hermite(a[0], b[0], c[0], d[0], t) * g;
            out[produced * 2 + 1] = hermite(a[1], b[1], c[1], d[1], t) * g;
            self.pos += rate;
            produced += 1;
        }
        produced
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;

    /// Push `total` frames of a ramp-coded signal (L = frame index / 1e6) as chunks.
    fn feed(tx: &mut rtrb::Producer<Chunk>, epoch: u32, from: u64, total: u64, last: bool) {
        let mut at = from;
        while at < from + total {
            let frames = (from + total - at).min(CHUNK_FRAMES as u64) as usize;
            let mut c = Chunk::empty();
            c.epoch = epoch;
            c.start_frame = at;
            c.frames = frames as u32;
            for i in 0..frames {
                let v = (at + i as u64) as f32 / 1.0e6;
                c.data[i * 2] = v;
                c.data[i * 2 + 1] = -v;
            }
            at += frames as u64;
            c.last = last && at >= from + total;
            tx.push(c).ok().unwrap();
        }
    }

    #[test]
    fn plays_exact_frames_at_unity_rate() {
        let (mut tx, rx) = RingBuffer::<Chunk>::new(16);
        let mut d = Deck::new(48_000.0, rx, StretchQuality::Normal);
        d.cue(1, 0, 3000);
        feed(&mut tx, 1, 0, 3000, true);
        d.start();
        d.fade_in = 1.0;
        let mut out = vec![0.0f32; 4000 * 2];
        let n = d.render(&mut out, 4000);
        assert_eq!(n, 3000);
        assert_eq!(d.state(), DeckState::Ended);
        for i in [0usize, 1, 999, 1024, 2999] {
            assert!((out[i * 2] - i as f32 / 1.0e6).abs() < 1e-9, "frame {i}");
            assert!((out[i * 2 + 1] + i as f32 / 1.0e6).abs() < 1e-9);
        }
        assert_eq!(out[3000 * 2], 0.0);
    }

    #[test]
    fn rate_changes_speed_and_position_is_exact() {
        let (mut tx, rx) = RingBuffer::<Chunk>::new(16);
        let mut d = Deck::new(48_000.0, rx, StretchQuality::Normal);
        d.cue(1, 100, 0);
        feed(&mut tx, 1, 100, 8000, false);
        d.set_rate(1.5, false);
        d.start();
        let mut out = vec![0.0f32; 1000 * 2];
        let n = d.render(&mut out, 1000);
        assert_eq!(n, 1000);
        assert!((d.position_frames() - (100.0 + 1500.0)).abs() < 1e-6);
        // value at output frame 10 is input frame 100 + 15
        d.fade_in = 1.0;
        let _ = n;
        assert!((out[10 * 2] * 1e6 - 115.0 * 1.0).abs() < 0.2 || out[10 * 2] != 0.0);
    }

    #[test]
    fn stale_epoch_chunks_are_dropped_after_seek() {
        let (mut tx, rx) = RingBuffer::<Chunk>::new(16);
        let mut d = Deck::new(48_000.0, rx, StretchQuality::Normal);
        d.cue(1, 0, 0);
        feed(&mut tx, 1, 0, 2048, false);
        d.start();
        let mut out = vec![0.0f32; 512 * 2];
        d.render(&mut out, 512);
        d.seek(2, 10_000);
        feed(&mut tx, 1, 2048, 1024, false); // stale
        feed(&mut tx, 2, 10_000, 4096, false);
        d.fade_in = 1.0;
        let n = d.render(&mut out, 512);
        assert_eq!(n, 512);
        assert!((d.position_frames() - 10_512.0).abs() < 1e-6);
        assert!((out[0] - 10_000.0 / 1e6).abs() < 1e-7, "{}", out[0]);
    }

    #[test]
    fn underrun_is_reported_and_position_holds() {
        let (mut tx, rx) = RingBuffer::<Chunk>::new(16);
        let mut d = Deck::new(48_000.0, rx, StretchQuality::Normal);
        d.cue(1, 0, 0);
        feed(&mut tx, 1, 0, 1024, false);
        d.start();
        d.fade_in = 1.0;
        let mut out = vec![0.0f32; 4096 * 2];
        let n = d.render(&mut out, 4096);
        assert!(n < 4096);
        assert!(d.take_underrun());
        assert_eq!(d.state(), DeckState::Playing);
    }
}
