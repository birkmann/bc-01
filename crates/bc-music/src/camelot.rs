//! Musical key, Camelot notation, and harmonic compatibility.
//!
//! Port of `services/metadata/camelot.py`. The Camelot wheel maps each of the 24 keys to a
//! number 1-12 plus a letter (A = minor, B = major). Adjacent numbers are a fifth apart.

use bc_types::analysis::{Compatibility, Verdict};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Major,
    Minor,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Major => "major",
            Mode::Minor => "minor",
        }
    }
    /// Anything starting with `min` is minor, everything else major (legacy rule).
    pub fn parse(s: &str) -> Mode {
        if s.to_ascii_lowercase().starts_with("min") { Mode::Minor } else { Mode::Major }
    }
}

pub const PITCH_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

/// (pitch class, mode) -> Camelot code. Pitch class 0 = C.
pub const CAMELOT: [(u8, Mode, &str); 24] = [
    (8, Mode::Major, "4B"),
    (5, Mode::Minor, "4A"),
    (3, Mode::Major, "5B"),
    (0, Mode::Minor, "5A"),
    (10, Mode::Major, "6B"),
    (7, Mode::Minor, "6A"),
    (5, Mode::Major, "7B"),
    (2, Mode::Minor, "7A"),
    (0, Mode::Major, "8B"),
    (9, Mode::Minor, "8A"),
    (7, Mode::Major, "9B"),
    (4, Mode::Minor, "9A"),
    (2, Mode::Major, "10B"),
    (11, Mode::Minor, "10A"),
    (9, Mode::Major, "11B"),
    (6, Mode::Minor, "11A"),
    (4, Mode::Major, "12B"),
    (1, Mode::Minor, "12A"),
    (11, Mode::Major, "1B"),
    (8, Mode::Minor, "1A"),
    (6, Mode::Major, "2B"),
    (3, Mode::Minor, "2A"),
    (1, Mode::Major, "3B"),
    (10, Mode::Minor, "3A"),
];

pub fn to_camelot(pitch_class: i32, mode: Mode) -> &'static str {
    let pc = pitch_class.rem_euclid(12) as u8;
    CAMELOT.iter().find(|(p, m, _)| *p == pc && *m == mode).map(|(_, _, c)| *c).unwrap_or("")
}

/// Legacy-shaped helper: `None` when either input is missing.
pub fn to_camelot_opt(pitch_class: Option<i32>, mode: Option<&str>) -> Option<&'static str> {
    Some(to_camelot(pitch_class?, Mode::parse(mode?)))
}

/// Human key, e.g. `Am` or `C`.
pub fn key_name(pitch_class: i32, mode: Mode) -> String {
    let suffix = if mode == Mode::Minor { "m" } else { "" };
    format!("{}{}", PITCH_NAMES[pitch_class.rem_euclid(12) as usize], suffix)
}

pub fn key_name_opt(pitch_class: Option<i32>, mode: Option<&str>) -> Option<String> {
    Some(key_name(pitch_class?, Mode::parse(mode?)))
}

/// Camelot code -> (pitch class, mode). Case-insensitive.
pub fn from_camelot(code: &str) -> Option<(u8, Mode)> {
    let up = code.to_ascii_uppercase();
    CAMELOT.iter().find(|(_, _, c)| *c == up).map(|(p, m, _)| (*p, *m))
}

fn split_code(code: &str) -> Option<(i32, char)> {
    let up = code.to_ascii_uppercase();
    from_camelot(&up)?;
    let letter = up.chars().last()?;
    let n: i32 = up[..up.len() - 1].parse().ok()?;
    Some((n, letter))
}

