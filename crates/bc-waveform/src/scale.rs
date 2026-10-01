//! Absolute perceptual scale: `u8 = round(255 * clamp((dBFS + 60) / 60, 0, 1))`.

/// Floor of the scale in dBFS (maps to 0).
pub const FLOOR_DB: f32 = -60.0;

/// dBFS to stored byte.
pub fn db_to_u8(db: f32) -> u8 {
    if db.is_nan() {
        return 0;
    }
    (255.0 * ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)).round() as u8
}

/// Linear amplitude (1.0 = full scale) to stored byte. Non-positive input is silence.
pub fn lin_to_u8(lin: f32) -> u8 {
    if lin.is_nan() || lin <= 0.0 {
        return 0;
    }
    db_to_u8(20.0 * lin.log10())
}

/// Stored byte to dBFS. `0` is "silence" and reports the floor, -60 dB.
pub fn u8_to_db(v: u8) -> f32 {
    FLOOR_DB + (v as f32 / 255.0) * -FLOOR_DB
}

/// Stored byte to linear amplitude. `0` is exactly 0.
pub fn u8_to_lin(v: u8) -> f32 {
    if v == 0 {
        return 0.0;
    }
    10f32.powf(u8_to_db(v) / 20.0)
}

/// 256-entry byte -> linear table (cheap energy pooling).
pub fn lin_lut() -> &'static [f32; 256] {
    static LUT: std::sync::OnceLock<[f32; 256]> = std::sync::OnceLock::new();
    LUT.get_or_init(|| std::array::from_fn(|i| u8_to_lin(i as u8)))
}

/// Energy mean of stored bytes: `sqrt(mean(lin^2))` re-encoded. Empty input is 0.
pub fn energy_mean_u8(vals: impl IntoIterator<Item = u8>) -> u8 {
    let lut = lin_lut();
    let (mut ss, mut n) = (0.0f64, 0u32);
    for v in vals {
        let l = lut[v as usize] as f64;
        ss += l * l;
        n += 1;
    }
    if n == 0 { 0 } else { lin_to_u8((ss / n as f64).sqrt() as f32) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors() {
        assert_eq!(db_to_u8(0.0), 255);
        assert_eq!(db_to_u8(-60.0), 0);
        assert_eq!(db_to_u8(-90.0), 0);
        assert_eq!(db_to_u8(6.0), 255);
        assert_eq!(db_to_u8(-30.0), 128);
        assert_eq!(lin_to_u8(1.0), 255);
        assert_eq!(lin_to_u8(0.0), 0);
        assert_eq!(lin_to_u8(-1.0), 0);
        assert_eq!(lin_to_u8(0.001), 0);
        assert_eq!(lin_to_u8(f32::NAN), 0);
    }

    #[test]
    fn absolute_not_normalised() {
        // half amplitude is -6.02 dB, always the same byte regardless of the track
        assert_eq!(lin_to_u8(0.5), 229);
        assert!(lin_to_u8(0.5) < lin_to_u8(1.0));
    }

    #[test]
    fn round_trip_is_close() {
        for v in 1..=255u8 {
            let back = lin_to_u8(u8_to_lin(v));
            assert!(back.abs_diff(v) <= 1, "{v} -> {back}");
        }
        assert_eq!(u8_to_lin(0), 0.0);
        assert!((u8_to_db(255) - 0.0).abs() < 1e-5);
        assert!((u8_to_db(0) + 60.0).abs() < 1e-5);
    }
}
