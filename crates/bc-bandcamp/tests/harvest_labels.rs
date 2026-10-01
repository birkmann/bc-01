//! Ports of `test_harvest_labels.py`: recovering labels from label pages the wishlist walk never
//! opened, backfilling label URLs, locating a label's page by search, and the label-page harvest
//! naming its own imprint.

mod harvest_common;

use std::sync::Arc;
use std::time::Duration;

use bc_bandcamp::extract::HarvestedRelease;
use bc_bandcamp::harvest::inbox::{self, AbsorbOpts, UpsertOpts};
use bc_bandcamp::harvest::labels::*;
use bc_bandcamp::sources::SearchHit;
use harvest_common::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;

const DCG: &str = "https://detroitclassicgallery.bandcamp.com";
const COT: &str = "https://childrenoftomorrowrecords.bandcamp.com";

fn seed_label_host(app: &App) {
    app.album_item(&format!("{DCG}/album/back-to-basics"), "E110101", "Back to Basics", None, "in_library");
    app.album_item(&format!("{DCG}/album/drowned-city"), "Sol Ortega", "Lost In A Drowned City", None, "in_library");
    app.album_item(&format!("{DCG}/album/away"), "ATYN", "Away", None, "in_library");
}

fn pages(v: Vec<(String, String)>) -> Arc<FakePages> {
    FakePages::new(v)
}

/// Run one resolution to completion; returns the final status.
async fn resolve(app: &App, src: Arc<FakePages>) -> bc_types::bandcamp::LabelResolveStatus {
    let r = app.ctx.expect::<LabelResolver>();
    r.set_source(src);
    r.start_run().await.expect("start");
    app.poll("/harvest/labels", |s| matches!(s["phase"].as_str(), Some("done" | "failed"))).await;
    r.status()
}

// -- candidates ------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn candidates_are_multi_artist_hosts_with_missing_labels() {
    let app = app().await;
    seed_label_host(&app);
    // A lone artist's own page: self-released, correctly unlabelled.
    app.album_item("https://solo.bandcamp.com/album/one", "Solo", "Some Record", None, "in_library");
    app.album_item("https://solo.bandcamp.com/album/two", "Solo", "Some Record", None, "in_library");
    // A label host with nothing left to fill.
    app.album_item("https://done.bandcamp.com/album/a", "A", "Some Record", Some("Done Recs"), "in_library");
    app.album_item("https://done.bandcamp.com/album/b", "B", "Some Record", Some("Done Recs"), "in_library");

    let candidates = app.q(find_candidates);

    assert_eq!(candidates.iter().map(|c| c.host.as_str()).collect::<Vec<_>>(), vec!["detroitclassicgallery.bandcamp.com"]);
    assert_eq!(candidates[0].sample_urls.len(), 2);
    // Two *different* artists' pages, so a label named like its first artist still resolves off
    // the second sample.
    assert_ne!(candidates[0].sample_urls[0], candidates[0].sample_urls[1]);
}

// -- applying --------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_fills_only_the_hosts_unlabelled_items() {
    let app = app().await;
    seed_label_host(&app);
    let kept = app.album_item(&format!("{DCG}/album/tagged"), "Dee Vek", "Some Record", Some("From The API"), "in_library");
    let other = app.album_item("https://elsewhere.bandcamp.com/album/x", "X", "Some Record", None, "in_library");

    let n = app.exec(|t| apply_label(t, "detroitclassicgallery.bandcamp.com", "Detroit Classic Gallery"));
    assert_eq!(n, 3);

    let mut filled: Vec<String> = app.q(|c| {
        let mut st = c.prepare("SELECT label_name FROM harvest_items WHERE url LIKE ?1")?;
        Ok(st.query_map([format!("{DCG}/%")], |r| r.get(0))?.collect::<Result<_, _>>()?)
    });
    filled.sort();
    filled.dedup();
    assert_eq!(filled, vec!["Detroit Classic Gallery".to_string(), "From The API".to_string()]);
    assert_eq!(app.item_label(kept).as_deref(), Some("From The API"));
    assert_eq!(app.item_label(other), None);
}