/// Parse `Am`, `A minor`, `F#` or a Camelot code like `8A`.
pub fn parse_key(text: &str) -> Option<(u8, Mode)> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    // Camelot code: 1-2 digits + A/B
    let b = t.as_bytes();
    let digits = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if (1..=2).contains(&digits) {
        let rest = t[digits..].trim();
        if rest.len() == 1 && matches!(rest.as_bytes()[0], b'A' | b'a' | b'B' | b'b') {
            let n: u32 = t[..digits].parse().ok()?;
            return from_camelot(&format!("{}{}", n, rest.to_ascii_uppercase()));
        }
    }

    let mut chars = t.chars().peekable();
    let letter = chars.next()?.to_ascii_uppercase();
    let base = match letter {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };
    let mut rest: String = chars.collect();
    let mut pitch: i32 = base;
    let trimmed = rest.trim_start().to_string();
    rest = trimmed;
    if let Some(c) = rest.chars().next() {
        let acc = match c {
            '#' | '♯' => Some(1),
            'b' | 'B' | '♭' => Some(-1),
            _ => None,
        };
        // 'b'/'B' is only an accidental when something valid (or nothing) follows it.
        if let Some(delta) = acc {
            let after = rest[c.len_utf8()..].trim();
            if is_quality(after) {
                pitch += delta;
                rest = after.to_string();
            }
        }
    }
    let q = rest.trim();
    if !is_quality(q) {
        return None;
    }
    let minor = q.to_ascii_lowercase().starts_with("min") || q == "m";
    Some((pitch.rem_euclid(12) as u8, if minor { Mode::Minor } else { Mode::Major }))
}

fn is_quality(q: &str) -> bool {
    matches!(q.to_ascii_lowercase().as_str(), "" | "maj" | "major" | "min" | "minor" | "m")
}

fn compat(score: f64, verdict: Verdict, reason: impl Into<String>) -> Compatibility {
    Compatibility { score, verdict, reason: reason.into() }
}

/// Score a transition between two Camelot codes.
pub fn key_compatibility(a: Option<&str>, b: Option<&str>) -> Compatibility {
    let (Some(a), Some(b)) = (a.filter(|s| !s.is_empty()), b.filter(|s| !s.is_empty())) else {
        return compat(0.0, Verdict::Clash, "key unknown");
    };
    let (Some((n_a, l_a)), Some((n_b, l_b))) = (split_code(a), split_code(b)) else {
        return compat(0.0, Verdict::Clash, "unrecognised key");
    };
    let delta = (n_b - n_a).rem_euclid(12);

    if l_a == l_b {
        match delta {
            0 => return compat(1.0, Verdict::Perfect, "same key"),
            1 | 11 => {
                let dir = if delta == 1 { "up" } else { "down" };
                return compat(0.9, Verdict::Good, format!("one step {dir} the wheel"));
            }
            7 => return compat(0.55, Verdict::Energy, "+7 energy boost"),
            2 | 10 => return compat(0.6, Verdict::Risky, "two steps -- audition first"),
            _ => {}
        }
    } else {
        if delta == 0 {
            return compat(0.85, Verdict::Good, "relative major/minor");
        }
        if (delta == 3 && l_a == 'A') || (delta == 9 && l_a == 'B') {
            return compat(0.65, Verdict::Risky, "diagonal mix");
        }
    }
    compat(0.0, Verdict::Clash, "keys clash")
}

/// Every Camelot code that mixes acceptably with `code` (deduped, stable order).
pub fn compatible_keys(code: Option<&str>, include_risky: bool) -> Vec<String> {
    let Some((number, letter)) = code.and_then(split_code) else { return vec![] };
    let other = if letter == 'A' { 'B' } else { 'A' };
    let mut out = vec![
        format!("{number}{letter}"),
        format!("{}{letter}", (number % 12) + 1),
        format!("{}{letter}", ((number - 2).rem_euclid(12)) + 1),
        format!("{number}{other}"),
        format!("{}{letter}", ((number + 6) % 12) + 1),
    ];
    if include_risky {
        out.push(format!("{}{letter}", ((number + 1) % 12) + 1));
        out.push(format!("{}{letter}", ((number - 3).rem_euclid(12)) + 1));
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|c| seen.insert(c.clone()));
    out
}

pub const DEFAULT_BPM_TOLERANCE: f64 = 0.06;

