//! The worker- and route-level cases of `test_download_dedup.py`: skipping already-downloaded URLs
//! without running the downloader (`test_preflight_*`, `test_parse_reports_already_have`,
//! `test_submit_*`, `test_clear_finished_*`), plus the submit logic of `POST /downloads`.

mod worker_common;

use std::sync::Arc;

use async_trait::async_trait;
use bc_bandcamp::api::downloads::{classify_url, normalise_url, parse_url_list};
use bc_bandcamp::download::dedup::normalise;
use bc_jobs::JobHooks;
use serde_json::json;
use worker_common::*;

const ALBUM: &str = "https://artist.bandcamp.com/album/great-record";

// -- worker pre-flight ----------------------------------------------------------------------------

#[tokio::test]
async fn preflight_skips_known_url() {
    let env = Env::new();
    seed_release(&env.db, "Great Record", Some(&normalise(ALBUM)));
    let id = env.queue_claimed(ALBUM);

    // The downloader is a binary that does not exist: it must never be started.
    env.run_item(&env.handler(bcdl_with("/nonexistent/bandcamp-dl", "success")), id).await;

    let item = env.item(id);
    assert_eq!(item.status, "skipped");
    assert_eq!(item.message.as_deref(), Some("Already in library — skipped"));
    let job = env.job(&item.job_id);
    assert_eq!((job.skipped, job.status.as_str()), (1, "completed"));
}

#[tokio::test]
async fn preflight_leaves_unknown_and_forced_urls_alone() {
    let env = Env::new();
    seed_release(&env.db, "Great Record", Some(&normalise(ALBUM)));
    let handler = env.handler(bcdl("success"));

    let unknown = env.queue_claimed("https://new.bandcamp.com/album/fresh");
    env.run_item(&handler, unknown).await;
    assert_eq!(env.item(unknown).status, "done", "an unknown URL is downloaded");

    let forced = env.queue_claimed_with(ALBUM, json!({"force": true}), None);
    env.run_item(&handler, forced).await;
    assert_eq!(env.item(forced).status, "done", "force overrides 'you already have this'");
}

#[tokio::test]
async fn preflight_never_lets_force_override_the_blacklist() {
    let env = Env::new();
    let key = normalise(ALBUM).to_lowercase();
    env.db
        .write(move |t| {
            t.execute(
                "INSERT INTO blacklist(url_key, url, artist_name, title, added_at) VALUES (?1, ?1, 'a', 't', CURRENT_TIMESTAMP)",
                [&key],
            )?;
            Ok(())
        })
        .expect("blacklist");
    let id = env.queue_claimed_with(ALBUM, json!({"force": true}), None);

    env.run_item(&env.handler(bcdl_with("/nonexistent/bandcamp-dl", "success")), id).await;

    let item = env.item(id);
    assert_eq!(item.status, "skipped");
    assert_eq!(item.message.as_deref(), Some("Blacklisted — skipped"));
}

#[tokio::test]
async fn a_skipped_item_settles_its_inbox_row() {
    let env = Env::new();
    seed_release(&env.db, "Great Record", Some(&normalise(ALBUM)));
    env.db
        .write(|t| {
            t.execute(
                "INSERT INTO harvest_items(url, url_kind, state, title, artist_name, tags, in_collection, in_wishlist, \
                 is_free_download, is_purchasable, is_preorder, discovered_at) \
                 VALUES (?1, 'album', 'queued', 't', 'a', '[]', 1, 0, 0, 0, 0, CURRENT_TIMESTAMP)",
                [ALBUM],
            )?;
            Ok(())
        })
        .expect("insert");
    let id = env.queue_claimed(ALBUM);

    env.run_item(&env.handler(bcdl("success")), id).await;

    let state: String = env.db.read(|c| Ok(c.query_row("SELECT state FROM harvest_items", [], |r| r.get(0))?)).expect("state");
    assert_eq!(state, "in_library");
}

// -- URL parsing (the legacy classify_url / normalise_url / parse_url_list) ------------------------

#[test]
fn classify_and_normalise_follow_the_legacy_helpers() {
    assert_eq!(classify_url("https://a.bandcamp.com/album/x"), Some("album"));
    assert_eq!(classify_url("https://a.bandcamp.com/track/x"), Some("track"));
    assert_eq!(classify_url("https://a.bandcamp.com"), Some("artist"));
    assert_eq!(classify_url("https://a.bandcamp.com/"), Some("artist"));
    assert_eq!(classify_url("https://a.bandcamp.com/music"), Some("artist"));
    // The legacy classifier does not know about /artists or fan pages.
    assert_eq!(classify_url("https://a.bandcamp.com/artists"), None);
    assert_eq!(classify_url("https://bandcamp.com/someone"), None);
    assert_eq!(classify_url("ftp://a.bandcamp.com/album/x"), None);
    assert_eq!(classify_url("not a url"), None);
    // Custom domains are legitimate: the path shape decides.
    assert_eq!(classify_url("https://music.example.com/album/thing"), Some("album"));
    assert_eq!(classify_url("https://music.example.com/music"), None);

    assert_eq!(normalise_url("https://A.bandcamp.com/album/x/?from=search&action=buy"), "https://A.bandcamp.com/album/x");
}

