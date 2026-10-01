//! State published from the audio thread: a seqlock over atomics, so the
//! real-time thread never blocks and readers never see a torn snapshot.

use std::sync::atomic::{AtomicU64, Ordering};

pub const DECKS: usize = 3; // A, B, preview

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DeckSnap {
    pub pos_frames: f64,
    pub rate: f64,
    /// 0 empty, 1 cued, 2 playing, 3 ended
    pub state: u8,
    pub ready: bool,
    pub epoch: u32,
    pub buffered_s: f64,
    pub len_frames: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Snapshot {
    /// Frames produced before this callback; the first frame of the callback
    /// leaves the device at `output_ts_ns`.
    pub frames_played: u64,
    pub sample_rate: u32,
    pub output_ts_ns: u64,
    pub active: u8,
    pub paused: bool,
    pub transitioning: bool,
    pub xruns: u64,
    pub peak_l: f32,
    pub peak_r: f32,
    pub limiter_gr_db: f32,
    /// Beat-phase error of the incoming deck against the outgoing, ms (NaN: not measured).
    pub phase_err_ms: f32,
    pub decks: [DeckSnap; DECKS],
}

const PER_DECK: usize = 6;
const HEAD: usize = 9;
const N: usize = HEAD + DECKS * PER_DECK + 1;

pub struct SharedState {
    seq: AtomicU64,
    w: [AtomicU64; N],
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn fb(x: f64) -> u64 {
    x.to_bits()
}

impl SharedState {
    pub fn new() -> Self {
        Self { seq: AtomicU64::new(0), w: std::array::from_fn(|_| AtomicU64::new(0)) }
    }

    /// Writer side (single writer: the audio thread).
    pub fn store(&self, s: &Snapshot) {
        let seq = self.seq.load(Ordering::Relaxed);
        self.seq.store(seq + 1, Ordering::Release);
        let w = &self.w;
        w[0].store(s.frames_played, Ordering::Relaxed);
        w[1].store(s.sample_rate as u64, Ordering::Relaxed);
        w[2].store(s.output_ts_ns, Ordering::Relaxed);
        w[3].store(
            s.active as u64 | (s.paused as u64) << 8 | (s.transitioning as u64) << 9,
            Ordering::Relaxed,
        );
        w[4].store(s.xruns, Ordering::Relaxed);
        w[5].store(s.peak_l.to_bits() as u64, Ordering::Relaxed);
        w[6].store(s.peak_r.to_bits() as u64, Ordering::Relaxed);
        w[7].store(s.limiter_gr_db.to_bits() as u64, Ordering::Relaxed);
        w[8].store(s.phase_err_ms.to_bits() as u64, Ordering::Relaxed);
        for (i, d) in s.decks.iter().enumerate() {
            let b = HEAD + i * PER_DECK;
            w[b].store(fb(d.pos_frames), Ordering::Relaxed);
            w[b + 1].store(fb(d.rate), Ordering::Relaxed);
            w[b + 2].store(d.state as u64 | (d.ready as u64) << 8 | (d.epoch as u64) << 16, Ordering::Relaxed);
            w[b + 3].store(fb(d.buffered_s), Ordering::Relaxed);
            w[b + 4].store(d.len_frames, Ordering::Relaxed);
        }
        self.seq.store(seq + 2, Ordering::Release);
    }

    /// Reader side: retries until it gets a consistent snapshot.
    pub fn load(&self) -> Snapshot {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let w = &self.w;
            let mut s = Snapshot {
                frames_played: w[0].load(Ordering::Relaxed),
                sample_rate: w[1].load(Ordering::Relaxed) as u32,
                output_ts_ns: w[2].load(Ordering::Relaxed),
                xruns: w[4].load(Ordering::Relaxed),
                peak_l: f32::from_bits(w[5].load(Ordering::Relaxed) as u32),
                peak_r: f32::from_bits(w[6].load(Ordering::Relaxed) as u32),
                limiter_gr_db: f32::from_bits(w[7].load(Ordering::Relaxed) as u32),
                phase_err_ms: f32::from_bits(w[8].load(Ordering::Relaxed) as u32),
                ..Default::default()
            };
            let f = w[3].load(Ordering::Relaxed);
            s.active = (f & 0xff) as u8;
            s.paused = f >> 8 & 1 == 1;
            s.transitioning = f >> 9 & 1 == 1;
            for i in 0..DECKS {
                let b = HEAD + i * PER_DECK;
                let st = w[b + 2].load(Ordering::Relaxed);
                s.decks[i] = DeckSnap {
                    pos_frames: f64::from_bits(w[b].load(Ordering::Relaxed)),
                    rate: f64::from_bits(w[b + 1].load(Ordering::Relaxed)),
                    state: (st & 0xff) as u8,
                    ready: st >> 8 & 1 == 1,
                    epoch: (st >> 16) as u32,
                    buffered_s: f64::from_bits(w[b + 3].load(Ordering::Relaxed)),
                    len_frames: w[b + 4].load(Ordering::Relaxed),
                };
            }
            if self.seq.load(Ordering::Acquire) == s1 {
                return s;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let sh = SharedState::new();
        let mut s = Snapshot { frames_played: 123, sample_rate: 48_000, active: 1, paused: true, phase_err_ms: 1.5, ..Default::default() };
        s.decks[1] = DeckSnap { pos_frames: 9.5, rate: 1.02, state: 2, ready: true, epoch: 77, buffered_s: 3.0, len_frames: 5 };
        sh.store(&s);
        assert_eq!(sh.load(), s);
    }
}
