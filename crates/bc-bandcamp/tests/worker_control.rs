//! `test_job_control.py`, the cases that need a live worker: pause, cancel and delete must always
//! be possible -- and take effect now.
//!
//! The scenario these guard: a 54,051-item wishlist job, two albums mid-download. "Cancel" flipped
//! the pending rows one write at a time, left the two in-flight albums running to the end (up to
//! the 45-minute timeout each), and so left the job `running` -- and therefore undeletable -- for
//! as long as an hour. There was no pause at all.
//!
//! The in-flight download is the real `BandcampDl` adapter on the fake binary in `hang` mode.

mod worker_common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bc_bandcamp::download::dedup::normalise;
use bc_bandcamp::download::worker::{DownloadDeps, DownloadWorker};
use bc_jobs::Interrupt;
use worker_common::*;

fn albums(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("https://a.bandcamp.com/album/r{i}")).collect()
}

fn queue(env: &Env, n: usize) -> bc_jobs::Job {
    let urls = albums(n);
    let refs: Vec<&str> = urls.iter().map(String::as_str).collect();
    env.create_download(&refs)
}

#[tokio::test]
async fn cancel_kills_the_download_in_flight_and_closes_the_job() {
    let env = Env::new();
    let worker = DownloadWorker::new(Arc::new(env.deps().with_downloader(bcdl("hang"))), env.store().clone());
    let job = queue(&env, 2);

    worker.start().await;
    wait_for(10.0, "the first item to run", || env.statuses(&job.id)[0] == "running").await;
    assert!(!worker.inflight_items(&job.id).is_empty());

    env.store().cancel_job(&job.id).expect("cancel_job");
    assert_eq!(env.job(&job.id).status, "running", "in-flight item keeps it open");

    let interrupted = worker.interrupt_job(&job.id, Interrupt::Cancel).await;
    assert_eq!(interrupted, 1);
    // Settled by the time interrupt_job returns -- no polling needed.
    assert_eq!(env.statuses(&job.id), ["cancelled", "cancelled"]);
    assert_eq!(env.job(&job.id).status, "cancelled");
    assert!(worker.inflight_items(&job.id).is_empty());
    worker.stop().await;
}

#[tokio::test]
async fn pause_hands_the_download_back_and_resume_picks_it_up() {
    let env = Env::new();
    let worker = DownloadWorker::new(Arc::new(env.deps().with_downloader(bcdl("hang"))), env.store().clone());
    let job = queue(&env, 2);

    worker.start().await;
    wait_for(10.0, "the first item to run", || env.statuses(&job.id)[0] == "running").await;

    env.store().pause_job(&job.id).expect("pause_job");
    assert_eq!(worker.interrupt_job(&job.id, Interrupt::Pause).await, 1);

    assert_eq!(env.statuses(&job.id), ["pending", "pending"]);
    assert_eq!(env.job(&job.id).status, "paused");
    let first = env.store().list_items(&job.id, None, &[], 0, Some(10)).expect("items").remove(0);
    assert_eq!(first.attempts, 0, "the interrupted attempt is refunded");
    assert_eq!(first.message.as_deref(), Some("Paused"));

    // Held: the worker's loop is running and finds nothing to take.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(env.statuses(&job.id), ["pending", "pending"]);

    env.store().resume_job(&job.id).expect("resume_job");
    worker.notify();
    wait_for(10.0, "the item to run again", || env.statuses(&job.id)[0] == "running").await;
    assert_eq!(env.job(&job.id).status, "running");
    // Leave nothing running behind us.
    env.store().cancel_job(&job.id).expect("cancel");
    worker.interrupt_job(&job.id, Interrupt::Cancel).await;
    worker.stop().await;
}

#[tokio::test]
async fn a_cancel_outside_the_download_phase_still_settles_the_item() {
    // The download step handles its own cancel; anywhere else must not leave the item 'running'
    // -- that is a job that never closes. Here the item is stuck in the preflight (the library's
    // `adopt_for_urls` never returns).
    let env = Env::new();
    let url = "https://a.bandcamp.com/album/r0";
    seed_release(&env.db, "R0", Some(&normalise(url)));
    let lib = Arc::new(FakeLibrary::new(env.db.clone()));
    lib.stall_adopt.store(true, Ordering::SeqCst);
    let deps = DownloadDeps::new(env.db.clone(), env.bus.clone(), env.cfg.clone(), None).with_library(lib).with_downloader(bcdl("hang"));
    let worker = DownloadWorker::new(Arc::new(deps), env.store().clone());
    let job = queue(&env, 1);

    worker.start().await;
    wait_for(10.0, "the item to be claimed", || env.statuses(&job.id)[0] == "running").await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    env.store().cancel_job(&job.id).expect("cancel_job");
    worker.interrupt_job(&job.id, Interrupt::Cancel).await;

    assert_eq!(env.statuses(&job.id), ["cancelled"]);
    assert_eq!(env.job(&job.id).status, "cancelled");
    worker.stop().await;
}

