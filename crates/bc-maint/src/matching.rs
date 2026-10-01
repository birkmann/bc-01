//! Match harvested releases against library rows that predate URL tracking (port of
//! `services/library/matching.py`).
//!
//! A library scanned in off disk has no `releases.bandcamp_url`, so URL dedup cannot see it.
//! Matching on `(artist, title)` closes the gap without new normalisation vocabulary: the library
//! side is already stored folded (`artists.name_key`, `releases.title_key`), so the join is free
//! and the harvest side only calls [`name_key`]. A **missed** match costs one wasted download
//! attempt; a **false** match would silently skip a release the user does not have -- so this is
//! exact equality on the folded key, ambiguous keys are dropped, and a key is consumed once taken.

use std::collections::{HashMap, HashSet};

use bc_db::rusqlite::Connection;
use bc_db::util::name_key;
use bc_libcore::ApiResult;

/// `(artist_key, title_key)` -> release id, for releases lacking a URL.
pub type ReleaseIndex = HashMap<(String, String), i64>;

/// Build the index. Releases that already carry a `bandcamp_url` are excluded (matched exactly
/// upstream); ambiguous keys are dropped rather than resolved.
pub fn build_release_index(c: &Connection) -> ApiResult<ReleaseIndex> {
    let mut index = ReleaseIndex::new();
    let mut ambiguous: HashSet<(String, String)> = HashSet::new();
    let mut st = c.prepare(
        "SELECT r.id, a.name_key, r.title_key FROM releases r JOIN artists a ON a.id = r.artist_id WHERE r.bandcamp_url IS NULL",
    )?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)))?;
    for row in rows {
        let (id, artist_key, title_key) = row?;
        let (Some(a), Some(t)) = (artist_key, title_key) else { continue };
        if a.is_empty() || t.is_empty() {
            continue;
        }
        let key = (a, t);
        if index.contains_key(&key) {
            ambiguous.insert(key);
            continue;
        }
        index.insert(key, id);
    }
    if !ambiguous.is_empty() {
        tracing::info!(n = ambiguous.len(), "release index: ambiguous key(s) dropped");
    }
    for k in ambiguous {
        index.remove(&k);
    }
    Ok(index)
}

/// Find a release id for a harvested `(artist, title)` and consume the key (a release can only be
/// the same thing as one Bandcamp URL; `bandcamp_url` is UNIQUE).
pub fn take(index: &mut ReleaseIndex, artist_name: &str, title: &str) -> Option<i64> {
    if artist_name.is_empty() || title.is_empty() {
        return None;
    }
    index.remove(&(name_key(artist_name), name_key(title)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn matches_on_folded_artist_and_title() {
        let db = test_db();
        let rid = seed_release(&db, "Etapp Kyle", "Klockworks 16", None, None);
        let mut index = db.read(|c| Ok(build_release_index(c).unwrap())).unwrap();
        assert_eq!(take(&mut index, "etapp  KYLE", "klockworks 16"), Some(rid));
    }

    #[test]
    fn folds_accents_and_punctuation() {
        let db = test_db();
        let rid = seed_release(&db, "Motörhead", "Ace of Spades!", None, None);
        let mut index = db.read(|c| Ok(build_release_index(c).unwrap())).unwrap();
        assert_eq!(take(&mut index, "Motorhead", "Ace of Spades"), Some(rid));
    }

    #[test]
    fn ignores_releases_that_already_have_a_url() {
        let db = test_db();
        seed_release(&db, "Markus Fix", "I'll House You EP", Some("https://markusfix.bandcamp.com/album/ill-house-you-ep"), None);
        let mut index = db.read(|c| Ok(build_release_index(c).unwrap())).unwrap();
        assert_eq!(take(&mut index, "Markus Fix", "I'll House You EP"), None);
    }

    #[test]
    fn drops_ambiguous_keys_rather_than_guessing() {
        let db = test_db();
        seed_release(&db, "Various", "Untitled", None, Some(2001));
        seed_release(&db, "Various", "Untitled", None, Some(2002));
        let mut index = db.read(|c| Ok(build_release_index(c).unwrap())).unwrap();
        assert_eq!(take(&mut index, "Various", "Untitled"), None);
    }

    #[test]
    fn a_release_is_only_claimed_once() {
        let db = test_db();
        let rid = seed_release(&db, "Pete Rock", "Petestrumentals", None, None);
        let mut index = db.read(|c| Ok(build_release_index(c).unwrap())).unwrap();
        assert_eq!(take(&mut index, "Pete Rock", "Petestrumentals"), Some(rid));
        assert_eq!(take(&mut index, "Pete Rock", "Petestrumentals"), None);
    }

    #[test]
    fn blank_sides_never_match() {
        let db = test_db();
        seed_release(&db, "Some Artist", "Some Title", None, None);
        let mut index = db.read(|c| Ok(build_release_index(c).unwrap())).unwrap();
        assert_eq!(take(&mut index, "", "Some Title"), None);
        assert_eq!(take(&mut index, "Some Artist", ""), None);
    }

    #[test]
    fn skips_releases_with_no_artist() {
        let db = test_db();
        db.write(|t| {
            t.execute("INSERT INTO releases(title,title_key,kind,added_at) VALUES ('Orphan','orphan','album','x')", [])?;
            Ok(())
        })
        .unwrap();
        let mut index = db.read(|c| Ok(build_release_index(c).unwrap())).unwrap();
        assert_eq!(take(&mut index, "Anything", "Orphan"), None);
    }
}
