//! Teaser clips filed as if they were tracks (port of `services/library/snippets.py`).
//!
//! A Bandcamp record that is not for sale as a download still has a page, and a page still has
//! audio: a 90-second cut of each side, or one montage of the whole record. Nothing in the file
//! says so; the label says it in the title, which is prose -- so this module is a reader of
//! prose, and it is wrong in the direction that costs least: a missed snippet is one bad track,
//! a false positive hides music the user owns. Three narrow rules, ordered by how much the
//! wording commits to:
//!
//! * `bracket`: a parenthetical says what the file *is* (`[SNIPPET]`, `(clip only!)`);
//!   every marker word counts inside brackets, including `clip`.
//! * `tail`: a marker alone after the last separator (`DJUS - Clip`, `Iso EP // Clips`).
//! * `suffix`: the title simply ends in a (non-`clip`) marker word (`Preview Snippets`),
//!   except the idiom *brain teaser*.
//!
//! Duration is deliberately not a signal. The verdict is stored (`tracks.is_snippet`,
//! `releases.snippet_only`) and re-read when [`RULES_VERSION`] changes.
//!
//! The classifier [`is_snippet_title`] is also called by the scanner at ingest.

use std::collections::HashMap;
use std::sync::LazyLock;

use bc_db::rusqlite::{Connection, Transaction};
use bc_db::util::name_key;
use bc_libcore::{ApiResult, Ctx};
use regex::Regex;

use crate::util::{ids_json, set_setting, setting};

/// Setting: `"1"` keeps snippets out of listings, pools and draws.
pub const HIDE_KEY: &str = "library.hide_snippets";
/// What an installation with no saved answer does: off (a filter nobody asked for that silently
/// shrinks the shelf is indistinguishable from a bug).
pub const HIDE_DEFAULT: bool = false;
pub const VERSION_KEY: &str = "library.snippet_rules_version";
/// Bumped whenever the rules change, which re-reads every title once.
pub const RULES_VERSION: &str = "1";

const MARKERS: &[&str] = &["snippet", "snippets", "preview", "previews", "teaser", "teasers", "excerpt", "excerpts"];
const BRACKET_ONLY: &[&str] = &["clip", "clips"];
/// Phrases that end in a marker word and are simply the name of a song.
const NOT_A_MARKER: &[&str] = &["brain teaser", "brain teasers"];

/// What a title must contain for any rule to have a chance of firing (the SQL prefilter).
pub fn sql_markers() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = MARKERS.iter().chain(BRACKET_ONLY).copied().collect();
    v.sort_unstable();
    v.dedup();
    v
}

struct Rules {
    bracketed: Regex,
    in_bracket: Regex,
    tail_segment: Regex,
    tail_marker: Regex,
    suffix: Regex,
}

static RULES: LazyLock<Rules> = LazyLock::new(|| {
    let all = MARKERS.iter().chain(BRACKET_ONLY).copied().collect::<Vec<_>>().join("|");
    let loose = MARKERS.join("|");
    // The en and em dashes are the point: a Bandcamp title separates with whichever the label typed.
    Rules {
        bracketed: Regex::new(r"[\[({]([^\])}]*)[\])}]").expect("static regex"),
        in_bracket: Regex::new(&format!(r"(?i)\b(?:{all})\b")).expect("static regex"),
        tail_segment: Regex::new(r"(?:--|[-\u{2013}\u{2014}|/])\s*([^-\u{2013}\u{2014}|/]*)$").expect("static regex"),
        tail_marker: Regex::new(&format!(r"(?i)^(?:{all})$")).expect("static regex"),
        suffix: Regex::new(&format!(r"(?i)\b(?:{loose})\s*$")).expect("static regex"),
    }
});

/// Which rule says this title names a teaser clip (`"bracket"`, `"tail"`, `"suffix"`), or `None`.
/// The reason, not just a boolean, because it makes a wrong verdict reportable.
pub fn snippet_reason(title: &str) -> Option<&'static str> {
    let text = title.trim();
    if text.is_empty() {
        return None;
    }
    let r = &*RULES;
    for caps in r.bracketed.captures_iter(text) {
        if r.in_bracket.is_match(caps.get(1).map(|m| m.as_str()).unwrap_or("")) {
            return Some("bracket");
        }
    }
    if let Some(caps) = r.tail_segment.captures(text)
        && r.tail_marker.is_match(caps.get(1).map(|m| m.as_str()).unwrap_or("").trim())
    {
        return Some("tail");
    }
    if r.suffix.is_match(text) {
        let folded = format!(" {} ", name_key(text));
        if !NOT_A_MARKER.iter().any(|p| folded.contains(&format!(" {p} "))) {
            return Some("suffix");
        }
    }
    None
}

