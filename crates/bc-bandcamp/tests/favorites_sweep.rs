//! Ports of `test_favorites_sweep.py`: one press, every pinned artist and label checked.
//!
//! The per-source flow already exists behind each artist's and label's menu. What this adds is
//! the version that needs no decision -- so the tests are mostly about what the button must
//! *not* ask of the user: it takes no arguments, it covers both kinds of pin in one walk, it
//! leaves pinned tags alone, and it is stoppable while the walk is still going.

mod harvest_common;

use std::sync::Arc;
use std::time::Duration;

use bc_bandcamp::harvest::sweep::ArtistStreamFn;
use bc_bandcamp::sources::EventStream;
use harvest_common::*;
use serde_json::{Value, json};

const ARTIST_URL: &str = "https://timewriter.bandcamp.com";
const LABEL_URL: &str = "https://rhythmsection.bandcamp.com";
const FS: &str = "/harvest/favorites/sweep";

async fn finished(app: &App) -> Value {
    app.poll(FS, |s| s["running"] == json!(false)).await
}

fn pin_artist(app: &App, name: &str, url: Option<&str>) -> i64 {
    let id = app.artist(name, url);
    app.favorite_artist(id);
    id
}

fn pin_label(app: &App, name: &str, url: Option<&str>) -> i64 {
    let id = app.label(name, url);
    app.favorite_label(id);
    id
}

/// The second page hangs for `delay`, the others answer at once.
fn one_then_hang(hang_on: &'static str, delay: Duration) -> ArtistStreamFn {
    Arc::new(move |_c, url, _d, _l| {
        let s: EventStream = Box::pin(async_stream::try_stream! {
            if url == hang_on {
                tokio::time::sleep(delay).await;
            }
            yield shallow_event(&format!("{url}/album/x"), "A", "X", 1, 1);
        });
        s
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_by_default() {
    let app = app().await;
    let (_, body) = app.get(FS).await;
    assert_eq!(body["phase"], "idle");
    assert_eq!(body["running"], json!(false));
}

/// The strip shows both kinds side by side, so a button under it that did only one of them would
/// be a lie.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_press_checks_pinned_artists_and_labels_together() {
    let app = app().await;
    pin_artist(&app, "The Timewriter", Some(ARTIST_URL));
    pin_label(&app, "Rhythm Section", Some(LABEL_URL));
    // A record by the artist you pinned, already on the shelf.
    app.release("The Timewriter", "Have It", None);
    app.sweeps().set_artist_stream(fake_pages(vec![
        (
            ARTIST_URL,
            Ok(grid(&[
                ("https://timewriter.bandcamp.com/album/have-it", "The Timewriter", "Have It"),
                ("https://timewriter.bandcamp.com/album/new-one", "The Timewriter", "New One"),
            ])),
        ),
        (LABEL_URL, Ok(grid(&[("https://rhythmsection.bandcamp.com/album/new-two", "Session Victim", "New Two")]))),
    ]));

    // No body at all: the whole point is that the press takes no decision.
    assert_eq!(app.post_empty(FS).await.0, 202);
    let state = finished(&app).await;

    assert_eq!(state["phase"], "done", "{state}");
    assert_eq!(state["total"], 2, "one artist and one label, in one walk");
    assert_eq!(state["done"], 2);
    assert_eq!(state["seen"], 3);
    assert_eq!(state["in_library"], 1, "the record already held must be recognised");
    assert_eq!(state["queued"], 2);
    assert!(state["job_id"].is_string());
}

/// Everything on a label's own page is on that label, and the grid never says so per item. An
/// artist's page proves nothing of the kind -- their records come out on other people's
/// imprints, and filing them under a label named after the artist would invent a shelf that
/// does not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_label_page_names_its_imprint_and_an_artist_page_does_not() {
    let app = app().await;
    pin_artist(&app, "The Timewriter", Some(ARTIST_URL));
    pin_label(&app, "Rhythm Section", Some(LABEL_URL));
    app.sweeps().set_artist_stream(fake_pages(vec![
        (ARTIST_URL, Ok(grid(&[("https://timewriter.bandcamp.com/album/a", "The Timewriter", "From Artist")]))),
        (LABEL_URL, Ok(grid(&[("https://rhythmsection.bandcamp.com/album/b", "Session Victim", "From Label")]))),
    ]));
    app.post_empty(FS).await;
    finished(&app).await;

    assert_eq!(app.item_by_title("From Label").1.as_deref(), Some("Rhythm Section"));
    assert_eq!(app.item_by_title("From Artist").1, None);
}

/// A tag has no catalogue page, so there is nothing to walk -- and nothing to report as skipped
/// either, or the total would imply a source that was never one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pinned_tag_is_neither_swept_nor_counted_as_skipped() {
    let app = app().await;
    pin_artist(&app, "The Timewriter", Some(ARTIST_URL));
    app.favorite_tag("deep house");
    app.sweeps().set_artist_stream(fake_pages(vec![(ARTIST_URL, Ok(grid(&[("https://timewriter.bandcamp.com/album/a", "T", "A")])))]));

    app.post_empty(FS).await;
    let state = finished(&app).await;

    assert_eq!(state["total"], 1);
    assert_eq!(state["no_url"], 0);
}