/// Score a tempo transition, honouring half and double time.
pub fn bpm_compatibility(a: Option<f64>, b: Option<f64>, tolerance: f64) -> Compatibility {
    let (Some(a), Some(b)) = (a, b) else { return compat(0.0, Verdict::Clash, "tempo unknown") };
    if a.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) || b.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return compat(0.0, Verdict::Clash, "tempo unknown");
    }
    let mut best = compat(0.0, Verdict::Clash, "tempo too far apart");
    let multipliers: [(f64, &str); 5] =
        [(1.0, ""), (2.0, " (double time)"), (0.5, " (half time)"), (1.5, " (3:2)"), (2.0 / 3.0, " (2:3)")];
    for (m, label) in multipliers {
        let drift = (b * m - a).abs() / a;
        if drift > tolerance {
            continue;
        }
        let penalty = if m == 1.0 { 1.0 } else { 0.85 };
        let score = (1.0 - drift / tolerance) * penalty;
        let percent = drift * 100.0;
        let verdict = if drift <= 0.01 {
            Verdict::Perfect
        } else if drift <= 0.03 {
            Verdict::Good
        } else {
            Verdict::Risky
        };
        if score > best.score {
            best = compat(score, verdict, format!("{percent:.1}% apart{label}"));
        }
    }
    best
}

/// Semitones a tempo change moves the key when key-lock is off (banker's rounding, like the
/// Python original).
pub fn pitch_shift_semitones(percent: f64) -> i32 {
    if percent == 0.0 {
        return 0;
    }
    (12.0 * (1.0 + percent / 100.0).log2()).round_ties_even() as i32
}

