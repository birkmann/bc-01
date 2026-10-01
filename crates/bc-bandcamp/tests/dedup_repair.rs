//! Ports of the second half of `test_download_dedup.py`: URL repair, harvest-link
//! repair, stranded `queued` rows and folder-twin merging.

mod dedup_common;

use bc_bandcamp::download::dedup::*;
use bc_db::rusqlite::params;
use dedup_common::{ALBUM, Fx, fx};

/// An owned release as the corruption left it: named, filed, mis-linked.
fn shelved(f: &Fx, artist: &str, title: &str, folder: Option<&str>, url: Option<&str>) -> i64 {
    let id = f.owned(artist, title);
    f.set_folder(id, folder);
    f.set_release_url(id, url.map(normalise).as_deref());
    id
}

/// What harvest scraped off the page at `url` -- the trusted side.
fn page(f: &Fx, url: &str, artist: &str, title: &str, release_id: Option<i64>) -> i64 {
    f.harvest(url, if release_id.is_some() { "in_library" } else { "new" }, artist, title, None, release_id)
}

fn repair(f: &Fx) -> (usize, usize) {
    f.db.write(|t| repair_release_urls(t)).unwrap()
}

// -- repair_release_urls ----------------------------------------------------------

/// The signature corruption: two concurrent items stamped each other's URL.
#[test]
fn repair_untangles_a_swapped_pair() {
    let f = fx();
    let url_a = "https://heikolaux.bandcamp.com/album/klockworks-24";
    let url_b = "https://mehen.bandcamp.com/album/hoxa-8";
    let a = shelved(&f, "Heiko Laux", "Klockworks 24", Some("m/heiko/klockworks-24"), Some(url_b));
    let b = shelved(&f, "Mehen", "HoxA 8", Some("m/mehen/hoxa-8"), Some(url_a));
    page(&f, url_a, "Heiko Laux", "Klockworks 24", None);
    page(&f, url_b, "Mehen", "HoxA 8", None);

    assert_eq!(repair(&f), (2, 0));
    assert_eq!(f.release_url(a), Some(normalise(url_a)));
    assert_eq!(f.release_url(b), Some(normalise(url_b)));
    // A repaired release no longer meets the corrupt predicate.
    assert_eq!(repair(&f), (0, 0));
}

/// No guessing: a URL that provably names another record, with nothing to
/// re-derive the right one from, is removed rather than reassigned.
#[test]
fn repair_clears_a_foreign_url_with_no_confident_match() {
    let f = fx();
    let foreign = "https://other.bandcamp.com/album/somebody-elses";
    let release = shelved(&f, "Roch", "Unconditional", Some("m/roch/unconditional"), Some(foreign));
    page(&f, foreign, "Somebody", "Elses", None);

    assert_eq!(repair(&f), (0, 1));
    assert_eq!(f.release_url(release), None);
}

/// Slug and folder disagreeing is not corruption when harvest says the page
/// really is about this record -- retitled albums do that legitimately.
#[test]
fn repair_trusts_a_url_the_page_vouches_for() {
    let f = fx();
    let url = "https://label.bandcamp.com/album/old-working-title";
    let release = shelved(&f, "Roch", "Unconditional", Some("m/roch/unconditional"), Some(url));
    page(&f, url, "Roch", "Unconditional", None);

    assert_eq!(repair(&f), (0, 0));
    assert_eq!(f.release_url(release), Some(normalise(url)));
}

/// A hand-set URL never scraped by harvest carries no evidence either way.
#[test]
fn repair_leaves_a_url_harvest_knows_nothing_about() {
    let f = fx();
    let url = "https://obscure.bandcamp.com/album/mystery";
    let release = shelved(&f, "Roch", "Unconditional", Some("m/roch/unconditional"), Some(url));
    page(&f, ALBUM, "Some Artist", "Great Record", None); // unrelated page

    assert_eq!(repair(&f), (0, 0));
    assert_eq!(f.release_url(release), Some(normalise(url)));
}

/// A release with no artist row still has its folder, and the page found at that
/// slug corroborates the title.
#[test]
fn repair_rederives_from_the_folder_when_the_name_fold_cannot() {
    let f = fx();
    let held = "https://other.bandcamp.com/album/not-kotosh";
    let right = "https://noise.bandcamp.com/album/kotosh";
    let release = f.release("Kotosh", Some(&normalise(held)));
    f.set_folder(release, Some("m/anderson-noise/kotosh"));
    page(&f, held, "Somebody", "Elses", None);
    page(&f, right, "Anderson Noise", "Kotosh", None);

    assert_eq!(repair(&f), (1, 0));
    assert_eq!(f.release_url(release), Some(normalise(right)));
}

