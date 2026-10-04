#![allow(clippy::needless_range_loop)]
//! Format, builder, mip, legacy and store tests (ports of test_waveform_bands plus the new
//! format contract).

use bc_waveform::builder::{WaveformBuilder, build_from_mono};
use bc_waveform::format::*;
use bc_waveform::legacy::*;
use bc_waveform::mip::Pyramid;
use bc_waveform::scale::*;

const SR: u32 = 22050;

fn tone(freq: f32, seconds: f32, sr: u32) -> Vec<f32> {
    (0..(sr as f32 * seconds) as usize)
        .map(|i| (std::f32::consts::TAU * freq * i as f32 / sr as f32).sin())
        .collect()
}

fn noise(seconds: usize, sr: u32) -> Vec<f32> {
    let mut x: u32 = 12345;
    (0..seconds * sr as usize)
        .map(|i| {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            let env = 0.2 + 0.8 * ((i as f32 / sr as f32) * 0.7).sin().abs();
            ((x >> 8) as f32 / 8_388_608.0 - 1.0) * env
        })
        .collect()
}

/// Incompressible levels so compressed sizes are predictable (detail much larger than overview).
fn random_wave() -> Waveform {
    let mut x: u32 = 99;
    let mut lv = |n: usize| {
        let mut l = Levels::zeros(n);
        for p in 0..6 {
            for i in 0..n {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                l.planes[p][i] = (x >> 24) as u8;
            }
        }
        l
    };
    let detail = lv(20_000);
    Waveform {
        sample_rate: 44100,
        hop_samples: 256,
        total_samples: 20_000 * 256,
        source_hash: [1; 16],
        overview: lv(2048),
        detail: Some(detail),
    }
}

fn build(samples: &[f32], sr: u32) -> Waveform {
    build_from_mono(sr, samples, [7; 16])
}

// --- band semantics (test_waveform_bands) -----------------------------------------------

/// Max of a plane ignoring the filter start-up transient (first/last 10 % of the points).
fn steady_max(p: &[u8]) -> u8 {
    let n = p.len();
    *p[n / 10..n - n / 10].iter().max().unwrap()
}

#[test]
fn low_tone_reads_in_the_low_band() {
    let w = build(&tone(60.0, 2.0, SR), SR);
    let o = &w.overview;
    let (low, mid, high) = (
        steady_max(o.low()),
        steady_max(o.mid()),
        steady_max(o.high()),
    );
    assert!(low > 200, "low {low}");
    assert!(low as u32 > mid as u32 * 2, "low {low} mid {mid}");
    assert!(low as u32 > high as u32 * 2, "low {low} high {high}");
}

#[test]
fn hat_range_reads_in_the_high_band() {
    let w = build(&tone(8000.0, 2.0, SR), SR);
    let o = &w.overview;
    let (low, high) = (
        *o.low().iter().max().unwrap(),
        *o.high().iter().max().unwrap(),
    );
    assert!(high > 200, "high {high}");
    assert!(high as u32 > low as u32 * 2);
}

#[test]
fn a_mix_shows_both() {
    let a = tone(60.0, 2.0, SR);
    let b = tone(8000.0, 2.0, SR);
    let mix: Vec<f32> = a.iter().zip(&b).map(|(x, y)| x * 0.8 + y * 0.5).collect();
    let w = build(&mix, SR);
    assert!(*w.overview.low().iter().max().unwrap() > 100);
    assert!(*w.overview.high().iter().max().unwrap() > 100);
}

#[test]
fn mid_tone_reads_in_mid() {
    let w = build(&tone(1000.0, 1.0, 44100), 44100);
    let d = w.detail.as_ref().unwrap();
    let m = d.mid()[d.n / 2];
    assert!(m > 200 && d.low()[d.n / 2] < m / 2 && d.high()[d.n / 2] < m / 2);
}

#[test]
fn empty_audio_yields_silence() {
    let w = build(&[], SR);
    assert_eq!(w.overview.n, OVERVIEW_POINTS);
    assert!(w.overview.planes.iter().all(|p| p.iter().all(|&v| v == 0)));
    assert_eq!(w.detail.as_ref().unwrap().n, 0);
    assert_eq!(w.total_samples, 0);
    let bytes = w.to_bytes(&EncodeOpts::default());
    assert_eq!(Waveform::from_bytes(&bytes).unwrap(), w);
    assert_eq!(peaks_json(&w, 10), vec![[0, 0]; 10]);
    assert_eq!(bands_json(&w, 10).len(), 10);
}

