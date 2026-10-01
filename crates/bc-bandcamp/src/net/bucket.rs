//! Async token bucket with adaptive backoff (port of `TokenBucket` in `net.py`).
//!
//! This replaces the gen-1 approach of opening four browser tabs and sleeping
//! 1.5-15 minutes between batches. That was a symptom of being throttled; a
//! steady polite rate avoids the throttling in the first place.
//!
//! Time comes from `tokio::time::Instant`, so tests can run on a paused clock.

use std::time::Duration;

use parking_lot::Mutex;
use tokio::time::Instant;

/// Legacy default sustained rate (requests per second).
pub const DEFAULT_RATE_PER_SEC: f64 = 0.67;
/// Legacy default burst.
pub const DEFAULT_BURST: u32 = 5;
/// Default sustained rate of the reserved lane (stream-URL pre-resolution).
pub const RESERVED_RATE_PER_SEC: f64 = 1.0;
/// Default burst of the reserved lane.
pub const RESERVED_BURST: u32 = 3;

#[derive(Debug)]
struct State {
    rate: f64,
    tokens: f64,
    updated: Instant,
    penalty_until: Option<Instant>,
    failures: u32,
    successes: u32,
}

/// Token bucket with penalty / gradual-recovery semantics.
#[derive(Debug)]
pub struct TokenBucket {
    base_rate: f64,
    burst: f64,
    state: Mutex<State>,
}

impl Default for TokenBucket {
    fn default() -> Self {
        Self::new(DEFAULT_RATE_PER_SEC, DEFAULT_BURST)
    }
}

impl TokenBucket {
    pub fn new(rate_per_sec: f64, burst: u32) -> Self {
        let rate = if rate_per_sec > 0.0 { rate_per_sec } else { DEFAULT_RATE_PER_SEC };
        let burst = burst.max(1) as f64;
        Self {
            base_rate: rate,
            burst,
            state: Mutex::new(State {
                rate,
                tokens: burst,
                updated: Instant::now(),
                penalty_until: None,
                failures: 0,
                successes: 0,
            }),
        }
    }

    /// Wait until `n` tokens are available (and no penalty is in force), then take them.
    pub async fn acquire(&self, n: u32) {
        let n = (n.max(1) as f64).min(self.burst);
        loop {
            let wait = {
                let mut s = self.state.lock();
                let now = Instant::now();
                match s.penalty_until {
                    Some(until) if now < until => (until - now).as_secs_f64(),
                    _ => {
                        s.penalty_until = None;
                        let add = now.saturating_duration_since(s.updated).as_secs_f64() * s.rate;
                        s.tokens = (s.tokens + add).min(self.burst);
                        s.updated = now;
                        if s.tokens >= n {
                            s.tokens -= n;
                            return;
                        }
                        (n - s.tokens) / s.rate
                    }
                }
            };
            tokio::time::sleep(Duration::from_secs_f64(wait.clamp(0.0, 30.0))).await;
        }
    }

    /// Called on 429/403. Drains the bucket and halves the sustained rate
    /// (floor: an eighth of the configured rate). With no explicit delay the
    /// penalty is `min(1800, 60 * 2^(failures-1))` seconds.
    pub fn penalise(&self, seconds: Option<f64>) {
        let mut s = self.state.lock();
        s.failures = s.failures.saturating_add(1);
        let delay = seconds.unwrap_or_else(|| {
            let exp = (s.failures - 1).min(20);
            (60.0 * f64::from(1u32 << exp)).min(1800.0)
        });
        let now = Instant::now();
        s.penalty_until = Some(now + Duration::from_secs_f64(delay.max(0.0)));
        s.tokens = 0.0;
        // Unlike the Python original, the refill clock restarts here so the
        // drain is real instead of being refunded when the penalty ends.
        s.updated = now;
        s.rate = (self.base_rate / 8.0).max(s.rate / 2.0);
        s.successes = 0;
        tracing::warn!(
            "rate limited: backing off {:.0}s, sustained rate now {:.2} req/s",
            delay,
            s.rate
        );
    }