/// Two corrupt releases folding to one candidate cannot both be it.
#[test]
fn repair_gives_a_contested_url_to_nobody() {
    let f = fx();
    let wrong_a = "https://x.bandcamp.com/album/aaa";
    let wrong_b = "https://x.bandcamp.com/album/bbb";
    let contested = "https://roch.bandcamp.com/album/unconditional";
    let a = shelved(&f, "Roch", "Unconditional", Some("m/roch/unconditional-1"), Some(wrong_a));
    let b = shelved(&f, "Roch", "Unconditional", Some("m/roch/unconditional-2"), Some(wrong_b));
    page(&f, wrong_a, "Somebody", "Elses", None);
    page(&f, wrong_b, "Another", "Record", None);
    page(&f, contested, "Roch", "Unconditional", None);

    assert_eq!(repair(&f), (0, 2));
    assert_eq!(f.release_url(a), None);
    assert_eq!(f.release_url(b), None);
}

#[test]
fn repair_does_not_steal_a_url_a_trusted_release_holds() {
    let f = fx();
    let url = "https://roch.bandcamp.com/album/unconditional";
    let wrong = "https://x.bandcamp.com/album/zzz";
    let keeper = shelved(&f, "Roch", "Unconditional", Some("m/roch/unconditional"), Some(url));
    // Same name fold, so it proposes the keeper's URL.
    let suspect = shelved(&f, "Roch", "Unconditional", Some("m/roch/other"), Some(wrong));
    page(&f, url, "Roch", "Unconditional", None);
    page(&f, wrong, "Somebody", "Elses", None);

    assert_eq!(repair(&f), (0, 1));
    assert_eq!(f.release_url(keeper), Some(normalise(url)));
    assert_eq!(f.release_url(suspect), None);
}

// -- repair_harvest_links -------------------------------------------------------------

