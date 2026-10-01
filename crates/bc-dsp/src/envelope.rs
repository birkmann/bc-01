//! Gain envelopes as data, so a running crossfade can be re-timed.
//!
//! Exact port of `web/frontend/src/player/envelope.ts`. Every fade is described
//! first (from, to, when, what shape) and the mixer evaluates it **per sample**;
//! re-timing is a new envelope that starts from the model's own value at "now"
//! and keeps its place on the curve, so nothing jumps.

use crate::beatmatch::EchoSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Linear,
    /// `sin(pi/2 * p)` -- the rising half of an equal-power pair.
    In,
    /// `1 - cos(pi/2 * p)` progress, i.e. `cos(pi/2 * p)` when going 1 -> 0.
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Env {
    pub from: f64,
    pub to: f64,
    /// Time (seconds, engine clock) the envelope starts.
    pub t0: f64,
    /// Time it reaches `to`.
    pub t1: f64,
    pub shape: Shape,
    /// Where on the shape this envelope begins (0..1); a re-timed envelope resumes mid-curve.
    pub p0: f64,
}

impl Env {
    pub fn new(from: f64, to: f64, t0: f64, t1: f64, shape: Shape) -> Self {
        Self { from, to, t0, t1, shape, p0: 0.0 }
    }
    /// The value at time `t`.
    pub fn value_at(&self, t: f64) -> f64 {
        value_at(self, t)
    }
}

/// The value at time `t`. For `In`/`Out` the shapes are the two halves of an
/// equal-power pair: in = sin(pi/2 p), out = cos(pi/2 p), so in^2 + out^2 = 1.
pub fn value_at(e: &Env, t: f64) -> f64 {
    let span = e.t1 - e.t0;
    let raw = if span <= 0.0 { 1.0 } else { (t - e.t0) / span };
    let p = raw.clamp(0.0, 1.0);
    let q = e.p0 + (1.0 - e.p0) * p;
    let half_pi = std::f64::consts::FRAC_PI_2;
    let f = match e.shape {
        Shape::Linear => q,
        Shape::In => (half_pi * q).sin(),
        Shape::Out => 1.0 - (half_pi * q).cos(),
    };
    e.from + (e.to - e.from) * f
}

/// Samples the envelope into a value curve (kept for tests and the browser host).
pub fn curve_for(e: &Env, points: usize) -> Vec<f32> {
    let n = points.max(2);
    (0..n).map(|i| value_at(e, e.t0 + (e.t1 - e.t0) * i as f64 / (n - 1) as f64) as f32).collect()
}

/// How many curve points for a fade of this length: ~25/s, bounded.
pub fn curve_points(seconds: f64) -> usize {
    ((seconds * 25.0).round() as i64).clamp(16, 512) as usize
}

/// The same fade, finishing at `t1` instead: resumes from where the model is
/// now and continues from the same place on the curve.
pub fn retimed(e: &Env, now: f64, t1: f64) -> Env {
    let span = e.t1 - e.t0;
    let raw = if span <= 0.0 { 1.0 } else { (now - e.t0) / span };
    let p = raw.clamp(0.0, 1.0);
    let p0 = e.p0 + (1.0 - e.p0) * p;
    Env { from: e.from, to: e.to, t0: now, t1: t1.max(now + 0.001), shape: e.shape, p0 }
}

/// The echo send over the end of a blend, as (time, value) breakpoints: open
/// shortly before the end, hold, close just after. Everything before `now` is
/// pulled up to `now`.
pub fn send_steps(end_t: f64, echo: &EchoSpec, now: f64) -> [(f64, f64); 4] {
    let open_at = now.max(end_t - echo.hold_s - echo.up_s);
    let full_at = (now + 0.001).max(open_at + echo.up_s);
    let close_start = full_at.max(end_t);
    let closed = close_start + 0.3;
    [(open_at, 0.0), (full_at, echo.send), (close_start, echo.send), (closed, 0.0)]
}

/// A piecewise-linear automation lane (the echo send), as the browser built
/// from `setValueAtTime(current, now)` plus `linearRampToValueAtTime` points:
/// it ramps from `v0` (value at `t_start`) to the first point, then follows the points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Piecewise {
    pub t_start: f64,
    pub v0: f64,
    pub t: [f64; 4],
    pub v: [f64; 4],
}

impl Piecewise {
    pub fn from_steps(t_start: f64, v0: f64, steps: [(f64, f64); 4]) -> Self {
        Self {
            t_start,
            v0,
            t: [steps[0].0, steps[1].0, steps[2].0, steps[3].0],
            v: [steps[0].1, steps[1].1, steps[2].1, steps[3].1],
        }
    }
    pub fn value_at(&self, now: f64) -> f64 {
        let seg = |t0: f64, v0: f64, t1: f64, v1: f64| -> f64 {
            let span = t1 - t0;
            if span <= 0.0 { v1 } else { v0 + (v1 - v0) * ((now - t0) / span) }
        };
        if now <= self.t_start {
            return self.v0;
        }
        if now <= self.t[0] {
            return seg(self.t_start, self.v0, self.t[0], self.v[0]);
        }
        for i in 1..4 {
            if now <= self.t[i] {
                return seg(self.t[i - 1], self.v[i - 1], self.t[i], self.v[i]);
            }
        }
        self.v[3]
    }
}