/// The guard for a mis-sampled artist page, same as the page extraction's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apply_never_files_an_artist_as_their_own_label() {
    let app = app().await;
    app.album_item("https://dying.bandcamp.com/album/a", "Dying", "Some Record", None, "in_library");
    app.album_item("https://dying.bandcamp.com/album/b", "Dying & Barakat", "Some Record", None, "in_library");

    assert_eq!(app.exec(|t| apply_label(t, "dying.bandcamp.com", "Dying")), 0);
}

// -- the sweep -------------------------------------------------------------------------

/// End to end: one page fetch resolves the host, every item gets the label, and the release on
/// the shelf is filed under it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sweep_labels_the_inbox_and_files_the_releases() {
    let app = app().await;
    seed_label_host(&app);
    let release = app.release("Sol Ortega", "Lost In A Drowned City", None);
    let src = pages(vec![(format!("{DCG}/album/back-to-basics"), album_html("E110101", "Back to Basics", "Detroit Classic Gallery"))]);

    let state = resolve(&app, src.clone()).await;

    assert_eq!(state.phase, "done");
    assert_eq!((state.resolved, state.labelled), (1, 3));
    assert_eq!(state.filed, 1);
    // The host's own /music page is asked first (it is not there, so the sample album settles
    // it) -- one album fetch resolved the whole host.
    assert_eq!(src.fetched(), vec![format!("{DCG}/music"), format!("{DCG}/album/back-to-basics")]);

    let labels: std::collections::HashSet<Option<String>> = app.q(|c| {
        let mut st = c.prepare("SELECT label_name FROM harvest_items WHERE url LIKE ?1")?;
        Ok(st.query_map([format!("{DCG}/%")], |r| r.get(0))?.collect::<Result<_, _>>()?)
    });
    assert_eq!(labels, [Some("Detroit Classic Gallery".to_string())].into_iter().collect());
    let name: Option<String> = app.q(move |c| {
        Ok(c.query_row("SELECT l.name FROM releases r JOIN labels l ON l.id = r.label_id WHERE r.id = ?1", [release], |r| r.get(0)).ok())
    });
    assert_eq!(name.as_deref(), Some("Detroit Classic Gallery"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sweep_survives_a_dead_sample_and_an_artist_page() {
    let app = app().await;
    seed_label_host(&app);
    // Both samples of this host will be fetched: the first 404s, the second is an artist page
    // stating no label (publisher == artist).
    let a = app.album_item("https://duo.bandcamp.com/album/a", "One Half", "Some Record", None, "in_library");
    let b = app.album_item("https://duo.bandcamp.com/album/b", "Other Half", "Some Record", None, "in_library");
    let src = pages(vec![
        (format!("{DCG}/album/back-to-basics"), album_html("E110101", "Back to Basics", "Detroit Classic Gallery")),
        ("https://duo.bandcamp.com/album/b".to_string(), album_html("Other Half", "B", "Other Half")),
    ]);

    let state = resolve(&app, src).await;

    assert_eq!(state.phase, "done");
    assert_eq!(state.resolved, 1, "the artist page must not resolve");
    assert_eq!((app.item_label(a), app.item_label(b)), (None, None));
}

/// The label page says what it is and what it sells: no album sample is needed, the inbox is
/// labelled, and every library release on that host is filed -- both the ones the grid lists and
/// the ones it left out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hosts_own_page_settles_it_and_files_the_library() {
    let app = app().await;
    app.album_item(&format!("{COT}/album/inercia-ep"), "Bartig Move", "Inercia EP", None, "in_library");
    app.album_item(&format!("{COT}/album/axe-ep"), "Arnaud Le Texier", "Axe EP", None, "in_library");
    let listed = app.release("Bartig Move", "Inercia EP", Some(&format!("{COT}/album/inercia-ep")));
    // On the host, but a truncated grid did not show it.
    let unlisted = app.release("RNGD", "Red Rose EP", Some(&format!("{COT}/album/red-rose-ep")));
    // The label putting out a record under its own name is not "on a label" -- an artist is
    // never filed as their own label.
    let own = app.release("Children Of Tomorrow Records", "Sampler", Some(&format!("{COT}/album/sampler")));
    let elsewhere = app.release("Someone", "Else", Some("https://other.bandcamp.com/album/x"));
    let src = pages(vec![(
        format!("{COT}/music"),
        label_page(
            "Children Of Tomorrow Records",
            &[(&format!("{COT}/album/inercia-ep"), "Bartig Move", "Inercia EP"), (&format!("{COT}/album/axe-ep"), "Arnaud Le Texier", "Axe EP")],
        ),
    )]);

    let state = resolve(&app, src.clone()).await;

    assert_eq!(state.phase, "done", "{:?}", state.error);
    assert_eq!((state.resolved, state.labelled, state.filed), (1, 2, 2));
    assert_eq!(src.fetched(), vec![format!("{COT}/music")], "no album sample once the page has spoken");

    let label_id: i64 = app.q(|c| Ok(c.query_row("SELECT id FROM labels WHERE name = 'Children Of Tomorrow Records'", [], |r| r.get(0))?));
    assert_eq!(app.label_url(label_id).as_deref(), Some(COT));
    assert_eq!(app.release_label_id(listed), Some(label_id));
    assert_eq!(app.release_label_id(unlisted), Some(label_id));
    assert_eq!(app.release_label_id(own), None);
    assert_eq!(app.release_label_id(elsewhere), None);
    let names: std::collections::HashSet<Option<String>> = app.q(|c| {
        let mut st = c.prepare("SELECT label_name FROM harvest_items WHERE url LIKE ?1")?;
        Ok(st.query_map([format!("{COT}/%")], |r| r.get(0))?.collect::<Result<_, _>>()?)
    });
    assert_eq!(names, [Some("Children Of Tomorrow Records".to_string())].into_iter().collect());
}

/// A record downloaded from a pasted URL never enters the inbox; its release row still names the
/// host, and two artists there is a label.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn library_releases_alone_make_a_host_a_candidate() {
    let app = app().await;
    app.release("A", "One", Some(&format!("{COT}/album/one")));
    app.release("B", "Two", Some(&format!("{COT}/album/two")));
    // One artist on their own page: not a candidate.
    app.release("Solo", "Three", Some("https://solo.bandcamp.com/album/three"));
    app.release("Solo", "Four", Some("https://solo.bandcamp.com/album/four"));

    let hosts: Vec<String> = app.q(find_candidates).into_iter().map(|c| c.host).collect();
    assert_eq!(hosts, vec!["childrenoftomorrowrecords.bandcamp.com".to_string()]);
}

