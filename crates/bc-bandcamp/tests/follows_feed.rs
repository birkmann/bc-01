//! Follows and the feed sweep: what you watch, checked in one pass, queued never
//! (`test_follows.py`), plus the scheduler's due/not-due logic with an injectable clock.
//!
//! A follow is a saved place new music appears -- a discover query, an artist or label page, or
//! the library's own shelves. The sweep absorbs them all into the inbox as
//! `source_kind = "follow"` and stops there: the feed is a triage surface, so nothing is queued
//! without a press.
#[path = "fans_common/mod.rs"]
mod fans_common;

use std::sync::Arc;
use std::time::Duration;

use bc_bandcamp::harvest::feed::{self, FeedScheduler};
use chrono::{DateTime, TimeZone, Utc};
use fans_common::*;
use serde_json::json;

const PLANET_URL: &str = "https://planet.bandcamp.com";
const SOLO_URL: &str = "https://soloartist.bandcamp.com";

fn seed_label(fx: &Fx, name: &str, url: Option<&str>) {
    let (n, u) = (name.to_string(), url.map(str::to_string));
    fx.ctx
        .db
        .write(move |tx| {
            tx.execute(
                "INSERT INTO labels(name, name_key, bandcamp_url) VALUES (?1, ?2, ?3)",
                bc_db::rusqlite::params![n, bc_db::util::name_key(&n), u],
            )?;
            Ok(())
        })
        .unwrap();
}

fn seed_artist(fx: &Fx, name: &str, url: Option<&str>) {
    let (n, u) = (name.to_string(), url.map(str::to_string));
    fx.ctx
        .db
        .write(move |tx| {
            tx.execute(
                "INSERT INTO artists(name, name_key, bandcamp_url, created_at) VALUES (?1, ?2, ?3, datetime('now'))",
                bc_db::rusqlite::params![n, bc_db::util::name_key(&n), u],
            )?;
            Ok(())
        })
        .unwrap();
}

fn pages(fx: &Fx, entries: Vec<(&str, std::result::Result<Vec<Row>, &str>)>) {
    let mut g = fx.feed.pages.lock();
    for (url, v) in entries {
        g.insert(url.to_string(), v.map_err(str::to_string));
    }
}

// ---------------------------------------------------------------------------
// CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn saving_the_same_query_twice_updates_the_one_row() {
    // The second press of Save means "keep it saved", never a 500 on the (kind, identifier)
    // unique constraint.
    let fx = fx().await;
    let mut body = json!({
        "kind": "discover",
        "label": "detroit techno · best-selling",
        "explore_params": {"genre": "electronic", "slice": "top", "tag": "detroit-techno"},
        "api_params": {"tags": ["detroit-techno"], "slice": "top"},
    });
    let (_, first) = fx.post("/follows", Some(body.clone())).await;
    body["label"] = json!("renamed");
    // Key order and blank params do not make a different query.
    body["api_params"] = json!({"slice": "top", "genre": "", "tags": ["detroit-techno"]});
    let (_, second) = fx.post("/follows", Some(body)).await;

    assert_eq!(second["id"], first["id"]);
    assert_eq!(second["label"], "renamed");
    let (_, listed) = fx.get("/follows").await;
    assert_eq!(listed["sources"].as_array().unwrap().len(), 1);
    assert_eq!(
        listed["sources"][0]["explore_params"],
        json!({"genre": "electronic", "slice": "top", "tag": "detroit-techno"})
    );
}

#[tokio::test]
async fn a_saved_search_is_recall_only() {
    // Autocomplete results are not a release feed, so a saved search can never be enabled --
    // not at save time, not by patching.
    let fx = fx().await;
    let (_, saved) =
        fx.post("/follows", Some(json!({"kind": "search", "label": "aphex", "explore_params": {"q": "aphex"}}))).await;
    assert_eq!(saved["enabled"], false);

    let (_, patched) = fx.patch(&format!("/follows/{}", saved["id"]), json!({"enabled": true})).await;
    assert_eq!(patched["enabled"], false);
    // A search without its q is a bad request.
    assert_eq!(fx.post("/follows", Some(json!({"kind": "search", "label": "x"}))).await.0, 400);
}

