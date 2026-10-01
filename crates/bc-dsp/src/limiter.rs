//! Look-ahead true-peak-aware limiter (ceiling default -1 dBTP).
//!
//! The output frame is the input delayed by `look` frames. Gain for that frame
//! is the minimum over the look-ahead window of `required(j) + slope * j`, so
//! gain reduction always arrives *before* the peak, falls at most `1/look` per
//! frame (a click-free linear attack) and recovers exponentially. Inter-sample
//! peaks are estimated with a 4-point cubic midpoint, a cheap oversampling stand-in.

pub struct Limiter {
    look: usize,
    ceiling: f64,
    release_k: f64,
    // delayed audio (stereo interleaved ring of `look + 1` frames)
    delay: Vec<f32>,
    // required gains for the last `look + 1` frames
    req: Vec<f32>,
    // 4-frame history for inter-sample peak estimate
    hist: [[f32; 2]; 4],
    w: usize,
    gain: f64,
    /// Peak gain reduction in the last block, for metering (linear <= 1).
    pub min_gain: f64,
}

impl Limiter {
    pub fn new(sr: f64, ceiling_db: f64, look_ms: f64, release_ms: f64) -> Self {
        let look = ((sr * look_ms / 1000.0).round() as usize).max(8);
        Self {
            look,
            ceiling: 10f64.powf(ceiling_db / 20.0),
            release_k: 1.0 - (-1.0 / (sr * release_ms / 1000.0)).exp(),
            delay: vec![0.0; (look + 1) * 2],
            req: vec![1.0; look + 1],
            hist: [[0.0; 2]; 4],
            w: 0,
            gain: 1.0,
            min_gain: 1.0,
        }
    }

    pub fn latency_frames(&self) -> usize {
        self.look
    }

    pub fn set_ceiling_db(&mut self, db: f64) {
        self.ceiling = 10f64.powf(db / 20.0);
    }

    /// Process one stereo frame, returning the (delayed, limited) frame.
    #[inline]
    pub fn process(&mut self, x: [f32; 2]) -> [f32; 2] {
        // inter-sample peak between the two newest frames of history+x
        self.hist.rotate_left(1);
        self.hist[3] = x;
        let mut peak = 0f32;
        let mut mid_peak = 0f32;
        for ch in 0..2 {
            let a = self.hist[0][ch];
            let b = self.hist[1][ch];
            let c = self.hist[2][ch];
            let d = self.hist[3][ch];
            let mid = (-a + 9.0 * b + 9.0 * c - d) * 0.0625;
            peak = peak.max(x[ch].abs());
            mid_peak = mid_peak.max(mid.abs());
        }
        let ceil = self.ceiling as f32;
        let required = if peak > ceil { ceil / peak } else { 1.0 };
        // The midpoint lies between the previous frame and this one; charge it
        // to the previous frame's slot too, so the gain there already covers it.
        let required_mid = if mid_peak > ceil { ceil / mid_peak } else { 1.0 };

        let n = self.look + 1;
        let w = self.w;
        let prev = (w + n - 1) % n;
        self.req[prev] = self.req[prev].min(required_mid);
        self.req[w] = required.min(required_mid);
        // output is the frame written `look` frames ago == slot (w + 1) % n
        let out_slot = (w + 1) % n;
        let out = [self.delay[out_slot * 2], self.delay[out_slot * 2 + 1]];
        self.delay[w * 2] = x[0];
        self.delay[w * 2 + 1] = x[1];

        let slope = 1.0 / self.look as f64;
        let mut bound = f64::MAX;
        for j in 0..n {
            let r = self.req[(out_slot + j) % n] as f64 + slope * j as f64;
            if r < bound {
                bound = r;
            }
        }
        let bound = bound.min(1.0);
        let released = self.gain + (1.0 - self.gain) * self.release_k;
        self.gain = released.min(bound);
        if self.gain < self.min_gain {
            self.min_gain = self.gain;
        }
        self.w = (w + 1) % n;

        let g = self.gain as f32;
        let hard = ceil;
        [(out[0] * g).clamp(-hard, hard), (out[1] * g).clamp(-hard, hard)]
    }

    pub fn reset(&mut self) {
        self.delay.iter_mut().for_each(|s| *s = 0.0);
        self.req.iter_mut().for_each(|s| *s = 1.0);
        self.hist = [[0.0; 2]; 4];
        self.gain = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_exceeds_ceiling_and_passes_quiet_material() {
        let sr = 48_000.0;
        let mut l = Limiter::new(sr, -1.0, 2.0, 80.0);
        let ceil = 10f32.powf(-1.0 / 20.0);
        let mut max = 0f32;
        // loud summed sines (peak 1.8)
        for i in 0..96_000 {
            let t = i as f64 / sr;
            let v = (0.9 * (2.0 * std::f64::consts::PI * 110.0 * t).sin()
                + 0.9 * (2.0 * std::f64::consts::PI * 3000.0 * t).sin()) as f32;
            let y = l.process([v, v]);
            max = max.max(y[0].abs());
        }
        assert!(max <= ceil + 1e-6, "peak {max} > {ceil}");
        assert!(max > ceil * 0.9, "limited too hard: {max}");

        let mut l = Limiter::new(sr, -1.0, 2.0, 80.0);
        let lat = l.latency_frames();
        let mut ys = vec![];
        for i in 0..4800 {
            let v = 0.25 * (i as f32 * 0.05).sin();
            ys.push(l.process([v, v])[0]);
        }
        for i in lat..4800 {
            let v = 0.25 * ((i - lat) as f32 * 0.05).sin();
            assert!((ys[i] - v).abs() < 1e-6);
        }
    }

    #[test]
    fn attack_is_ahead_of_the_peak() {
        let sr = 48_000.0;
        let mut l = Limiter::new(sr, -1.0, 2.0, 80.0);
        let lat = l.latency_frames();
        let mut out = vec![];
        for i in 0..2000 {
            let v = if i == 1000 { 1.5 } else { 0.0 };
            out.push(l.process([v, v])[0]);
        }
        assert!(out[1000 + lat] <= 10f32.powf(-1.0 / 20.0) + 1e-6);
        assert!(out[1000 + lat] > 0.5);
    }
}