#[test]
fn parse_url_list_dedupes_counts_and_skips_comments() {
    let text = "# a comment\n// another\nhttps://a.bandcamp.com/album/x?from=1\nhttps://a.bandcamp.com/album/x/, https://a.bandcamp.com/track/y\n\
                https://b.bandcamp.com/music\nnope\n\n";
    let p = parse_url_list(text);
    assert_eq!(p.valid, ["https://a.bandcamp.com/album/x", "https://a.bandcamp.com/track/y", "https://b.bandcamp.com/music"]);
    assert_eq!(p.invalid, ["nope"]);
    assert_eq!((p.duplicates, p.albums, p.tracks, p.artists), (1, 1, 1, 1));
}

// -- API routes -------------------------------------------------------------------------------------

async fn app_with_seeded_release() -> App {
    let app = App::new(Some(bcdl("hang"))).await;
    seed_release(&app.env.db, "Seeded", Some(&normalise(ALBUM)));
    app
}

#[tokio::test]
async fn parse_reports_already_have() {
    let app = app_with_seeded_release().await;
    let (status, body) = app.post("/downloads/parse", json!({"text": format!("{ALBUM}\nhttps://new.bandcamp.com/album/fresh")})).await;
    assert_eq!(status, 200);
    assert_eq!(body["valid"].as_array().map(Vec::len), Some(2));
    assert_eq!(body["already_have"], 1);
}

#[tokio::test]
async fn submit_skips_known_urls_up_front() {
    let app = app_with_seeded_release().await;
    let (status, body) = app.post("/downloads", json!({"urls": [ALBUM]})).await;
    assert_eq!(status, 200);
    assert_eq!(body["skipped"], 1);
    assert_eq!(body["completed"], 0);
    assert_eq!(body["status"], "completed");

    let (_, items) = app.get(&format!("/jobs/{}/items", body["id"].as_str().expect("id"))).await;
    assert_eq!(items[0]["status"], "skipped");
    assert!(items[0]["message"].as_str().unwrap_or("").contains("Already in library"));
}

#[tokio::test]
async fn submit_with_force_does_not_skip() {
    let app = app_with_seeded_release().await;
    let (_, body) = app.post("/downloads", json!({"urls": [ALBUM], "force": true})).await;
    assert_eq!(body["skipped"], 0);
}

#[tokio::test]
async fn submit_always_skips_the_blacklisted_even_with_force() {
    let app = app_with_seeded_release().await;
    let key = normalise(ALBUM).to_lowercase();
    app.env
        .db
        .write(move |t| {
            t.execute("DELETE FROM releases", [])?;
            t.execute("INSERT INTO blacklist(url_key, url, artist_name, title, added_at) VALUES (?1, ?1, 'a', 't', CURRENT_TIMESTAMP)", [&key])?;
            Ok(())
        })
        .expect("blacklist");
    let (_, body) = app.post("/downloads", json!({"urls": [ALBUM], "force": true})).await;
    assert_eq!(body["skipped"], 1);
    let (_, items) = app.get(&format!("/jobs/{}/items", body["id"].as_str().expect("id"))).await;
    assert_eq!(items[0]["message"], "Blacklisted — skipped");
}

#[tokio::test]
async fn submit_validates_is_idempotent_and_stores_the_params() {
    let app = App::new(Some(bcdl("hang"))).await; // no worker started: items stay pending
    let (status, body) = app.post("/downloads", json!({"urls": ["nope", "https://a.bandcamp.com/artists"]})).await;
    assert_eq!(status, 400);
    assert!(body["detail"].as_str().unwrap_or("").contains("No valid Bandcamp album or track URLs"), "{body}");

    let req = json!({
        "urls": ["https://a.bandcamp.com/album/x?from=1", "https://a.bandcamp.com/album/x", "https://b.bandcamp.com/music"],
        "target_subdir": "my/dir", "job_id": "fixed-id", "force": true, "single_folder": true, "tracks_only": true,
        "label_name": " Player ", "label_url": "https://player.bandcamp.com", "source_fan_id": 7, "priority": 5,
    });
    let (status, job) = app.post("/downloads", req.clone()).await;
    assert_eq!(status, 200, "{job}");
    assert_eq!(job["id"], "fixed-id");
    assert_eq!(job["total"], 2, "the duplicate is dropped");
    assert_eq!(job["label"], "my_dir");
    let params: serde_json::Value = serde_json::from_str(&app.env.job("fixed-id").params).expect("params");
    assert_eq!(
        params,
        json!({"target_subdir": "my_dir", "force": true, "layout": "flat", "tracks_only": true,
               "label_name": "Player", "label_url": "https://player.bandcamp.com", "source_fan_id": 7})
    );
    let (_, items) = app.get("/jobs/fixed-id/items").await;
    assert_eq!(items[0]["url_kind"], "album");
    assert_eq!(items[1]["url_kind"], "artist");

    // Idempotent: the same job_id returns the existing job, creating nothing.
    let (status, again) = app.post("/downloads", req).await;
    assert_eq!(status, 200);
    assert_eq!(again["id"], "fixed-id");
    assert_eq!(app.env.count("SELECT count(*) FROM jobs"), 1);
}

