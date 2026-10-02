//! Pure bits of the Home page: Top 10 windows, crate-dig paging, the "is there
//! something newer" check behind the refresh dot, and the first-run scan wording.
use bc_types::library::ScanResult;
use crate::logic::format::format_count;

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

/// How many releases the crate holds, judged from the snapshot's single page: a short page
/// is the whole crate; a full one means "at least one more page" until a query says otherwise.
pub fn crate_total_guess(snapshot_len: usize) -> usize {
    if snapshot_len < CRATE_PER_PAGE { snapshot_len } else { CRATE_PER_PAGE + 1 }
}

/// The header line: "1 album · 12 tracks · 1 artist".
pub fn library_summary(releases: i64, tracks: i64, artists: i64) -> String {
    let n = |v: i64, one: &str, many: &str| format!("{} {}", format_count(v), if v == 1 { one } else { many });
    format!("{} · {} · {}", n(releases, "album", "albums"), n(tracks, "track", "tracks"), n(artists, "artist", "artists"))
}

/// The refresh dot: something newer than the hero on show exists.
pub fn is_fresh(shown: Option<i64>, latest: Option<i64>) -> bool {
    matches!((shown, latest), (Some(s), Some(l)) if s != l)
}

// ---- first run ---------------------------------------------------------------------------------

/// What a scan on an empty library came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanOutcome {
    /// Tracks went in: show the shelves.
    Filled,
    /// Files were walked but none were music bc could read.
    Empty { seen: i64 },
    /// Nothing went in and the scan reported why (the first error).
    Errors(String),
}

pub fn scan_outcome(results: &[ScanResult]) -> ScanOutcome {
    let added: i64 = results.iter().map(|r| r.tracks_added + r.files_added + r.files_updated).sum();
    if added > 0 {
        return ScanOutcome::Filled;
    }
    if let Some(e) = results.iter().flat_map(|r| r.errors.first()).next() {
        return ScanOutcome::Errors(format!("The scan added nothing: {e}"));
    }
    ScanOutcome::Empty { seen: results.iter().map(|r| r.files_seen).sum() }
}

pub fn empty_scan_note(seen: i64) -> String {
    match seen {
        0 => "No music turned up in that folder. bc reads MP3, FLAC, AIFF, WAV, M4A, AAC, Ogg, Opus and WMA: check the path, or pick the folder one level up.".into(),
        n => format!("Found {} audio file{}, but none were added. Settings \u{203a} Library has the scan details.", format_count(n), if n == 1 { "" } else { "s" }),
    }
}

pub fn scan_phase_label(phase: Option<&str>) -> &'static str {
    match phase {
        Some("walk") => "Finding files\u{2026}",
        Some("read") => "Reading tags\u{2026}",
        Some("write") => "Adding to the library\u{2026}",
        Some("done") => "Finishing up\u{2026}",
        _ => "Starting the scan\u{2026}",
    }
}

pub fn scan_count(seen: i64, total: Option<i64>) -> String {
    match total.filter(|t| *t > 0) {
        Some(t) => format!("{} / {}", format_count(seen), format_count(t)),
        None => format!("{} files", format_count(seen)),
    }
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
    fn crate_total_trusts_a_short_page() {
        assert_eq!(page_count(crate_total_guess(0), CRATE_PER_PAGE), 1);
        assert_eq!(page_count(crate_total_guess(5), CRATE_PER_PAGE), 1);
        assert_eq!(page_count(crate_total_guess(CRATE_PER_PAGE), CRATE_PER_PAGE), 2);
    }

    fn result(seen: i64, added: i64, errors: &[&str]) -> ScanResult {
        ScanResult {
            root_id: 1,
            root_path: "/m".into(),
            files_seen: seen,
            files_added: added,
            files_updated: 0,
            files_missing: 0,
            files_unchanged: 0,
            tracks_added: added,
            errors: errors.iter().map(|e| e.to_string()).collect(),
            duration_ms: 1,
        }
    }

    #[test]
    fn scan_outcomes() {
        assert_eq!(scan_outcome(&[result(10, 4, &["bad.mp3: no frames"])]), ScanOutcome::Filled);
        assert_eq!(scan_outcome(&[result(3, 0, &[])]), ScanOutcome::Empty { seen: 3 });
        assert_eq!(scan_outcome(&[]), ScanOutcome::Empty { seen: 0 });
        assert!(matches!(scan_outcome(&[result(3, 0, &["permission denied"])]), ScanOutcome::Errors(e) if e.contains("permission denied")));
        assert!(empty_scan_note(1).contains("1 audio file,"));
        assert!(empty_scan_note(0).contains("No music"));
    }

    #[test]
    fn scan_wording() {
        assert_eq!(scan_count(5, Some(10)), "5 / 10");
        assert_eq!(scan_count(5, None), "5 files");
        assert_eq!(scan_count(5, Some(0)), "5 files");
        assert_eq!(scan_phase_label(None), scan_phase_label(Some("queued")));
    }

    #[test]
    fn summary_counts_agree_with_their_nouns() {
        assert_eq!(library_summary(1, 2, 0), "1 album · 2 tracks · 0 artists");
        assert_eq!(library_summary(3, 1, 1), "3 albums · 1 track · 1 artist");
    }

    #[test]
    fn freshness_needs_both_sides() {
        assert!(!is_fresh(None, Some(3)));
        assert!(!is_fresh(Some(3), None));
        assert!(!is_fresh(Some(3), Some(3)));
        assert!(is_fresh(Some(3), Some(4)));
    }
}