#[test]
fn very_short_file_does_not_crash() {
    for n in [1usize, 2, 100, 255, 256, 257] {
        let w = build(&vec![0.5; n], 44100);
        assert_eq!(w.overview.n, OVERVIEW_POINTS);
        assert_eq!(w.detail.as_ref().unwrap().n, n.div_ceil(256));
        assert_eq!(
            w.overview.planes[0].iter().filter(|&&v| v > 0).count(),
            OVERVIEW_POINTS
        );
        assert_eq!(peaks_json(&w, 200).len(), 200);
    }
}

#[test]
fn absolute_scale_not_normalised() {
    let loud: Vec<f32> = tone(440.0, 1.0, 44100);
    let quiet: Vec<f32> = loud.iter().map(|x| x * 0.1).collect();
    let (l, q) = (build(&loud, 44100), build(&quiet, 44100));
    let (lp, qp) = (
        *l.overview.peak_pos().iter().max().unwrap(),
        *q.overview.peak_pos().iter().max().unwrap(),
    );
    assert_eq!(lp, 255);
    assert_eq!(qp, lin_to_u8(0.1));
    assert!(qp < lp);
}

#[test]
fn peaks_are_asymmetric() {
    let w = build(&vec![0.5; 4410], 44100);
    let p = w.detail.unwrap().point(3);
    assert_eq!(p[0], lin_to_u8(0.5));
    assert_eq!(p[1], 0);
    assert_eq!(p[2], lin_to_u8(0.5));
}

#[test]
fn hop_is_rate_normalised() {
    assert_eq!(hop_for_rate(44100), 256);
    assert_eq!(hop_for_rate(48000), 279);
    assert_eq!(hop_for_rate(96000), 557);
    assert_eq!(hop_for_rate(8000), 46);
    for sr in [8000u32, 22050, 44100, 48000, 96000, 192000] {
        let w = build(&tone(1000.0, 1.0, sr), sr);
        assert_eq!(w.hop_samples, hop_for_rate(sr));
        assert!(
            (w.detail_rate_hz() - 172.27).abs() < 2.0,
            "{sr}: {}",
            w.detail_rate_hz()
        );
        assert_eq!(
            w.detail.as_ref().unwrap().n,
            sr.div_ceil(w.hop_samples) as usize
        );
        assert_eq!(w.duration_ms(), 1000);
        // a mid tone is sane at every rate
        let d = w.detail.unwrap();
        assert!(d.mid()[d.n / 2] > 200, "{sr}");
    }
}

#[test]
fn streaming_invariance() {
    let s: Vec<f32> = tone(100.0, 1.5, 44100)
        .iter()
        .zip(tone(5000.0, 1.5, 44100))
        .map(|(a, b)| a * 0.5 + b * 0.3)
        .collect();
    let whole = build(&s, 44100);
    for chunk in [1usize, 17, 4096, 100_000] {
        let mut b = WaveformBuilder::new(44100);
        for c in s.chunks(chunk) {
            b.push_mono(c);
        }
        assert_eq!(b.finish([7; 16]), whole, "chunk {chunk}");
    }
}

#[test]
fn non_finite_input_is_silence() {
    let w = build(&[f32::NAN, f32::INFINITY, 0.0], 44100);
    assert_eq!(w.detail.unwrap().point(0), [0; 6]);
}

#[test]
fn overview_pools_detail_peaks_max_and_energy_mean() {
    let n = 256 * 10_000;
    let mut s = vec![0.0f32; n];
    s[256 * 4000 + 5] = 0.9; // one spike
    let w = build(&s, 44100);
    let d = w.detail.as_ref().unwrap();
    assert_eq!(d.n, 10_000);
    for j in 0..OVERVIEW_POINTS {
        let a = j * d.n / OVERVIEW_POINTS;
        let b = (j + 1) * d.n / OVERVIEW_POINTS;
        for p in 0..PLANES {
            let want = if p >= RMS {
                energy_mean_u8(d.planes[p][a..b].iter().copied())
            } else {
                *d.planes[p][a..b].iter().max().unwrap()
            };
            assert_eq!(w.overview.planes[p][j], want);
        }
    }
    assert!(w.overview.peak_pos().contains(&lin_to_u8(0.9)));
}

#[test]
fn few_detail_points_repeat_nearest() {
    let w = build(&vec![0.3; 256 * 4], 44100);
    assert_eq!(w.detail.as_ref().unwrap().n, 4);
    let o = &w.overview;
    // 2048 points over 4 detail points: each repeated 512 times
    assert!(o.peak_pos().iter().all(|&v| v > 0));
}