/// A page without the label flag proves nothing about the host, so the album sample decides --
/// and, stating no publisher, leaves it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_artists_page_is_not_a_label_and_falls_back_to_the_sample() {
    let app = app().await;
    app.album_item("https://duo.bandcamp.com/album/a", "One Half", "Some Record", None, "in_library");
    app.album_item("https://duo.bandcamp.com/album/b", "Other Half", "Some Record", None, "in_library");
    let src = pages(vec![
        ("https://duo.bandcamp.com/music".to_string(), music_page(&[("https://duo.bandcamp.com/album/a", "One Half", "A")])),
        ("https://duo.bandcamp.com/album/a".to_string(), album_html("One Half", "A", "One Half")),
        ("https://duo.bandcamp.com/album/b".to_string(), album_html("Other Half", "B", "Other Half")),
    ]);

    let state = resolve(&app, src.clone()).await;

    assert_eq!(state.phase, "done");
    assert_eq!(state.resolved, 0);
    assert_eq!(src.fetched()[0], "https://duo.bandcamp.com/music");
}

/// Nobody should have to find a button: when a job settles, the hosts it brought in are
/// resolved. Progress events on the way do not count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_settled_download_job_starts_a_sweep_by_itself() {
    let app = app().await;
    seed_label_host(&app);
    let src = pages(vec![(format!("{DCG}/album/back-to-basics"), album_html("E110101", "Back to Basics", "DCG"))]);
    let r = app.ctx.expect::<LabelResolver>();
    r.set_source(src);
    r.watch(Duration::from_millis(50));
    tokio::time::sleep(Duration::from_millis(10)).await;

    app.ctx.bus.publish("job.progress", &json!({"job_id": "j", "status": "running"}));
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(r.status().phase, "idle", "a running job is not a settled one");

    app.ctx.bus.publish("job.progress", &json!({"job_id": "j", "status": "completed"}));
    let state = app.poll("/harvest/labels", |s| s["phase"] == "done").await;
    assert_eq!(state["resolved"], 1);
    r.stop_watch();
}