    /// Called after every good response: resets the failure streak and, every
    /// 50 successes, recovers the rate by 10% (never above the base rate).
    pub fn record_success(&self) {
        let mut s = self.state.lock();
        s.failures = 0;
        s.successes += 1;
        // Recover gradually rather than snapping back to full speed.
        if s.successes >= 50 && s.rate < self.base_rate {
            s.rate = self.base_rate.min(s.rate * 1.1);
            s.successes = 0;
        }
    }

    /// Current sustained rate (req/s).
    pub fn rate(&self) -> f64 {
        self.state.lock().rate
    }

    /// The configured (undegraded) rate.
    pub fn base_rate(&self) -> f64 {
        self.base_rate
    }

    /// Seconds of penalty still to serve (0 when none).
    pub fn penalised_for(&self) -> f64 {
        let s = self.state.lock();
        match s.penalty_until {
            Some(until) => until.saturating_duration_since(Instant::now()).as_secs_f64(),
            None => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn burst_is_allowed_then_throttled() {
        let bucket = TokenBucket::new(100.0, 3);
        let start = Instant::now();
        for _ in 0..3 {
            bucket.acquire(1).await;
        }
        assert!(start.elapsed() < Duration::from_millis(50), "the burst should not wait");
        bucket.acquire(1).await;
        assert!(start.elapsed() >= Duration::from_millis(5));
    }

    #[tokio::test(start_paused = true)]
    async fn sustained_rate_is_enforced() {
        let bucket = TokenBucket::new(2.0, 1);
        let start = Instant::now();
        for _ in 0..5 {
            bucket.acquire(1).await;
        }
        // 1 free + 4 at 2/s
        assert!(start.elapsed() >= Duration::from_millis(1990));
        assert!(start.elapsed() < Duration::from_millis(2100));
    }

    #[tokio::test(start_paused = true)]
    async fn penalty_blocks_acquire() {
        let bucket = TokenBucket::new(100.0, 3);
        bucket.penalise(Some(10.0));
        let start = Instant::now();
        bucket.acquire(1).await;
        assert!(start.elapsed() >= Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn penalise_halves_the_rate_and_blocks() {
        let bucket = TokenBucket::new(1.0, 5);
        bucket.penalise(Some(2.0));
        assert!((bucket.rate() - 0.5).abs() < 1e-9);
        assert!(bucket.penalised_for() > 1.0);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_penalties_compound_but_stay_bounded() {
        let bucket = TokenBucket::new(1.0, 5);
        for _ in 0..10 {
            bucket.penalise(None);
        }
        // Never below an eighth of the configured rate, so it can still recover.
        assert!(bucket.rate() >= 1.0 / 8.0 - 1e-12);
        assert!(bucket.penalised_for() <= 1800.0 + 1e-6);
    }

    #[tokio::test(start_paused = true)]
    async fn default_penalty_doubles_from_sixty_seconds() {
        let bucket = TokenBucket::new(1.0, 5);
        bucket.penalise(None);
        assert!((bucket.penalised_for() - 60.0).abs() < 1.0);
        bucket.penalise(None);
        assert!((bucket.penalised_for() - 120.0).abs() < 1.0);
        bucket.record_success();
        bucket.penalise(None); // streak reset -> back to 60s
        assert!((bucket.penalised_for() - 60.0).abs() < 1.0);
    }

    #[tokio::test(start_paused = true)]
    async fn success_recovers_the_rate_gradually() {
        // Snapping straight back to full speed after a block invites another one.
        let bucket = TokenBucket::new(1.0, 5);
        bucket.penalise(Some(0.0));
        let reduced = bucket.rate();
        for _ in 0..50 {
            bucket.record_success();
        }
        assert!(bucket.rate() > reduced);
        assert!(bucket.rate() <= 1.0);
        for _ in 0..5000 {
            bucket.record_success();
        }
        assert!((bucket.rate() - 1.0).abs() < 1e-9, "capped at the base rate");
    }

    #[tokio::test(start_paused = true)]
    async fn retry_after_header_is_respected() {
        let bucket = TokenBucket::default();
        bucket.penalise(Some(45.0));
        let p = bucket.penalised_for();
        assert!((44.0..=46.0).contains(&p));
    }

    #[test]
    fn legacy_defaults() {
        let b = TokenBucket::default();
        assert!((b.rate() - 0.67).abs() < 1e-12);
    }
}
