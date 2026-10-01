//! Small shared helpers: name keys, timestamps in the DB's own format, shuffle hash.

use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use unicode_normalization::UnicodeNormalization;

/// Fold a name to a comparison key (port of `db.models.name_key`): NFKD, strip
/// combining marks, keep alphanumerics and whitespace, lowercase, collapse spaces.
pub fn name_key(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_space = true;
    for c in value.nfkd() {
        if unicode_combining(c) {
            continue;
        }
        let c = if c.is_alphanumeric() { c } else { ' ' };
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            for l in c.to_lowercase() {
                out.push(l);
            }
            last_space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

fn unicode_combining(c: char) -> bool {
    // Combining diacritical marks and friends (what Python's unicodedata.combining() > 0 covers
    // for the scripts we meet in music metadata).
    matches!(c as u32,
        0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F
        | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x05BF | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7
        | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x06E7..=0x06E8 | 0x06EA..=0x06ED
        | 0x3099..=0x309A)
}

/// `now` in the format the legacy schema stores (`YYYY-MM-DD HH:MM:SS.ffffff`, naive UTC).
pub fn now_db() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

/// A unix timestamp (seconds) in DB format.
pub fn db_from_unix(secs: i64) -> String {
    Utc.timestamp_opt(secs, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
        .unwrap_or_default()
}

/// DB timestamp -> ISO-8601 UTC for API output (`2026-07-28T07:06:39.490752Z`).
pub fn iso(db: &str) -> String {
    if db.is_empty() {
        return String::new();
    }
    let mut s = db.replacen(' ', "T", 1);
    if !(s.ends_with('Z') || s.contains('+') || s[10.min(s.len())..].contains('-')) {
        s.push('Z');
    }
    s
}

pub fn iso_opt(db: Option<String>) -> Option<String> {
    db.map(|s| iso(&s)).filter(|s| !s.is_empty())
}

/// Parse an incoming bound (`2026-07-01`, `2026-07-01T10:00:00`, RFC 3339 with offset)
/// into the DB's naive-UTC text form so it compares correctly as a string. A naive value
/// is taken to be UTC; an aware one is converted first (the legacy `_utc` rule).
pub fn parse_bound(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // No fractional part when it is zero: stored values are sometimes `YYYY-MM-DD HH:MM:SS` and
    // `...:00` sorts before `...:00.000000` as text.
    use chrono::Timelike;
    let fmt = |d: NaiveDateTime| {
        if d.nanosecond() == 0 { d.format("%Y-%m-%d %H:%M:%S").to_string() } else { d.format("%Y-%m-%d %H:%M:%S%.6f").to_string() }
    };
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(fmt(d.with_timezone(&Utc).naive_utc()));
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(s, f) {
            return Some(fmt(d));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0).map(fmt);
    }
    None
}

pub fn iso_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Knuth multiplicative hash constants (shared with the fan-walk shuffle).
pub const SHUFFLE_MULT: i64 = 2_654_435_761;
pub const SHUFFLE_MOD: i64 = 1 << 32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_keys() {
        assert_eq!(name_key("Motörhead"), "motorhead");
        assert_eq!(name_key("  MOTORHEAD!! "), "motorhead");
        assert_eq!(name_key("Boards of Canada"), "boards of canada");
        assert_eq!(name_key("A--B"), "a b");
        assert_eq!(name_key("Ｆｕｌｌ"), "full");
        assert_eq!(name_key(""), "");
    }

    #[test]
    fn bounds_and_iso() {
        assert_eq!(parse_bound("2026-07-01").unwrap(), "2026-07-01 00:00:00");
        assert_eq!(parse_bound("2026-07-01T10:00:00+02:00").unwrap(), "2026-07-01 08:00:00");
        assert_eq!(iso("2026-07-28 07:06:39.490752"), "2026-07-28T07:06:39.490752Z");
        assert!(parse_bound("garbage").is_none());
    }
}