/// Locating a page is a search plus page fetches and occasionally wrong, so it stays a
/// deliberate per-source action rather than something a sweep of everything does silently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_favourite_with_no_bandcamp_page_is_counted_and_skipped() {
    let app = app().await;
    pin_artist(&app, "The Timewriter", Some(ARTIST_URL));
    pin_label(&app, "No Page Records", None);
    app.sweeps().set_artist_stream(fake_pages(vec![(ARTIST_URL, Ok(grid(&[("https://timewriter.bandcamp.com/album/a", "T", "A")])))]));

    app.post_empty(FS).await;
    let state = finished(&app).await;

    assert_eq!(state["total"], 1);
    assert_eq!(state["no_url"], 1);
    assert_eq!(state["phase"], "done");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_dead_page_does_not_kill_the_sweep() {
    let app = app().await;
    pin_artist(&app, "Gone", Some("https://gone.bandcamp.com"));
    pin_label(&app, "Rhythm Section", Some(LABEL_URL));
    app.sweeps().set_artist_stream(fake_pages(vec![
        ("https://gone.bandcamp.com", Err("404 not found")),
        (LABEL_URL, Ok(grid(&[("https://rhythmsection.bandcamp.com/album/b", "Session Victim", "Two")]))),
    ]));

    app.post_empty(FS).await;
    let state = finished(&app).await;

    assert_eq!(state["phase"], "done");
    assert_eq!(state["queued"], 1, "the reachable favourite still got through");
    assert!(state["errors"].as_array().unwrap().iter().any(|e| e.as_str().unwrap().contains("Gone")), "{state}");
}

/// The strip hides itself when nothing is pinned, but the endpoint must still answer rather
/// than fail on an empty walk.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_pinned_is_a_clean_no_op() {
    let app = app().await;
    app.post_empty(FS).await;
    let state = finished(&app).await;
    assert_eq!(state["phase"], "done");
    assert_eq!(state["total"], 0);
    assert_eq!(state["queued"], 0);
}

/// A second walk would re-fetch every page at the same polite rate, so it is refused rather
/// than queued.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_press_is_refused_while_one_runs() {
    let app = app().await;
    pin_artist(&app, "The Timewriter", Some(ARTIST_URL));
    app.sweeps().set_artist_stream(one_then_hang(ARTIST_URL, Duration::from_millis(500)));

    assert_eq!(app.post_empty(FS).await.0, 202);
    app.poll(FS, |s| s["running"] == json!(true)).await;
    assert_eq!(app.post_empty(FS).await.0, 400);
    app.delete(FS).await;
}

/// They share a class and an app; they must not share a report. A label sweep's numbers showing
/// up under the Home button would be nonsense.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_two_sweeps_keep_their_own_state() {
    let app = app().await;
    pin_artist(&app, "The Timewriter", Some(ARTIST_URL));
    app.sweeps().set_artist_stream(fake_pages(vec![(ARTIST_URL, Ok(grid(&[("https://timewriter.bandcamp.com/album/a", "T", "A")])))]));
    app.post_empty(FS).await;
    finished(&app).await;

    assert_eq!(app.get(FS).await.1["phase"], "done");
    assert_eq!(app.get("/harvest/labels/sweep").await.1["phase"], "idle");
}

/// Stopping means "stop fetching pages", not "throw away what you found": the walk is the slow
/// half, so a press of Stop still leaves the downloads the sweep had turned up by then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_keeps_what_the_walk_had_already_found() {
    let app = app().await;
    pin_artist(&app, "AAA First", Some(ARTIST_URL));
    pin_artist(&app, "ZZZ Second", Some("https://second.bandcamp.com"));
    app.sweeps().set_artist_stream(one_then_hang("https://second.bandcamp.com", Duration::from_secs(5)));

    app.post_empty(FS).await;
    app.poll(FS, |s| s["done"].as_i64().unwrap_or(0) >= 1).await;
    app.delete(FS).await;

    let (_, state) = app.get(FS).await;
    assert_eq!(state["phase"], "done");
    assert_eq!(state["error"], "Stopped", "a partial walk has to say so");
    assert_eq!(state["queued"], 1);
}