/// One automation lane of a channel parameter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Auto {
    Const(f64),
    Env(Env),
    Pw(Piecewise),
}

impl Auto {
    #[inline]
    pub fn value(&self, t: f64) -> f64 {
        match self {
            Auto::Const(v) => *v,
            Auto::Env(e) => value_at(e, t),
            Auto::Pw(p) => p.value_at(t),
        }
    }
    /// A short click-free move from the current value to `to`.
    pub fn ramp_to(&self, now: f64, to: f64, secs: f64) -> Auto {
        let cur = self.value(now);
        if (cur - to).abs() < 1e-9 {
            return Auto::Const(to);
        }
        Auto::Env(Env::new(cur, to, now, now + secs.max(0.001), Shape::Linear))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    #[test]
    fn value_at_hits_endpoints_and_clamps() {
        let e = Env::new(0.0, 1.0, 10.0, 20.0, Shape::In);
        assert_eq!(value_at(&e, 5.0), 0.0);
        assert_eq!(value_at(&e, 10.0), 0.0);
        close(value_at(&e, 20.0), 1.0);
        close(value_at(&e, 25.0), 1.0);
    }

    #[test]
    fn linear_is_linear() {
        close(value_at(&Env::new(1.0, 0.0, 0.0, 4.0, Shape::Linear), 1.0), 0.75);
    }

    #[test]
    fn in_and_out_are_an_equal_power_pair() {
        let fi = Env::new(0.0, 1.0, 0.0, 10.0, Shape::In);
        let fo = Env::new(1.0, 0.0, 0.0, 10.0, Shape::Out);
        let mut t = 0.0;
        while t <= 10.0 {
            let (a, b) = (value_at(&fi, t), value_at(&fo, t));
            assert!((a * a + b * b - 1.0).abs() < 1e-6);
            t += 0.5;
        }
    }

    #[test]
    fn retimed_starts_from_current_value_and_lands_on_target() {
        let e = Env::new(0.0, 1.0, 0.0, 10.0, Shape::In);
        let r = retimed(&e, 4.0, 6.0);
        close(value_at(&r, 4.0), value_at(&e, 4.0));
        close(value_at(&r, 6.0), 1.0);
        close(r.p0, 0.4);
    }

    #[test]
    fn retimed_pair_stays_equal_power() {
        let a = Env::new(0.0, 1.0, 0.0, 10.0, Shape::In);
        let b = Env::new(1.0, 0.0, 0.0, 10.0, Shape::Out);
        let (ra, rb) = (retimed(&a, 3.0, 20.0), retimed(&b, 3.0, 20.0));
        let mut t = 3.0;
        while t <= 20.0 {
            let (x, y) = (value_at(&ra, t), value_at(&rb, t));
            assert!((x * x + y * y - 1.0).abs() < 1e-6);
            t += 1.0;
        }
    }

    #[test]
    fn retimed_never_zero_length() {
        let r = retimed(&Env::new(0.0, 1.0, 0.0, 10.0, Shape::Linear), 12.0, 5.0);
        assert!(r.t1 > r.t0);
        assert_eq!(value_at(&r, 12.0), 1.0);
    }

    #[test]
    fn curve_for_samples_endpoints() {
        let c = curve_for(&Env::new(1.0, 0.0, 0.0, 2.0, Shape::Out), 5);
        assert_eq!(c.len(), 5);
        assert!((c[0] - 1.0).abs() < 1e-6);
        assert!(c[4].abs() < 1e-6);
    }

    #[test]
    fn send_steps_open_hold_close() {
        let echo = EchoSpec { send: 0.65, hold_s: 3.0, up_s: 0.3, bpm: Some(128.0) };
        let s = send_steps(100.0, &echo, 80.0);
        let ts: Vec<f64> = s.iter().map(|p| p.0).collect();
        let want = [96.7, 97.0, 100.0, 100.3];
        for (a, b) in ts.iter().zip(want) {
            close(*a, b);
        }
        assert_eq!(s.map(|p| p.1), [0.0, 0.65, 0.65, 0.0]);
    }

    #[test]
    fn send_steps_never_schedule_in_the_past() {
        let echo = EchoSpec { send: 0.65, hold_s: 3.0, up_s: 0.3, bpm: Some(128.0) };
        let s = send_steps(101.0, &echo, 100.0);
        assert_eq!(s[0].0, 100.0);
        for i in 1..4 {
            assert!(s[i].0 >= s[i - 1].0);
        }
        close(s[3].0, 101.3);
    }

    #[test]
    fn piecewise_follows_the_steps() {
        let echo = EchoSpec { send: 0.5, hold_s: 1.0, up_s: 0.5, bpm: None };
        let p = Piecewise::from_steps(0.0, 0.0, send_steps(10.0, &echo, 0.0));
        close(p.value_at(0.0), 0.0);
        close(p.value_at(8.5), 0.0);
        close(p.value_at(8.75), 0.25);
        close(p.value_at(9.0), 0.5);
        close(p.value_at(10.0), 0.5);
        close(p.value_at(10.15), 0.25);
        close(p.value_at(11.0), 0.0);
    }
}