/// Whether the title names a teaser clip. Called by the scanner at ingest.
pub fn is_snippet_title(title: &str) -> bool {
    snippet_reason(title).is_some()
}

// ---------------------------------------------------------------------------- the saved switch

pub fn hidden(c: &Connection) -> ApiResult<bool> {
    Ok(match setting(c, HIDE_KEY)?.as_deref() {
        Some("1") => true,
        Some("0") => false,
        _ => HIDE_DEFAULT,
    })
}

pub fn set_hidden(t: &Transaction<'_>, value: bool) -> ApiResult<()> {
    set_setting(t, HIDE_KEY, if value { "1" } else { "0" })
}

/// Whether the library holds a snippet at all (one index probe).
pub fn any_snippets(c: &Connection) -> ApiResult<bool> {
    bc_libcore::scope::any_snippets(c)
}

/// `(snippet tracks, releases that are nothing but snippets)`.
pub fn counts(c: &Connection) -> ApiResult<(i64, i64)> {
    let t = c.query_row("SELECT COUNT(*) FROM tracks WHERE is_snippet = 1", [], |r| r.get(0))?;
    let r = c.query_row("SELECT COUNT(*) FROM releases WHERE snippet_only = 1", [], |r| r.get(0))?;
    Ok((t, r))
}

// ---------------------------------------------------------------------------- keeping the verdict true

/// Re-read titles and store the verdict; returns how many rows changed. `track_ids` narrows it
/// to what an ingest just touched; `None` reads the whole library (one scan, prefiltered in SQL).
/// Runs in the caller's write transaction.
pub fn refresh_tracks(c: &Connection, track_ids: Option<&[i64]>) -> ApiResult<usize> {
    let rows: Vec<(i64, String, bool)> = match track_ids {
        Some([]) => return Ok(0),
        Some(ids) => {
            let mut st = c.prepare("SELECT id, title, is_snippet FROM tracks WHERE id IN (SELECT value FROM json_each(?1))")?;
            st.query_map([ids_json(ids)], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, bool>(2)?)))?.collect::<Result<_, _>>()?
        }
        None => {
            let like = sql_markers().iter().map(|m| format!("lower(title) LIKE '%{m}%'")).collect::<Vec<_>>().join(" OR ");
            let mut st = c.prepare(&format!("SELECT id, title, is_snippet FROM tracks WHERE {like} OR is_snippet = 1"))?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, bool>(2)?)))?.collect::<Result<_, _>>()?
        }
    };
    let (mut on, mut off) = (Vec::new(), Vec::new());
    for (id, title, stored) in rows {
        let verdict = is_snippet_title(&title);
        if verdict != stored {
            if verdict { on.push(id) } else { off.push(id) }
        }
    }
    let mut total = 0;
    for (verdict, ids) in [(1, on), (0, off)] {
        for chunk in ids.chunks(5000) {
            total += c.execute(
                "UPDATE tracks SET is_snippet = ?1 WHERE id IN (SELECT value FROM json_each(?2))",
                (verdict, ids_json(chunk)),
            )?;
        }
    }
    Ok(total)
}

/// Recompute `releases.snippet_only`: a release is snippet-only when it holds tracks and every
/// one of them is a snippet. The mixed case is left alone (one real record with a bonus montage
/// is still a record). `None` covers the two sets that can differ from the stored flag: releases
/// holding a snippet and releases already flagged. Never a scan of every release.
pub fn refresh_releases(c: &Connection, release_ids: Option<&[i64]>) -> ApiResult<usize> {
    let candidates: Vec<i64> = match release_ids {
        Some(ids) => crate::util::dedup_sorted(ids.iter().copied()),
        None => {
            let mut st = c.prepare(
                "SELECT release_id FROM tracks WHERE is_snippet = 1 AND release_id IS NOT NULL
                 UNION SELECT id FROM releases WHERE snippet_only = 1",
            )?;
            st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
        }
    };
    if candidates.is_empty() {
        return Ok(0);
    }
    let mut verdicts: HashMap<i64, bool> = candidates.iter().map(|i| (*i, false)).collect();
    for chunk in candidates.chunks(5000) {
        let mut st = c.prepare(
            "SELECT release_id, MIN(COALESCE(is_snippet, 0)), COUNT(id) FROM tracks
              WHERE release_id IN (SELECT value FROM json_each(?1)) GROUP BY release_id",
        )?;
        let rows = st.query_map([ids_json(chunk)], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))?;
        for row in rows {
            let (rid, all, n) = row?;
            verdicts.insert(rid, all != 0 && n > 0);
        }
    }
    let mut changed = 0;
    for verdict in [true, false] {
        let mut ids: Vec<i64> = verdicts.iter().filter(|(_, v)| **v == verdict).map(|(k, _)| *k).collect();
        ids.sort_unstable();
        for chunk in ids.chunks(5000) {
            changed += c.execute(
                "UPDATE releases SET snippet_only = ?1 WHERE id IN (SELECT value FROM json_each(?2)) AND snippet_only = ?3",
                (verdict as i64, ids_json(chunk), (!verdict) as i64),
            )?;
        }
    }
    Ok(changed)
}

