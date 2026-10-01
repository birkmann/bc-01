//! Pure bits of the Home page: Top 10 windows, crate-dig paging, the "is there
//! something newer" check behind the refresh dot.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    All,
    Days(i64),
}

pub struct Window {
    pub range: Range,
    pub label: &'static str,
    pub noun: &'static str,
}

pub const WINDOWS: [Window; 5] = [
    Window { range: Range::All, label: "All time", noun: "all time" },
    Window { range: Range::Days(7), label: "7 days", noun: "the last 7 days" },
    Window { range: Range::Days(30), label: "30 days", noun: "the last 30 days" },
    Window { range: Range::Days(90), label: "90 days", noun: "the last 90 days" },
    Window { range: Range::Days(365), label: "Year", noun: "the last year" },
];

pub const RANGE_KEY: &str = "bc:home:top-ten-window";

pub fn range_to_string(r: Range) -> String {
    match r {
        Range::All => "all".into(),
        Range::Days(d) => d.to_string(),
    }
}

/// What the choice was stored as; anything else is the default.
pub fn parse_range(raw: Option<&str>) -> Range {
    match raw {
        Some("all") => Range::All,
        Some(s) => s.parse::<i64>().ok().filter(|d| WINDOWS.iter().any(|w| w.range == Range::Days(*d))).map(Range::Days).unwrap_or(Range::All),
        None => Range::All,
    }
}

pub fn window_of(r: Range) -> &'static Window {
    WINDOWS.iter().find(|w| w.range == r).unwrap_or(&WINDOWS[0])
}

pub const MAX_SEED: i64 = 1_000_000;

/// The seed after `roll` re-deals, always in `1..=MAX_SEED`.
pub fn seed_for(session: i64, roll: i64) -> i64 {
    ((session + roll * 7919 - 1).rem_euclid(MAX_SEED)) + 1
}

pub const CRATE_PER_PAGE: usize = 12;

pub fn page_count(total: usize, per_page: usize) -> usize {
    total.div_ceil(per_page.max(1)).max(1)
}

/// The refresh dot: something newer than the hero on show exists.
pub fn is_fresh(shown: Option<i64>, latest: Option<i64>) -> bool {
    matches!((shown, latest), (Some(s), Some(l)) if s != l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_roundtrip_and_fallback() {
        for w in WINDOWS.iter() {
            assert_eq!(parse_range(Some(&range_to_string(w.range))), w.range);
        }
        assert_eq!(parse_range(Some("12")), Range::All);
        assert_eq!(parse_range(Some("junk")), Range::All);
        assert_eq!(parse_range(None), Range::All);
    }

    #[test]
    fn seeds_stay_in_range_and_differ_per_roll() {
        for roll in 0..50 {
            let s = seed_for(999_999, roll);
            assert!((1..=MAX_SEED).contains(&s));
        }
        assert_ne!(seed_for(5, 0), seed_for(5, 1));
        assert_eq!(seed_for(5, 0), 5);
    }

    #[test]
    fn paging_math() {
        assert_eq!(page_count(0, 12), 1);
        assert_eq!(page_count(12, 12), 1);
        assert_eq!(page_count(13, 12), 2);
    }

    #[test]
    fn freshness_needs_both_sides() {
        assert!(!is_fresh(None, Some(3)));
        assert!(!is_fresh(Some(3), None));
        assert!(!is_fresh(Some(3), Some(3)));
        assert!(is_fresh(Some(3), Some(4)));
    }
}