/// The inbox trusted the corrupt URLs and linked items to foreign releases.
#[test]
fn harvest_links_to_the_wrong_release_are_reset() {
    let f = fx();
    let mine_url = "https://roch.bandcamp.com/album/unconditional";
    let mine = shelved(&f, "Roch", "Unconditional", None, Some(mine_url));
    let named = f.owned("Karenn", "Kind Of Green");
    let foreign = f.owned("Somebody", "Elses");

    page(&f, mine_url, "Roch", "Unconditional", Some(mine));
    page(&f, "https://karenn.bandcamp.com/album/kind-of-green", "Karenn", "Kind Of Green", Some(named));
    let stale = "https://x.bandcamp.com/album/something-else";
    let stale_id = page(&f, stale, "X", "Something Else", Some(foreign));

    assert_eq!(f.db.write(|t| repair_harvest_links(t)).unwrap(), 1);

    let (state, release_id, _) = f.harvest_state(stale_id);
    assert_eq!((state.as_str(), release_id), ("new", None));
    let kept: std::collections::BTreeSet<i64> = f
        .db
        .read(|c| {
            let mut stmt = c.prepare("SELECT release_id FROM harvest_items WHERE state='in_library'")?;
            Ok(stmt.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
        })
        .unwrap();
    assert_eq!(kept, [mine, named].into_iter().collect());
    // Nothing left to repair.
    assert_eq!(f.db.write(|t| repair_harvest_links(t)).unwrap(), 0);
}

// -- stranded 'queued' rows -----------------------------------------------------------

fn queued(f: &Fx, url: &str, artist: &str, title: &str) -> i64 {
    f.harvest(url, "queued", artist, title, None, None)
}

fn stale(f: &Fx) -> (usize, usize) {
    f.db.write(|t| resolve_stale_queued(t)).unwrap()
}

#[test]
fn stale_queued_resolves_against_the_url_it_downloaded() {
    let f = fx();
    let url = "https://roch.bandcamp.com/album/unconditional";
    let release = shelved(&f, "Roch", "Unconditional", None, Some(url));
    let item = queued(&f, url, "Roch", "Unconditional");

    assert_eq!(stale(&f), (1, 0));
    let (state, rid, _) = f.harvest_state(item);
    assert_eq!((state.as_str(), rid), ("in_library", Some(release)));
    // Idempotent: nothing is left claiming to download.
    assert_eq!(stale(&f), (0, 0));
}

/// Most of a library has no URL at all, so the folded pair has to answer.
#[test]
fn stale_queued_resolves_against_a_scanned_release_by_name() {
    let f = fx();
    let url = "https://roch.bandcamp.com/album/unconditional";
    let release = f.owned("Roch", "Unconditional");
    let item = queued(&f, url, "Roch", "Unconditional");

    assert_eq!(stale(&f), (1, 0));
    assert_eq!(f.harvest_state(item).1, Some(release));
    // And the URL is recorded, so the exact path covers it from here on.
    assert_eq!(f.release_url(release), Some(normalise(url)));
}

/// Honest, and the state the "awaiting download" action can act on.
#[test]
fn stale_queued_with_nothing_on_disk_goes_back_to_new() {
    let f = fx();
    let item = queued(&f, ALBUM, "An Artist", "Great Record");

    assert_eq!(stale(&f), (0, 1));
    let (state, rid, resolved) = f.harvest_state(item);
    assert_eq!(state, "new");
    assert!(rid.is_none() && resolved.is_none());
}

/// This one really is downloading -- leave it alone.
#[test]
fn a_live_job_item_keeps_its_queued_row() {
    let f = fx();
    let item = queued(&f, ALBUM, "An Artist", "Great Record");
    f.pending_item(ALBUM);

    assert_eq!(stale(&f), (0, 0));
    assert_eq!(f.harvest_state(item).0, "queued");
}

/// The same consume-on-match rule the inbox index enforces.
#[test]
fn two_stranded_rows_cannot_claim_one_release() {
    let f = fx();
    let release = f.owned("Roch", "Unconditional");
    let first = queued(&f, "https://roch.bandcamp.com/album/a", "Roch", "Unconditional");
    let second = queued(&f, "https://roch.bandcamp.com/album/b", "Roch", "Unconditional");

    assert_eq!(stale(&f), (1, 1));
    let claimed: Vec<i64> = [first, second].into_iter().filter(|i| f.harvest_state(*i).1.is_some()).collect();
    assert_eq!(claimed.len(), 1);
    assert_eq!(f.harvest_state(claimed[0]).1, Some(release));
}

// -- merge_folder_twins -----------------------------------------------------------------

/// The stub's tracks move rather than being deleted, so plays, ratings, loved
/// flags and every playlist they sit in follow them; nothing on disk is touched.
/// The row holding the completed download keeps the shelf.
#[test]
fn merge_folder_twins_folds_the_stub_onto_the_complete_row() {
    let f = fx();
    let artist = f.artist("UVB");
    let folder = "/library/uvb/second-life-ep";
    let url = "https://uvb.bandcamp.com/album/second-life-ep";
    let (_stub, full, loved) = f
        .db
        .write(move |t| {
            let key = name_key("Second Life EP");
            t.execute(
                "INSERT INTO releases(title, title_key, kind, artist_id, year, folder_path, bandcamp_url, added_at) \
                 VALUES ('Second Life EP', ?1, 'album', ?2, 2014, ?3, ?4, '2020-01-01 00:00:00')",
                params![key, artist, folder, url],
            )?;
            let stub = t.last_insert_rowid();
            t.execute(
                "INSERT INTO releases(title, title_key, kind, artist_id, year, folder_path, added_at) \
                 VALUES ('Second Life EP', ?1, 'album', ?2, 2021, ?3, '2024-01-01 00:00:00')",
                params![key, artist, folder],
            )?;
            let full = t.last_insert_rowid();
            let track = "INSERT INTO tracks(release_id, title, title_key, track_no, loved, play_count, skip_count, added_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, datetime('now'))";
            t.execute(track, params![stub, "Zakajèn", name_key("Zakajèn"), 5, 1])?;
            let loved = t.last_insert_rowid();
            for n in 1..=10 {
                t.execute(track, params![full, format!("t{n}"), format!("t{n}"), n, 0])?;
            }
            Ok((stub, full, loved))
        })
        .unwrap();

    assert_eq!(f.db.write(|t| merge_folder_twins(t)).unwrap(), 1);

    assert_eq!(f.count("releases"), 1, "the stub is gone");
    let (release_id, is_loved): (i64, bool) = f
        .db
        .read(move |c| Ok(c.query_row("SELECT release_id, loved FROM tracks WHERE id=?1", [loved], |r| Ok((r.get(0)?, r.get(1)?)))?))
        .unwrap();
    assert_eq!(release_id, full, "its tracks moved rather than being deleted");
    assert!(is_loved, "and brought the user's own data with them");
    assert_eq!(
        f.release_url(full).as_deref(),
        Some(url),
        "the URL a later fill needs is inherited from the row that had it"
    );
    let added: String =
        f.db.read(move |c| Ok(c.query_row("SELECT added_at FROM releases WHERE id=?1", [full], |r| r.get(0))?)).unwrap();
    assert_eq!(added, "2020-01-01 00:00:00", "the keeper takes the older added_at");
    assert_eq!(f.count("tracks"), 11);
    assert_eq!(f.db.write(|t| merge_folder_twins(t)).unwrap(), 0, "a second start folds nothing");
}

/// A folder holding several album titles is a container, not an album: two
/// records agreeing on artist and title there are no evidence of anything.
#[test]
fn merge_folder_twins_never_folds_on_a_shelf_root() {
    let f = fx();
    let artist = f.artist("Ripperton");
    let shelf = "/library/fan-rfbrk";
    f.db
        .write(move |t| {
            for (title, year) in [("Alias", 2016), ("Alias", 2024), ("Elusive", 2019)] {
                t.execute(
                    "INSERT INTO releases(title, title_key, kind, artist_id, year, folder_path, added_at) \
                     VALUES (?1, ?2, 'album', ?3, ?4, ?5, datetime('now'))",
                    params![title, name_key(title), artist, year, shelf],
                )?;
            }
            Ok(())
        })
        .unwrap();

    assert_eq!(f.db.write(|t| merge_folder_twins(t)).unwrap(), 0);
    assert_eq!(f.count("releases"), 3, "all three survive");
}
