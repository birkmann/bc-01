//! Port of `test_job_items_groups.py` (host grouping, move, remove) and the API
//! cases of `test_job_control.py`, driven through `JobsService::router()`.
mod common;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use bc_core::EventBus;
use bc_db::Db;
use bc_jobs::{Interrupt, JobHooks, JobsService, NewItem, NewJob, host_of};
use http_body_util::BodyExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tower::ServiceExt;

const URLS: [&str; 6] = [
    "https://julieslick.bandcamp.com/album/terroir",
    "https://julieslick.bandcamp.com/album/julie-slick",
    "https://perctrax.bandcamp.com/album/temperatures-rising",
    "http://WWW.Malingenie.net/album/hedonic-setpoint",
    "https://malingenie.net/album/second",
    "https://g-man-techno.bandcamp.com/album/beautiful",
];

#[derive(Default)]
struct Rec {
    released: Mutex<Vec<String>>,
    interrupted: Mutex<Vec<(String, Interrupt)>>,
}
#[async_trait]
impl JobHooks for Rec {
    async fn interrupt_job(&self, job_id: &str, reason: Interrupt) {
        self.interrupted.lock().push((job_id.to_string(), reason));
    }
    async fn release_urls(&self, urls: &[String]) {
        self.released.lock().extend(urls.iter().cloned());
    }
}

struct Ctx {
    _dir: tempfile::TempDir,
    svc: JobsService,
    app: Router,
    rec: Arc<Rec>,
}

fn ctx() -> Ctx {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("jobs.db")).unwrap();
    let svc = JobsService::new(db, Arc::new(EventBus::new()));
    let rec = Arc::new(Rec::default());
    svc.add_hooks(rec.clone());
    let app = svc.router();
    Ctx { _dir: dir, svc, app, rec }
}

impl Ctx {
    fn job(&self, urls: &[&str]) -> String {
        self.svc
            .store()
            .create_job(NewJob::new("download", urls.iter().map(|u| NewItem::url(*u, "album")).collect()))
            .unwrap()
            .id
    }
    async fn call(&self, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let resp = self.app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        self.call(Method::GET, uri, None).await
    }
    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, uri, Some(body)).await
    }
    fn items(&self, job_id: &str) -> Vec<bc_jobs::JobItem> {
        self.svc.store().list_items(job_id, None, &[], 0, None).unwrap()
    }
    fn urls(&self, job_id: &str) -> Vec<String> {
        self.items(job_id).into_iter().map(|i| i.url.unwrap_or_default()).collect()
    }
    /// finish: "done" | "failed" | "running"
    fn finish(&self, job_id: &str, urls: &[&str], how: &str) {
        let s = self.svc.store();
        for it in self.items(job_id) {
            if !urls.contains(&it.url.as_deref().unwrap_or("")) {
                continue;
            }
            match how {
                "running" => {
                    s.db().write(move |tx| Ok(tx.execute("UPDATE job_items SET status='running' WHERE id=?1", [it.id])?)).unwrap();
                }
                "done" | "failed" => {
                    // claim-like transition to running first so complete/fail apply
                    s.db().write(move |tx| Ok(tx.execute("UPDATE job_items SET status='running' WHERE id=?1", [it.id])?)).unwrap();
                    if how == "done" {
                        s.complete_item(it.id, bc_jobs::Complete::default()).unwrap();
                    } else {
                        s.fail_item(it.id, "boom", "x", false).unwrap();
                    }
                }
                _ => unreachable!(),
            }
        }
    }
    fn claim_order(&self) -> Vec<String> {
        let mut out = vec![];
        while let Some(c) = self.svc.store().claim_item("download", 1, 300.0).unwrap() {
            out.push(c.item.url.unwrap_or_default());
        }
        out
    }
}

