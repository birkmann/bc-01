//! The harvest run as a job (`POST /harvest/run` -> `202 {job_id}`, `GET /harvest/runs/{id}`):
//! the legacy `run` route's behaviours (own-wishlist flag, label filing, validation) plus the job
//! lifecycle (progress/completed events, cancel, failure).

mod harvest_common;

use std::sync::Arc;
use std::time::Duration;

use bc_bandcamp::error::HarvestError;
use bc_bandcamp::harvest::runs::{RunService, StreamBuilder};
use bc_bandcamp::sources::EventStream;
use harvest_common::*;
use serde_json::{Value, json};

const RUN: &str = "/harvest/run";

async fn run_and_wait(app: &App, body: Value) -> Value {
    let (status, accepted) = app.post(RUN, body).await;
    assert_eq!(status, 202, "{accepted}");
    let job_id = accepted["job_id"].as_str().expect("job_id").to_string();
    app.poll(&format!("/jobs/{job_id}"), |j| matches!(j["status"].as_str(), Some("completed" | "failed" | "cancelled"))).await;
    let (status, result) = app.get(&format!("/harvest/runs/{job_id}")).await;
    assert_eq!(status, 200, "{result}");
    result
}

fn builder(f: impl Fn(&bc_bandcamp::net::BandcampClient, &bc_types::bandcamp::RunRequest) -> EventStream + Send + Sync + 'static) -> StreamBuilder {
    Arc::new(move |c, r| Ok(f(c, r)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_url_list_run_is_a_job_and_lands_in_the_inbox() {
    let app = app().await;
    let mut rx = app.listen();
    let text = "https://a.bandcamp.com/album/one\nnot a url\nhttps://b.bandcamp.com/track/two?from=x\nhttps://a.bandcamp.com/album/one";

    let (status, accepted) = app.post(RUN, json!({"kind": "url_list", "text": text})).await;
    assert_eq!(status, 202);
    let job_id = accepted["job_id"].as_str().unwrap().to_string();
    // The job exists at once, kind `harvest`, one item.
    let (_, job) = app.get(&format!("/jobs/{job_id}")).await;
    assert_eq!((job["kind"].as_str(), job["total"].as_i64()), (Some("harvest"), Some(1)));

    app.poll(&format!("/jobs/{job_id}"), |j| j["status"] == "completed").await;
    let (status, result) = app.get(&format!("/harvest/runs/{job_id}")).await;
    assert_eq!(status, 200);
    assert_eq!((result["kind"].as_str(), result["seen"].as_i64(), result["new"].as_i64()), (Some("url_list"), Some(2), Some(2)));
    assert_eq!(result["label"], "url_list");
    assert_eq!(result["pending_item_ids"].as_array().unwrap().len(), 2);
    assert_eq!(app.get("/harvest/stats").await.1, json!({"new": 2}));

    let topics: Vec<String> = drain_events(&mut rx).into_iter().map(|e| e.topic).collect();
    assert!(topics.contains(&"harvest.completed".to_string()), "{topics:?}");
    // Re-running finds them already known.
    let again = run_and_wait(&app, json!({"kind": "url_list", "text": text})).await;
    assert_eq!((again["new"].as_i64(), again["already_known"].as_i64()), (Some(0), Some(2)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_runs_are_refused_before_any_job_exists() {
    let app = app().await;
    for (body, want) in [
        (json!({"kind": "artist"}), 400),
        (json!({"kind": "wishlist"}), 400),
        (json!({"kind": "nonsense", "url": "https://x.bandcamp.com"}), 400),
        (json!({"kind": "url_list", "limit": 0}), 422),
        (json!({"kind": "url_list", "limit": 25001}), 422),
        (json!({"kind": "url_list", "depth": "deep"}), 422),
    ] {
        assert_eq!(app.post(RUN, body.clone()).await.0, want, "{body}");
    }
    assert_eq!(app.count("jobs"), 0);
    assert_eq!(app.get("/harvest/runs/nope").await.0, 404);
}

/// A label run is the one source that names the label for everything it yields, so the shelf
/// reflects it at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_label_run_names_and_files_what_it_found() {
    let app = app().await;
    let release = app.release("Ben Klock", "Alpha", None);
    app.ctx.expect::<RunService>().set_stream_builder(builder(|_, _| {
        stream_of(grid(&[("https://ostgut.bandcamp.com/album/alpha", "Ben Klock", "Alpha"), ("https://ostgut.bandcamp.com/album/beta", "Dettmann", "Beta")]))
    }));

    let result = run_and_wait(&app, json!({"kind": "label", "url": "https://ostgut.bandcamp.com", "label_name": "Ostgut Ton"})).await;

    assert_eq!((result["seen"].as_i64(), result["in_library"].as_i64()), (Some(2), Some(1)));
    assert_eq!(result["label"], "ostgut");
    assert_eq!(app.item_by_title("Beta").1.as_deref(), Some("Ostgut Ton"));
    assert!(app.release_label_id(release).is_some(), "the owned release is filed under the label");
}

/// "in_wishlist" is the sticky "on MY wishlist" flag: walking anyone else's list through the
/// generic route must not set it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_my_own_wishlist_sets_the_sticky_flag() {
    let app = app().await;
    app.exec(|t| {
        Ok(t.execute("INSERT INTO fans(username, url, is_self, created_at) VALUES ('Me', 'https://bandcamp.com/me', 1, datetime('now'))", []).map(|_| ())?)
    });
    app.ctx.expect::<RunService>().set_stream_builder(builder(|_, r| {
        let who = if r.url.as_deref().unwrap_or("").contains("/me") { "mine" } else { "theirs" };
        stream_of(grid(&[(&format!("https://x.bandcamp.com/album/{who}"), "A", who)]))
    }));

    run_and_wait(&app, json!({"kind": "wishlist", "url": "https://bandcamp.com/me/wishlist"})).await;
    run_and_wait(&app, json!({"kind": "wishlist", "url": "https://bandcamp.com/someone/wishlist"})).await;

    let flags: Vec<(String, bool)> = app.q(|c| {
        let mut st = c.prepare("SELECT title, in_wishlist FROM harvest_items ORDER BY title")?;
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?)
    });
    assert_eq!(flags, vec![("mine".to_string(), true), ("theirs".to_string(), false)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_big_run_reports_coalesced_progress() {
    let app = app().await;
    let mut rx = app.listen();
    let entries: Grid = (0..120).map(|i| (format!("https://x.bandcamp.com/album/r{i}"), "A".to_string(), format!("R{i}"))).collect();
    app.ctx.expect::<RunService>().set_stream_builder(builder(move |_, _| stream_of(entries.clone())));

    let result = run_and_wait(&app, json!({"kind": "artist", "url": "https://x.bandcamp.com", "limit": 500})).await;
    assert_eq!(result["seen"], 120);

    let events = drain_events(&mut rx);
    let progress: Vec<&Value> = events.iter().filter(|e| e.topic == "harvest.progress").map(|e| &e.payload).collect();
    assert!(!progress.is_empty() && progress.len() <= 3, "coalesced: {} events", progress.len());
    assert!(progress.iter().all(|p| p["kind"] == "artist" && p["job_id"].is_string()));
    let done = events.iter().find(|e| e.topic == "harvest.completed").expect("completed event");
    assert_eq!((done.payload["seen"].as_i64(), done.payload["new"].as_i64()), (Some(120), Some(120)));
}

/// Cancelling stops the fetching, not the remembering: the partial batch is flushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_run_keeps_what_it_had_flushed() {
    let app = app().await;
    app.ctx.expect::<RunService>().set_stream_builder(builder(|_, _| {
        Box::pin(async_stream::try_stream! {
            yield shallow_event("https://x.bandcamp.com/album/first", "A", "First", 1, 2);
            tokio::time::sleep(Duration::from_secs(30)).await;
            yield shallow_event("https://x.bandcamp.com/album/second", "A", "Second", 2, 2);
        })
    }));
    let (_, accepted) = app.post(RUN, json!({"kind": "artist", "url": "https://x.bandcamp.com"})).await;
    let job_id = accepted["job_id"].as_str().unwrap().to_string();
    app.poll(&format!("/jobs/{job_id}"), |j| j["status"] == "running").await;
    // Not finished yet: no result.
    assert_eq!(app.get(&format!("/harvest/runs/{job_id}")).await.0, 404);

    let (status, job) = app.post_empty(&format!("/jobs/{job_id}/cancel")).await;
    assert_eq!((status, job["status"].as_str()), (200, Some("cancelled")));

    assert_eq!(app.get(&format!("/harvest/runs/{job_id}")).await.0, 409);
    // The partial batch is flushed by the run task as it unwinds, which can land just after the
    // cancel response under load; allow it a bounded moment.
    for _ in 0..200 {
        if app.count("harvest_items") >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(app.count("harvest_items"), 1, "the first release was flushed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_source_fails_the_job_and_identity_expiry_is_a_401() {
    let app = app().await;
    app.ctx.expect::<RunService>().set_stream_builder(builder(|_, r| {
        let expired = r.url.as_deref() == Some("https://bandcamp.com/me");
        Box::pin(async_stream::try_stream! {
            yield shallow_event("https://x.bandcamp.com/album/first", "A", "First", 1, 2);
            if expired {
                Err(HarvestError::IdentityExpired("must be logged in".into()))?;
            } else {
                Err(HarvestError::other("HTTP 500 for x"))?;
            }
        })
    }));

    let (_, a) = app.post(RUN, json!({"kind": "artist", "url": "https://x.bandcamp.com"})).await;
    let id = a["job_id"].as_str().unwrap().to_string();
    app.poll(&format!("/jobs/{id}"), |j| j["status"] == "failed").await;
    let (status, body) = app.get(&format!("/harvest/runs/{id}")).await;
    assert_eq!(status, 400);
    assert!(body["detail"].as_str().unwrap().contains("harvest failed: HTTP 500"), "{body}");

    let (_, a) = app.post(RUN, json!({"kind": "wishlist", "url": "https://bandcamp.com/me"})).await;
    let id = a["job_id"].as_str().unwrap().to_string();
    app.poll(&format!("/jobs/{id}"), |j| j["status"] == "failed").await;
    assert_eq!(app.get(&format!("/harvest/runs/{id}")).await.0, 401);
}
