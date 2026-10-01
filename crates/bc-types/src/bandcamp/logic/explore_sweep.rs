//! Filling a play queue from a grid of Bandcamp releases, on demand.
//!
//! Ports the pure core of `exploreSweep.ts`. A card is only a URL; the tracks live on the release
//! page (one rate-limited scrape, ~1.5 s). Playback starts on the first release that streams and
//! the queue grows underneath it, but only enough to stay [`LOOKAHEAD`] tracks ahead of the
//! needle, so the shared rate limiter stays free. The sweep lives at app scope, not inside the
//! grid component: leaving a page is not a decision to stop listening.
//!
//! | TS | Rust |
//! | --- | --- |
//! | `LOOKAHEAD` / `MAX_RELEASES` / `POLL_MS` / `QUEUE_MAX` (queue.ts) | constants |
//! | `SweepProgress` | [`SweepProgress`] |
//! | module state `progress`/`note`/`run`, `stopSweep`, `stop(mine, why)`, `sweepState` | [`SweepTracker`] |
//! | `remaining()` | [`remaining`] |
//! | `shuffled` | [`shuffled`] (rng injected) |
//! | pick of releases in `startSweep` | [`pick_releases`] |
//! | `startSweep` loop | [`run_sweep`] over the [`SweepEnv`] trait (the UI implements it over the player store, `fetchCardQueue`, a timer and a signal) |
//!
//! The Explore grid's "Play" / "Shuffle" buttons call `run_sweep`; the Stop button calls
//! [`SweepTracker::stop_sweep`].

#![allow(async_fn_in_trait)]

/// How far ahead of the needle the queue is kept.
pub const LOOKAHEAD: usize = 12;
/// Releases considered in one sweep, however deep the grid goes.
pub const MAX_RELEASES: usize = 100;
/// How often the runner re-checks whether the queue has drained.
pub const POLL_MS: u64 = 2000;
/// Player queue cap (`QUEUE_MAX` in `lib/queue.ts`).
pub const QUEUE_MAX: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepProgress {
    pub done: usize,
    pub total: usize,
    pub tracks: usize,
    /// Paused because the queue is deep enough, rather than finished.
    pub idle: bool,
}

/// Tracks still ahead of the needle in the player's queue.
pub fn remaining(queue_len: usize, queue_index: usize) -> usize {
    queue_len.saturating_sub(queue_index).saturating_sub(1)
}

/// Fisher-Yates with injected randomness (`rng` yields `[0, 1)`).
pub fn shuffled<T: Clone>(list: &[T], mut rng: impl FnMut() -> f64) -> Vec<T> {
    let mut out = list.to_vec();
    shuffle_in_place(&mut out, &mut rng);
    out
}

pub fn shuffle_in_place<T>(list: &mut [T], mut rng: impl FnMut() -> f64) {
    for i in (1..list.len()).rev() {
        let j = ((rng() * (i + 1) as f64).floor().max(0.0) as usize).min(i);
        list.swap(i, j);
    }
}

/// The releases one sweep considers: the list (shuffled if asked), capped at [`MAX_RELEASES`].
pub fn pick_releases<T: Clone>(items: &[T], shuffle: bool, rng: impl FnMut() -> f64) -> Vec<T> {
    let mut v = if shuffle { shuffled(items, rng) } else { items.to_vec() };
    v.truncate(MAX_RELEASES);
    v
}

/// The observable state of the (single, app-wide) sweep and its cancellation token: the TS module
/// variables `progress`, `note` and `run`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SweepTracker {
    pub progress: Option<SweepProgress>,
    pub note: Option<String>,
    run: u64,
}