#[test]
fn builder_throughput_over_100x_realtime() {
    let sr = 44100;
    let s = tone(220.0, 60.0, sr);
    let t = std::time::Instant::now();
    let w = build(&s, sr);
    let el = t.elapsed().as_secs_f64();
    let x = 60.0 / el;
    eprintln!("builder throughput: {x:.0}x realtime ({} samples)", s.len());
    assert!(w.detail.is_some());
    // debug builds are slow; the hard gate applies to optimised test runs
    if !cfg!(debug_assertions) {
        assert!(x > 100.0, "{x}x");
    }
}

// --- format ----------------------------------------------------------------------------

fn sample() -> Waveform {
    let s: Vec<f32> = tone(100.0, 3.0, 44100)
        .iter()
        .zip(tone(7000.0, 3.0, 44100))
        .map(|(a, b)| a * 0.5 + b * 0.2)
        .collect();
    build(&s, 44100)
}

#[test]
fn round_trip_raw_and_zstd() {
    let w = sample();
    for opts in [EncodeOpts::wire(), EncodeOpts::file()] {
        let b = w.to_bytes(&opts);
        let h = read_header(&b).unwrap();
        assert_eq!(h.is_raw(), !opts.compress || !cfg!(feature = "zstd"));
        assert_eq!(h.total_len(), b.len());
        assert_eq!(h.overview_points, 2048);
        assert_eq!(h.sample_rate, 44100);
        assert_eq!(Waveform::from_bytes(&b).unwrap(), w);
    }
    if cfg!(feature = "zstd") {
        let z = w.to_bytes(&EncodeOpts::file()).len();
        let r = w.to_bytes(&EncodeOpts::wire()).len();
        assert!(z < r, "{z} !< {r}");
    }
}

#[test]
fn single_level_containers() {
    let w = sample();
    let ov = Waveform::from_bytes(&w.to_bytes(&EncodeOpts::wire_overview())).unwrap();
    assert!(ov.has_overview() && ov.detail.is_none());
    assert_eq!(ov.overview, w.overview);
    let b = w.to_bytes(&EncodeOpts::wire_detail());
    let h = read_header(&b).unwrap();
    assert!(!h.has_overview() && h.has_detail() && h.overview_len == 0);
    let mut de = Waveform::from_bytes(&b).unwrap();
    assert!(!de.has_overview());
    assert_eq!(de.detail, w.detail);
    de.ensure_overview();
    assert_eq!(de.overview, w.overview);
    // header of an overview-only file still knows the detail point count
    let h = read_header(&w.to_bytes(&EncodeOpts::wire_overview())).unwrap();
    assert_eq!(h.detail_points as usize, w.detail.as_ref().unwrap().n);
    assert_eq!(h.detail_len, 0);
}

#[test]
fn corrupt_and_truncated_rejected() {
    let w = sample();
    let b = w.to_bytes(&EncodeOpts::wire());
    assert!(matches!(
        read_header(&b[..10]),
        Err(WaveformError::Truncated { .. })
    ));
    assert!(matches!(
        read_header(b"nope-nope"),
        Err(WaveformError::BadMagic)
    ));
    assert!(matches!(
        read_header(&b[..30]),
        Err(WaveformError::Truncated { .. })
    ));
    assert!(matches!(read_header(b"nope"), Err(WaveformError::BadMagic)));
    assert!(matches!(read_header(&[]), Err(WaveformError::BadMagic)));
    assert!(matches!(
        Waveform::from_bytes(&b[..b.len() - 1]),
        Err(WaveformError::Truncated { .. })
    ));
    let mut bad = b.clone();
    bad[0] = b'X';
    assert!(matches!(
        Waveform::from_bytes(&bad),
        Err(WaveformError::BadMagic)
    ));
    let mut bad = b.clone();
    bad[4] = 9;
    assert!(matches!(
        Waveform::from_bytes(&bad),
        Err(WaveformError::UnsupportedVersion(9))
    ));
    let mut bad = b.clone();
    bad[8..12].copy_from_slice(&0u32.to_le_bytes());
    assert!(Waveform::from_bytes(&bad).is_err());
    let mut bad = b.clone();
    bad[44..48].copy_from_slice(&5u32.to_le_bytes()); // detail_points lies
    assert!(Waveform::from_bytes(&bad).is_err());
    // compressed garbage
    #[cfg(feature = "zstd")]
    {
        let mut z = w.to_bytes(&EncodeOpts::file());
        let n = z.len();
        for x in &mut z[100..n.min(400)] {
            *x ^= 0xA5;
        }
        assert!(Waveform::from_bytes(&z).is_err());
    }
}