/// What a scan or a finished download calls: re-read only what it wrote.
pub fn refresh_for_ingest(c: &Connection, track_ids: &[i64], release_ids: &[i64]) -> ApiResult<()> {
    refresh_tracks(c, Some(track_ids))?;
    refresh_releases(c, Some(release_ids))?;
    Ok(())
}

/// Transaction-level backfill: read every title once, if the rules changed since the last read.
/// Returns `(tracks flagged-or-cleared, releases changed)`.
pub fn backfill_tx(t: &Transaction<'_>) -> ApiResult<(usize, usize)> {
    if setting(t, VERSION_KEY)?.as_deref() == Some(RULES_VERSION) {
        return Ok((0, 0));
    }
    let tracks = refresh_tracks(t, None)?;
    let releases = refresh_releases(t, None)?;
    set_setting(t, VERSION_KEY, RULES_VERSION)?;
    if tracks > 0 || releases > 0 {
        tracing::info!(tracks, releases, "marked snippet track(s) and snippet-only release(s)");
    }
    Ok((tracks, releases))
}

/// Read every title once, guarded by [`RULES_VERSION`] (a no-op afterwards).
pub fn backfill(ctx: &Ctx) -> ApiResult<(usize, usize)> {
    ctx.write(backfill_tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use bc_db::Db;
    use bc_libcore::Scope;
    use bc_types::ScopeMode;

    // -- reading the title ----------------------------------------------------------

    #[test]
    fn a_marked_title_is_read_as_a_snippet() {
        for (title, reason) in [
            // bracket: a parenthetical describes the file, never names the song
            ("b1 Natural Piece Of Aloe (Kirill Matveev) - Salto Mortale [SNIPPET]", "bracket"),
            ("A1 - Myon - Jazz Life (Snippet Vinyl Only)", "bracket"),
            ("Blister (snippet vinly only)", "bracket"), // the label's typo, not ours
            ("Jon - 31 Seconds (clip)", "bracket"),
            ("Shelter (Audio Preview Only)", "bracket"),
            ("Untitled (1min snippet)", "bracket"),
            ("Deprivation (Previews)", "bracket"),
            ("DG1[Preview]", "bracket"),
            ("Reactor (clip only!)", "bracket"),
            // tail: a marker alone after the last separator
            ("Disco Manina (Vaudafunk Edit) - Snippet", "tail"),
            ("DJUS - Clip", "tail"),
            ("Iso EP // Clips", "tail"),
            // suffix: the title simply ends in the word
            ("Preview Snippets", "suffix"),
            ("PREVIEW SNIPPETS", "suffix"),
            ("Immolation Previews", "suffix"),
            ("Track Previews", "suffix"),
            ("Dissonant: Previews", "suffix"),
            ("Advance Teaser", "suffix"),
            ("Mbass123 Excerpt", "suffix"),
            ("Preview", "suffix"),
        ] {
            assert_eq!(snippet_reason(title), Some(reason), "{title}");
        }
    }

    #[test]
    fn a_real_title_survives() {
        for title in [
            // "clip" is a real word in a real title, so it counts only in brackets or alone after
            // a separator -- never as a bare suffix.
            "Full Clip",
            "Clip On",
            "Avision - Clips (Original Mix)",
            "Paperclip People - 4 My Peepz",
            "Psyk - Eclipse",
            "Ecliptic Flow",
            "Declan James - Eclipsing",
            // The one English idiom that ends in a marker word and means a song.
            "Brain Teaser",
            "Techno Brain Teaser",
            // A marker word that is not where a marker word goes.
            "Preview Of A Life Half Lived",
            "Snippets Of Us (Original Mix)",
            // Nothing to read.
            "",
            "   ",
        ] {
            assert_eq!(snippet_reason(title), None, "{title:?}");
        }
    }

    #[test]
    fn the_en_and_em_dash_tails_read_like_a_hyphen() {
        assert_eq!(snippet_reason("Pulse \u{2014} Snippet"), Some("tail"));
        assert_eq!(snippet_reason("Pulse \u{2013} Clip"), Some("tail"));
        assert_eq!(snippet_reason("Pulse | Preview"), Some("tail"));
    }

    #[test]
    fn every_marker_word_appears_in_the_sql_prefilter() {
        // The backfill narrows 183,000 titles with LIKE before reading any in Rust, so a marker
        // missing from that list is a rule that never fires on an existing library.
        let markers = sql_markers();
        for title in ["[SNIPPET]", "(clip)", "Advance Teaser", "Mbass123 Excerpt", "Track Previews"] {
            assert!(is_snippet_title(title));
            assert!(markers.iter().any(|m| title.to_lowercase().contains(m)), "{title}");
        }
    }

    // -- the exclusion and the stored verdict ------------------------------------------

    /// Three records: a real one, one that is nothing but clips, and a real one carrying a single
    /// montage track -- the mixed case, which must keep its album while losing that one track.
    fn seed(db: &Db) -> std::collections::HashMap<String, i64> {
        let mut ids = std::collections::HashMap::new();
        let root = seed_root(db, "/tmp/bc-snippet-test", "downloads");
        for (key, titles) in [("real", vec!["Salto Mortale", "Regrets"]), ("clips", vec!["a1 Flute Meditation [SNIPPET]", "a2 Regrets [SNIPPET]"]), ("mixed", vec!["Endurance", "Preview Snippets"])] {
            let rid = seed_release(db, &format!("{key} artist"), &format!("{key} album"), None, Some(2024));
            for (n, title) in titles.iter().enumerate() {
                let t = seed_track(db, rid, title, Some(n as i64 + 1));
                exec(db, &format!("UPDATE tracks SET is_snippet={}, duration_ms=120000 WHERE id={t}", is_snippet_title(title) as i32));
                seed_file(db, t, root, &format!("/tmp/bc-snippet-test/{key}/{}.mp3", n + 1), &format!("{key}/{}.mp3", n + 1), 1000);
                ids.insert(format!("{key}_track_{}", n + 1), t);
            }
            ids.insert(key.to_string(), rid);
        }
        db.write(|t| Ok(refresh_releases(t, None).unwrap())).unwrap();
        ids
    }

    fn scope_of(db: &Db) -> Scope {
        db.read(|c| Ok(Scope::resolve(c, Some(ScopeMode::All), None).unwrap())).unwrap()
    }

    fn titles(db: &Db, sql: String) -> Vec<String> {
        db.read(move |c| {
            let mut st = c.prepare(&sql)?;
            let mut v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            v.sort();
            Ok(v)
        })
        .unwrap()
    }

    #[test]
    fn the_switch_is_off_until_it_is_asked_for() {
        let db = test_db();
        seed(&db);
        assert!(!db.read(|c| Ok(hidden(c).unwrap())).unwrap());
        assert_eq!(db.read(|c| Ok(counts(c).unwrap())).unwrap(), (3, 1));
        let s = scope_of(&db);
        assert!(!s.no_snippets);
        assert_eq!(titles(&db, format!("SELECT r.title FROM releases r WHERE 1=1{}", s.and_release("r"))), vec!["clips album", "mixed album", "real album"]);
    }

    #[test]
    fn hiding_snippets_takes_the_tracks_and_the_all_snippet_release() {
        let db = test_db();
        seed(&db);
        db.write(|t| Ok(set_hidden(t, true).unwrap())).unwrap();
        let s = scope_of(&db);
        assert!(s.no_snippets);
        // The all-clip record goes; the mixed one stays, because it is a record.
        assert_eq!(titles(&db, format!("SELECT r.title FROM releases r WHERE 1=1{}", s.and_release("r"))), vec!["mixed album", "real album"]);
        assert_eq!(titles(&db, format!("SELECT t.title FROM tracks t WHERE 1=1{}", s.and_track("t"))), vec!["Endurance", "Regrets", "Salto Mortale"]);
        // And an artist with nothing left is off the shelf too.
        let artists = titles(&db, format!("SELECT a.name FROM artists a WHERE 1=1{}", s.and_artist("a")));
        assert!(!artists.contains(&"clips artist".to_string()));
    }

    #[test]
    fn hiding_snippets_leaves_artists_it_has_nothing_to_do_with_alone() {
        // Phrased as "keep the artists with something left" the rule would also drop every artist
        // row with nothing behind it (3,019 of a real library's 28,884, created by harvests that
        // never downloaded anything), so it names the artists it removes instead.
        let db = test_db();
        seed(&db);
        seed_artist(&db, "Pinned Only");
        let before = titles(&db, "SELECT name FROM artists".into());
        db.write(|t| Ok(set_hidden(t, true).unwrap())).unwrap();
        let s = scope_of(&db);
        let after = titles(&db, format!("SELECT a.name FROM artists a WHERE 1=1{}", s.and_artist("a")));
        let gone: Vec<&String> = before.iter().filter(|b| !after.contains(b)).collect();
        assert_eq!(gone, vec!["clips artist"], "only the all-clips artist goes");
        assert!(after.contains(&"Pinned Only".to_string()));
        assert!(after.contains(&"mixed artist".to_string()), "one montage track does not unmake an artist");
    }

    #[test]
    fn the_switch_is_reversible() {
        let db = test_db();
        seed(&db);
        db.write(|t| Ok(set_hidden(t, true).unwrap())).unwrap();
        db.write(|t| Ok(set_hidden(t, false).unwrap())).unwrap();
        let s = scope_of(&db);
        assert_eq!(titles(&db, format!("SELECT t.title FROM tracks t WHERE 1=1{}", s.and_track("t"))).len(), 6);
    }

    #[test]
    fn an_album_page_still_shows_its_own_clips() {
        // A direct read by id is an address, not a listing: nothing in the stored verdict hides it.
        let db = test_db();
        let ids = seed(&db);
        db.write(|t| Ok(set_hidden(t, true).unwrap())).unwrap();
        let rid = ids["clips"];
        assert_eq!(q_i64(&db, &format!("SELECT snippet_only FROM releases WHERE id={rid}")), 1);
        assert_eq!(q_i64(&db, &format!("SELECT COUNT(*) FROM tracks WHERE release_id={rid} AND is_snippet=1")), 2);
    }

    #[test]
    fn refresh_releases_flags_only_the_all_snippet_record() {
        let db = test_db();
        let ids = seed(&db);
        let flag = |k: &str| q_i64(&db, &format!("SELECT snippet_only FROM releases WHERE id={}", ids[k]));
        assert_eq!((flag("real"), flag("clips"), flag("mixed")), (0, 1, 0));
    }

    #[test]
    fn a_release_stops_being_snippet_only_when_a_real_track_lands_on_it() {
        // The fill case: a record that arrived as two clips, then downloaded properly.
        let db = test_db();
        let ids = seed(&db);
        let rid = ids["clips"];
        seed_track(&db, rid, "Flute Meditation", Some(1));
        let changed = db.write(move |t| Ok(refresh_releases(t, Some(&[rid])).unwrap())).unwrap();
        assert_eq!(changed, 1);
        assert_eq!(q_i64(&db, &format!("SELECT snippet_only FROM releases WHERE id={rid}")), 0);
    }

    #[test]
    fn the_backfill_reads_every_title_once_and_then_stops() {
        // Guarded by the rules version, not run every boot.
        let db = test_db();
        let rid = seed_release(&db, "x", "x", None, Some(2024));
        seed_track(&db, rid, "Regrets [SNIPPET]", None);
        seed_track(&db, rid, "Regrets", None);
        // First run flags the clip and records the version.
        assert_eq!(db.write(|t| Ok(backfill_tx(t).unwrap())).unwrap(), (1, 0));
        // A title inserted behind its back is not re-read ...
        seed_track(&db, rid, "Another [SNIPPET]", None);
        assert_eq!(db.write(|t| Ok(backfill_tx(t).unwrap())).unwrap(), (0, 0));
        // ... until the rules themselves change, which is what clearing the guard means.
        exec(&db, &format!("DELETE FROM settings WHERE key='{VERSION_KEY}'"));
        assert_eq!(db.write(|t| Ok(backfill_tx(t).unwrap())).unwrap().0, 1);
    }

    #[test]
    fn a_library_with_no_snippets_pays_nothing_for_the_feature() {
        // `resolve` drops the predicate entirely when nothing could match it.
        let db = test_db();
        db.write(|t| Ok(set_hidden(t, true).unwrap())).unwrap();
        assert!(!scope_of(&db).filtered());
        seed(&db);
        let s = scope_of(&db);
        assert!(s.no_snippets && s.filtered());
    }

    #[test]
    fn backfill_with_ctx_is_guarded_too() {
        let env = test_env();
        let rid = seed_release(&env.db, "x", "x", None, None);
        seed_track(&env.db, rid, "A (clip only)", None);
        assert_eq!(backfill(&env).unwrap(), (1, 1));
        assert_eq!(backfill(&env).unwrap(), (0, 0));
    }
}