#[tokio::test]
async fn shutdown_still_requeues_for_the_restart() {
    // A stop with no reason is the old contract: retryable, for reconcile.
    let env = Env::new();
    let worker = DownloadWorker::new(Arc::new(env.deps().with_downloader(bcdl("hang"))), env.store().clone());
    let job = queue(&env, 1);
    worker.start().await;
    wait_for(10.0, "the item to run", || env.statuses(&job.id)[0] == "running").await;

    worker.stop().await;

    let item = env.store().list_items(&job.id, None, &[], 0, Some(10)).expect("items").remove(0);
    assert_eq!(item.status, "pending");
    assert_eq!(item.error_class.as_deref(), Some("cancelled"));
}

// -- the routes: one press, and it has happened when the response comes back ------------------------

async fn app() -> App {
    let app = App::with(Env::with(|c| c.download_concurrency = 1), Some(bcdl("hang"))).await;
    app.start().await;
    app
}

async fn queue_via_api(app: &App, n: usize) -> String {
    let (status, body) = app.post("/downloads", serde_json::json!({"urls": albums(n)})).await;
    assert_eq!(status, 200, "{body}");
    body["id"].as_str().expect("job id").to_string()
}

async fn items(app: &App, job_id: &str) -> Vec<String> {
    let (_, body) = app.get(&format!("/jobs/{job_id}/items")).await;
    body.as_array().expect("items").iter().map(|i| i["status"].as_str().unwrap_or("").to_string()).collect()
}

