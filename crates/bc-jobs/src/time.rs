//! Timestamp helpers. The legacy DB stores naive UTC as `YYYY-MM-DD HH:MM:SS.ffffff`
//! (SQLAlchemy `DateTime`); some rows carry a `+00:00` suffix. Lexicographic order
//! is chronological for both, so `<=` comparisons in SQL work on either.

use chrono::{DateTime, NaiveDateTime, Utc};

const FMT: &str = "%Y-%m-%d %H:%M:%S%.6f";

/// "now" in the DB's text format.
pub fn now() -> String {
    format_db(Utc::now())
}

pub fn plus_secs(secs: f64) -> String {
    format_db(Utc::now() + chrono::Duration::milliseconds((secs * 1000.0) as i64))
}

pub fn format_db(t: DateTime<Utc>) -> String {
    t.naive_utc().format(FMT).to_string()
}

/// Parse either DB flavour (with or without `+00:00` / `T` / `Z`).
pub fn parse_db(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    let core = s.trim_end_matches('Z');
    let core = match core.rfind(['+', '-']) {
        Some(i) if i > 10 && core[i..].contains(':') && core[i..].len() == 6 => &core[..i],
        _ => core,
    };
    for f in [FMT, "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(core, f) {
            return Some(n.and_utc());
        }
    }
    None
}

/// DB text -> RFC 3339 UTC (what the API emits).
pub fn to_iso(s: &str) -> String {
    match parse_db(s) {
        Some(dt) => dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_flavours() {
        let a = parse_db("2026-09-25 06:05:16.035848").unwrap();
        let b = parse_db("2026-09-25 06:05:16.035848+00:00").unwrap();
        assert_eq!(a, b);
        assert_eq!(to_iso("2026-09-25 06:05:16.035848"), "2026-09-25T06:05:16.035848Z");
        assert!(parse_db("garbage").is_none());
    }

    #[test]
    fn now_roundtrips() {
        let n = now();
        assert!(parse_db(&n).is_some());
        assert!(plus_secs(60.0) > n);
    }
}