// -- label urls ------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn label_urls_come_from_a_dominant_multi_artist_host() {
    let app = app().await;
    // One multi-artist host holding all the label's items: pinned.
    app.album_item(&format!("{DCG}/album/a"), "X", "Some Record", Some("Detroit Classic Gallery"), "in_library");
    app.album_item(&format!("{DCG}/album/b"), "Y", "Some Record", Some("Detroit Classic Gallery"), "in_library");
    let pinned = app.label("Detroit Classic Gallery", None);

    // An imprint spread across its artists' pages: no single page is it.
    let ot = Some("Ostgut Ton");
    app.album_item("https://benklock.bandcamp.com/album/one", "Ben Klock", "Some Record", ot, "in_library");
    app.album_item("https://dettmann.bandcamp.com/album/two", "Dettmann", "Some Record", ot, "in_library");
    let spread = app.label("Ostgut Ton", None);

    // One host naming two labels: ambiguous, neither gets it.
    app.album_item("https://mixed.bandcamp.com/album/p", "P", "Some Record", Some("Left Recs"), "in_library");
    app.album_item("https://mixed.bandcamp.com/album/q", "Q", "Some Record", Some("Right Recs"), "in_library");
    let left = app.label("Left Recs", None);

    // A URL set by hand stands.
    let manual = app.label("Manual", Some("https://manual.example.com"));
    app.album_item("https://elsewhere.bandcamp.com/album/m", "M", "Some Record", Some("Manual"), "in_library");

    assert_eq!(app.exec(|t| backfill_label_urls(t)), 1);
    assert_eq!(app.label_url(pinned).as_deref(), Some("https://detroitclassicgallery.bandcamp.com"));
    assert_eq!(app.label_url(spread), None);
    assert_eq!(app.label_url(left), None);
    assert_eq!(app.label_url(manual).as_deref(), Some("https://manual.example.com"));
    // Idempotent.
    assert_eq!(app.exec(|t| backfill_label_urls(t)), 0);
}

// -- pending downloads -----------------------------------------------------------------

/// `pending_ids` is what the label page's Download button queues: fresh finds AND items from
/// earlier runs never queued -- but nothing owned, queued, or downloaded already.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn absorb_reports_items_still_awaiting_download() {
    let app = app().await;
    let entries = || {
        grid(&[
            ("https://pr.bandcamp.com/album/fresh-one", "Klint", "Fresh One"),
            ("https://pr.bandcamp.com/album/unconditional", "Roch", "Unconditional"),
            ("https://pr.bandcamp.com/album/queued-one", "Trunkline", "Queued One"),
        ])
    };
    app.release("Roch", "Unconditional", None); // already on the shelf
    app.album_item("https://pr.bandcamp.com/album/queued-one", "Trunkline", "Queued One", None, "queued");

    let opts = AbsorbOpts::new("label", "pr");
    let counts = inbox::absorb(&app.db, stream_of(entries()), &opts, None, &CancellationToken::new()).await.counts;
    let fresh_id: i64 = app.q(|c| Ok(c.query_row("SELECT id FROM harvest_items WHERE url LIKE '%fresh-one'", [], |r| r.get(0))?));
    assert_eq!(counts.pending_ids, vec![fresh_id]);

    // A re-run finds nothing new, but the unqueued item is still pending -- the Download button
    // must not vanish just because the run repeated.
    let again = inbox::absorb(&app.db, stream_of(entries()), &opts, None, &CancellationToken::new()).await.counts;
    assert_eq!(again.new, 0);
    assert_eq!(again.pending_ids, vec![fresh_id]);
}