#[tokio::test]
async fn following_a_page_normalises_to_its_root() {
    let fx = fx().await;
    let (_, saved) =
        fx.post("/follows", Some(json!({"kind": "artist", "label": "Solo", "url": format!("{SOLO_URL}/music")}))).await;
    assert_eq!(saved["url"], SOLO_URL);

    let (status, _) = fx.post("/follows", Some(json!({"kind": "artist", "label": "No URL"}))).await;
    assert_eq!(status, 400);

    // Rename and unfollow; delete.
    let id = saved["id"].as_i64().unwrap();
    let (_, p) = fx.patch(&format!("/follows/{id}"), json!({"label": "  Solo Two ", "enabled": false})).await;
    assert_eq!(p["label"], "Solo Two");
    assert_eq!(p["enabled"], false);
    assert_eq!(fx.patch("/follows/999", json!({"enabled": true})).await.0, 404);
    assert_eq!(fx.delete(&format!("/follows/{id}")).await.0, 204);
    assert_eq!(fx.delete(&format!("/follows/{id}")).await.0, 404);
}

#[tokio::test]
async fn settings_round_trip() {
    let fx = fx().await;
    let (_, body) = fx.put("/follows/settings", json!({"include_library_artists": false, "poll_hours": 6})).await;
    assert_eq!(body["include_library_artists"], false);
    assert_eq!(body["include_library_labels"], true);
    assert_eq!(body["poll_hours"], 6.0);

    assert_eq!(fx.get("/follows").await.1["include_library_artists"], false);
    assert_eq!(fx.put("/follows/settings", json!({"poll_hours": 9999})).await.0, 400);
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sweep_covers_follows_and_library_shelves_and_queues_nothing() {
    let fx = fx().await;
    seed_label(&fx, "Planet Rhythm", Some(PLANET_URL));
    seed_artist(&fx, "No Page Artist", None);
    fx.post(
        "/follows",
        Some(json!({"kind": "discover", "label": "detroit techno", "api_params": {"tags": ["detroit-techno"], "slice": "top"}})),
    )
    .await;
    *fx.feed.discover.lock() = vec![("https://x.bandcamp.com/album/one", "Fresh Artist", "One")];
    pages(&fx, vec![(PLANET_URL, Ok(vec![("https://planet.bandcamp.com/album/two", "Other Artist", "Two")]))]);

    assert_eq!(fx.post("/follows/sweep", None).await.0, 202);
    let state = fx.wait_for_sweep().await;

    assert_eq!(state["phase"], "done", "{state}");
    assert_eq!(state["total"], 2, "the saved query and the library label");
    assert_eq!(state["no_url"], 1, "the artist without a page is counted, not fetched");
    assert_eq!(state["new"], 2);
    // The saved query's parameters reach the walk in API form.
    let q = fx.feed.last_query.lock().clone().expect("discover was asked");
    assert_eq!(q.tags, ["detroit-techno"]);
    assert_eq!(q.slice, "top");

    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM harvest_items WHERE source_kind = 'follow'"), 2);
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM harvest_items WHERE state = 'new'"), 2, "the feed queues nothing on its own");
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'download'"), 0);
    // The label target names the imprint; the grid never does.
    assert_eq!(fx.q::<String>("SELECT label_name FROM harvest_items WHERE title = 'Two'"), "Planet Rhythm");
    // The follow row's ledger moved.
    assert_eq!(fx.q::<i64>("SELECT items_new FROM harvest_sources WHERE kind = 'discover'"), 1);
    assert!(fx.q::<Option<String>>("SELECT last_run_at FROM harvest_sources WHERE kind = 'discover'").is_some());
    // The sweep is one `sweep` job tagged `feed`.
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'sweep' AND status = 'completed' AND params LIKE '%\"feed\"%'"), 1);
}

#[tokio::test]
async fn a_selection_checks_only_those_follows() {
    // Per-source "check now" must not drag the whole library behind it.
    let fx = fx().await;
    seed_label(&fx, "Planet Rhythm", Some(PLANET_URL));
    let (_, followed) = fx.post("/follows", Some(json!({"kind": "artist", "label": "Solo", "url": SOLO_URL}))).await;
    pages(&fx, vec![(SOLO_URL, Ok(vec![("https://soloartist.bandcamp.com/album/one", "Solo", "One")]))]);

    fx.post("/follows/sweep", Some(json!({"source_ids": [followed["id"]]}))).await;
    let state = fx.wait_for_sweep().await;

    assert_eq!(state["phase"], "done");
    assert_eq!(state["scope"], "selection");
    assert_eq!(state["total"], 1, "the library label was not opened");
}

#[tokio::test]
async fn a_disabled_page_follow_excludes_it_from_the_library_walk() {
    // A paused follow of a shelf entity is a deliberate opt-out, not a dead row.
    let fx = fx().await;
    seed_label(&fx, "Planet Rhythm", Some(PLANET_URL));
    let (_, saved) = fx.post("/follows", Some(json!({"kind": "label", "label": "Planet Rhythm", "url": PLANET_URL}))).await;
    fx.patch(&format!("/follows/{}", saved["id"]), json!({"enabled": false})).await;
    pages(&fx, vec![(PLANET_URL, Err("must not be fetched"))]);

    fx.post("/follows/sweep", None).await;
    let state = fx.wait_for_sweep().await;

    assert_eq!(state["phase"], "done");
    assert_eq!(state["total"], 0);
}

#[tokio::test]
async fn one_dead_page_does_not_kill_the_sweep() {
    let fx = fx().await;
    fx.post("/follows", Some(json!({"kind": "artist", "label": "Dead", "url": SOLO_URL}))).await;
    seed_label(&fx, "Planet Rhythm", Some(PLANET_URL));
    pages(
        &fx,
        vec![
            (SOLO_URL, Err("HTTP 404")),
            (PLANET_URL, Ok(vec![("https://planet.bandcamp.com/album/one", "Fresh Artist", "One")])),
        ],
    );

    fx.post("/follows/sweep", None).await;
    let state = fx.wait_for_sweep().await;

    assert_eq!(state["phase"], "done");
    assert_eq!(state["done"], 2);
    assert_eq!(state["new"], 1);
    assert!(state["errors"].as_array().unwrap().iter().any(|e| e.as_str().unwrap().contains("Dead")), "{state}");
    // The failure is written back to the follow row for the sources rail.
    let err: String = fx.q("SELECT last_error FROM harvest_sources WHERE kind = 'artist'");
    assert!(err.contains("404"), "{err}");
}

#[tokio::test]
async fn feed_items_are_filterable_by_source() {
    // (The `/harvest/items` filter is the harvest agent's; this is what it filters on.)
    let fx = fx().await;
    seed_label(&fx, "Planet Rhythm", Some(PLANET_URL));
    fx.post("/follows", Some(json!({"kind": "artist", "label": "Solo", "url": SOLO_URL}))).await;
    pages(
        &fx,
        vec![
            (PLANET_URL, Ok(vec![("https://planet.bandcamp.com/album/one", "Fresh Artist", "One")])),
            (SOLO_URL, Ok(vec![("https://soloartist.bandcamp.com/album/two", "Solo", "Two")])),
        ],
    );

    fx.post("/follows/sweep", None).await;
    fx.wait_for_sweep().await;

    assert_eq!(fx.q::<String>("SELECT title FROM harvest_items WHERE source_kind = 'follow' AND source_label = 'Solo'"), "Two");
}

#[tokio::test]
async fn a_second_sweep_is_refused_while_one_runs() {
    let fx = fx().await;
    seed_label(&fx, "Planet Rhythm", Some(PLANET_URL));
    *fx.feed.slow.lock() = Some(Duration::from_secs(5));

    assert_eq!(fx.post("/follows/sweep", None).await.0, 202);
    let (status, body) = fx.post("/follows/sweep", None).await;
    assert_eq!(status, 400);
    assert!(body["detail"].as_str().unwrap().to_lowercase().contains("already running"), "{body}");

    // Stop keeps whatever was found and ends the run.
    let (_, stopped) = fx.delete("/follows/sweep").await;
    let _ = stopped;
    let state = fx.wait_for_sweep().await;
    assert_eq!(state["phase"], "done");
    assert_eq!(state["error"], "Stopped");
    // And a new one may start.
    *fx.feed.slow.lock() = None;
    assert_eq!(fx.post("/follows/sweep", None).await.0, 202);
    assert_eq!(fx.wait_for_sweep().await["error"], serde_json::Value::Null);
}

#[tokio::test]
async fn a_sweep_waiting_for_its_worker_is_closed_when_stopped() {
    // Cancelling before any worker claimed the job must not leave the status "harvesting".
    let fx = fx().await;
    fx.sweep_worker.stop().await;
    assert_eq!(fx.post("/follows/sweep", None).await.0, 202);
    assert_eq!(fx.get("/follows/sweep").await.1["running"], true);
    let (_, st) = fx.delete("/follows/sweep").await;
    assert_eq!(st["running"], false, "{st}");
    assert_eq!(st["error"], "Stopped");
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'sweep' AND status = 'cancelled'"), 1);
}

#[tokio::test]
async fn a_label_sweep_blocks_a_feed_sweep() {
    let fx = fx().await;
    fx.ctx
        .db
        .write(|tx| {
            tx.execute(
                "INSERT INTO jobs(id, kind, status, priority, params, total, completed, failed, skipped, cancel_requested, created_at) \
                 VALUES ('ls','sweep','running',100,'{\"sweep\":\"labels\"}',1,0,0,0,0,datetime('now'))",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let (s, b) = fx.post("/follows/sweep", None).await;
    assert_eq!(s, 400);
    assert!(b["detail"].as_str().unwrap().contains("label sweep"), "{b}");
}

#[tokio::test]
async fn the_backfill_runs_before_a_full_sweep_but_not_a_selection() {
    let fx = fx().await;
    let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let r = ran.clone();
    fx.sweeper.set_backfill(Arc::new(move |_c| {
        r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    fx.post("/follows/sweep", None).await;
    fx.wait_for_sweep().await;
    assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
    let (_, f) = fx.post("/follows", Some(json!({"kind": "artist", "label": "Solo", "url": SOLO_URL}))).await;
    fx.post("/follows/sweep", Some(json!({"source_ids": [f["id"]]}))).await;
    fx.wait_for_sweep().await;
    assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// The scheduler (due / not due, injectable clock)
// ---------------------------------------------------------------------------

fn at(h: u32, m: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 5, 1, h, m, 0).unwrap()
}

#[test]
fn poll_due_logic() {
    let last = "2026-05-01T00:00:00.000000";
    // Off.
    assert!(!feed::poll_due(0.0, None, at(12, 0)));
    assert!(!feed::poll_due(-1.0, Some(last), at(23, 0)));
    // Never polled, or unreadable: due.
    assert!(feed::poll_due(12.0, None, at(0, 1)));
    assert!(feed::poll_due(12.0, Some("garbage"), at(0, 1)));
    // Interval elapsed or not.
    assert!(!feed::poll_due(12.0, Some(last), at(11, 59)));
    assert!(feed::poll_due(12.0, Some(last), at(12, 0)));
    assert!(feed::poll_due(0.5, Some(last), at(0, 30)));
    assert!(!feed::poll_due(0.5, Some(last), at(0, 29)));
    // A stored value with a timezone suffix reads too.
    assert!(feed::poll_due(1.0, Some("2026-05-01 00:00:00+00:00"), at(1, 0)));
}

#[tokio::test]
async fn the_scheduler_starts_a_sweep_only_when_due_and_never_overlaps() {
    let fx = fx().await;
    let sched = fx.ctx.expect::<FeedScheduler>();
    fx.put_poll_hours(1.0).await;

    // First ever tick: due (no last poll). It records the poll and starts a sweep.
    assert!(sched.tick(at(10, 0)).await.unwrap());
    let stored: String = fx.q("SELECT value FROM settings WHERE key = 'follows.last_poll'");
    assert!(stored.starts_with("2026-05-01T10:00:00"), "{stored}");
    fx.wait_for_sweep().await;
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'sweep'"), 1);

    // Thirty minutes later: not due.
    assert!(!sched.tick(at(10, 30)).await.unwrap());
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'sweep'"), 1);

    // An hour on: due -- but not while a sweep is still running.
    *fx.feed.slow.lock() = Some(Duration::from_secs(5));
    seed_label(&fx, "Planet Rhythm", Some(PLANET_URL));
    assert!(sched.tick(at(11, 0)).await.unwrap());
    assert!(fx.sweeper.running());
    assert!(!sched.tick(at(13, 0)).await.unwrap(), "never overlaps a running sweep");
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'sweep'"), 2);
    fx.delete("/follows/sweep").await;
    fx.wait_for_sweep().await;

    // Polling off: never due.
    fx.put_poll_hours(0.0).await;
    assert!(!sched.tick(at(23, 0)).await.unwrap());
    // The stored interval is re-read every tick: changing it applies without a restart.
    fx.put_poll_hours(2.0).await;
    *fx.feed.slow.lock() = None;
    assert!(sched.tick(at(23, 0)).await.unwrap());
}

#[tokio::test]
async fn the_scheduler_task_waits_before_its_first_check_and_uses_the_injected_clock() {
    let fx = fx().await;
    fx.put_poll_hours(1.0).await;
    let sched = fx.ctx.expect::<FeedScheduler>();
    sched.set_clock(Arc::new(|| at(9, 0)));
    // Not at boot: nothing runs before the initial delay.
    sched.start(Duration::from_millis(300), Duration::from_millis(50));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'sweep'"), 0);
    // Then the first check fires (due: never polled).
    for _ in 0..100 {
        if fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'sweep'") >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'sweep'"), 1);
    // The fixed clock is "not due" for every later tick: still one job.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'sweep'"), 1);
    sched.stop();
    let d = FeedScheduler::initial_delay();
    assert!((20..=90).contains(&d.as_secs()));
}

#[test]
fn canonical_identifier_ignores_key_order_and_blank_params() {
    use std::collections::BTreeMap;
    let a: BTreeMap<String, serde_json::Value> =
        [("tags".to_string(), json!(["x"])), ("slice".to_string(), json!("top")), ("genre".to_string(), json!(""))].into();
    let b: BTreeMap<String, serde_json::Value> = [("slice".to_string(), json!("top")), ("tags".to_string(), json!(["x"]))].into();
    assert_eq!(feed::canonical_identifier("discover", None, &a), feed::canonical_identifier("discover", None, &b));
    assert_eq!(feed::canonical_identifier("discover", None, &b), r#"{"slice":"top","tags":["x"]}"#);
    assert_eq!(feed::canonical_identifier("label", Some("https://planet.bandcamp.com/music"), &a), "https://planet.bandcamp.com");
    // Python's ensure_ascii escaping is kept so legacy rows collide with new saves.
    let c: BTreeMap<String, serde_json::Value> = [("tags".to_string(), json!(["café"]))].into();
    assert_eq!(feed::canonical_identifier("discover", None, &c), "{\"tags\":[\"caf\\u00e9\"]}");
}
