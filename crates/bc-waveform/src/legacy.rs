//! Helpers that keep the old JSON endpoints (`/peaks`, `/bands`) and the mix-point planner
//! fed from the new data.

use crate::format::{Levels, PEAK_NEG, PEAK_POS, Waveform};
use crate::scale::u8_to_lin;

/// Best level to draw `points` columns from: the overview when it is dense enough (or the
/// only level), else the detail.
fn source_for(w: &Waveform, points: usize) -> &Levels {
    match &w.detail {
        Some(d) if w.overview.n < points || w.overview.is_empty() => d,
        _ => &w.overview,
    }
}

/// Legacy `/peaks`: exactly `points` int8 `[min, max]` pairs, normalised so the track's loudest
/// peak is 127 (like the old per-track normalisation).
pub fn peaks_json(w: &Waveform, points: usize) -> Vec<[i8; 2]> {
    let src = source_for(w, points);
    let r = src.resample_max(points);
    let norm = u8_to_lin(src.max_peak());
    if norm <= 0.0 {
        return vec![[0, 0]; points];
    }
    let q = |v: u8| ((u8_to_lin(v) / norm) * 127.0).round().clamp(0.0, 127.0) as i8;
    (0..points)
        .map(|i| [-q(r.planes[PEAK_NEG][i]), q(r.planes[PEAK_POS][i])])
        .collect()
}

/// Legacy `/bands`: exactly `points` `[low, mid, high]` triples in 0..1 (stored byte / 255).
pub fn bands_json(w: &Waveform, points: usize) -> Vec<[f32; 3]> {
    let r = source_for(w, points).resample_max(points);
    (0..points)
        .map(|i| {
            [
                r.low()[i] as f32 / 255.0,
                r.mid()[i] as f32 / 255.0,
                r.high()[i] as f32 / 255.0,
            ]
        })
        .collect()
}

/// Inputs for `bc_music::mixpoints::Envelope { level, low }` from the overview (2048 points):
/// `level = max(peak_pos, peak_neg, rms)` and the low plane, both as linear 0..1.
/// Falls back to the detail resampled to 2048 when only that is present.
pub fn mix_envelope(w: &Waveform) -> (Vec<f32>, Vec<f32>) {
    let owned;
    let ov = if w.overview.is_empty() {
        match &w.detail {
            Some(d) => {
                owned = d.resample_max(crate::format::OVERVIEW_POINTS);
                &owned
            }
            None => return (vec![], vec![]),
        }
    } else {
        &w.overview
    };
    let level = (0..ov.n)
        .map(|i| {
            let p = ov.point(i);
            u8_to_lin(p[0].max(p[1]).max(p[2]))
        })
        .collect();
    let low = ov.low().iter().map(|&v| u8_to_lin(v)).collect();
    (level, low)
}
