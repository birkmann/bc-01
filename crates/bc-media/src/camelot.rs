//! Camelot <-> musical key helpers.
//!
//! Port of `services/metadata/camelot.py` (`to_camelot`, `key_name`).
//! TODO(ws3): `bc-music::camelot` owns the canonical copy; this is the minimal local subset
//! the tag writer needs so that `bc-media` does not depend on `bc-music`.

/// Pitch class names, 0 = C.
pub const PITCH_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// `(pitch class, is_minor) -> Camelot code`.
const CAMELOT: [(u8, bool, &str); 24] = [
    (8, false, "4B"),
    (5, true, "4A"),
    (3, false, "5B"),
    (0, true, "5A"),
    (10, false, "6B"),
    (7, true, "6A"),
    (5, false, "7B"),
    (2, true, "7A"),
    (0, false, "8B"),
    (9, true, "8A"),
    (7, false, "9B"),
    (4, true, "9A"),
    (2, false, "10B"),
    (11, true, "10A"),
    (9, false, "11B"),
    (6, true, "11A"),
    (4, false, "12B"),
    (1, true, "12A"),
    (11, false, "1B"),
    (8, true, "1A"),
    (6, false, "2B"),
    (3, true, "2A"),
    (1, false, "3B"),
    (10, true, "3A"),
];

fn is_minor(mode: &str) -> bool {
    mode.to_ascii_lowercase().starts_with("min")
}

/// Camelot code for a pitch class (0 = C) and mode (`"major"` / `"minor"`; anything starting
/// with `min` is minor). `None` when either input is missing.
pub fn to_camelot(pitch_class: Option<i32>, mode: Option<&str>) -> Option<&'static str> {
    let pc = pitch_class?.rem_euclid(12) as u8;
    let minor = is_minor(mode?);
    CAMELOT
        .iter()
        .find(|(p, m, _)| *p == pc && *m == minor)
        .map(|(_, _, c)| *c)
}

/// Human key name, e.g. `Am` or `C`. `None` when either input is missing.
pub fn key_name(pitch_class: Option<i32>, mode: Option<&str>) -> Option<String> {
    let pc = pitch_class?.rem_euclid(12) as usize;
    let suffix = if is_minor(mode?) { "m" } else { "" };
    Some(format!("{}{}", PITCH_NAMES[pc], suffix))
}

/// Camelot code (case-insensitive) -> `(pitch class, is_minor)`.
pub fn from_camelot(code: &str) -> Option<(u8, bool)> {
    let up = code.trim().to_ascii_uppercase();
    CAMELOT
        .iter()
        .find(|(_, _, c)| *c == up)
        .map(|(p, m, _)| (*p, *m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn am_is_8a() {
        assert_eq!(to_camelot(Some(9), Some("minor")), Some("8A"));
        assert_eq!(key_name(Some(9), Some("minor")).as_deref(), Some("Am"));
        assert_eq!(to_camelot(Some(0), Some("major")), Some("8B"));
        assert_eq!(key_name(Some(1), Some("major")).as_deref(), Some("C#"));
        assert_eq!(to_camelot(None, Some("minor")), None);
        assert_eq!(key_name(Some(3), None), None);
    }

    #[test]
    fn every_code_round_trips() {
        for (p, m, c) in CAMELOT {
            assert_eq!(from_camelot(c), Some((p, m)));
            let mode = if m { "minor" } else { "major" };
            assert_eq!(to_camelot(Some(p as i32), Some(mode)), Some(c));
        }
        assert_eq!(from_camelot("8a"), Some((9, true)));
        assert_eq!(from_camelot("13A"), None);
    }

    #[test]
    fn pitch_class_wraps() {
        assert_eq!(to_camelot(Some(21), Some("minor")), Some("8A"));
        assert_eq!(to_camelot(Some(-3), Some("minor")), Some("8A"));
    }
}