/// The Soma Records shape: half the items live on the label's page, half on one artist's page
/// that merely credits the label. The multi-artist host is the label; the artist's page must
/// never be.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn label_url_picks_the_label_page_over_an_artists_own() {
    let app = app().await;
    let soma = "https://soma-records.bandcamp.com";
    for (slug, artist) in [("a", "Lakej"), ("b", "Rebekah"), ("c", "Temudo")] {
        app.album_item(&format!("{soma}/album/{slug}"), artist, "Some Record", Some("Soma Records"), "in_library");
    }
    for slug in ["x", "y"] {
        app.album_item(&format!("https://slam-djs.bandcamp.com/album/{slug}"), "Slam", "Some Record", Some("Soma Records"), "in_library");
    }
    let label = app.label("Soma Records", None);

    assert_eq!(app.exec(|t| backfill_label_urls(t)), 1);
    assert_eq!(app.label_url(label).as_deref(), Some(soma));
}

/// A few parent-label records on a sub-label's (multi-artist) page must not make that page the
/// parent's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn label_url_is_not_claimed_by_a_minority_sublabel_host() {
    let app = app().await;
    // Two Ostgut items on the sub-label's multi-artist host...
    app.album_item("https://unterton.bandcamp.com/album/a", "Etapp Kyle", "Some Record", Some("Ostgut Ton"), "in_library");
    app.album_item("https://unterton.bandcamp.com/album/b", "Barker", "Some Record", Some("Ostgut Ton"), "in_library");
    // ...but most Ostgut items live on single-artist pages.
    for (host, artist) in [("benklock", "Ben Klock"), ("dettmann", "Dettmann"), ("fixmer", "Fixmer")] {
        app.album_item(&format!("https://{host}.bandcamp.com/album/one"), artist, "Some Record", Some("Ostgut Ton"), "in_library");
    }
    let label = app.label("Ostgut Ton", None);

    assert_eq!(app.exec(|t| backfill_label_urls(t)), 0);
    assert_eq!(app.label_url(label), None);
}

// -- locating by search ----------------------------------------------------------------

fn hit(name: &str, url: &str) -> SearchHit {
    SearchHit { kind: "label".into(), name: name.into(), url: url.into(), ..Default::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locate_takes_the_search_hit_the_library_confirms() {
    let app = app().await;
    let release = app.release("Rebekah", "Murder In Birmingham EP", None);
    let label = app.label("Soma Records", None);
    app.set_release_label(release, label);

    let src = FakePages::with_search(
        vec![
            // First hit: same-ish name, but its catalogue shares nothing with the shelf --
            // must be rejected.
            ("https://somarecordsuk.bandcamp.com/music".to_string(), music_page(&[("/album/other", "Somebody", "Something Else")])),
            (
                "https://soma-records.bandcamp.com/music".to_string(),
                music_page(&[("/album/murder-in-birmingham-ep", "Rebekah", "Murder In Birmingham EP"), ("/album/unrelated", "Lakej", "Modified")]),
            ),
        ],
        vec![hit("Soma Records", "https://somarecordsuk.bandcamp.com"), hit("Soma Records", "https://soma-records.bandcamp.com")],
    );

    let result = locate_label_page(&app.db, &*src, label).await.expect("label exists");

    assert_eq!(result.url.as_deref(), Some("https://soma-records.bandcamp.com"));
    assert_eq!(result.matched, 1);
    assert_eq!(app.label_url(label).as_deref(), Some("https://soma-records.bandcamp.com"));
}

/// Search alone is not evidence: a same-named stranger must be rejected rather than harvested
/// forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locate_refuses_a_page_no_owned_release_confirms() {
    let app = app().await;
    let release = app.release("Roch", "Unconditional", None);
    let label = app.label("Left Recs", None);
    app.set_release_label(release, label);

    let src = FakePages::with_search(
        vec![("https://leftrecs.bandcamp.com/music".to_string(), music_page(&[("/album/not-ours", "Stranger", "Not Ours")]))],
        vec![hit("Left Recs", "https://leftrecs.bandcamp.com")],
    );

    let result = locate_label_page(&app.db, &*src, label).await.expect("label exists");

    assert_eq!(result.url, None);
    assert_eq!(app.label_url(label), None);
}

// -- label pages name their own releases -----------------------------------------------

fn upsert_label_run(app: &App, release: HarvestedRelease) -> Option<String> {
    let url = release.url.clone();
    app.exec(move |t| {
        inbox::upsert(
            t,
            &release,
            &UpsertOpts { source_kind: "label", source_label: "ostgut", label_name: Some("Ostgut Ton"), in_collection: false, in_wishlist: false, claim_source: true },
            None,
        )
        .map_err(|e| bc_db::DbError::Other(e.to_string()))?;
        Ok(())
    });
    app.q(move |c| Ok(c.query_row("SELECT label_name FROM harvest_items WHERE url = ?1", [url], |r| r.get(0))?))
}

/// A release on ostgut.bandcamp.com/music is on Ostgut Ton. The grid says so nowhere per item,
/// so the caller does -- without it a shelf showed 39 of the 137 records actually held from that
/// label.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_label_page_harvest_names_the_label_on_every_item() {
    let app = app().await;
    let label = upsert_label_run(
        &app,
        HarvestedRelease {
            url: "https://ostgut.bandcamp.com/album/alpha".into(),
            title: "Alpha".into(),
            artist_name: "Ben Klock".into(),
            ..Default::default()
        },
    );
    assert_eq!(label.as_deref(), Some("Ostgut Ton"));
}

