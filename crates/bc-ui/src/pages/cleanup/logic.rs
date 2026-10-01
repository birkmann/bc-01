//! Pure cleanup logic: threshold persistence, selection maths, chunked-delete
//! aggregation and the result sentence. Native-testable.
use std::collections::BTreeSet;

use bc_types::library::maint::{CleanupCandidate, DeleteReleasesResult};

pub const THRESHOLDS: [i64; 6] = [20, 45, 60, 90, 120, 180];
pub const DEFAULT_THRESHOLD: i64 = 60;
pub const THRESHOLD_KEY: &str = "bc:cleanup:threshold:v1";
/// Releases deleted per request, so a big selection shows progress and can be stopped.
pub const CHUNK: usize = 25;

pub fn parse_threshold(stored: Option<&str>) -> i64 {
    stored.and_then(|s| s.trim().parse::<i64>().ok()).filter(|n| THRESHOLDS.contains(n)).unwrap_or(DEFAULT_THRESHOLD)
}

/// Toggle `id` in the selection.
pub fn toggle(sel: &mut BTreeSet<i64>, id: i64) {
    if !sel.remove(&id) {
        sel.insert(id);
    }
}

pub fn all_ids(items: &[CleanupCandidate]) -> BTreeSet<i64> {
    items.iter().map(|i| i.release.id).collect()
}

/// Tracks held by the selected candidates.
pub fn selected_tracks(items: &[CleanupCandidate], sel: &BTreeSet<i64>) -> i64 {
    items.iter().filter(|i| sel.contains(&i.release.id)).map(|i| i.track_count).sum()
}

/// Selection restricted to what is still listed (a new list must never carry ids that are off screen).
pub fn retain_listed(sel: &BTreeSet<i64>, items: &[CleanupCandidate]) -> BTreeSet<i64> {
    let listed = all_ids(items);
    sel.intersection(&listed).copied().collect()
}

pub fn merge_results(parts: &[DeleteReleasesResult]) -> DeleteReleasesResult {
    let mut out = DeleteReleasesResult::default();
    for p in parts {
        out.releases += p.releases;
        out.tracks += p.tracks;
        out.files += p.files;
        out.blacklisted += p.blacklisted;
        out.inbox_ignored += p.inbox_ignored;
        out.errors.extend(p.errors.iter().cloned());
    }
    out
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

pub fn result_sentence(r: &DeleteReleasesResult) -> String {
    let mut s = format!("Deleted {} and {}", plural(r.releases, "album", "albums"), plural(r.files, "file", "files"));
    if r.blacklisted > 0 {
        s += &format!(", blacklisted {}", r.blacklisted);
    }
    if r.inbox_ignored > 0 {
        s += &format!(", retired {} inbox items", r.inbox_ignored);
    }
    if !r.errors.is_empty() {
        s += &format!(", {} failed: {}", r.errors.len(), r.errors[0]);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_types::library::ReleaseOut;

    fn cand(id: i64, tracks: i64) -> CleanupCandidate {
        CleanupCandidate { release: ReleaseOut { id, ..Default::default() }, longest_ms: Some(1000), track_count: tracks, reasons: vec![], matched_phrases: vec![] }
    }

    #[test]
    fn threshold_parse() {
        assert_eq!(parse_threshold(Some("90")), 90);
        assert_eq!(parse_threshold(Some("91")), DEFAULT_THRESHOLD);
        assert_eq!(parse_threshold(None), DEFAULT_THRESHOLD);
        assert_eq!(parse_threshold(Some("x")), DEFAULT_THRESHOLD);
    }

    #[test]
    fn selection() {
        let items = vec![cand(1, 3), cand(2, 5), cand(3, 1)];
        let mut sel = BTreeSet::new();
        toggle(&mut sel, 1);
        toggle(&mut sel, 3);
        assert_eq!(selected_tracks(&items, &sel), 4);
        toggle(&mut sel, 1);
        assert_eq!(sel.len(), 1);
        let fewer = vec![cand(2, 5)];
        assert!(retain_listed(&sel, &fewer).is_empty());
        assert_eq!(all_ids(&items).len(), 3);
    }

    #[test]
    fn aggregate_and_sentence() {
        let a = DeleteReleasesResult { releases: 2, tracks: 9, files: 9, blacklisted: 2, inbox_ignored: 0, errors: vec![] };
        let b = DeleteReleasesResult { releases: 1, tracks: 1, files: 1, blacklisted: 1, inbox_ignored: 3, errors: vec!["disk busy".into()] };
        let m = merge_results(&[a, b]);
        assert_eq!((m.releases, m.files, m.blacklisted, m.inbox_ignored), (3, 10, 3, 3));
        let s = result_sentence(&m);
        assert!(s.starts_with("Deleted 3 albums and 10 files"));
        assert!(s.contains("blacklisted 3") && s.contains("retired 3 inbox items") && s.contains("1 failed: disk busy"));
        assert_eq!(result_sentence(&DeleteReleasesResult { releases: 1, files: 1, ..Default::default() }), "Deleted 1 album and 1 file");
    }
}