#[test]
fn meta_dto() {
    let w = sample();
    let m = w.meta(42);
    assert_eq!(m.track_id, 42);
    assert_eq!(m.duration_ms, 3000);
    assert_eq!(m.overview_points, 2048);
    assert_eq!(m.detail_points as usize, w.detail.as_ref().unwrap().n);
    assert_eq!(m.source_hash, "07".repeat(16));
    assert!((m.detail_rate_hz - 172.27).abs() < 0.01);
}

// --- mip -------------------------------------------------------------------------------

#[test]
fn pyramid_levels_and_selection() {
    let long = build(&noise(120, 44100), 44100);
    let w = sample();
    let d = long.detail.as_ref().unwrap();
    let p = Pyramid::from_waveform(&long);
    assert_eq!(p.levels[0], *d);
    assert!(p.levels.last().unwrap().n <= 2048);
    assert!(p.levels.len() >= 2 && p.levels[p.levels.len() - 2].n > 2048);
    for k in 1..p.levels.len() {
        assert_eq!(p.levels[k].n, p.levels[k - 1].n.div_ceil(2));
        for i in 0..p.levels[k].n {
            for pl in 0..6 {
                let a = p.levels[k - 1].planes[pl][2 * i];
                let b = p.levels[k - 1].planes[pl].get(2 * i + 1).copied();
                let want = match (pl >= RMS, b) {
                    (true, Some(b)) => energy_mean_u8([a, b]),
                    (true, None) => a,
                    (false, b) => a.max(b.unwrap_or(0)),
                };
                assert_eq!(p.levels[k].planes[pl][i], want);
            }
        }
    }
    assert_eq!(p.level_for(0.1), 0);
    assert_eq!(p.level_for(1.9), 0);
    assert_eq!(p.level_for(2.0), 1);
    assert_eq!(p.level_for(8.5), 3);
    assert_eq!(p.level_for(1e9), p.levels.len() - 1);
    // overview-only pyramid
    let ov = Waveform::from_bytes(&w.to_bytes(&EncodeOpts::wire_overview())).unwrap();
    let po = Pyramid::from_waveform(&ov);
    assert_eq!(po.levels.len(), 1);
    assert!((po.dt0_s * 2048.0 - 3.0).abs() < 1e-6);
}

#[test]
fn sample_column_is_max_over_range() {
    let w = sample();
    let p = Pyramid::from_waveform(&w);
    let d = w.detail.as_ref().unwrap();
    let dt = w.detail_dt_s();
    // exactly one point
    assert_eq!(
        p.sample_column(10.0 * dt + 1e-9, 11.0 * dt - 1e-9),
        d.point(10)
    );
    // wide range = max of all of them
    let all = p.sample_column(0.0, 3.0);
    for pl in 0..6 {
        assert_eq!(all[pl], *d.planes[pl].iter().max().unwrap());
    }
    assert_eq!(p.sample_column(100.0, 101.0), [0; 6]);
    assert_eq!(p.sample_column(-5.0, -4.0), [0; 6]);
}

// --- legacy ----------------------------------------------------------------------------

#[test]
fn legacy_json_exact_counts_and_normalisation() {
    let w = sample();
    for n in [20usize, 50, 200, 400, 2000, 2048, 3000] {
        assert_eq!(peaks_json(&w, n).len(), n);
        assert_eq!(bands_json(&w, n).len(), n);
    }
    let p = peaks_json(&w, 200);
    assert_eq!(p.iter().map(|x| x[1]).max().unwrap(), 127);
    assert!(p.iter().all(|x| x[0] <= 0 && x[1] >= 0));
    let b = bands_json(&w, 200);
    assert!(b.iter().all(|x| x.iter().all(|v| (0.0..=1.0).contains(v))));
    assert!(b.iter().any(|x| x[0] > 0.5) && b.iter().any(|x| x[2] > 0.3));
    // overview-only waveform still answers
    let ov = Waveform::from_bytes(&w.to_bytes(&EncodeOpts::wire_overview())).unwrap();
    assert_eq!(peaks_json(&ov, 3000).len(), 3000);
}

