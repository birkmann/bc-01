//! Finding albums that are not music you would ever play (port of `services/library/cleanup.py`).
//!
//! A Bandcamp collection accumulates two kinds of record that look like albums and are useless in
//! a DJ set: **sample packs** (dozens of half-second files) and **preview stubs** (one 40-second
//! teaser). Two independent signals, reported separately because they fail in opposite
//! directions:
//!
//! * `short-tracks`: the longest track is under a threshold (objective; the caller chooses it).
//! * `title`: the title says what it is. A title is prose and prose lies ("Loop Research", every
//!   label *sampler*), so the phrases are narrow, matched on word boundaries, and a title hit
//!   alone selects nothing -- it is shown as a reason and the user decides.
//!
//! Nothing here deletes anything.

use bc_db::rusqlite::{Connection, OptionalExtension};
use bc_db::util::name_key;
use bc_libcore::ApiResult;

/// Longest-track threshold: 60s clears every sample pack in the library this was built against
/// while leaving the shortest real record (a 61s locked-groove side) alone.
pub const DEFAULT_MAX_TRACK_MS: i64 = 60_000;

/// Phrases, never bare words ("sample" alone would condemn every label *sampler*).
pub const JUNK_PHRASES: &[&str] = &[
    "sample pack", "samples pack", "sample packs", "sample bundle", "samples", "preset", "presets", "oneshot", "oneshots", "one shot",
    "one shots", "drum kit", "drumkit", "toolkit", "loop pack", "loops pack", "loop kit", "loops vol", "midi pack", "midi kit",
    "construction kit", "sound kit", "stem pack", "stems pack",
];

/// Which junk phrases this title contains, on word boundaries. Folds through [`name_key`] first,
/// so punctuation and case cannot hide a phrase.
pub fn title_flags(title: &str) -> Vec<&'static str> {
    let folded = format!(" {} ", name_key(title));
    JUNK_PHRASES.iter().copied().filter(|p| folded.contains(&format!(" {p} "))).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub release_id: i64,
    /// Longest track; `None` when no track has a readable duration (never flagged short).
    pub longest_ms: Option<i64>,
    pub track_count: i64,
    /// `short-tracks` and/or `title`.
    pub reasons: Vec<String>,
    pub matched_phrases: Vec<String>,
}

/// Releases flagged by either signal, shortest first (the half-second kick packs first, the
/// judgement calls sink to the bottom). A release whose tracks all have an unreadable duration is
/// never flagged by duration: unknown is not short.
pub fn find_candidates(c: &Connection, max_track_ms: i64, include_titles: bool) -> ApiResult<Vec<Candidate>> {
    let mut st = c.prepare(
        "SELECT r.id, r.title, MAX(t.duration_ms), COUNT(t.id) FROM releases r JOIN tracks t ON t.release_id = r.id GROUP BY r.id",
    )?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, i64>(3)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (id, title, longest, tracks) = row?;
        let mut reasons = Vec::new();
        if longest.is_some_and(|l| l < max_track_ms) {
            reasons.push("short-tracks".to_string());
        }
        let phrases: Vec<String> = if include_titles { title_flags(&title).into_iter().map(String::from).collect() } else { vec![] };
        if !phrases.is_empty() {
            reasons.push("title".to_string());
        }
        if reasons.is_empty() {
            continue;
        }
        out.push(Candidate { release_id: id, longest_ms: longest, track_count: tracks, reasons, matched_phrases: phrases });
    }
    // Unknown durations sort last: they are title-only hits, which most need a human look.
    out.sort_by_key(|c| (c.longest_ms.is_none(), c.longest_ms.unwrap_or(0), c.release_id));
    Ok(out)
}

/// The performer's display name, for the blacklist's name key and its UI.
pub fn release_artist_name(c: &Connection, release_id: i64) -> ApiResult<String> {
    Ok(c.query_row("SELECT COALESCE(a.name,'') FROM releases r LEFT JOIN artists a ON a.id = r.artist_id WHERE r.id = ?1", [release_id], |r| r.get(0))
        .optional()?
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    /// Seed one release with tracks of the given durations; with `on_disk` real files too.
    fn album(db: &bc_db::Db, artist: &str, title: &str, durations: &[Option<i64>]) -> i64 {
        let rid = seed_release(db, artist, title, None, None);
        for (n, ms) in durations.iter().enumerate() {
            let t = seed_track(db, rid, &format!("{title} {}", n + 1), Some(n as i64 + 1));
            if let Some(ms) = ms {
                exec(db, &format!("UPDATE tracks SET duration_ms={ms} WHERE id={t}"));
            }
        }
        rid
    }

    fn found(db: &bc_db::Db, max_s: i64, titles: bool) -> Vec<Candidate> {
        db.read(move |c| Ok(find_candidates(c, max_s * 1000, titles).unwrap())).unwrap()
    }

    #[test]
    fn title_phrases_spare_the_words_they_live_inside() {
        assert_eq!(title_flags("Raw Kicks Samples Pack"), vec!["samples pack", "samples"]);
        assert_eq!(title_flags("Sample-Pack Vol.1"), vec!["sample pack"]);
        assert_eq!(title_flags("TOOLKIT III"), vec!["toolkit"]);
        for innocent in ["VA SAMPLER", "Album Sampler", "Loop Research", "Time Looper EP"] {
            assert!(title_flags(innocent).is_empty(), "{innocent}");
        }
    }

    #[test]
    fn short_tracks_flagged_only_below_the_threshold() {
        let db = test_db();
        let kicks = album(&db, "Somebody", "Analog Kicks", &[Some(900), Some(1200), Some(800)]);
        let record = album(&db, "Somebody", "A Real Record", &[Some(380_000), Some(420_000)]);
        let at60 = found(&db, 60, true);
        assert_eq!(at60.iter().map(|c| c.release_id).collect::<Vec<_>>(), vec![kicks]);
        assert_eq!(at60[0].reasons, vec!["short-tracks"]);
        assert_eq!(at60[0].longest_ms, Some(1200));
        // Widening the threshold past the real record catches it too -- which is exactly why the
        // threshold is the user's to choose.
        let mut at600: Vec<i64> = found(&db, 600, true).iter().map(|c| c.release_id).collect();
        at600.sort();
        assert_eq!(at600, vec![kicks, record]);
    }

    #[test]
    fn unreadable_durations_are_never_junk() {
        let db = test_db();
        album(&db, "Somebody", "Unknown Lengths", &[None, None]);
        assert!(found(&db, 600, true).is_empty());
    }

    #[test]
    fn a_title_hit_alone_is_reported_with_its_phrase() {
        let db = test_db();
        album(&db, "Producer", "Techno Sample Pack Vol. I", &[Some(240_000)]);
        let body = found(&db, 60, true);
        assert_eq!(body.len(), 1);
        assert_eq!(body[0].reasons, vec!["title"]);
        assert_eq!(body[0].matched_phrases, vec!["sample pack"]);
        assert!(found(&db, 60, false).is_empty());
    }

    #[test]
    fn both_signals_are_reported_together() {
        let db = test_db();
        album(&db, "Producer", "Hihats & Rides Sample Pack", &[Some(1000), Some(1100)]);
        assert_eq!(found(&db, 60, true)[0].reasons, vec!["short-tracks", "title"]);
    }
}