/// A sub-label record sold on its parent's page is not the parent's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_that_states_its_own_label_keeps_it() {
    let app = app().await;
    let label = upsert_label_run(
        &app,
        HarvestedRelease {
            url: "https://ostgut.bandcamp.com/album/continuum".into(),
            title: "Continuum EP".into(),
            artist_name: "Etapp Kyle".into(),
            label_name: Some("Unterton".into()),
            ..Default::default()
        },
    );
    assert_eq!(label.as_deref(), Some("Unterton"));
}

/// The repair for everything already harvested: source_label is the page's slug, and a label
/// whose URL yields that slug is where it came from.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backfill_names_labels_on_past_label_page_harvests() {
    let app = app().await;
    let label = app.label("Ostgut Ton", Some("https://ostgut.bandcamp.com"));
    let row = |url: &str, state: &str, artist: &str, title: &str, source_label: &str| {
        let (url, state, artist, title, sl) = (url.to_string(), state.to_string(), artist.to_string(), title.to_string(), source_label.to_string());
        app.exec(move |t| {
            t.execute(
                "INSERT INTO harvest_items(url, url_kind, state, title, artist_name, tags, source_kind, source_label, in_collection, in_wishlist, \
                 is_free_download, is_purchasable, is_preorder, discovered_at) VALUES (?1,'album',?2,?3,?4,'[]','label',?5,0,0,0,1,0,datetime('now'))",
                bc_db::rusqlite::params![url, state, title, artist, sl],
            )?;
            Ok(())
        });
    };
    row("https://ostgut.bandcamp.com/album/alpha", "in_library", "Ben Klock", "Alpha", "ostgut");
    // A different page's harvest must not be touched.
    row("https://elsewhere.bandcamp.com/album/x", "new", "Someone", "X", "elsewhere");
    // An artist filed as their own label is the one thing this must never write.
    row("https://ostgut.bandcamp.com/album/self", "new", "Ostgut Ton", "Label Compilation", "ostgut");

    assert_eq!(app.exec(|t| backfill_harvest_label_names(t)), 1);
    let rows: std::collections::HashMap<String, Option<String>> = app.q(|c| {
        let mut st = c.prepare("SELECT url, label_name FROM harvest_items")?;
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?)
    });
    assert_eq!(rows["https://ostgut.bandcamp.com/album/alpha"].as_deref(), Some("Ostgut Ton"));
    assert_eq!(rows["https://elsewhere.bandcamp.com/album/x"], None);
    assert_eq!(rows["https://ostgut.bandcamp.com/album/self"], None);
    assert_eq!(app.label_url(label).as_deref(), Some("https://ostgut.bandcamp.com"));
    // Idempotent.
    assert_eq!(app.exec(|t| backfill_harvest_label_names(t)), 0);
}
