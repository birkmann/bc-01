//! Small SQL / text helpers shared by the recommenders.
//!
//! Id lists are inlined as integer literals (they are `i64`, so injection-proof): that sidesteps
//! the bound-parameter cap for long lists and keeps one statement shape per query.

use std::collections::BTreeSet;

use unicode_normalization::UnicodeNormalization;

/// Chunk size for id lists (legacy `IN_CHUNK`; SQLite's historical variable limit is 999).
pub const IN_CHUNK: usize = 900;

/// `1,2,3` for an `IN (...)` list. Empty gives `NULL` so `IN (NULL)` matches nothing.
pub fn in_list(ids: &[i64]) -> String {
    if ids.is_empty() {
        return "NULL".into();
    }
    let mut s = String::with_capacity(ids.len() * 8);
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&id.to_string());
    }
    s
}

/// A SQL string literal (quotes doubled).
pub fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Legacy `name_key`: NFKD, strip marks, non-alnum to space, lowercase, collapse spaces.
pub fn name_key(value: &str) -> String {
    let stripped: String =
        value.nfkd().filter(|c| !unicode_normalization::char::is_combining_mark(*c)).collect();
    let kept: String = stripped.chars().map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' }).collect();
    kept.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Distinct non-blank name keys of a tag list, as an SQL `IN` list of literals.
pub fn name_key_list<'a>(tags: impl IntoIterator<Item = &'a String>) -> Option<String> {
    let keys: BTreeSet<String> =
        tags.into_iter().filter(|t| !t.trim().is_empty()).map(|t| name_key(t)).collect();
    if keys.is_empty() {
        return None;
    }
    Some(keys.iter().map(|k| lit(k)).collect::<Vec<_>>().join(","))
}

/// Subquery of tracks carrying any of these tags (OR: a genre switch is a choice between doors).
pub fn tagged_tracks_sql(keys_literals: &str) -> String {
    format!(
        "SELECT tt.track_id FROM track_tags tt JOIN tags g ON g.id = tt.tag_id WHERE g.name_key IN ({keys_literals})"
    )
}

/// `EXISTS` form of "has a file on disk" (PLAN 9l: beats `IN (SELECT track_id FROM files ..)`).
pub const PRESENT: &str = "EXISTS (SELECT 1 FROM files f WHERE f.track_id = t.id AND f.missing_since IS NULL)";

/// Unix seconds of a stored timestamp (`2026-09-01 14:21:19.089129`, `...T...Z`).
pub fn parse_ts(s: &str) -> Option<f64> {
    let s = s.trim().trim_end_matches('Z');
    let (date, time) = s.split_once(['T', ' ']).unwrap_or((s, "00:00:00"));
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let m: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let time = time.split(['+', 'Z']).next().unwrap_or(time);
    let mut t = time.split(':');
    let h: f64 = t.next()?.parse().ok()?;
    let mi: f64 = t.next().unwrap_or("0").parse().ok()?;
    let sec: f64 = t.next().unwrap_or("0").parse().ok()?;
    // days from civil (Howard Hinnant)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days as f64 * 86_400.0 + h * 3600.0 + mi * 60.0 + sec)
}

/// Now, in unix seconds.
pub fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Legacy stored timestamp (`YYYY-MM-DD HH:MM:SS.ffffff`) -> ISO-8601 UTC for the API.
pub fn iso(ts: Option<String>) -> Option<String> {
    ts.map(|s| {
        if s.contains('T') {
            s
        } else {
            format!("{}Z", s.replacen(' ', "T", 1))
        }
    })
}

/// Python `round(x, 4)`-style rounding used by the scorers.
pub fn round4(x: f64) -> f64 {
    bc_music::setmath::round_to(x, 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_key_folds_like_the_legacy_one() {
        assert_eq!(name_key("Motörhead"), "motorhead");
        assert_eq!(name_key("  Dub-Techno  "), "dub techno");
        assert_eq!(name_key("A  &  B"), "a b");
    }

    #[test]
    fn timestamps_parse() {
        let a = parse_ts("2026-08-17 12:00:00").unwrap();
        let b = parse_ts("2026-08-17T10:00:00.5Z").unwrap();
        assert!((a - b - 7199.5).abs() < 1e-6);
        assert_eq!(parse_ts("1970-01-02 00:00:00"), Some(86_400.0));
    }

    #[test]
    fn iso_converts() {
        assert_eq!(iso(Some("2026-09-01 14:21:19.089129".into())).unwrap(), "2026-09-01T14:21:19.089129Z");
    }
}
