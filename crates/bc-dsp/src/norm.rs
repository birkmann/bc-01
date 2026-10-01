//! Live loudness normalisation: a per-deck trim from analysed loudness toward a
//! target (default -14 LUFS), bounded and true-peak aware.

/// Never boost or cut by more than this, dB.
pub const MAX_TRIM_DB: f64 = 12.0;

/// Linear trim for a track with `lufs` (and optionally `true_peak_dbtp`).
/// With a known true peak the boost is limited so the peak stays below the
/// ceiling (-1 dBTP); the master limiter handles the rest.
pub fn trim_gain(lufs: Option<f64>, true_peak_dbtp: Option<f64>, target_lufs: f64, ceiling_dbtp: f64) -> f64 {
    let Some(l) = lufs else { return 1.0 };
    let mut db = (target_lufs - l).clamp(-MAX_TRIM_DB, MAX_TRIM_DB);
    if let Some(tp) = true_peak_dbtp
        && db > 0.0 {
            db = db.min((ceiling_dbtp - tp).max(0.0));
        }
    10f64.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_toward_target_with_bounds() {
        assert!((trim_gain(Some(-14.0), None, -14.0, -1.0) - 1.0).abs() < 1e-12);
        assert!((trim_gain(Some(-20.0), None, -14.0, -1.0) - 10f64.powf(6.0 / 20.0)).abs() < 1e-9);
        assert!((trim_gain(Some(-40.0), None, -14.0, -1.0) - 10f64.powf(12.0 / 20.0)).abs() < 1e-9);
        assert!((trim_gain(Some(-8.0), None, -14.0, -1.0) - 10f64.powf(-6.0 / 20.0)).abs() < 1e-9);
        assert_eq!(trim_gain(None, None, -14.0, -1.0), 1.0);
    }

    #[test]
    fn boost_respects_true_peak() {
        // 6 dB wanted, but the peak is already at -3 dBTP: only 2 dB of room.
        let g = trim_gain(Some(-20.0), Some(-3.0), -14.0, -1.0);
        assert!((g - 10f64.powf(2.0 / 20.0)).abs() < 1e-9);
        // cuts are never limited
        assert!(trim_gain(Some(-8.0), Some(0.0), -14.0, -1.0) < 1.0);
    }
}