impl SweepTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a new sweep, superseding any running one. Returns its run id.
    pub fn begin(&mut self, total: usize) -> u64 {
        self.run += 1;
        self.note = None;
        self.progress = Some(SweepProgress { done: 0, total, tracks: 0, idle: false });
        self.run
    }

    /// `run += 1` without a new sweep: used by `begin` for the id and by the caller to supersede
    /// when the list is empty (nothing to start).
    pub fn supersede(&mut self) -> u64 {
        self.run += 1;
        self.run
    }

    pub fn is_current(&self, mine: u64) -> bool {
        self.run == mine
    }

    /// Stop queueing. What is already queued keeps playing.
    pub fn stop_sweep(&mut self) {
        self.run += 1;
        self.progress = None;
    }

    pub fn set_idle(&mut self, idle: bool) {
        if let Some(p) = &mut self.progress {
            p.idle = idle;
        }
    }

    pub fn set_progress(&mut self, done: usize, tracks: usize) {
        if let Some(p) = &mut self.progress {
            p.done = done;
            p.tracks = tracks;
            p.idle = false;
        }
    }

    /// Finish sweep `mine` with an optional note; no-op if it was superseded.
    pub fn stop(&mut self, mine: u64, why: Option<String>) {
        if self.run != mine {
            return;
        }
        self.progress = None;
        self.note = why;
    }
}

/// The side effects of a sweep, supplied by the UI.
pub trait SweepEnv {
    type Track;
    type Item;
    /// Fetch one release's playable tracks (`fetchCardQueue`); `Err` = unreadable release.
    async fn fetch(&self, item: &Self::Item) -> Result<Vec<Self::Track>, ()>;
    /// Tracks still ahead of the needle ([`remaining`] over the player store).
    fn remaining(&self) -> usize;
    /// `ownsQueue(built)`: the player's queue is still the one the sweep built.
    fn owns_queue(&self, built_len: usize) -> bool;
    /// `playQueue(tracks, 0)`.
    fn play_queue(&self, tracks: Vec<Self::Track>);
    /// `addToQueue(tracks)`.
    fn add_to_queue(&self, tracks: Vec<Self::Track>);
    fn set_shuffle(&self, shuffle: bool);
    fn shuffle_tracks(&self, tracks: &mut Vec<Self::Track>);
    fn random(&self) -> f64;
    async fn sleep(&self, ms: u64);
    /// Called after every tracker change (the TS `emit()`).
    fn emit(&self, tracker: &SweepTracker);
}