#[test]
fn host_of_cases() {
    for (url, host) in [
        (Some("https://julieslick.bandcamp.com/album/terroir"), "julieslick.bandcamp.com"),
        (Some("http://WWW.Malingenie.net/album/x"), "malingenie.net"),
        (Some("https://Label.COM"), "label.com"),
        (Some("malingenie.net/album/x"), "malingenie.net"),
        (None, ""),
    ] {
        assert_eq!(host_of(url), host);
    }
}

fn keys(v: &Value) -> Vec<String> {
    v["items"].as_array().unwrap().iter().map(|g| g["key"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn groups_cluster_by_host_in_queue_order() {
    let c = ctx();
    let id = c.job(&URLS);
    let (_, page) = c.get(&format!("/jobs/{id}/groups")).await;
    assert_eq!(page["total"], 4);
    assert_eq!(keys(&page), ["julieslick.bandcamp.com", "perctrax.bandcamp.com", "malingenie.net", "g-man-techno.bandcamp.com"]);
    let by = |k: &str| page["items"].as_array().unwrap().iter().find(|g| g["key"] == k).unwrap().clone();
    assert_eq!(by("malingenie.net")["total"], 2);
    assert_eq!(by("malingenie.net")["pending"], 2);
    assert_eq!(by("perctrax.bandcamp.com")["item"]["url"], URLS[2]);
    assert!(by("malingenie.net")["item"].is_null());
}

#[tokio::test]
async fn groups_filter_by_status_but_count_everything() {
    let c = ctx();
    let id = c.job(&URLS);
    c.finish(&id, &[URLS[0], URLS[1], URLS[3]], "done");
    c.finish(&id, &[URLS[2]], "failed");
    let (_, queued) = c.get(&format!("/jobs/{id}/groups?status=pending,running")).await;
    assert_eq!(keys(&queued), ["malingenie.net", "g-man-techno.bandcamp.com"]);
    let m = &queued["items"][0];
    assert_eq!((m["total"].as_i64(), m["done"].as_i64(), m["pending"].as_i64()), (Some(2), Some(1), Some(1)));
    assert_eq!(m["visible"], 1);
    assert_eq!(m["item"]["url"], URLS[4]);
    let (_, failed) = c.get(&format!("/jobs/{id}/groups?status=failed")).await;
    assert_eq!(keys(&failed), ["perctrax.bandcamp.com"]);
    assert_eq!(c.get(&format!("/jobs/{id}/groups?status=bogus")).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn groups_page() {
    let c = ctx();
    let id = c.job(&URLS);
    let (_, page) = c.get(&format!("/jobs/{id}/groups?offset=1&limit=2")).await;
    assert_eq!(page["total"], 4);
    assert_eq!(keys(&page), ["perctrax.bandcamp.com", "malingenie.net"]);
}

#[tokio::test]
async fn items_can_be_narrowed_to_a_group_and_paged() {
    let c = ctx();
    let id = c.job(&URLS);
    let urls = |v: &Value| v.as_array().unwrap().iter().map(|i| i["url"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(urls(&c.get(&format!("/jobs/{id}/items?group=malingenie.net")).await.1), [URLS[3], URLS[4]]);
    assert_eq!(urls(&c.get(&format!("/jobs/{id}/items?group=malingenie.net&offset=1&limit=1")).await.1), [URLS[4]]);
    assert_eq!(urls(&c.get(&format!("/jobs/{id}/items")).await.1), URLS);
}

#[tokio::test]
async fn move_group_to_top_changes_what_the_worker_claims_first() {
    let c = ctx();
    let id = c.job(&URLS);
    let (st, body) = c.post(&format!("/jobs/{id}/items/move"), json!({"groups": ["g-man-techno.bandcamp.com"], "place": "top"})).await;
    assert_eq!(st, StatusCode::OK);
    assert!(body["moved"].as_i64().unwrap() > 0);
    let mut want = vec![URLS[5]];
    want.extend(&URLS[..5]);
    assert_eq!(c.claim_order(), want);
}

#[tokio::test]
async fn move_items_before_and_after_an_anchor() {
    let c = ctx();
    let id = c.job(&URLS);
    let items = c.items(&id);
    let perctrax = items.iter().find(|i| i.url.as_deref() == Some(URLS[2])).unwrap().id;
    let julie: Vec<i64> = items.iter().filter(|i| i.url.as_deref().unwrap().contains("julieslick")).map(|i| i.id).collect();
    c.post(&format!("/jobs/{id}/items/move"), json!({"item_ids": julie, "place": "after", "anchor_item_id": perctrax})).await;
    assert_eq!(c.urls(&id), [URLS[2], URLS[0], URLS[1], URLS[3], URLS[4], URLS[5]]);
    c.post(&format!("/jobs/{id}/items/move"), json!({"item_ids": julie, "place": "before", "anchor_group": "perctrax.bandcamp.com"})).await;
    assert_eq!(c.urls(&id), URLS);
}

#[tokio::test]
async fn move_after_group_lands_right_after_its_first_item() {
    let c = ctx();
    let id = c.job(&URLS);
    c.post(&format!("/jobs/{id}/items/move"), json!({"groups": ["julieslick.bandcamp.com"], "place": "after", "anchor_group": "malingenie.net"})).await;
    assert_eq!(c.urls(&id), [URLS[2], URLS[3], URLS[0], URLS[1], URLS[4], URLS[5]]);
}

#[tokio::test]
async fn move_to_bottom_keeps_the_blocks_internal_order() {
    let c = ctx();
    let id = c.job(&URLS);
    c.post(&format!("/jobs/{id}/items/move"), json!({"groups": ["julieslick.bandcamp.com"], "place": "bottom"})).await;
    let mut want: Vec<&str> = URLS[2..].to_vec();
    want.extend([URLS[0], URLS[1]]);
    assert_eq!(c.urls(&id), want);
    let seqs: Vec<i64> = c.items(&id).iter().map(|i| i.seq).collect();
    let mut sorted = seqs.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(seqs, sorted);
}

#[tokio::test]
async fn move_respects_the_status_filter_for_groups() {
    let c = ctx();
    let id = c.job(&URLS);
    c.finish(&id, &[URLS[3]], "done");
    c.post(&format!("/jobs/{id}/items/move"), json!({"groups": ["malingenie.net"], "status": "pending,running", "place": "top"})).await;
    let urls = c.urls(&id);
    assert_eq!(urls[0], URLS[4]);
    assert_eq!(urls.iter().position(|u| u == URLS[3]), Some(4));
}

#[tokio::test]
async fn move_needs_an_anchor_for_relative_placement() {
    let c = ctx();
    let id = c.job(&URLS);
    let (st, _) = c.post(&format!("/jobs/{id}/items/move"), json!({"groups": ["malingenie.net"], "place": "before"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn move_with_an_anchor_inside_the_selection_is_a_no_op() {
    let c = ctx();
    let id = c.job(&URLS);
    let first = c.items(&id)[0].id;
    let (st, body) = c.post(&format!("/jobs/{id}/items/move"), json!({"groups": ["julieslick.bandcamp.com"], "place": "after", "anchor_item_id": first})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["moved"], 0);
    assert_eq!(c.urls(&id), URLS);
}

#[tokio::test]
async fn move_preserves_seq_gaps() {
    let c = ctx();
    let id = c.job(&URLS);
    c.post(&format!("/jobs/{id}/items/remove"), json!({"groups": ["perctrax.bandcamp.com"]})).await;
    c.post(&format!("/jobs/{id}/items/move"), json!({"groups": ["g-man-techno.bandcamp.com"], "place": "top"})).await;
    assert_eq!(c.urls(&id), [URLS[5], URLS[0], URLS[1], URLS[3], URLS[4]]);
    let seqs: Vec<i64> = c.items(&id).iter().map(|i| i.seq).collect();
    let mut u = seqs.clone();
    u.sort();
    u.dedup();
    assert_eq!(u.len(), seqs.len());
}

#[tokio::test]
async fn remove_group_drops_the_items_and_keeps_counters_honest() {
    let c = ctx();
    let id = c.job(&URLS);
    c.finish(&id, &[URLS[0]], "done");
    c.finish(&id, &[URLS[1]], "failed");
    let (st, body) = c.post(&format!("/jobs/{id}/items/remove"), json!({"groups": ["julieslick.bandcamp.com"]})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!((body["removed"].as_i64(), body["kept_running"].as_i64()), (Some(2), Some(0)));
    assert_eq!(body["job"]["total"], 4);
    assert_eq!(body["job"]["completed"], 0);
    assert_eq!(body["job"]["failed"], 0);
    assert_eq!(c.urls(&id), &URLS[2..]);
    assert_eq!(c.claim_order(), &URLS[2..]);
}

#[tokio::test]
async fn remove_running_item_is_refused_and_reported() {
    let c = ctx();
    let id = c.job(&URLS);
    c.finish(&id, &[URLS[3]], "running");
    let (_, body) = c.post(&format!("/jobs/{id}/items/remove"), json!({"groups": ["malingenie.net"]})).await;
    assert_eq!((body["removed"].as_i64(), body["kept_running"].as_i64()), (Some(1), Some(1)));
    let left: Vec<String> = c.urls(&id).into_iter().filter(|u| u.to_lowercase().contains("malingenie")).collect();
    assert_eq!(left, [URLS[3]]);
}

#[tokio::test]
async fn remove_respects_the_status_filter() {
    let c = ctx();
    let id = c.job(&URLS);
    c.finish(&id, &[URLS[0]], "failed");
    let (_, body) = c.post(&format!("/jobs/{id}/items/remove"), json!({"groups": ["julieslick.bandcamp.com"], "status": "failed"})).await;
    assert_eq!(body["removed"], 1);
    let left: Vec<String> = c.urls(&id).into_iter().filter(|u| u.contains("julieslick")).collect();
    assert_eq!(left, [URLS[1]]);
}

#[tokio::test]
async fn removing_the_last_item_removes_the_job() {
    let c = ctx();
    let id = c.job(&URLS[..1]);
    let (_, body) = c.post(&format!("/jobs/{id}/items/remove"), json!({"groups": ["julieslick.bandcamp.com"]})).await;
    assert_eq!(body["removed"], 1);
    assert!(body["job"].is_null());
    assert_eq!(c.get(&format!("/jobs/{id}")).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn removing_every_pending_item_settles_the_job() {
    let c = ctx();
    let id = c.job(&URLS[..2]);
    c.finish(&id, &[URLS[0]], "done");
    let other = c.items(&id).iter().find(|i| i.url.as_deref() == Some(URLS[1])).unwrap().id;
    let (_, body) = c.post(&format!("/jobs/{id}/items/remove"), json!({"item_ids": [other]})).await;
    assert_eq!(body["job"]["status"], "completed");
    assert_eq!(body["job"]["total"], 1);
    assert_eq!(body["job"]["completed"], 1);
}

#[tokio::test]
async fn removed_unrun_items_are_handed_back_to_the_inbox() {
    let c = ctx();
    let id = c.job(&URLS[..2]);
    c.post(&format!("/jobs/{id}/items/remove"), json!({"groups": ["julieslick.bandcamp.com"]})).await;
    let mut released = c.rec.released.lock().clone();
    released.sort();
    let mut want = vec![URLS[0], URLS[1]];
    want.sort();
    assert_eq!(released, want);
    assert_eq!(c.get(&format!("/jobs/{id}")).await.0, StatusCode::NOT_FOUND, "job emptied -> deleted");
}

#[tokio::test]
async fn selection_ignores_items_of_other_jobs() {
    let c = ctx();
    let a = c.job(&URLS[..2]);
    let b = c.job(&URLS[2..]);
    let foreign: Vec<i64> = c.items(&b).iter().map(|i| i.id).collect();
    let (_, body) = c.post(&format!("/jobs/{a}/items/remove"), json!({"item_ids": foreign})).await;
    assert_eq!(body["removed"], 0);
    assert_eq!(c.items(&b).len(), 4);
}

// -- API cases of test_job_control.py (without a download worker) --------------------

#[tokio::test]
async fn cancel_returns_the_job_already_cancelled_and_interrupts() {
    let c = ctx();
    let id = c.job(&URLS);
    let (st, body) = c.post(&format!("/jobs/{id}/cancel"), json!({})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["status"], "cancelled");
    assert_eq!(c.rec.interrupted.lock()[0], (id.clone(), Interrupt::Cancel));
    assert_eq!(c.rec.released.lock().len(), URLS.len(), "unrun urls go back to the inbox");
}

#[tokio::test]
async fn pause_and_resume_via_the_api() {
    let c = ctx();
    let id = c.job(&URLS);
    let (_, p) = c.post(&format!("/jobs/{id}/pause"), json!({})).await;
    assert_eq!(p["status"], "paused");
    assert_eq!(c.rec.interrupted.lock()[0].1, Interrupt::Pause);
    let (_, r) = c.post(&format!("/jobs/{id}/resume"), json!({})).await;
    assert_eq!(r["status"], "queued");
}

#[tokio::test]
async fn pausing_a_finished_job_is_refused() {
    let c = ctx();
    let id = c.job(&URLS[..1]);
    c.finish(&id, &[URLS[0]], "done");
    let (st, _) = c.post(&format!("/jobs/{id}/pause"), json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn delete_stops_a_running_job_and_removes_it() {
    let c = ctx();
    let id = c.job(&URLS);
    c.svc.store().claim_item("download", 1, 300.0).unwrap();
    // the worker (hook) would settle the in-flight item; emulate it
    c.svc.store().db().write(|tx| Ok(tx.execute("UPDATE job_items SET status='cancelled' WHERE status='running'", [])?)).unwrap();
    let (st, _) = c.call(Method::DELETE, &format!("/jobs/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(c.get(&format!("/jobs/{id}")).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_of_a_paused_job_and_clear_finished() {
    let c = ctx();
    let paused = c.job(&URLS[..2]);
    c.post(&format!("/jobs/{paused}/pause"), json!({})).await;
    assert_eq!(c.call(Method::DELETE, &format!("/jobs/{paused}"), None).await.0, StatusCode::NO_CONTENT);

    let done = c.job(&URLS[..1]);
    c.finish(&done, &[URLS[0]], "done");
    let failed = c.job(&URLS[1..2]);
    c.finish(&failed, &[URLS[1]], "failed");
    let (_, body) = c.post("/jobs/clear", json!({})).await;
    assert_eq!(body["deleted"], 1, "only clean completed jobs go; a job holding a failure keeps its retry button");
    assert_eq!(c.get(&format!("/jobs/{failed}")).await.0, StatusCode::OK);
    assert_eq!(c.get(&format!("/jobs/{done}")).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_jobs_filters_and_pages() {
    let c = ctx();
    let a = c.job(&URLS[..1]);
    c.job(&URLS[1..2]);
    c.svc.store().cancel_job(&a).unwrap();
    let (_, all) = c.get("/jobs").await;
    assert_eq!(all["total"], 2);
    let (_, cancelled) = c.get("/jobs?status=cancelled").await;
    assert_eq!(cancelled["total"], 1);
    assert_eq!(cancelled["items"][0]["id"], a);
    let (_, none) = c.get("/jobs?kind=scan").await;
    assert_eq!(none["total"], 0);
}