/// Move a Camelot code by `semitones`: a semitone is seven steps around the wheel.
pub fn transpose(code: Option<&str>, semitones: i32) -> Option<String> {
    let code = code?;
    let Some((number, letter)) = split_code(code) else { return Some(code.to_string()) };
    if semitones == 0 {
        return Some(code.to_string());
    }
    let shifted = (number - 1 + semitones * 7).rem_euclid(12) + 1;
    Some(format!("{shifted}{letter}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_codes() -> Vec<&'static str> {
        CAMELOT.iter().map(|c| c.2).collect()
    }

    #[test]
    fn all_24_keys_map_to_distinct_codes() {
        let set: std::collections::HashSet<_> = all_codes().into_iter().collect();
        assert_eq!(CAMELOT.len(), 24);
        assert_eq!(set.len(), 24);
    }

    #[test]
    fn wheel_anchors() {
        assert_eq!(to_camelot(9, Mode::Minor), "8A");
        assert_eq!(to_camelot(0, Mode::Major), "8B");
        assert_eq!(to_camelot(4, Mode::Minor), "9A");
        assert_eq!(to_camelot(7, Mode::Major), "9B");
    }

    #[test]
    fn relative_major_minor_share_a_number() {
        let a = to_camelot(9, Mode::Minor);
        let b = to_camelot(0, Mode::Major);
        assert_eq!(&a[..a.len() - 1], &b[..b.len() - 1]);
    }

    #[test]
    fn to_camelot_handles_missing_input() {
        assert_eq!(to_camelot_opt(None, Some("minor")), None);
        assert_eq!(to_camelot_opt(Some(9), None), None);
        assert_eq!(to_camelot_opt(Some(9), Some("minor")), Some("8A"));
    }

    #[test]
    fn key_names() {
        assert_eq!(key_name(9, Mode::Minor), "Am");
        assert_eq!(key_name(0, Mode::Major), "C");
        assert_eq!(key_name(6, Mode::Minor), "F#m");
    }

    #[test]
    fn parse_key_cases() {
        let cases: [(&str, Option<(u8, Mode)>); 10] = [
            ("Am", Some((9, Mode::Minor))),
            ("A minor", Some((9, Mode::Minor))),
            ("A", Some((9, Mode::Major))),
            ("C", Some((0, Mode::Major))),
            ("F#m", Some((6, Mode::Minor))),
            ("Bb", Some((10, Mode::Major))),
            ("8A", Some((9, Mode::Minor))),
            ("8B", Some((0, Mode::Major))),
            ("nonsense", None),
            ("", None),
        ];
        for (t, e) in cases {
            assert_eq!(parse_key(t), e, "{t}");
        }
        assert_eq!(parse_key("Bm"), Some((11, Mode::Minor)));
        assert_eq!(parse_key("Bbm"), Some((10, Mode::Minor)));
        assert_eq!(parse_key("Abmin"), Some((8, Mode::Minor)));
        assert_eq!(parse_key("B"), Some((11, Mode::Major)));
    }

    fn kc(a: &str, b: &str) -> Compatibility {
        key_compatibility(Some(a), Some(b))
    }

    #[test]
    fn same_key_is_perfect() {
        let r = kc("8A", "8A");
        assert_eq!(r.verdict, Verdict::Perfect);
        assert_eq!(r.score, 1.0);
    }

    #[test]
    fn wheel_neighbours_are_good() {
        assert_eq!(kc("8A", "9A").verdict, Verdict::Good);
        assert_eq!(kc("8A", "7A").verdict, Verdict::Good);
    }

    #[test]
    fn relative_major_minor_is_good() {
        assert_eq!(kc("8A", "8B").verdict, Verdict::Good);
    }

    #[test]
    fn energy_boost_is_recognised() {
        assert_eq!(kc("8A", "3A").verdict, Verdict::Energy);
    }

    #[test]
    fn distant_keys_clash() {
        assert_eq!(kc("8A", "2A").verdict, Verdict::Clash);
        assert_eq!(kc("8A", "1B").verdict, Verdict::Clash);
    }

    #[test]
    fn wheel_wraps_around_12() {
        assert_eq!(kc("12A", "1A").verdict, Verdict::Good);
        assert_eq!(kc("1A", "12A").verdict, Verdict::Good);
    }

    #[test]
    fn unknown_key_never_claims_compatibility() {
        assert_eq!(key_compatibility(None, Some("8A")).verdict, Verdict::Clash);
        assert_eq!(kc("8A", "99Z").verdict, Verdict::Clash);
    }

    #[test]
    fn compatible_keys_are_all_actually_compatible() {
        for code in all_codes() {
            for cand in compatible_keys(Some(code), false) {
                assert!(kc(code, &cand).ok(), "{code} -> {cand}");
            }
        }
    }

    #[test]
    fn compatible_keys_includes_the_expected_neighbours() {
        let keys = compatible_keys(Some("8A"), false);
        for k in ["8A", "9A", "7A", "8B"] {
            assert!(keys.iter().any(|x| x == k), "{k}");
        }
    }

    fn bc(a: f64, b: f64) -> Compatibility {
        bpm_compatibility(Some(a), Some(b), DEFAULT_BPM_TOLERANCE)
    }

    #[test]
    fn identical_tempo_is_perfect() {
        assert_eq!(bc(128.0, 128.0).verdict, Verdict::Perfect);
    }

    #[test]
    fn small_drift_is_acceptable() {
        assert!(bc(128.0, 130.0).ok());
    }

    #[test]
    fn half_and_double_time_are_the_same_tempo() {
        assert!(bc(174.0, 87.0).ok());
        assert!(bc(87.0, 174.0).ok());
        assert!(bc(174.0, 87.0).reason.contains("time"));
    }

    #[test]
    fn distant_tempo_clashes() {
        assert_eq!(bc(128.0, 100.0).verdict, Verdict::Clash);
    }

    #[test]
    fn missing_tempo_never_claims_compatibility() {
        assert_eq!(bpm_compatibility(None, Some(128.0), 0.06).verdict, Verdict::Clash);
        assert_eq!(bc(128.0, 0.0).verdict, Verdict::Clash);
    }

    #[test]
    fn pitch_shift_semitones_values() {
        assert_eq!(pitch_shift_semitones(0.0), 0);
        assert_eq!(pitch_shift_semitones(6.0), 1);
        assert_eq!(pitch_shift_semitones(-6.0), -1);
        assert_eq!(pitch_shift_semitones(12.5), 2);
    }

    #[test]
    fn transpose_moves_seven_steps_per_semitone() {
        assert_eq!(transpose(Some("8A"), 1).as_deref(), Some("3A"));
        assert_eq!(transpose(Some("8A"), 0).as_deref(), Some("8A"));
        assert_eq!(transpose(None, 1), None);
    }

    #[test]
    fn transpose_round_trips() {
        for code in all_codes() {
            let up = transpose(Some(code), 3).unwrap();
            assert_eq!(transpose(Some(&up), -3).as_deref(), Some(code));
        }
    }
}