async fn until_running(app: &App, job_id: &str) {
    for _ in 0..200 {
        if items(app, job_id).await.iter().any(|s| s == "running") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the worker never started the job");
}

#[tokio::test]
async fn cancel_returns_the_job_already_cancelled() {
    let app = app().await;
    let job_id = queue_via_api(&app, 2).await;
    until_running(&app, &job_id).await;

    let (_, body) = app.post(&format!("/jobs/{job_id}/cancel"), serde_json::json!({})).await;

    assert_eq!(body["status"], "cancelled");
    assert_eq!(items(&app, &job_id).await, ["cancelled", "cancelled"]);
    app.worker().stop().await;
}

#[tokio::test]
async fn pause_and_resume_via_the_api() {
    let app = app().await;
    let job_id = queue_via_api(&app, 2).await;
    until_running(&app, &job_id).await;

    let (_, body) = app.post(&format!("/jobs/{job_id}/pause"), serde_json::json!({})).await;
    assert_eq!(body["status"], "paused");
    assert_eq!(items(&app, &job_id).await, ["pending", "pending"]);
    // Idempotent.
    assert_eq!(app.post(&format!("/jobs/{job_id}/pause"), serde_json::json!({})).await.1["status"], "paused");

    let (_, body) = app.post(&format!("/jobs/{job_id}/resume"), serde_json::json!({})).await;
    assert!(["queued", "running"].contains(&body["status"].as_str().unwrap_or("")));
    until_running(&app, &job_id).await;
    app.post(&format!("/jobs/{job_id}/cancel"), serde_json::json!({})).await;
    app.worker().stop().await;
}

#[tokio::test]
async fn pausing_a_finished_job_is_refused() {
    let app = app().await;
    let job_id = queue_via_api(&app, 2).await;
    app.post(&format!("/jobs/{job_id}/cancel"), serde_json::json!({})).await;

    let (status, body) = app.post(&format!("/jobs/{job_id}/pause"), serde_json::json!({})).await;

    assert_eq!(status, 400);
    assert!(body["detail"].as_str().unwrap_or("").contains("cancelled"), "{body}");
    app.worker().stop().await;
}

#[tokio::test]
async fn delete_stops_a_running_job_and_removes_it() {
    // The one the user hit: 'Cancel the job before deleting it', with a cancel that did not end it.
    // Delete now does the whole thing itself.
    let app = app().await;
    let job_id = queue_via_api(&app, 2).await;
    until_running(&app, &job_id).await;
    assert_eq!(app.delete(&format!("/jobs/{job_id}")).await, 204);
    assert_eq!(app.get(&format!("/jobs/{job_id}")).await.0, 404);

    // And the worker is free: a fresh job starts rather than the slot being held by a task still
    // writing to a row that no longer exists.
    let fresh = queue_via_api(&app, 1).await;
    until_running(&app, &fresh).await;
    app.post(&format!("/jobs/{fresh}/cancel"), serde_json::json!({})).await;
    app.worker().stop().await;
}

#[tokio::test]
async fn delete_of_a_paused_job() {
    let app = app().await;
    let job_id = queue_via_api(&app, 2).await;
    until_running(&app, &job_id).await;
    app.post(&format!("/jobs/{job_id}/pause"), serde_json::json!({})).await;
    assert_eq!(app.delete(&format!("/jobs/{job_id}")).await, 204);
    assert_eq!(app.get(&format!("/jobs/{job_id}")).await.0, 404);
    app.worker().stop().await;
}

#[tokio::test]
async fn delete_of_a_running_inbox_job_reopens_its_rows() {
    // Whatever did not download goes back to 'new' -- including the in-flight one.
    use bc_bandcamp::harvest::inbox;
    let app = app().await;
    bc_bandcamp::harvest::inbox::init(&app.ctx); // the hook that reopens rows of a deleted job
    // `init` registered its hooks on the shared jobs service the router also uses.
    for url in albums(2) {
        app.env
            .db
            .write(move |t| {
                t.execute(
                    "INSERT INTO harvest_items(url, url_kind, state, title, artist_name, tags, in_collection, in_wishlist, \
                     is_free_download, is_purchasable, is_preorder, discovered_at) \
                     VALUES (?1, 'album', 'new', 't', 'a', '[]', 1, 0, 0, 0, 0, CURRENT_TIMESTAMP)",
                    [&url],
                )?;
                Ok(())
            })
            .expect("insert");
    }
    let ids: Vec<i64> = app
        .env
        .db
        .read(|c| {
            let mut st = c.prepare("SELECT id FROM harvest_items ORDER BY id")?;
            Ok(st.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?)
        })
        .expect("ids");
    let rows = app.env.db.read_with(|c| inbox::load_rows(c, &ids)).map_err(|e: bc_bandcamp::HarvestError| e.to_string()).expect("rows");
    let out = inbox::queue(&app.env.db, app.env.store(), &rows, &inbox::QueueOpts { allow_unowned: true, ..Default::default() }).expect("queue");
    let job_id = out.job_id.expect("job id");
    until_running(&app, &job_id).await;
    let states = |app: &App| -> Vec<String> {
        app.env
            .db
            .read(|c| {
                let mut st = c.prepare("SELECT DISTINCT state FROM harvest_items")?;
                Ok(st.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?)
            })
            .expect("states")
    };
    assert_eq!(states(&app), ["queued"]);

    assert_eq!(app.delete(&format!("/jobs/{job_id}")).await, 204);

    assert_eq!(states(&app), ["new"]);
    app.worker().stop().await;
}

#[tokio::test]
async fn the_service_wires_the_worker_hooks_and_the_library_port() {
    // `BandcampService::new` -> `worker::init` (services + JobHooks) and `start` -> recovery + loop.
    use bc_bandcamp::download::worker::{DownloadServices, set_library};
    let env = Env::new();
    let svc = bc_bandcamp::service::BandcampService::new(env.db.clone(), env.bus.clone(), env.cfg.clone(), env.jobs.clone());
    let ctx = svc.ctx().clone();
    set_library(&ctx, Arc::new(FakeLibrary::new(env.db.clone())));
    let url = "https://a.bandcamp.com/album/known";
    seed_release(&env.db, "Known", Some(&normalise(url)));
    let job = env.create_download(&[url]);

    svc.start().await;
    wait_for(10.0, "the preflight to skip the known release", || env.statuses(&job.id)[0] == "skipped").await;
    assert_eq!(env.job(&job.id).status, "completed");
    // The registered worker is what the routes talk to.
    assert!(ctx.get::<DownloadServices>().is_some());
    ctx.expect::<DownloadServices>().worker.stop().await;
}