#[test]
fn mix_envelope_levels() {
    let w = sample();
    let (level, low) = mix_envelope(&w);
    assert_eq!(level.len(), 2048);
    assert_eq!(low.len(), 2048);
    assert!(level.iter().all(|v| (0.0..=1.0).contains(v)));
    assert!(level[100] >= low[100]);
    let silent = build(&[], 44100);
    assert!(mix_envelope(&silent).0.iter().all(|&v| v == 0.0));
}

// --- store -----------------------------------------------------------------------------

#[cfg(feature = "store")]
mod store {
    use super::*;
    use bc_waveform::store::{WaveformStore, source_hash};

    #[test]
    fn put_get_remove_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let st = WaveformStore::new(dir.path(), u64::MAX);
        let w = sample();
        st.put(7, &w).unwrap();
        st.put(12345, &w).unwrap();
        assert!(dir.path().join("0/7.bcw2").is_file());
        assert!(dir.path().join("12/12345.bcw2").is_file());
        assert_eq!(st.get(7).unwrap(), w);
        assert!(st.get(8).is_none());
        let ov = st.get_overview_only(7).unwrap();
        assert!(ov.detail.is_none());
        assert_eq!(ov.overview, w.overview);
        let h = st.header(7).unwrap();
        assert!(h.has_detail() && h.has_overview());
        assert_eq!(st.stats().files, 2);
        assert!(st.remove(7).unwrap());
        assert!(!st.remove(7).unwrap());
        assert_eq!(st.stats().files, 1);
    }

    #[test]
    fn eviction_downgrades_lru_to_overview_only() {
        let dir = tempfile::tempdir().unwrap();
        let w = random_wave();
        let one = w.to_bytes(&EncodeOpts::file()).len() as u64;
        let st = WaveformStore::new(dir.path(), one * 2 + one / 2);
        st.put(1, &w).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        st.put(2, &w).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(st.stats().overview_only_files, 0);
        st.get(1).unwrap(); // 1 is now more recent than 2
        std::thread::sleep(std::time::Duration::from_millis(30));
        st.put(3, &w).unwrap(); // over the cap -> evict LRU = 2
        let s = st.stats();
        assert_eq!(s.files, 3);
        assert!(s.bytes <= st.cap_bytes(), "{s:?}");
        // evicted to the low-water mark, so the next put does not trigger another pass
        assert!(s.bytes <= st.cap_bytes() / 1000 * 900, "{s:?}");
        assert!(!st.header(2).unwrap().has_detail());
        assert!(st.header(1).unwrap().has_detail());
        assert!(st.header(3).unwrap().has_detail());
        // the overview is never deleted
        let o = st.get(2).unwrap();
        assert_eq!(o.overview, w.overview);
        assert!(o.detail.is_none());
        // idempotent
        assert_eq!(st.evict_to_cap().unwrap(), 0);
        // tiny cap: everything ends overview-only but still present
        let st2 = WaveformStore::new(dir.path(), 1);
        st2.evict_to_cap().unwrap();
        let s = st2.stats();
        assert_eq!((s.files, s.overview_only_files), (3, 3));
    }

    #[test]
    fn concurrent_puts() {
        let dir = tempfile::tempdir().unwrap();
        let st = std::sync::Arc::new(WaveformStore::new(dir.path(), u64::MAX));
        let w = std::sync::Arc::new(sample());
        let hs: Vec<_> = (0..8)
            .map(|t| {
                let (st, w) = (st.clone(), w.clone());
                std::thread::spawn(move || {
                    for i in 0..5 {
                        st.put(i, &w).unwrap(); // same ids from all threads
                        assert!(st.get(i).is_some());
                    }
                    let _ = t;
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(st.stats().files, 5);
    }

    #[test]
    fn source_hash_covers_size_head_tail() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.bin");
        let mut data = vec![1u8; 300_000];
        std::fs::write(&p, &data).unwrap();
        let h1 = source_hash(&p).unwrap();
        assert_eq!(h1, source_hash(&p).unwrap());
        data[150_000] = 9; // middle: not covered
        std::fs::write(&p, &data).unwrap();
        assert_eq!(h1, source_hash(&p).unwrap());
        data[299_999] = 9; // tail
        std::fs::write(&p, &data).unwrap();
        assert_ne!(h1, source_hash(&p).unwrap());
        std::fs::write(&p, b"").unwrap();
        assert!(source_hash(&p).is_ok());
        std::fs::write(&p, b"abc").unwrap();
        assert!(source_hash(&p).is_ok());
        assert!(source_hash(&dir.path().join("missing")).is_err());
    }
}
