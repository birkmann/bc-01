//! A small in-place radix-2 complex FFT (no allocation after `new`).

use std::f32::consts::PI;

pub struct Fft {
    n: usize,
    cos: Vec<f32>,
    sin: Vec<f32>,
    rev: Vec<u32>,
}

impl Fft {
    pub fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && n >= 4);
        let cos = (0..n / 2).map(|k| (2.0 * PI * k as f32 / n as f32).cos()).collect();
        let sin = (0..n / 2).map(|k| -(2.0 * PI * k as f32 / n as f32).sin()).collect();
        let bits = n.trailing_zeros();
        let rev = (0..n as u32).map(|i| i.reverse_bits() >> (32 - bits)).collect();
        Self { n, cos, sin, rev }
    }

    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// Forward transform (e^{-i}). `inverse` conjugates the twiddles (no 1/N scaling).
    pub fn run(&self, re: &mut [f32], im: &mut [f32], inverse: bool) {
        let n = self.n;
        for i in 0..n {
            let j = self.rev[i] as usize;
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let half = len / 2;
            let step = n / len;
            let mut start = 0;
            while start < n {
                for k in 0..half {
                    let wr = self.cos[k * step];
                    let wi = if inverse { -self.sin[k * step] } else { self.sin[k * step] };
                    let a = start + k;
                    let b = a + half;
                    let tr = re[b] * wr - im[b] * wi;
                    let ti = re[b] * wi + im[b] * wr;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
                start += len;
            }
            len <<= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_peak_bin() {
        let n = 256;
        let f = Fft::new(n);
        let mut re: Vec<f32> = (0..n).map(|i| (2.0 * PI * 8.0 * i as f32 / n as f32).sin()).collect();
        let orig = re.clone();
        let mut im = vec![0.0; n];
        f.run(&mut re, &mut im, false);
        let mag = |k: usize| (re[k] * re[k] + im[k] * im[k]).sqrt();
        assert!(mag(8) > 100.0 && mag(7) < 1e-2 && mag(9) < 1e-2);
        f.run(&mut re, &mut im, true);
        for i in 0..n {
            assert!((re[i] / n as f32 - orig[i]).abs() < 1e-4);
        }
    }
}
