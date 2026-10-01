#![allow(clippy::needless_range_loop)]
//! Energy-mean pooling, the bars maths (breakdown contrast, p95 fill) and format v3 staleness.

use bc_waveform::bars::{Refs, compute_bars, reference_levels};
use bc_waveform::builder::build_from_mono;
use bc_waveform::format::*;
use bc_waveform::mip::{Pyramid, pool2};
use bc_waveform::scale::{lin_to_u8, u8_to_lin};
use bc_waveform::view::WaveStyle;

fn db(d: f32) -> u8 {
    lin_to_u8(10f32.powf(d / 20.0))
}

/// Level of `n` points from per-point (rms_db, peak_db, band_db).
fn level(n: usize, f: impl Fn(usize) -> (f32, f32, f32)) -> Levels {
    let mut l = Levels::zeros(n);
    for i in 0..n {
        let (r, p, b) = f(i);
        l.set_point(i, [db(p), db(p), db(r), db(b), db(b - 3.0), db(b - 6.0)]);
    }
    l
}

#[test]
fn energy_mean_pooling_sits_well_below_max_pooling_on_kicks() {
    // one -3 dB kick per 8 points over a -30 dB floor
    let l = level(8 * 256, |i| if i % 8 == 0 { (-3.0, -1.0, -6.0) } else { (-30.0, -20.0, -33.0) });
    let mean = l.resample_overview(256);
    let max = l.resample_max(256);
    for j in 0..256 {
        assert_eq!(max.planes[RMS][j], db(-3.0));
        // sqrt((0.5^2 + 7 * 0.03^2) / 8) is about -12 dB: far below the max
        assert!(mean.planes[RMS][j] + 20 < max.planes[RMS][j], "{} vs {}", mean.planes[RMS][j], max.planes[RMS][j]);
        assert!(mean.planes[RMS][j] > db(-30.0));
        // peaks stay max-pooled
        assert_eq!(mean.planes[PEAK_POS][j], max.planes[PEAK_POS][j]);
    }
    // the mip pyramid pools the same way
    let p2 = pool2(&l);
    assert_eq!(p2.planes[PEAK_POS][0], l.planes[PEAK_POS][0]);
    assert!(p2.planes[RMS][0] < l.planes[RMS][0]);
    assert!(p2.planes[RMS][0] > l.planes[RMS][1]);
    let e = |a: u8, b: u8| ((u8_to_lin(a).powi(2) + u8_to_lin(b).powi(2)) / 2.0).sqrt();
    assert!((u8_to_lin(p2.planes[RMS][0]) - e(l.planes[RMS][0], l.planes[RMS][1])).abs() < 0.01);
}

#[test]
fn builder_overview_uses_energy_mean() {
    // 300 s of full-scale clicks every 0.1 s over silence-ish noise: RMS per overview point must be
    // well below the click ceiling
    let sr = 22_050u32;
    let mut s = vec![0.001f32; sr as usize * 300];
    for k in 0..2990 {
        for j in 0..40 {
            s[k * (sr as usize / 10) + j] = 0.9;
        }
    }
    let w = build_from_mono(sr, &s, [1; 16]);
    let d = w.detail.as_ref().expect("detail");
    let (dmax, omax) = (*d.planes[RMS].iter().max().unwrap(), *w.overview.planes[RMS].iter().max().unwrap());
    assert!(omax < dmax, "overview RMS {omax} must be an average, not the detail max {dmax}");
}

/// 2048 points: loud, a -10 dB breakdown in the middle, loud.
fn drop_break_drop() -> Levels {
    level(2048, |i| {
        let in_break = (800..1250).contains(&i);
        let wob = ((i * 7) % 5) as f32 * 0.4;
        if in_break { (-16.0 - wob, -8.0, -20.0) } else { (-6.0 - wob, -1.0, -9.0) }
    })
}

#[test]
fn breakdown_is_visibly_lower_than_the_drop() {
    let l = drop_break_drop();
    let refs = reference_levels(&l);
    let bars = compute_bars(&l, &refs, 0.0, 1.0, 400);
    let drop = bars[10..90].iter().map(|b| b.h).sum::<f32>() / 80.0;
    let brk = bars[170..230].iter().map(|b| b.h).sum::<f32>() / 60.0;
    assert!(brk <= 0.4 * drop, "break {brk} vs drop {drop}");
    assert!(drop > 0.8);
}

#[test]
fn p95_normalisation_fills_the_height_for_a_loud_master() {
    // a heavily limited master: RMS -6.5 .. -5 dBFS, peaks near 0 dBFS
    let l = level(2048, |i| (-6.5 + ((i * 13) % 7) as f32 * 0.25, -0.5, -9.0));
    let refs = reference_levels(&l);
    let bars = compute_bars(&l, &refs, 0.0, 1.0, 300);
    let mut hs: Vec<f32> = bars.iter().map(|b| b.h).collect();
    hs.sort_by(f32::total_cmp);
    assert!(hs[hs.len() / 2] >= 0.85, "median {}", hs[hs.len() / 2]);
    assert!(hs[hs.len() - 1] <= 1.0);
    // a quiet ambient track normalises up by the same mechanism
    let q = level(2048, |i| (-30.0 + ((i * 13) % 7) as f32 * 0.25, -20.0, -33.0));
    let qb = compute_bars(&q, &reference_levels(&q), 0.0, 1.0, 300);
    let mut qh: Vec<f32> = qb.iter().map(|b| b.h).collect();
    qh.sort_by(f32::total_cmp);
    assert!(qh[qh.len() / 2] >= 0.85, "quiet median {}", qh[qh.len() / 2]);
}