/// `startSweep`: play a grid, in order or shuffled, topping the queue up as it drains. Returns
/// once the sweep has nothing left to add (callers need not await it). `tracker` is the shared
/// state; it is only borrowed briefly (never across an await).
pub async fn run_sweep<E: SweepEnv>(
    env: &E,
    tracker: &std::cell::RefCell<SweepTracker>,
    items: &[E::Item],
    shuffle: bool,
) where
    E::Item: Clone,
{
    let mine = tracker.borrow_mut().supersede();
    let picked = pick_releases(items, shuffle, || env.random());
    if picked.is_empty() {
        return;
    }
    env.set_shuffle(shuffle);
    {
        let mut t = tracker.borrow_mut();
        t.note = None;
        t.progress = Some(SweepProgress { done: 0, total: picked.len(), tracks: 0, idle: false });
        env.emit(&t);
    }

    let mut built_len = 0usize;
    let mut started = false;
    let mut unreadable = 0usize;
    let current = || tracker.borrow().is_current(mine);
    let finish = |why: Option<String>| {
        let mut t = tracker.borrow_mut();
        t.stop(mine, why);
        env.emit(&t);
    };

    for (i, item) in picked.iter().enumerate() {
        // Wait out the part of the queue already fetched.
        while started && env.remaining() >= LOOKAHEAD {
            if !current() {
                return;
            }
            if !env.owns_queue(built_len) {
                return finish(None);
            }
            {
                let mut t = tracker.borrow_mut();
                if t.progress.is_some_and(|p| !p.idle) {
                    t.set_idle(true);
                    env.emit(&t);
                }
            }
            env.sleep(POLL_MS).await;
        }
        if !current() {
            return;
        }
        {
            let mut t = tracker.borrow_mut();
            if t.progress.is_some_and(|p| p.idle) {
                t.set_idle(false);
                env.emit(&t);
            }
        }

        let mut tracks = match env.fetch(item).await {
            Ok(t) => t,
            // One dead release page must not end the sweep; it is counted and reported at the end.
            Err(()) => {
                unreadable += 1;
                Vec::new()
            }
        };
        if !current() {
            return;
        }

        if !tracks.is_empty() {
            let n = tracks.len();
            if !started {
                // Shuffled means shuffled from the first note.
                if shuffle {
                    env.shuffle_tracks(&mut tracks);
                }
                built_len = n;
                env.play_queue(tracks);
                started = true;
            } else {
                if !env.owns_queue(built_len) {
                    return finish(None);
                }
                if shuffle {
                    env.shuffle_tracks(&mut tracks);
                }
                env.add_to_queue(tracks);
                built_len += n;
            }
            if built_len >= QUEUE_MAX {
                return finish(Some(format!("Queued {built_len} tracks.")));
            }
        }
        let mut t = tracker.borrow_mut();
        t.set_progress(i + 1, built_len);
        env.emit(&t);
    }

    if !started {
        return finish(Some("None of these releases stream from Bandcamp.".into()));
    }
    if unreadable > 0 {
        return finish(Some(format!(
            "Queued {built_len} tracks \u{b7} {unreadable} release(s) could not be read."
        )));
    }
    finish(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bandcamp::logic::testutil::block_on;
    use std::cell::{Cell, RefCell};

    #[test]
    fn remaining_counts_tracks_after_the_needle() {
        assert_eq!(remaining(10, 0), 9);
        assert_eq!(remaining(10, 9), 0);
        assert_eq!(remaining(0, 0), 0);
        assert_eq!(remaining(3, 7), 0);
    }

    #[test]
    fn shuffled_is_a_permutation_and_deterministic_for_a_given_rng() {
        let list: Vec<u32> = (0..20).collect();
        let mk = || {
            let mut s = 12345u64;
            move || {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (s >> 33) as f64 / (1u64 << 31) as f64
            }
        };
        let a = shuffled(&list, mk());
        let b = shuffled(&list, mk());
        assert_eq!(a, b);
        assert_ne!(a, list);
        let mut sorted = a.clone();
        sorted.sort();
        assert_eq!(sorted, list);
    }

    #[test]
    fn pick_releases_caps_at_max_and_keeps_order_when_not_shuffled() {
        let items: Vec<u32> = (0..250).collect();
        let picked = pick_releases(&items, false, || 0.0);
        assert_eq!(picked.len(), MAX_RELEASES);
        assert_eq!(picked[..3], [0, 1, 2]);
        assert!(pick_releases::<u32>(&[], true, || 0.0).is_empty());
    }

    #[test]
    fn tracker_ignores_a_superseded_sweep() {
        let mut t = SweepTracker::new();
        let a = t.begin(5);
        let b = t.begin(3);
        t.stop(a, Some("stale".into()));
        assert_eq!(t.progress.map(|p| p.total), Some(3));
        assert_eq!(t.note, None);
        t.stop(b, Some("done".into()));
        assert_eq!(t.progress, None);
        assert_eq!(t.note.as_deref(), Some("done"));
        let c = t.begin(1);
        t.stop_sweep();
        assert!(!t.is_current(c));
    }

    struct Env {
        releases: Vec<Option<usize>>, // tracks per release; None = unreadable
        fetched: Cell<usize>,
        queue_len: Cell<usize>,
        index: Cell<usize>,
        plays: Cell<usize>,
        sleeps: Cell<usize>,
    }
    impl Env {
        fn new(releases: Vec<Option<usize>>) -> Self {
            Env {
                releases,
                fetched: Cell::new(0),
                queue_len: Cell::new(0),
                index: Cell::new(0),
                plays: Cell::new(0),
                sleeps: Cell::new(0),
            }
        }
    }
    impl SweepEnv for Env {
        type Track = u32;
        type Item = usize;
        async fn fetch(&self, item: &usize) -> Result<Vec<u32>, ()> {
            self.fetched.set(self.fetched.get() + 1);
            self.releases[*item].map(|n| (0..n as u32).collect()).ok_or(())
        }
        fn remaining(&self) -> usize {
            remaining(self.queue_len.get(), self.index.get())
        }
        fn owns_queue(&self, built_len: usize) -> bool {
            self.queue_len.get() == built_len
        }
        fn play_queue(&self, tracks: Vec<u32>) {
            self.plays.set(self.plays.get() + 1);
            self.queue_len.set(tracks.len());
        }
        fn add_to_queue(&self, tracks: Vec<u32>) {
            self.queue_len.set(self.queue_len.get() + tracks.len());
        }
        fn set_shuffle(&self, _: bool) {}
        fn shuffle_tracks(&self, _: &mut Vec<u32>) {}
        fn random(&self) -> f64 {
            0.0
        }
        async fn sleep(&self, _ms: u64) {
            // The "player" advances while we wait.
            self.sleeps.set(self.sleeps.get() + 1);
            self.index.set(self.queue_len.get().saturating_sub(1));
        }
        fn emit(&self, _: &SweepTracker) {}
    }

    #[test]
    fn sweep_fetches_on_demand_and_reports_unreadable_releases() {
        // 5 tracks each: after 3 releases the queue is 15 deep (>= 12 ahead) so it must wait.
        let env = Env::new(vec![Some(5), None, Some(5), Some(5), Some(5), Some(5)]);
        let tracker = RefCell::new(SweepTracker::new());
        let items: Vec<usize> = (0..6).collect();
        block_on(run_sweep(&env, &tracker, &items, false));
        assert_eq!(env.plays.get(), 1);
        assert_eq!(env.fetched.get(), 6);
        assert!(env.sleeps.get() >= 1, "should have paused at the lookahead");
        let t = tracker.borrow();
        assert_eq!(t.progress, None);
        assert_eq!(
            t.note.as_deref(),
            Some("Queued 25 tracks \u{b7} 1 release(s) could not be read.")
        );
    }

    #[test]
    fn sweep_with_nothing_streamable_says_so() {
        let env = Env::new(vec![None, Some(0)]);
        let tracker = RefCell::new(SweepTracker::new());
        block_on(run_sweep(&env, &tracker, &[0usize, 1], false));
        assert_eq!(env.plays.get(), 0);
        assert_eq!(
            tracker.borrow().note.as_deref(),
            Some("None of these releases stream from Bandcamp.")
        );
    }

    #[test]
    fn sweep_stops_when_the_user_replaced_the_queue() {
        let env = Env::new(vec![Some(20), Some(5)]);
        let tracker = RefCell::new(SweepTracker::new());
        // After the first release the queue is 20 deep; sleep() moves the needle but we also
        // simulate the user replacing the queue by growing it behind the sweep's back.
        struct Hijack<'a>(&'a Env);
        impl SweepEnv for Hijack<'_> {
            type Track = u32;
            type Item = usize;
            async fn fetch(&self, i: &usize) -> Result<Vec<u32>, ()> {
                self.0.fetch(i).await
            }
            fn remaining(&self) -> usize {
                self.0.remaining()
            }
            fn owns_queue(&self, _built_len: usize) -> bool {
                false
            }
            fn play_queue(&self, t: Vec<u32>) {
                self.0.play_queue(t)
            }
            fn add_to_queue(&self, t: Vec<u32>) {
                self.0.add_to_queue(t)
            }
            fn set_shuffle(&self, s: bool) {
                self.0.set_shuffle(s)
            }
            fn shuffle_tracks(&self, t: &mut Vec<u32>) {
                self.0.shuffle_tracks(t)
            }
            fn random(&self) -> f64 {
                0.0
            }
            async fn sleep(&self, ms: u64) {
                self.0.sleep(ms).await
            }
            fn emit(&self, _: &SweepTracker) {}
        }
        block_on(run_sweep(&Hijack(&env), &tracker, &[0usize, 1], false));
        assert_eq!(env.fetched.get(), 1);
        assert_eq!(tracker.borrow().progress, None);
        assert_eq!(tracker.borrow().note, None);
    }
}
