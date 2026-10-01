//! Ports of the first half of `test_download_dedup.py`: url_key, find_known,
//! backfill (urls and labels), the name fallback and the label catalogue filing.
//!
//! Not ported here (wave 2, they need the download worker / HTTP routes):
//! `test_preflight_skips_known_url`, `test_preflight_leaves_unknown_and_forced_urls_alone`,
//! `test_parse_reports_already_have`, `test_submit_skips_known_urls_up_front`,
//! `test_clear_finished_takes_the_settled_jobs_and_leaves_the_retryable_one`,
//! `test_clear_finished_is_harmless_with_nothing_to_clear`,
//! `test_submit_with_force_does_not_skip`, and
//! `test_a_moved_release_year_does_not_make_a_second_release_row` (library ingest,
//! WS1's `get_or_create_release`).

mod dedup_common;

use std::collections::HashMap;

use bc_bandcamp::download::dedup::*;
use dedup_common::{ALBUM, fx};

fn names(pairs: &[(&str, &str, &str)]) -> NameLookup {
    pairs.iter().map(|(u, a, t)| (u.to_string(), (a.to_string(), t.to_string()))).collect()
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

fn known(db: &bc_db::Db, urls: &[&str], n: Option<&NameLookup>) -> HashMap<String, String> {
    db.read(|c| find_known(c, &s(urls), n)).unwrap()
}

// -- url_key ------------------------------------------------------------------

#[test]
fn url_key_reconciles_both_normalisers() {
    let variants = [
        "https://Artist.Bandcamp.com/album/great-record",
        "https://artist.bandcamp.com/album/great-record/",
        "https://artist.bandcamp.com/album/great-record?action=buy",
        "https://artist.bandcamp.com/album/great-record?from=discover",
    ];
    // The downloads-route normaliser: scheme://netloc/path with no query, no trailing slash.
    let normalise_url = |raw: &str| {
        let raw = raw.trim();
        raw.split(['?', '#']).next().unwrap().trim_end_matches('/').to_string()
    };
    for raw in variants {
        assert_eq!(url_key(raw), url_key(ALBUM));
        // Historical job_items.url was written by the downloads-route normaliser;
        // both forms must map to the same key.
        assert_eq!(url_key(&normalise_url(raw)), url_key(&normalise(raw)));
    }
}

// -- find_known -----------------------------------------------------------------

#[test]
fn find_known_matches_done_history_case_insensitively() {
    let f = fx();
    // Simulate a legacy row: host case preserved by the old normaliser.
    f.done_item("https://ARTIST.bandcamp.com/album/great-record", None);
    let got = known(&f.db, &[ALBUM], None);
    assert_eq!(got, HashMap::from([(url_key(ALBUM), "history".to_string())]));
}

#[test]
fn find_known_ignores_unfinished_items() {
    let f = fx();
    f.pending_item(ALBUM);
    assert!(known(&f.db, &[ALBUM], None).is_empty());
}

#[test]
fn find_known_matches_release_and_harvest() {
    let f = fx();
    let other = "https://other.bandcamp.com/album/b-sides";
    f.release("Great Record", Some(&normalise(ALBUM)));
    f.harvest(other, "in_library", "", "", None, None);
    let got = known(&f.db, &[ALBUM, other, "https://new.bandcamp.com/album/unknown"], None);
    assert_eq!(got[&url_key(ALBUM)], "library");
    assert_eq!(got[&url_key(other)], "harvest");
    assert_eq!(got.len(), 2);
}

#[test]
fn find_known_prefers_library_over_history() {
    let f = fx();
    f.done_item(ALBUM, None);
    f.release("Great Record", Some(&normalise(ALBUM)));
    assert_eq!(known(&f.db, &[ALBUM], None), HashMap::from([(url_key(ALBUM), "library".to_string())]));
}

#[test]
fn find_known_batches_large_inputs() {
    let f = fx();
    let urls: Vec<String> = (0..950).map(|i| format!("https://a.bandcamp.com/album/rec-{i}")).collect();
    f.done_item(&urls[0], None);
    f.done_item(&urls[500], None);
    f.done_item(&urls[949], None);
    let got = f.db.read(|c| find_known(c, &urls, None)).unwrap();
    let keys: std::collections::HashSet<_> = got.keys().cloned().collect();
    assert_eq!(keys, [&urls[0], &urls[500], &urls[949]].iter().map(|u| url_key(u)).collect());
}

#[test]
fn a_blacklisted_url_reports_blacklist_first() {
    let f = fx();
    f.release("Great Record", Some(&normalise(ALBUM)));
    f.blacklist(Some(ALBUM), "Artist", "Great Record");
    assert_eq!(known(&f.db, &[ALBUM], None)[&url_key(ALBUM)], "blacklist");
    let ids = f.db.read(|c| find_known_ids(c, &s(&[ALBUM]), None)).unwrap();
    assert_eq!(ids[&url_key(ALBUM)], ("blacklist".to_string(), None));
}

#[test]
fn a_urlless_blacklist_entry_blocks_by_name() {
    let f = fx();
    f.owned("Roch", "Unconditional");
    f.blacklist(None, "Roch", "Unconditional");
    let page = "https://label.bandcamp.com/album/unconditional";
    // The record is on the shelf *and* blacklisted: the deliberate reason wins.
    let n = names(&[(page, "ROCH", "unconditional")]);
    assert_eq!(known(&f.db, &[page], Some(&n))[&url_key(page)], "blacklist");
}

// -- backfill -------------------------------------------------------------------

#[test]
fn backfill_sets_urls_from_done_album_items() {
    let f = fx();
    let release = f.release("Great Record", None);
    f.done_item(ALBUM, Some(release));

    assert_eq!(f.db.write(|t| backfill_release_urls(t)).unwrap(), 1);
    assert_eq!(f.release_url(release), Some(normalise(ALBUM)));
    // Second run is a no-op.
    assert_eq!(f.db.write(|t| backfill_release_urls(t)).unwrap(), 0);
}

#[test]
fn backfill_leaves_existing_urls_and_conflicts_alone() {
    let f = fx();
    let other = "https://other.bandcamp.com/album/b-sides";
    let kept = f.release("Already Linked", Some(&normalise(other)));
    let conflicted = f.release("Great Record", None);
    f.done_item(other, Some(kept)); // url already taken
    f.done_item(other, Some(conflicted)); // conflicting claim

    assert_eq!(f.db.write(|t| backfill_release_urls(t)).unwrap(), 0);
    assert_eq!(f.release_url(kept), Some(normalise(other)));
    assert_eq!(f.release_url(conflicted), None);
}

// -- name fallback ----------------------------------------------------------------

/// A third of a scanned library has no URL, and one can even be plain wrong.
/// Without the fallback a browse view offers to download records already on the
/// shelf -- the one thing those badges exist to prevent.
#[test]
fn known_falls_back_to_artist_and_title_when_no_url_matches() {
    let f = fx();
    let page = "https://label.bandcamp.com/album/unconditional";
    f.owned("Roch", "Unconditional");

    assert!(known(&f.db, &[page], None).is_empty(), "no URL anywhere to match on");
    let n = names(&[(page, "Roch", "Unconditional")]);
    assert_eq!(known(&f.db, &[page], Some(&n)), HashMap::from([(url_key(page), "match".to_string())]));
}

/// Case and punctuation differ between a page and a file's tags constantly.
#[test]
fn name_match_folds_the_way_the_rest_of_the_app_folds() {
    let f = fx();
    let page = "https://label.bandcamp.com/album/kind-of-green";
    f.owned("Karenn", "Kind Of Green");
    let n = names(&[(page, "KARENN", "kind of green!")]);
    assert!(!known(&f.db, &[page], Some(&n)).is_empty());
}

/// Half a match is not a match: a false "already have it" hides a record behind
/// a badge and the user never gets it.
#[test]
fn name_match_needs_both_the_artist_and_the_title() {
    let f = fx();
    f.owned("Roch", "Unconditional");
    let cases = names(&[
        ("https://label.bandcamp.com/album/a", "Roch", "Something Else"),
        ("https://label.bandcamp.com/album/b", "Another Artist", "Unconditional"),
        ("https://label.bandcamp.com/album/c", "Roch", ""),
    ]);
    let urls: Vec<&str> = cases.keys().map(String::as_str).collect();
    assert!(known(&f.db, &urls, Some(&cases)).is_empty());
}

/// Exactly the reported case: the release carries another record's URL.
#[test]
fn a_wrong_url_on_the_release_does_not_shadow_the_name_match() {
    let f = fx();
    let page = "https://label.bandcamp.com/album/unconditional";
    let release = f.owned("Roch", "Unconditional");
    f.set_release_url(release, Some(&normalise("https://label.bandcamp.com/album/chante")));
    let n = names(&[(page, "Roch", "Unconditional")]);
    assert!(!known(&f.db, &[page], Some(&n)).is_empty());
}

/// A URL link to a release row has to agree with what the page says.
#[test]
fn a_url_linked_release_that_contradicts_the_page_is_not_a_library_hit() {
    let f = fx();
    let release = f.owned("Baka G", "No Breaks");
    f.set_release_url(release, Some(&normalise(ALBUM)));
    let n = names(&[(ALBUM, "Roch", "Unconditional")]);
    assert!(known(&f.db, &[ALBUM], Some(&n)).is_empty());
    // Without names there is nothing to contradict it.
    assert_eq!(known(&f.db, &[ALBUM], None)[&url_key(ALBUM)], "library");
}

/// The id is what lets a browse view play the shelf's own files instead of
/// streaming a record it has just badged as owned -- for both a URL match and the
/// name fallback a scanned-in library depends on.
#[test]
fn find_known_ids_names_the_release_row() {
    let f = fx();
    let page = "https://label.bandcamp.com/album/unconditional";
    let linked = f.release("Great Record", Some(&normalise(ALBUM)));
    let named = f.owned("Roch", "Unconditional");

    let n = names(&[(page, "Roch", "Unconditional")]);
    let got = f.db.read(|c| find_known_ids(c, &s(&[ALBUM, page]), Some(&n))).unwrap();
    assert_eq!(got[&url_key(ALBUM)], ("library".to_string(), Some(linked)));
    assert_eq!(got[&url_key(page)], ("match".to_string(), Some(named)));
}

#[tokio::test]
async fn async_wrappers_match_the_sync_functions() {
    let f = fx();
    f.done_item(ALBUM, None);
    let got = find_known_async(&f.db, s(&[ALBUM]), None).await.unwrap();
    assert_eq!(got[&url_key(ALBUM)], "history");
    assert_eq!(backfill_release_urls_async(&f.db).await.unwrap(), 0);
}

// -- backfill_release_labels ---------------------------------------------------------

/// Downloaded files carry no publisher tag, so this is the only label there is.
#[test]
fn backfill_files_releases_under_the_label_bandcamp_named() {
    let f = fx();
    let other = "https://artist.bandcamp.com/album/b-sides";
    // No URL on either release: the join is on the names, which is the point.
    let listed = f.owned("Some Artist", "Great Record");
    let unlisted = f.owned("Some Artist", "B Sides");
    f.harvest(ALBUM, "in_library", "Some Artist", "Great Record", Some("  Ostgut Ton "), None);
    f.harvest(other, "in_library", "Some Artist", "B Sides", None, None);

    assert_eq!(f.db.write(|t| backfill_release_labels(t)).unwrap(), 1);

    let label = f.release_label(listed).expect("labelled");
    assert_eq!(label.1, "Ostgut Ton", "trimmed, not raw");
    assert_eq!(f.release_label_id(unlisted), None, "no label stated, none invented");
    // Second run is a no-op, and the same imprint stays one row.
    assert_eq!(f.db.write(|t| backfill_release_labels(t)).unwrap(), 0);
    assert_eq!(f.count("labels"), 1);
}

/// Joining through the URL filed albums under the wrong label wholesale.
#[test]
fn backfill_ignores_the_stored_url_when_it_names_another_record() {
    let f = fx();
    let release = f.owned("Roch", "Unconditional");
    f.set_release_url(release, Some(&normalise(ALBUM)));
    // That URL is somebody else's record, on somebody else's label.
    f.harvest(ALBUM, "in_library", "Baka G", "No Breaks", Some("SEVEN"), None);

    assert_eq!(f.db.write(|t| backfill_release_labels(t)).unwrap(), 0);
    assert_eq!(f.release_label_id(release), None);
}

/// Repairs what the URL join already wrote, on evidence rather than a guess.
#[test]
fn backfill_takes_back_a_label_inherited_from_a_foreign_url() {
    let f = fx();
    let release = f.owned("Roch", "Unconditional");
    let wrong = f.add_label("SEVEN", None);
    f.set_release_url(release, Some(&normalise(ALBUM)));
    f.set_release_label(release, wrong);
    f.harvest(ALBUM, "in_library", "Baka G", "No Breaks", Some("SEVEN"), None);

    assert_eq!(f.db.write(|t| backfill_release_labels(t)).unwrap(), 1);
    assert_eq!(f.release_label_id(release), None);
}

/// Only a label that provably came off another record's page is removed.
#[test]
fn backfill_leaves_a_label_it_cannot_explain() {
    let f = fx();
    let release = f.owned("Roch", "Unconditional");
    let tagged = f.add_label("From The Tags", None);
    f.set_release_url(release, Some(&normalise(ALBUM)));
    f.set_release_label(release, tagged);
    f.harvest(ALBUM, "in_library", "Baka G", "No Breaks", Some("SEVEN"), None);

    assert_eq!(f.db.write(|t| backfill_release_labels(t)).unwrap(), 0);
    assert_eq!(f.release_label(release).unwrap().1, "From The Tags");
}

#[test]
fn backfill_gives_neither_label_when_two_pages_dispute_the_release() {
    let f = fx();
    let release = f.owned("Some Artist", "Great Record");
    f.harvest(ALBUM, "in_library", "Some Artist", "Great Record", Some("Label One"), None);
    f.harvest("https://artist.bandcamp.com/album/great-record-2", "in_library", "Some Artist", "Great Record", Some("Label Two"), None);
    assert_eq!(f.db.write(|t| backfill_release_labels(t)).unwrap(), 0);
    assert_eq!(f.release_label_id(release), None);
}

// -- file_known_releases_under_label --------------------------------------------------

const LABEL_PAGE: &str = "https://player.bandcamp.com";

fn entry(url: &str, artist: &str, title: &str) -> Vec<(String, String, String)> {
    vec![(url.to_string(), artist.to_string(), title.to_string())]
}

/// A label catalogue re-run labels what an earlier download left bare.
#[test]
fn catalogue_files_url_matched_releases_and_stamps_the_label_url() {
    let f = fx();
    let release = f.owned("Player", "Player Three");
    f.set_release_url(release, Some(&normalise(ALBUM)));

    let e = entry(ALBUM, "Player", "Player Three");
    let filed = f.db.write({ let e = e.clone(); move |t| file_known_releases_under_label(t, "Player", Some(LABEL_PAGE), &e) }).unwrap();

    assert_eq!(filed, 1);
    let label = f.release_label(release).expect("labelled");
    assert_eq!(label.1, "Player");
    assert_eq!(label.2, Some(normalise(LABEL_PAGE)));
    // Re-run is a no-op.
    let again = f.db.write(move |t| file_known_releases_under_label(t, "Player", Some(LABEL_PAGE), &e)).unwrap();
    assert_eq!(again, 0);
    assert_eq!(f.count("labels"), 1);
}

/// A library scanned off disk has no URLs; the folded names still match.
#[test]
fn catalogue_files_urlless_releases_by_name() {
    let f = fx();
    let release = f.owned("Player", "Protect Ya Neck");
    let e = entry("https://player.bandcamp.com/album/protect-ya-neck", "Player", "Protect Ya Neck");
    let filed = f.db.write(move |t| file_known_releases_under_label(t, "Player", Some(LABEL_PAGE), &e)).unwrap();
    assert_eq!(filed, 1);
    assert_eq!(f.release_label(release).unwrap().1, "Player");
}

#[test]
fn catalogue_never_overwrites_a_label_the_files_named() {
    let f = fx();
    let release = f.owned("Player", "Player Three");
    let tagged = f.add_label("From The Tags", None);
    f.set_release_url(release, Some(&normalise(ALBUM)));
    f.set_release_label(release, tagged);

    let e = entry(ALBUM, "Player", "Player Three");
    let filed = f.db.write(move |t| file_known_releases_under_label(t, "Player", Some(LABEL_PAGE), &e)).unwrap();
    assert_eq!(filed, 0);
    assert_eq!(f.release_label(release).unwrap().1, "From The Tags");
}

/// A corrupt bandcamp_url must not drag a stranger's record onto the label.
#[test]
fn catalogue_ignores_a_url_stamped_onto_another_record() {
    let f = fx();
    let release = f.owned("Roch", "Unconditional");
    f.set_release_url(release, Some(&normalise(ALBUM)));

    let e = entry(ALBUM, "Baka G", "No Breaks");
    let filed = f.db.write(move |t| file_known_releases_under_label(t, "Player", Some(LABEL_PAGE), &e)).unwrap();
    assert_eq!(filed, 0);
    assert_eq!(f.release_label_id(release), None);
}

/// A URL collision must not take the whole filing transaction down.
#[test]
fn catalogue_survives_a_label_url_already_taken() {
    let f = fx();
    f.add_label("Old Import", Some(&normalise(LABEL_PAGE)));
    let release = f.owned("Player", "Player Three");
    f.set_release_url(release, Some(&normalise(ALBUM)));

    let e = entry(ALBUM, "Player", "Player Three");
    let filed = f.db.write(move |t| file_known_releases_under_label(t, "Player", Some(LABEL_PAGE), &e)).unwrap();
    assert_eq!(filed, 1);
    let label = f.release_label(release).expect("labelled");
    assert_eq!(label.1, "Player");
    assert_eq!(label.2, None, "the taken URL stays where it was");
}
