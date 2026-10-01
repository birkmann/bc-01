//! Ports of the Bandcamp-pinning half of `test_artist_pages.py` (`backfill_artist_urls`,
//! `locate_artist_page`) plus the `POST /artists/{id}/locate` and `/labels/{id}/locate` routes.
//!
//! Not ported here (WS1's artist routes: listing sorts, detail counts, related shelves) and the
//! pure `extract` cases (done in `extract/tests.rs`).

mod harvest_common;

use bc_bandcamp::harvest::artists::{backfill_artist_urls, locate_artist_page};
use bc_bandcamp::harvest::labels::PageSource;
use bc_bandcamp::sources::SearchHit;
use harvest_common::*;
use serde_json::json;

// -- backfill_artist_urls ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backfill_accepts_majority_single_artist_host() {
    let app = app().await;
    let solo = app.artist("Solo", None);
    app.release("Solo", "One", Some("https://solo.bandcamp.com/album/one"));
    app.release("Solo", "Two", Some("https://solo.bandcamp.com/album/two"));
    app.release("Solo", "Stray", Some("https://elsewhere.bandcamp.com/album/stray"));
    let rostered = app.artist("Rostered", None);
    app.release("Rostered", "Three", Some("https://shared.bandcamp.com/album/three"));
    app.release("Sibling", "Four", Some("https://shared.bandcamp.com/album/four"));

    assert_eq!(app.exec(|t| backfill_artist_urls(t)), 1);

    assert_eq!(app.artist_url(solo).as_deref(), Some("https://solo.bandcamp.com"));
    // A host two artists publish on is a label page, not either's own.
    assert_eq!(app.artist_url(rostered), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backfill_refuses_known_label_hosts_and_rostered_inbox_hosts() {
    let app = app().await;
    app.label("Solo Recs", Some("https://solo.bandcamp.com"));
    app.release("Solo", "One", Some("https://solo.bandcamp.com/album/one"));

    // The inbox saw a second artist on lone.bandcamp.com even though the library only holds
    // Lone's own records from it.
    app.release("Lone", "Mine", Some("https://lone.bandcamp.com/album/mine"));
    for (artist, slug) in [("Lone", "mine"), ("Somebody Else", "other")] {
        app.album_item(&format!("https://lone.bandcamp.com/album/{slug}"), artist, slug, None, "new");
    }

    assert_eq!(app.exec(|t| backfill_artist_urls(t)), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backfill_never_overwrites_and_never_duplicates() {
    let app = app().await;
    let pinned = app.artist("Pinned", Some("https://pinned.bandcamp.com"));
    app.release("Pinned", "One", Some("https://elsewhere.bandcamp.com/album/one"));
    app.artist("Squatter", Some("https://solo.bandcamp.com"));
    let solo = app.artist("Solo", None);
    app.release("Solo", "Two", Some("https://solo.bandcamp.com/album/two"));

    assert_eq!(app.exec(|t| backfill_artist_urls(t)), 0);

    assert_eq!(app.artist_url(pinned).as_deref(), Some("https://pinned.bandcamp.com"));
    assert_eq!(app.artist_url(solo), None);
}

// -- locate -----------------------------------------------------------------------------

fn hit(kind: &str, name: &str, url: &str) -> SearchHit {
    SearchHit { kind: kind.into(), name: name.into(), url: url.into(), ..Default::default() }
}

/// An artist's own /music grid omits the byline; blank must match, a different name must not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locate_accepts_blank_grid_artist_as_the_pages_own() {
    let app = app().await;
    app.release("Kaiserdisco", "KD RAW 100", None);
    let artist_id = app.artist("Kaiserdisco", None);
    let src = FakePages::with_search(
        vec![("https://kaiserdisco.bandcamp.com/music".to_string(), music_page(&[("/album/kd-raw-100", "", "KD RAW 100")]))],
        vec![hit("artist", "Kaiserdisco", "https://kaiserdisco.bandcamp.com")],
    );

    let result = locate_artist_page(&app.db, &*src, artist_id).await.expect("artist exists");

    assert_eq!(result.url.as_deref(), Some("https://kaiserdisco.bandcamp.com"));
    assert_eq!(result.matched, 1);
    assert_eq!(app.artist_url(artist_id).as_deref(), Some("https://kaiserdisco.bandcamp.com"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locate_rejects_pages_whose_titles_belong_to_someone_else() {
    let app = app().await;
    app.release("Kaiserdisco", "KD RAW 100", None);
    let artist_id = app.artist("Kaiserdisco", None);
    // Title matches but the grid credits another artist: a label page.
    let src = FakePages::with_search(
        vec![("https://kdlabel.bandcamp.com/music".to_string(), music_page(&[("/album/kd-raw-100", "Somebody Else", "KD RAW 100")]))],
        vec![hit("label", "Kaiserdisco", "https://kdlabel.bandcamp.com")],
    );

    let result = locate_artist_page(&app.db, &*src, artist_id).await.expect("artist exists");

    assert_eq!(result.url, None);
    assert_eq!(app.artist_url(artist_id), None);
}

/// A page already pinned to another artist is refused with its holder named.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locate_refuses_a_page_another_artist_already_holds() {
    let app = app().await;
    app.release("Kaiserdisco", "KD RAW 100", None);
    let artist_id = app.artist("Kaiserdisco", None);
    app.artist("Holder", Some("https://kaiserdisco.bandcamp.com"));
    let src = FakePages::with_search(
        vec![("https://kaiserdisco.bandcamp.com/music".to_string(), music_page(&[("/album/kd-raw-100", "", "KD RAW 100")]))],
        vec![hit("artist", "Kaiserdisco", "https://kaiserdisco.bandcamp.com")],
    );

    let result = locate_artist_page(&app.db, &*src, artist_id).await.expect("artist exists");

    assert_eq!(result.url, None);
    assert!(result.detail.contains("Holder"), "{}", result.detail);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locate_reports_the_other_early_exits() {
    let app = app().await;
    let src = FakePages::new(vec![]);
    assert!(locate_artist_page(&app.db, &*src, 999).await.is_none(), "missing row");

    let bare = app.artist("No Releases", None);
    let r = locate_artist_page(&app.db, &*src, bare).await.unwrap();
    assert_eq!(r.detail, "no releases by this artist to validate against");

    let known = app.artist("Known", Some("https://known.bandcamp.com"));
    let r = locate_artist_page(&app.db, &*src, known).await.unwrap();
    assert_eq!((r.url.as_deref(), r.detail.as_str()), (Some("https://known.bandcamp.com"), "already known"));
}

// -- the routes --------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locate_routes_pin_the_page_and_404_a_missing_row() {
    let app = app().await;
    app.release("Kaiserdisco", "KD RAW 100", None);
    let artist_id = app.artist("Kaiserdisco", None);
    let rel = app.release("Rebekah", "Murder", None);
    let label = app.label("Soma Records", None);
    app.set_release_label(rel, label);
    let src = FakePages::with_search(
        vec![
            ("https://kaiserdisco.bandcamp.com/music".to_string(), music_page(&[("/album/kd-raw-100", "", "KD RAW 100")])),
            ("https://soma-records.bandcamp.com/music".to_string(), music_page(&[("/album/murder", "Rebekah", "Murder")])),
        ],
        // One answer for both lookups: the page fetch is what proves it.
        vec![hit("artist", "Kaiserdisco", "https://kaiserdisco.bandcamp.com"), hit("label", "Soma Records", "https://soma-records.bandcamp.com")],
    );
    app.ctx.expect::<bc_bandcamp::harvest::labels::LabelResolver>().set_source(src as std::sync::Arc<dyn PageSource>);

    let (status, body) = app.post_empty(&format!("/artists/{artist_id}/locate")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["url"], "https://kaiserdisco.bandcamp.com");
    assert_eq!(body["matched"], 1);

    let (status, body) = app.post_empty(&format!("/labels/{label}/locate")).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["url"], "https://soma-records.bandcamp.com");

    assert_eq!(app.post_empty("/artists/99999/locate").await.0, 404);
    assert_eq!(app.post_empty("/labels/99999/locate").await.0, 404);
    let _ = json!(null);
}