#[tokio::test]
async fn a_personal_submit_adopts_a_known_record_from_a_shelf() {
    let app = app_with_seeded_release().await;
    let fan = app.env.fan("alice");
    app.env.db.write(move |t| Ok(t.execute("UPDATE releases SET source_fan_id = ?1", [fan]).map(|_| ())?)).expect("shelve");

    let (_, shelf_job) = app.post("/downloads", json!({"urls": [ALBUM], "source_fan_id": fan})).await;
    assert_eq!(shelf_job["skipped"], 1);
    let on_shelf = |app: &App| app.env.count("SELECT count(source_fan_id) FROM releases");
    assert_eq!(on_shelf(&app), 1, "a shelf job leaves the shelf alone");

    app.post("/downloads", json!({"urls": [ALBUM]})).await;
    assert_eq!(on_shelf(&app), 0, "'already have it' is only an answer when it is where I can see it");
}

// -- /jobs/clear (JobsService) --------------------------------------------------------------------------

struct Recorder(parking_lot::Mutex<Vec<String>>);

#[async_trait]
impl JobHooks for Recorder {
    async fn release_urls(&self, urls: &[String]) {
        self.0.lock().extend(urls.iter().cloned());
    }
}

#[tokio::test]
async fn clear_finished_takes_the_settled_jobs_and_leaves_the_retryable_one() {
    // One press for a list of finished jobs, without losing a retry: a job that ended with a
    // failure keeps its place -- the retry button on it is the only route back to those items.
    let app = app_with_seeded_release().await;
    let recorder = Arc::new(Recorder(Default::default()));
    app.env.jobs.add_hooks(recorder.clone());
    let (_, clean) = app.post("/downloads", json!({"urls": [ALBUM]})).await;
    let (_, kept) = app.post("/downloads", json!({"urls": [ALBUM]})).await;
    assert_eq!((clean["status"].as_str(), kept["status"].as_str()), (Some("completed"), Some("completed")));
    let kept_id = kept["id"].as_str().expect("id").to_string();
    let k = kept_id.clone();
    app.env.db.write(move |t| Ok(t.execute("UPDATE jobs SET failed = 1 WHERE id = ?1", [&k]).map(|_| ())?)).expect("fail");

    let (_, body) = app.post("/jobs/clear", json!({})).await;

    assert_eq!(body["deleted"], 1);
    let (_, jobs) = app.get("/jobs").await;
    let ids: Vec<&str> = jobs["items"].as_array().expect("items").iter().filter_map(|j| j["id"].as_str()).collect();
    assert_eq!(ids, [kept_id.as_str()]);
}

#[tokio::test]
async fn clear_finished_is_harmless_with_nothing_to_clear() {
    let app = App::new(None).await;
    let (_, body) = app.post("/jobs/clear", json!({})).await;
    assert_eq!(body["deleted"], 0);
}

#[tokio::test]
async fn clear_finished_releases_the_urls_of_unrun_items_through_the_hooks() {
    let app = App::new(None).await;
    let recorder = Arc::new(Recorder(Default::default()));
    app.env.jobs.add_hooks(recorder.clone());
    let job = app.env.create_download(&[ALBUM, "https://a.bandcamp.com/album/other"]);
    // One item skipped, the other cancelled; the job otherwise completed.
    let id = job.id.clone();
    app.env
        .db
        .write(move |t| {
            t.execute("UPDATE job_items SET status = 'skipped' WHERE job_id = ?1 AND seq = 0", [&id])?;
            t.execute("UPDATE job_items SET status = 'cancelled' WHERE job_id = ?1 AND seq = 1", [&id])?;
            t.execute("UPDATE jobs SET status = 'completed', skipped = 1 WHERE id = ?1", [&id])?;
            Ok(())
        })
        .expect("settle");

    let (_, body) = app.post("/jobs/clear", json!({})).await;

    assert_eq!(body["deleted"], 1);
    assert_eq!(*recorder.0.lock(), ["https://a.bandcamp.com/album/other"]);
}