#[test]
fn bars_edge_cases() {
    let l = drop_break_drop();
    let refs = reference_levels(&l);
    assert!(compute_bars(&l, &refs, 0.0, 1.0, 0).is_empty());
    // a window past the end of the track is empty
    assert!(compute_bars(&l, &refs, 1.0, 2.0, 10).iter().all(|b| b.h == 0.0));
    // more bars than points: nearest point, still finite and in 0..=1
    let b = compute_bars(&l, &refs, 0.0, 1.0, 5000);
    assert!(b.iter().all(|b| (0.0..=1.0).contains(&b.h)));
    // a zoomed half equals the matching bars of the full view
    let full = compute_bars(&l, &refs, 0.0, 1.0, 200);
    let half = compute_bars(&l, &refs, 0.0, 0.5, 100);
    for i in 0..100 {
        assert!((full[i].h - half[i].h).abs() < 1e-5);
    }
    // empty / silent levels fall back to the absolute references
    assert_eq!(reference_levels(&Levels::zeros(100)), Refs::absolute());
    assert!(compute_bars(&Levels::empty(), &Refs::absolute(), 0.0, 1.0, 4).iter().all(|b| b.h == 0.0));
}

#[test]
fn pyramid_column_mean_tracks_the_direct_bars() {
    let l = drop_break_drop();
    let refs = reference_levels(&l);
    let p = Pyramid::build(&l, 0.01);
    let dur = 20.48;
    let direct = compute_bars(&l, &refs, 0.0, 1.0, 100);
    for i in [5usize, 20, 60, 90] {
        let (t0, t1) = (dur * i as f64 / 100.0, dur * (i + 1) as f64 / 100.0);
        let h = bc_waveform::bars::bar_in_pyramid(&p, &refs, t0, t1).expect("inside");
        assert!((h - direct[i].h).abs() < 0.08, "bar {i}: {h} vs {}", direct[i].h);
        let c = p.sample_column_mean(t0, t1);
        assert!(c[RMS] > 0 && c[PEAK_POS] > 0);
    }
    assert!(bc_waveform::bars::bar_in_pyramid(&p, &refs, dur + 1.0, dur + 2.0).is_none());
}

#[test]
fn old_format_versions_are_stale_and_v3_round_trips() {
    assert_eq!(VERSION, 3);
    let w = build_from_mono(22_050, &vec![0.3f32; 22_050 * 3], [9; 16]);
    let bytes = w.to_bytes(&EncodeOpts::wire());
    assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 3);
    let back = Waveform::from_bytes(&bytes).expect("v3");
    assert_eq!(back.overview, w.overview);
    assert_eq!(back.detail, w.detail);
    for old in [1u16, 2] {
        let mut b = bytes.clone();
        b[4..6].copy_from_slice(&old.to_le_bytes());
        assert!(matches!(read_header(&b), Err(WaveformError::UnsupportedVersion(v)) if v == old));
        assert!(matches!(Waveform::from_bytes(&b), Err(WaveformError::UnsupportedVersion(_))));
    }
}

#[cfg(feature = "store")]
#[test]
fn store_treats_a_v2_file_as_a_miss() {
    let dir = tempfile::tempdir().expect("tmp");
    let store = bc_waveform::store::WaveformStore::new(dir.path(), 1 << 30);
    let w = build_from_mono(22_050, &vec![0.3f32; 22_050 * 3], [9; 16]);
    store.put(7, &w).expect("put");
    assert!(store.get(7).is_some());
    let path = store.path_for(7);
    let mut b = std::fs::read(&path).expect("read");
    b[4..6].copy_from_slice(&2u16.to_le_bytes());
    std::fs::write(&path, b).expect("write");
    assert!(store.get(7).is_none(), "v2 on disk is a cache miss");
    assert!(store.get_overview_only(7).is_none());
    assert!(store.header(7).is_none());
}

#[test]
fn style_alias_has_bars() {
    assert_eq!(bc_waveform::Style::Bars, WaveStyle::Bars);
    assert_ne!(WaveStyle::Bars, WaveStyle::Mono);
}

#[test]
fn loud_sections_keep_bar_to_bar_variation() {
    // a loud section: kick / no-kick alternation with slow level wobble, plus a quiet intro so
    // the references are track-wide
    let l = level(2048, |i| {
        let kick = i % 6 < 3;
        let wob = ((i * 11) % 9) as f32 * 0.25;
        if i < 300 { (-24.0, -14.0, -27.0) } else if kick { (-5.0 - wob * 0.5, -0.8 - wob * 0.2, -8.0) } else { (-8.5 - wob, -3.0 - wob, -11.0) }
    });
    let refs = reference_levels(&l);
    let bars = compute_bars(&l, &refs, 0.0, 1.0, 500);
    let loud: Vec<f32> = bars[100..480].iter().map(|b| b.h).collect();
    let mean = loud.iter().sum::<f32>() / loud.len() as f32;
    let var = loud.iter().map(|h| (h - mean).powi(2)).sum::<f32>() / loud.len() as f32;
    assert!(var.sqrt() / mean >= 0.05, "cv {}", var.sqrt() / mean);
    let clipped = loud.iter().filter(|h| **h >= 0.9999).count();
    assert!((clipped as f32) < 0.1 * loud.len() as f32, "{clipped} of {} at 1.0", loud.len());
}
