//! Ports of `test_harvest_sweep.py`: the label sweep -- many shelves' new releases, one action.
//!
//! The point of the feature is that check -> absorb -> queue happens once for a whole armful of
//! labels, because doing it label by label meant hundreds of rounds of menu, wait, Download
//! button. The armful is either the folders that were ticked on the shelf or, with nothing
//! ticked, every label there is. The route-level cases (202 + job, cancel through the generic
//! job route) are the Rust additions.

mod harvest_common;

use std::sync::Arc;
use std::time::Duration;

use bc_bandcamp::harvest::sweep::ArtistStreamFn;
use bc_bandcamp::sources::EventStream;
use harvest_common::*;
use serde_json::{Value, json};

const PLANET_URL: &str = "https://planet.bandcamp.com";
const SOMA_URL: &str = "https://soma.bandcamp.com";
const LS: &str = "/harvest/labels/sweep";

async fn finished(app: &App) -> Value {
    app.poll(LS, |s| s["running"] == json!(false)).await
}

/// A stream per page that sleeps `delay` before yielding its one release (the label the stop
/// interrupts), the others answer at once.
fn hanging(hang_on: &'static str, delay: Duration) -> ArtistStreamFn {
    Arc::new(move |_c, url, _d, _l| {
        let s: EventStream = Box::pin(async_stream::try_stream! {
            if url == hang_on {
                tokio::time::sleep(delay).await;
            }
            yield shallow_event(&format!("{url}/album/one"), "Fresh Artist", &url, 1, 1);
        });
        s
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_by_default() {
    let app = app().await;
    let (_, body) = app.get(LS).await;
    assert_eq!(body["phase"], "idle");
    assert_eq!(body["running"], json!(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sweep_checks_every_label_and_queues_what_is_missing() {
    let app = app().await;
    app.label("Planet Rhythm", Some(PLANET_URL));
    app.label("Soma", Some(SOMA_URL));
    app.release("Owned Artist", "Owned Album", None);
    app.sweeps().set_artist_stream(fake_pages(vec![
        (
            PLANET_URL,
            Ok(grid(&[
                ("https://planet.bandcamp.com/album/owned-album", "Owned Artist", "Owned Album"),
                ("https://planet.bandcamp.com/album/fresh-one", "Fresh Artist", "One"),
            ])),
        ),
        (SOMA_URL, Ok(grid(&[("https://soma.bandcamp.com/album/fresh-two", "Other Artist", "Two")]))),
    ]));

    let (status, started) = app.post_empty(LS).await;
    assert_eq!(status, 202);
    assert_eq!(started["running"], json!(true));
    let state = finished(&app).await;

    assert_eq!(state["phase"], "done", "{state}");
    assert_eq!(state["done"], 2);
    assert_eq!(state["total"], 2);
    assert_eq!(state["seen"], 3);
    assert_eq!(state["in_library"], 1, "the seeded release must be recognised, not re-queued");
    assert_eq!(state["queued"], 2);
    assert!(state["job_id"].is_string());

    // Everything on a label's own page is on that label; the sweep has to say so per item
    // because the grid never does.
    let (st, label) = app.item_by_title("One");
    assert_eq!(st, "queued");
    assert_eq!(label.as_deref(), Some("Planet Rhythm"));

    // The sweep itself is a job (kind `sweep`), the downloads a separate one.
    let (_, jobs) = app.get("/jobs?kind=sweep").await;
    assert_eq!(jobs["total"], 1);
    let (_, dl) = app.get(&format!("/jobs/{}", state["job_id"].as_str().unwrap())).await;
    assert_eq!(dl["kind"], "download");
    assert_eq!(dl["total"], 2);
}

/// Same flags as the per-label Download button: unowned releases are the point of the sweep, and
/// a subfolder would duplicate artists already at the root and blind bandcamp-dl's already-have
/// check.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sweep_queues_unowned_items_into_the_downloads_root() {
    let app = app().await;
    app.label("Planet Rhythm", Some(PLANET_URL));
    app.sweeps().set_artist_stream(fake_pages(vec![(PLANET_URL, Ok(grid(&[("https://planet.bandcamp.com/album/one", "Fresh Artist", "One")])))]));

    app.post_empty(LS).await;
    let state = finished(&app).await;

    assert_eq!(state["queued"], 1);
    assert_eq!(app.target_dirs(), vec![None::<String>]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn labels_without_a_page_are_counted_and_skipped() {
    let app = app().await;
    app.label("Planet Rhythm", Some(PLANET_URL));
    app.label("No Page Records", None);
    app.sweeps().set_artist_stream(fake_pages(vec![(PLANET_URL, Ok(grid(&[("https://planet.bandcamp.com/album/one", "Fresh Artist", "One")])))]));

    app.post_empty(LS).await;
    let state = finished(&app).await;

    assert_eq!(state["phase"], "done");
    assert_eq!(state["total"], 1);
    assert_eq!(state["no_url"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_dead_label_page_does_not_kill_the_sweep() {
    let app = app().await;
    app.label("Dead Air", Some("https://dead.bandcamp.com"));
    app.label("Planet Rhythm", Some(PLANET_URL));
    app.sweeps().set_artist_stream(fake_pages(vec![
        ("https://dead.bandcamp.com", Err("HTTP 404")),
        (PLANET_URL, Ok(grid(&[("https://planet.bandcamp.com/album/one", "Fresh Artist", "One")]))),
    ]));

    app.post_empty(LS).await;
    let state = finished(&app).await;

    assert_eq!(state["phase"], "done");
    assert_eq!(state["done"], 2);
    assert_eq!(state["queued"], 1);
    assert!(state["errors"].as_array().unwrap().iter().any(|e| e.as_str().unwrap().contains("Dead Air")), "{state}");
}

/// Ticking eight folders on a shelf of thousands must fetch eight pages.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_selection_checks_only_the_labels_it_was_given() {
    let app = app().await;
    app.label("Planet Rhythm", Some(PLANET_URL));
    let soma = app.label("Soma", Some(SOMA_URL));
    app.sweeps().set_artist_stream(fake_pages(vec![
        (PLANET_URL, Ok(grid(&[("https://planet.bandcamp.com/album/one", "Fresh Artist", "One")]))),
        (SOMA_URL, Ok(grid(&[("https://soma.bandcamp.com/album/two", "Other Artist", "Two")]))),
    ]));

    let (status, _) = app.post(LS, json!({"label_ids": [soma]})).await;
    assert_eq!(status, 202);
    let state = finished(&app).await;

    assert_eq!(state["phase"], "done");
    assert_eq!(state["scope"], "selection");
    assert_eq!(state["total"], 1);
    assert_eq!(state["queued"], 1);
    // Planet Rhythm was never opened, so nothing of its page is here.
    let titles: Vec<String> = app.q(|c| {
        let mut st = c.prepare("SELECT title FROM harvest_items")?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
    });
    assert_eq!(titles, vec!["Two".to_string()]);
}

/// A press with nothing ticked is the all-labels run, not a run of nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_selection_sweeps_the_whole_shelf() {
    let app = app().await;
    app.label("Planet Rhythm", Some(PLANET_URL));
    app.label("Soma", Some(SOMA_URL));
    app.sweeps().set_artist_stream(fake_pages(vec![
        (PLANET_URL, Ok(grid(&[("https://planet.bandcamp.com/album/one", "Fresh Artist", "One")]))),
        (SOMA_URL, Ok(grid(&[("https://soma.bandcamp.com/album/two", "Other Artist", "Two")]))),
    ]));

    app.post(LS, json!({"label_ids": []})).await;
    let state = finished(&app).await;

    assert_eq!(state["scope"], "all");
    assert_eq!(state["total"], 2);
    assert_eq!(state["queued"], 2);
}

/// Stop ends the fetching, not the downloading: an hour-long walk called off after two labels
/// still queues those two labels' releases.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_keeps_what_the_walk_had_already_found() {
    let app = app().await;
    app.label("Alpha", Some(PLANET_URL));
    app.label("Zeta", Some(SOMA_URL));
    app.sweeps().set_artist_stream(hanging(SOMA_URL, Duration::from_secs(30)));

    app.post_empty(LS).await;
    app.poll(LS, |s| s["done"].as_i64().unwrap_or(0) >= 1).await;
    let (status, body) = app.delete(LS).await;

    assert_eq!(status, 200);
    assert_eq!(body["phase"], "done");
    assert_eq!(body["error"], "Stopped");
    assert_eq!(body["done"], 1, "the second label was still being fetched");
    assert_eq!(body["queued"], 1, "what the walk had found is queued all the same");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_an_idle_sweep_is_not_an_error() {
    let app = app().await;
    let (status, body) = app.delete(LS).await;
    assert_eq!(status, 200);
    assert_eq!(body["phase"], "idle");
}

/// Two concurrent sweeps would double the requests to Bandcamp for nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_sweep_is_refused_while_one_runs() {
    let app = app().await;
    app.label("Planet Rhythm", Some(PLANET_URL));
    app.sweeps().set_artist_stream(hanging(PLANET_URL, Duration::from_secs(5)));

    assert_eq!(app.post_empty(LS).await.0, 202);
    let (status, body) = app.post_empty(LS).await;

    assert_eq!(status, 400);
    assert!(body["detail"].as_str().unwrap().to_lowercase().contains("already running"), "{body}");
    app.delete(LS).await;
}

/// Cancelling the sweep's job through the generic job route is the same "stop": the walk ends,
/// what it found is queued.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_sweep_job_stops_the_walk_and_keeps_what_was_found() {
    let app = app().await;
    app.label("Alpha", Some(PLANET_URL));
    app.label("Zeta", Some(SOMA_URL));
    app.sweeps().set_artist_stream(hanging(SOMA_URL, Duration::from_secs(30)));

    app.post_empty(LS).await;
    app.poll(LS, |s| s["done"].as_i64().unwrap_or(0) >= 1).await;
    let (_, jobs) = app.get("/jobs?kind=sweep").await;
    let job_id = jobs["items"][0]["id"].as_str().unwrap().to_string();
    let (status, job) = app.post_empty(&format!("/jobs/{job_id}/cancel")).await;
    assert_eq!(status, 200);
    assert_eq!(job["status"], "cancelled");

    let state = app.poll(LS, |s| s["running"] == json!(false)).await;
    assert_eq!(state["phase"], "done");
    assert_eq!(state["error"], "Stopped");
    assert_eq!(state["queued"], 1);
}
