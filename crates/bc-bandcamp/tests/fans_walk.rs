//! Followed wishlists: mine (the old saved wishlist) and other people's (`test_fans.py`).
//!
//! The self fan keeps the one-button backfill contract the saved wishlist had -- harvest ->
//! match -> queue into the downloads root -- and every other fan is a list to walk, order, play
//! through and download from without touching my library's provenance. Plus the job-backed
//! behaviours the port adds: FIFO with a `queued` phase, stop keeps what was absorbed,
//! crash-recovery of a `walk` item left running.
mod fans_common;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bc_bandcamp::error::HarvestError;
use bc_bandcamp::harvest::fans::{self, FanOrder, FanTab};
use bc_bandcamp::sources::EventStream;
use bc_bandcamp::harvest::inbox::{self, QueueOpts};
use fans_common::*;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::Notify;

// -- following -----------------------------------------------------------------

#[tokio::test]
async fn adding_a_fan_probes_the_page_and_remembers_what_it_says() {
    let fx = fx().await;
    let body = fx.add_fan(ALICE, false).await;
    assert_eq!(body["username"], "alice");
    assert_eq!(body["is_self"], false);
    assert_eq!(body["wishlist_count"], 3);
    assert_eq!(body["collection_count"], 7);
    assert_eq!(body["shelf"], "fan-alice");
    assert_eq!(body["wishlist_url"], "https://bandcamp.com/alice/wishlist");

    let (_, listed) = fx.get("/fans").await;
    let names: Vec<&str> = listed.as_array().unwrap().iter().map(|f| f["username"].as_str().unwrap()).collect();
    assert_eq!(names, ["alice"]);

    // Adding the same account again, by another of its links, is not a second fan.
    let again = fx.add_fan("bandcamp.com/alice", false).await;
    assert_eq!(again["id"], body["id"]);
    assert_eq!(fx.get("/fans").await.1.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_bare_username_is_enough() {
    let fx = fx().await;
    assert_eq!(fx.add_fan("bob", false).await["url"], "https://bandcamp.com/bob");
}

#[tokio::test]
async fn non_fan_urls_are_refused() {
    let fx = fx().await;
    let (s, b) = fx.post("/fans", Some(json!({"url": "https://artist.bandcamp.com/album/x"}))).await;
    assert_eq!(s, 400);
    assert!(b["detail"].as_str().unwrap().to_lowercase().contains("fan page"), "{b}");
}

#[tokio::test]
async fn self_fan_lists_first() {
    let fx = fx().await;
    fx.add_fan(ALICE, false).await;
    fx.make_self(MINE);
    let (_, listed) = fx.get("/fans").await;
    let flags: Vec<bool> = listed.as_array().unwrap().iter().map(|f| f["is_self"].as_bool().unwrap()).collect();
    assert_eq!(flags, [true, false]);
}

// -- the walk ------------------------------------------------------------------

#[tokio::test]
async fn walk_records_membership_in_wishlist_order() {
    let fx = fx().await;
    fx.fake.set_fixed(
        vec![
            ("https://fresh.bandcamp.com/album/newest", "Fresh", "Newest"),
            ("https://fresh.bandcamp.com/album/middle", "Fresh", "Middle"),
            ("https://fresh.bandcamp.com/album/oldest", "Fresh", "Oldest"),
        ],
        vec![],
    );
    let fan = fx.add_fan(ALICE, true).await;
    let id = fan["id"].as_i64().unwrap();
    let walk = fx.wait_for_walk(id).await;

    assert_eq!(walk["phase"], "done", "{walk}");
    assert_eq!(walk["seen"], 3);
    assert_eq!(walk["new"], 3);
    assert_eq!(walk["queued"], 0, "someone else's wishlist is not downloaded by walking it");

    let (_, body) = fx.get(&format!("/fans/{id}")).await;
    assert_eq!(body["items"], 3);
    assert_eq!(body["counts"], json!({"new": 3}));

    let order = fx.titles_in_order(id);
    assert_eq!(order, [("Newest".into(), Some(1)), ("Middle".into(), Some(2)), ("Oldest".into(), Some(3))]);
    // Not my wishlist: the sticky flag stays off, and the provenance says so.
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM harvest_items WHERE in_wishlist = 1"), 0);
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM harvest_items WHERE source_kind = 'fan'"), 3);
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'download'"), 0);
    // The walk itself is one job with one item.
    // (The state goes out before the runner settles the item: give it a moment.)
    for _ in 0..100 {
        if fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'walk' AND status = 'completed'") == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'walk' AND status = 'completed' AND total = 1"), 1);
    let params: String = fx.q("SELECT params FROM jobs WHERE kind = 'walk'");
    let p: Value = serde_json::from_str(&params).unwrap();
    assert_eq!(p["fan_id"], id);
    assert_eq!(p["tabs"], json!(["wishlist", "collection"]));
}

#[tokio::test]
async fn self_walk_queues_only_what_is_missing_into_the_downloads_root() {
    // The backfill contract the saved wishlist had, unchanged. Straight into the downloads root
    // and unowned: a wishlist is by definition things you do not own.
    let fx = fx().await;
    fx.seed_release("Owned Artist", "Owned Album", None);
    fx.fake.set_fixed(
        vec![
            ("https://owned.bandcamp.com/album/owned-album", "Owned Artist", "Owned Album"),
            ("https://fresh.bandcamp.com/album/one", "Fresh Artist", "One"),
            ("https://fresh.bandcamp.com/album/two", "Fresh Artist", "Two"),
        ],
        vec![],
    );
    let id = fx.make_self(MINE);

    assert_eq!(fx.post(&format!("/fans/{id}/walk"), None).await.0, 202);
    let walk = fx.wait_for_walk(id).await;

    assert_eq!(walk["phase"], "done", "{walk}");
    assert_eq!(walk["in_library"], 1, "the seeded release must be recognised, not re-queued");
    assert_eq!(walk["queued"], 2);
    assert!(walk["job_id"].is_string());

    let dirs: Vec<Option<String>> = fx
        .ctx
        .db
        .read(|c| {
            let mut st = c.prepare(
                "SELECT ji.target_dir FROM job_items ji JOIN jobs j ON j.id = ji.job_id WHERE j.kind = 'download'",
            )?;
            Ok(st.query_map([], |r| r.get(0))?.collect::<bc_db::rusqlite::Result<Vec<_>>>()?)
        })
        .unwrap();
    assert_eq!(dirs, [None, None]);
    let states: HashSet<String> = fx
        .ctx
        .db
        .read(|c| {
            let mut st = c.prepare("SELECT DISTINCT state FROM harvest_items")?;
            Ok(st.query_map([], |r| r.get(0))?.collect::<bc_db::rusqlite::Result<_>>()?)
        })
        .unwrap();
    assert_eq!(states, HashSet::from(["in_library".to_string(), "queued".to_string()]));
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM harvest_items WHERE in_wishlist = 0"), 0);
    let params: String = fx.q("SELECT params FROM jobs WHERE kind = 'download'");
    assert!(!params.contains("source_fan_id"), "my wishlist downloads into my library: {params}");

    let (_, body) = fx.get(&format!("/fans/{id}")).await;
    assert_eq!(body["counts"], json!({"in_library": 1, "queued": 2}));
}

#[tokio::test]
async fn walking_someone_elses_list_keeps_my_provenance() {
    // A record on both lists is one inbox row; the later walk of the other person's list must
    // not claim it or flag it as theirs.
    let fx = fx().await;
    fx.fake.set_fixed(vec![("https://shared.bandcamp.com/album/both", "Shared", "Both")], vec![]);
    let mine = fx.make_self(MINE);
    fx.post(&format!("/fans/{mine}/walk"), Some(json!({"queue_new": false}))).await;
    fx.wait_for_walk(mine).await;

    let alice = fx.add_fan(ALICE, true).await["id"].as_i64().unwrap();
    fx.wait_for_walk(alice).await;

    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM harvest_items"), 1);
    assert_eq!(fx.q::<i64>("SELECT in_wishlist FROM harvest_items"), 1);
    assert_eq!(fx.q::<String>("SELECT source_kind FROM harvest_items"), "wishlist");
    let fans_on: HashSet<i64> = fx
        .ctx
        .db
        .read(|c| {
            let mut st = c.prepare("SELECT DISTINCT fan_id FROM fan_items")?;
            Ok(st.query_map([], |r| r.get(0))?.collect::<bc_db::rusqlite::Result<_>>()?)
        })
        .unwrap();
    assert_eq!(fans_on, HashSet::from([mine, alice]));
}

#[tokio::test]
async fn a_complete_rewalk_prunes_what_left_the_list() {
    let fx = fx().await;
    fx.fake.set_fixed(
        vec![("https://a.bandcamp.com/album/keep", "A", "Keep"), ("https://a.bandcamp.com/album/gone", "A", "Gone")],
        vec![],
    );
    let id = fx.make_self(MINE);
    fx.post(&format!("/fans/{id}/walk"), Some(json!({"queue_new": false}))).await;
    fx.wait_for_walk(id).await;
    assert_eq!(fx.get(&format!("/fans/{id}")).await.1["items"], 2);

    fx.fake.set_fixed(vec![("https://a.bandcamp.com/album/keep", "A", "Keep")], vec![]);
    fx.post(&format!("/fans/{id}/walk"), Some(json!({"queue_new": false}))).await;
    fx.wait_for_walk(id).await;

    assert_eq!(fx.get(&format!("/fans/{id}")).await.1["items"], 1);
    // The inbox row itself stays; only the membership goes -- and the sticky flag follows the list.
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM harvest_items WHERE title = 'Gone'"), 1);
    assert_eq!(fx.q::<i64>("SELECT in_wishlist FROM harvest_items WHERE title = 'Gone'"), 0);
}

fn slow_after_first(started: Arc<Notify>) -> impl Fn(&str) -> EventStream + Send + Sync + 'static {
    move |_| {
        let started = started.clone();
        Box::pin(async_stream::stream! {
            yield Ok(event("https://a.bandcamp.com/album/first", "A", "First", 1, 2));
            started.notify_one();
            tokio::time::sleep(Duration::from_secs(5)).await;
            yield Ok(bc_bandcamp::sources::HarvestEvent { seen: 2, total: Some(2), ..Default::default() });
        })
    }
}

#[tokio::test]
async fn stopping_a_walk_keeps_what_it_found() {
    let fx = fx().await;
    let started = Arc::new(Notify::new());
    fx.fake.set(slow_after_first(started.clone()));
    let fan = fx.add_fan(ALICE, true).await;
    let id = fan["id"].as_i64().unwrap();
    tokio::time::timeout(Duration::from_secs(5), started.notified()).await.expect("the walk never got going");

    fx.delete(&format!("/fans/{id}/walk")).await;
    let walk = fx.wait_for_walk(id).await;

    assert_eq!(walk["error"], "Stopped");
    assert_eq!(fx.get(&format!("/fans/{id}")).await.1["items"], 1);
}

#[tokio::test]
async fn a_stopped_self_walk_still_queues_what_it_has_seen() {
    let fx = fx().await;
    let started = Arc::new(Notify::new());
    fx.fake.set(slow_after_first(started.clone()));
    let id = fx.make_self(MINE);
    fx.post(&format!("/fans/{id}/walk"), None).await;
    tokio::time::timeout(Duration::from_secs(5), started.notified()).await.expect("the walk never got going");

    fx.delete(&format!("/fans/{id}/walk")).await;
    let walk = fx.wait_for_walk(id).await;
    assert_eq!(walk["error"], "Stopped");
    assert_eq!(walk["queued"], 1, "{walk}");
    assert_eq!(fx.q::<String>("SELECT state FROM harvest_items"), "queued");
}

fn gated(gate: Arc<AtomicBool>) -> impl Fn(&str) -> EventStream + Send + Sync + 'static {
    move |_| {
        let gate = gate.clone();
        Box::pin(async_stream::stream! {
            while !gate.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            yield Ok(bc_bandcamp::sources::HarvestEvent { seen: 0, total: Some(0), ..Default::default() });
        })
    }
}

#[tokio::test]
async fn walks_queue_up_rather_than_refuse() {
    // A second wishlist asked for during a walk waits its turn (FIFO, one in flight).
    let fx = fx().await;
    let gate = Arc::new(AtomicBool::new(false));
    fx.fake.set(gated(gate.clone()));
    let alice = fx.add_fan(ALICE, true).await["id"].as_i64().unwrap();
    let bob = fx.add_fan("bob", true).await["id"].as_i64().unwrap();
    let (_, listed) = fx.get("/fans").await;
    let states: std::collections::BTreeMap<String, String> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["username"].as_str().unwrap().to_string(), f["walk"]["phase"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(states["alice"], "harvesting");
    assert_eq!(states["bob"], "queued");
    // Two jobs, one claimed at a time.
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'walk'"), 2);
    // Asking again for a fan that is already waiting does not queue it twice.
    fx.post(&format!("/fans/{bob}/walk"), None).await;
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'walk'"), 2);

    gate.store(true, Ordering::SeqCst);
    assert_eq!(fx.wait_for_walk(alice).await["phase"], "done");
    assert_eq!(fx.wait_for_walk(bob).await["phase"], "done");
    for _ in 0..100 {
        if fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'walk' AND status = 'completed'") == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'walk' AND status = 'completed'"), 2);
}

#[tokio::test]
async fn stopping_a_waiting_walk_cancels_its_job() {
    let fx = fx().await;
    let gate = Arc::new(AtomicBool::new(false));
    fx.fake.set(gated(gate.clone()));
    let alice = fx.add_fan(ALICE, true).await["id"].as_i64().unwrap();
    let bob = fx.add_fan("bob", true).await["id"].as_i64().unwrap();
    let (_, b) = fx.delete(&format!("/fans/{bob}/walk")).await;
    assert!(b["walk"].is_null() || b["walk"]["phase"] != "queued", "{b}");
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'walk' AND status = 'cancelled'"), 1);
    gate.store(true, Ordering::SeqCst);
    assert_eq!(fx.wait_for_walk(alice).await["phase"], "done");
}

#[tokio::test]
async fn walk_covers_the_collection_too_and_both_read_as_one_list() {
    // A fan page has a wishlist and a collection; both are walked, a record on both is one item
    // on two lists, and the unified view never shows it twice.
    let fx = fx().await;
    fx.fake.set_fixed(
        vec![("https://a.bandcamp.com/album/wished", "A", "Wished"), ("https://a.bandcamp.com/album/both", "A", "Both")],
        vec![("https://a.bandcamp.com/album/both", "A", "Both"), ("https://a.bandcamp.com/album/owned", "A", "Owned")],
    );
    let fan = fx.add_fan(ALICE, true).await;
    let id = fan["id"].as_i64().unwrap();
    let walk = fx.wait_for_walk(id).await;
    assert_eq!(walk["phase"], "done", "{walk}");
    assert_eq!(walk["tabs"], json!(["wishlist", "collection"]));
    assert_eq!(walk["seen"], 4);

    let (_, body) = fx.get(&format!("/fans/{id}")).await;
    assert_eq!(body["items"], 3, "three distinct records");
    assert_eq!(body["tabs"]["wishlist"]["items"], 2);
    assert_eq!(body["tabs"]["collection"]["items"], 2);
    assert_eq!(body["tabs"]["collection"]["reported"], 7);

    let titles = |tab: &str| -> Vec<String> {
        let tab = tab.to_string();
        fx.ctx
            .db
            .read(move |c| {
                let t = if tab == "all" { None } else { Some(tab.as_str()) };
                let sql = format!(
                    "SELECT h.title FROM harvest_items h JOIN {} m ON m.item_id = h.id ORDER BY COALESCE(m.position, 1000000000), h.id",
                    fans::members_sql(t)
                );
                let mut st = c.prepare(&sql)?;
                Ok(st
                    .query_map(bc_db::rusqlite::params_from_iter(fans::member_params(id, t)), |r| r.get(0))?
                    .collect::<bc_db::rusqlite::Result<Vec<_>>>()?)
            })
            .unwrap()
    };
    assert_eq!(titles("all"), ["Wished", "Both", "Owned"]);
    assert_eq!(titles("wishlist"), ["Wished", "Both"]);
    assert_eq!(titles("collection"), ["Both", "Owned"]);

    let badges = fx
        .ctx
        .db
        .read(move |c| {
            let ids: Vec<i64> = c
                .prepare("SELECT id FROM harvest_items")?
                .query_map([], |r| r.get(0))?
                .collect::<bc_db::rusqlite::Result<_>>()?;
            let tabs = fans::tabs_of(c, id, &ids).map_err(|e| bc_db::DbError::Other(e.to_string()))?;
            let mut by_title = std::collections::BTreeMap::new();
            for i in ids {
                let t: String = c.query_row("SELECT title FROM harvest_items WHERE id = ?1", [i], |r| r.get(0))?;
                by_title.insert(t, tabs.get(&i).cloned().unwrap_or_default());
            }
            Ok(by_title)
        })
        .unwrap();
    assert_eq!(badges["Wished"], ["wishlist"]);
    assert_eq!(badges["Both"], ["wishlist", "collection"]);
    assert_eq!(badges["Owned"], ["collection"]);
    // Someone else's collection is not *my* collection.
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM harvest_items WHERE in_collection = 1"), 0);

    // Play order can be one list or the other.
    let (_, nxt) = fx.get(&format!("/fans/{id}/next?tab=collection&limit=5")).await;
    let names: Vec<&str> = nxt["items"].as_array().unwrap().iter().map(|i| i["title"].as_str().unwrap()).collect();
    assert_eq!(names, ["Both", "Owned"]);
    let (_, nxt) = fx.get(&format!("/fans/{id}/next?limit=5")).await;
    let names: Vec<&str> = nxt["items"].as_array().unwrap().iter().map(|i| i["title"].as_str().unwrap()).collect();
    assert_eq!(names, ["Wished", "Both", "Owned"]);
}

#[tokio::test]
async fn walking_one_list_leaves_the_other_alone() {
    let fx = fx().await;
    fx.fake.set_fixed(
        vec![("https://a.bandcamp.com/album/wished", "A", "Wished")],
        vec![("https://a.bandcamp.com/album/owned", "A", "Owned")],
    );
    let id = fx.add_fan(ALICE, false).await["id"].as_i64().unwrap();
    fx.post(&format!("/fans/{id}/walk"), Some(json!({"tabs": ["collection"]}))).await;
    let walk = fx.wait_for_walk(id).await;
    assert_eq!(walk["tabs"], json!(["collection"]));
    let (_, body) = fx.get(&format!("/fans/{id}")).await;
    assert_eq!(
        body["tabs"],
        json!({
            "wishlist": {"items": 0, "counts": {}, "reported": 3},
            "collection": {"items": 1, "counts": {"new": 1}, "reported": 7},
        })
    );
}

/// A fan whose wishlist Bandcamp will not show, over a collection that reads: a private
/// wishlist looks exactly like this from here.
fn refusing(collection: Vec<Row>) -> impl Fn(&str) -> EventStream + Send + Sync + 'static {
    move |which| {
        if which != "collection" {
            return futures::stream::iter(vec![Err(HarvestError::ListUnavailable(
                "Bandcamp does not show this fan's wishlist".into(),
            ))])
            .boxed();
        }
        rows_stream(&collection)
    }
}

#[tokio::test]
async fn a_list_bandcamp_will_not_show_does_not_sink_the_walk() {
    // A private wishlist is a fact about that list. The collection beside it is still readable,
    // and walking it is the whole point of following them.
    let fx = fx().await;
    fx.fake.set(refusing(vec![("https://a.bandcamp.com/album/owned", "A", "Owned")]));
    let id = fx.add_fan(ALICE, true).await["id"].as_i64().unwrap();
    let walk = fx.wait_for_walk(id).await;

    assert_eq!(walk["phase"], "done", "{}", walk["error"]);
    assert!(walk["error"].is_null());
    assert_eq!(walk["seen"], 1);
    assert_eq!(walk["errors"], json!(["Bandcamp does not show this fan's wishlist."]));

    let (_, body) = fx.get(&format!("/fans/{id}")).await;
    assert_eq!(body["tabs"]["collection"]["items"], 1);
    assert_eq!(body["tabs"]["wishlist"]["items"], 0);
    assert!(body["last_error"].is_null());
}

#[tokio::test]
async fn a_walk_that_reads_no_list_at_all_fails_with_the_reason() {
    let fx = fx().await;
    fx.fake.set(refusing(vec![]));
    let id = fx.add_fan(ALICE, false).await["id"].as_i64().unwrap();
    fx.post(&format!("/fans/{id}/walk"), Some(json!({"tabs": ["wishlist"]}))).await;
    let walk = fx.wait_for_walk(id).await;

    assert_eq!(walk["phase"], "failed");
    assert_eq!(walk["error"], "Bandcamp does not show this fan's wishlist");
    // The failed walk's reason is written on the fan, and its job failed.
    assert_eq!(fx.get(&format!("/fans/{id}")).await.1["last_error"], "Bandcamp does not show this fan's wishlist");
    for _ in 0..100 {
        if fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'walk' AND status = 'failed'") == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM jobs WHERE kind = 'walk' AND status = 'failed'"), 1);
}

#[tokio::test]
async fn one_list_failing_still_walks_the_other() {
    // Not a refusal but a fault -- it is noted, the other list is still read, and the walk
    // reports what it managed.
    let fx = fx().await;
    fx.fake.set(|which| {
        if which == "wishlist" {
            return futures::stream::iter(vec![Err(HarvestError::other("bandcamp said no"))]).boxed();
        }
        rows_stream(&[("https://a.bandcamp.com/album/owned", "A", "Owned")])
    });
    let id = fx.add_fan(ALICE, true).await["id"].as_i64().unwrap();
    let walk = fx.wait_for_walk(id).await;

    assert_eq!(walk["phase"], "done");
    assert_eq!(walk["errors"], json!(["wishlist: bandcamp said no"]));
    assert_eq!(fx.get(&format!("/fans/{id}")).await.1["tabs"]["collection"]["items"], 1);
}

#[tokio::test]
async fn my_own_collection_marks_what_i_own() {
    let fx = fx().await;
    fx.fake.set_fixed(vec![], vec![("https://a.bandcamp.com/album/owned", "A", "Owned")]);
    let id = fx.make_self(MINE);
    fx.post(&format!("/fans/{id}/walk"), Some(json!({"tabs": ["collection"], "queue_new": false}))).await;
    fx.wait_for_walk(id).await;
    assert_eq!(fx.q::<i64>("SELECT in_collection FROM harvest_items"), 1);
    assert_eq!(fx.q::<i64>("SELECT in_wishlist FROM harvest_items"), 0);
    assert_eq!(fx.q::<String>("SELECT source_kind FROM harvest_items"), "collection");
}

// -- crash recovery --------------------------------------------------------------

#[tokio::test]
async fn a_walk_item_left_running_is_requeued_and_the_walk_restarts_cleanly() {
    let fx = fx_opts(false).await;
    fx.fake.set_fixed(vec![("https://a.bandcamp.com/album/one", "A", "One")], vec![]);
    let id = fx.add_fan(ALICE, false).await["id"].as_i64().unwrap();

    // A walk job the previous process had claimed when it died.
    let store = fx.ctx.jobs.store().clone();
    let nj = bc_jobs::NewJob::new(bc_types::jobs::KIND_WALK, vec![bc_jobs::NewItem::default()])
        .label("Walk alice")
        .params(json!({"fan_id": id, "tabs": ["wishlist"], "queue_new": false}));
    let job = store.create_job(nj).unwrap();
    let claimed = store.claim_item("walk", 1, 60.0).unwrap().expect("claimed");
    assert_eq!(claimed.item.status, "running");

    let report = store.reconcile().unwrap();
    assert_eq!(report.requeued, 1);
    assert_eq!(store.get_item(claimed.item.id).unwrap().unwrap().status, "pending");

    // Boot: the waiting walk is re-adopted (and reads as queued/harvesting meanwhile), then runs.
    fans::start(&fx.ctx).await;
    let walk = fx.wait_for_walk(id).await;
    assert_eq!(walk["phase"], "done", "{walk}");
    assert_eq!(walk["tabs"], json!(["wishlist"]));
    assert_eq!(fx.get(&format!("/fans/{id}")).await.1["items"], 1);
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "completed");
    assert_eq!(store.get_item(claimed.item.id).unwrap().unwrap().status, "done");
}

// -- migration from the saved wishlist ------------------------------------------

#[tokio::test]
async fn the_saved_wishlist_becomes_the_self_fan() {
    let fx = fx_opts(false).await;
    fx.ctx
        .db
        .write(|tx| {
            bc_db::settings::set(tx, fans::LEGACY_URL_KEY, MINE)?;
            for (url, wish, coll) in [
                ("https://a.bandcamp.com/album/x", 1, 0),
                ("https://a.bandcamp.com/album/y", 1, 0),
                ("https://a.bandcamp.com/album/z", 0, 0),
                ("https://a.bandcamp.com/album/w", 0, 1),
            ] {
                tx.execute(
                    "INSERT INTO harvest_items(url, url_kind, state, title, artist_name, tags, in_collection, in_wishlist, \
                     is_free_download, is_purchasable, is_preorder, discovered_at) \
                     VALUES (?1,'album','new','','','[]',?2,?3,0,1,0,datetime('now'))",
                    bc_db::rusqlite::params![url, coll, wish],
                )?;
            }
            Ok(())
        })
        .unwrap();

    let migrate = |fx: &Fx| {
        fx.ctx
            .db
            .write_with::<_, HarvestError>(fans::ensure_self_fan)
            .unwrap()
    };
    let fan = migrate(&fx).expect("self fan");
    assert!(fan.is_self);
    assert_eq!(fan.username, "someone");
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM fan_items"), 3);
    let tabs: HashSet<String> = fx
        .ctx
        .db
        .read(|c| {
            let mut st = c.prepare("SELECT DISTINCT tab FROM fan_items")?;
            Ok(st.query_map([], |r| r.get(0))?.collect::<bc_db::rusqlite::Result<_>>()?)
        })
        .unwrap();
    assert_eq!(tabs, HashSet::from(["wishlist".to_string(), "collection".to_string()]));
    // Idempotent: a second start creates nothing new.
    let again = migrate(&fx).expect("self fan");
    assert_eq!(again.id, fan.id);
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM fans"), 1);
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM fan_items"), 3);
}

#[tokio::test]
async fn without_a_saved_wishlist_there_is_no_self_fan() {
    let fx = fx_opts(false).await;
    let none = fx.ctx.db.write_with::<_, HarvestError>(fans::ensure_self_fan).unwrap();
    assert!(none.is_none());
}

// -- play order ------------------------------------------------------------------

/// `n` members of a fan at positions 1..=n, ids returned in list order.
fn members(fx: &Fx, fan_id: i64, n: i64) -> Vec<i64> {
    fx.ctx
        .db
        .write(move |tx| {
            let mut ids = Vec::new();
            for pos in 1..=n {
                tx.execute(
                    "INSERT INTO harvest_items(url, url_kind, state, title, artist_name, tags, in_collection, in_wishlist, \
                     is_free_download, is_purchasable, is_preorder, discovered_at) \
                     VALUES (?1,'album','new',?2,'','[]',0,0,0,1,0,datetime('now'))",
                    bc_db::rusqlite::params![format!("https://a.bandcamp.com/album/r{pos}"), format!("R{pos}")],
                )?;
                let item = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO fan_items(fan_id, item_id, tab, position, first_seen_at, last_seen_at) \
                     VALUES (?1, ?2, 'wishlist', ?3, datetime('now'), datetime('now'))",
                    bc_db::rusqlite::params![fan_id, item, pos],
                )?;
                ids.push(item);
            }
            Ok(ids)
        })
        .unwrap()
}

fn item_ids(v: &Value) -> Vec<i64> {
    v["items"].as_array().unwrap().iter().map(|i| i["item_id"].as_i64().unwrap()).collect()
}

#[tokio::test]
async fn next_in_list_order_walks_the_wishlist_and_ends() {
    let fx = fx().await;
    let fan = fx.add_fan(ALICE, false).await["id"].as_i64().unwrap();
    let ids = members(&fx, fan, 3);

    let (_, first) = fx.get(&format!("/fans/{fan}/next")).await;
    assert_eq!(item_ids(&first), [ids[0]]);
    assert_eq!(first["exhausted"], false);

    let (_, after_second) = fx.get(&format!("/fans/{fan}/next?after={}", ids[1])).await;
    assert_eq!(item_ids(&after_second), [ids[2]]);
    assert_eq!(after_second["exhausted"], true);

    let (_, nothing) = fx.get(&format!("/fans/{fan}/next?after={}", ids[2])).await;
    assert_eq!(nothing, json!({"items": [], "exhausted": true}));
    assert_eq!(fx.get("/fans/999/next").await.0, 404);
}

#[tokio::test]
async fn shuffle_order_is_fixed_by_seed_and_never_repeats() {
    let fx = fx().await;
    let fan = fx.add_fan(ALICE, false).await["id"].as_i64().unwrap();
    let ids = members(&fx, fan, 12);

    async fn walk(fx: &Fx, fan: i64, seed: u32) -> Vec<i64> {
        let mut seen = Vec::new();
        let mut after: Option<i64> = None;
        loop {
            let q = match after {
                Some(a) => format!("/fans/{fan}/next?order=shuffle&seed={seed}&after={a}&limit=5"),
                None => format!("/fans/{fan}/next?order=shuffle&seed={seed}&limit=5"),
            };
            let (_, page) = fx.get(&q).await;
            let got = item_ids(&page);
            seen.extend(&got);
            if page["exhausted"] == true || got.is_empty() {
                return seen;
            }
            after = got.last().copied();
        }
    }
    let order_a = walk(&fx, fan, 7).await;
    let mut sorted_a = order_a.clone();
    sorted_a.sort();
    assert_eq!(sorted_a, ids, "every item exactly once");
    assert_ne!(order_a, ids, "and not in list order");
    assert_eq!(walk(&fx, fan, 7).await, order_a, "the same seed gives the same order after a reload");
    assert_ne!(walk(&fx, fan, 8).await, order_a);
}

#[test]
fn the_shuffle_key_is_a_bijection_so_no_seed_repeats_or_skips_an_item() {
    for seed in [0u32, 1, 7, 8, 12345, i32::MAX as u32] {
        let keys: HashSet<i64> = (1..=20_000).map(|id| fans::shuffle_key(id, seed)).collect();
        assert_eq!(keys.len(), 20_000, "seed {seed}");
        // Spot-check the formula itself: ((id + seed) * 2654435761) % 2^32.
        assert_eq!(fans::shuffle_key(3, seed), ((3 + i64::from(seed)) * 2_654_435_761) % 4_294_967_296);
    }
}

#[tokio::test]
async fn fan_next_pages_a_shuffle_without_repeats_at_any_page_size() {
    let fx = fx().await;
    let fan = fx.add_fan(ALICE, false).await["id"].as_i64().unwrap();
    let ids = members(&fx, fan, 101);
    // Unplaced memberships (no position) sort last, in id order, in list order.
    let extra = fx
        .ctx
        .db
        .write(move |tx| {
            tx.execute(
                "INSERT INTO harvest_items(url, url_kind, state, title, artist_name, tags, in_collection, in_wishlist, \
                 is_free_download, is_purchasable, is_preorder, discovered_at) \
                 VALUES ('https://a.bandcamp.com/album/unplaced','album','new','U','','[]',0,0,0,1,0,datetime('now'))",
                [],
            )?;
            let item = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO fan_items(fan_id, item_id, tab, position, first_seen_at, last_seen_at) \
                 VALUES (?1, ?2, 'wishlist', NULL, datetime('now'), datetime('now'))",
                [fan, item],
            )?;
            Ok(item)
        })
        .unwrap();
    let mut all_ids = ids.clone();
    all_ids.push(extra);

    for limit in [1usize, 7, 50] {
        let mut seen: Vec<i64> = Vec::new();
        let mut after = None;
        loop {
            let page = fans::fan_next(&fx.ctx, fan, after, FanOrder::Shuffle, 42, &[], FanTab::All, limit).await.unwrap();
            seen.extend(page.items.iter().map(|i| i.item_id));
            if page.exhausted || page.items.is_empty() {
                break;
            }
            after = page.items.last().map(|i| i.item_id);
        }
        let unique: HashSet<i64> = seen.iter().copied().collect();
        assert_eq!(seen.len(), unique.len(), "limit {limit}: no repeats");
        assert_eq!(unique, all_ids.iter().copied().collect::<HashSet<_>>(), "limit {limit}: nothing skipped");
    }

    // In list order the unplaced one comes last.
    let mut seq: Vec<i64> = Vec::new();
    let mut after = None;
    loop {
        let page = fans::fan_next(&fx.ctx, fan, after, FanOrder::Seq, 0, &[], FanTab::All, 50).await.unwrap();
        seq.extend(page.items.iter().map(|i| i.item_id));
        if page.exhausted || page.items.is_empty() {
            break;
        }
        after = page.items.last().map(|i| i.item_id);
    }
    assert_eq!(seq, all_ids);
}

#[tokio::test]
async fn next_can_be_narrowed_to_states() {
    let fx = fx().await;
    let fan = fx.add_fan(ALICE, false).await["id"].as_i64().unwrap();
    let ids = members(&fx, fan, 3);
    let (a, b) = (ids[0], ids[1]);
    fx.ctx
        .db
        .write(move |tx| {
            tx.execute("UPDATE harvest_items SET state = 'ignored' WHERE id = ?1", [a])?;
            tx.execute("UPDATE harvest_items SET state = 'in_library' WHERE id = ?1", [b])?;
            Ok(())
        })
        .unwrap();

    let (_, default) = fx.get(&format!("/fans/{fan}/next?limit=5")).await;
    assert_eq!(item_ids(&default), [ids[1], ids[2]], "ignored is skipped");
    let (_, only_new) = fx.get(&format!("/fans/{fan}/next?state=new")).await;
    assert_eq!(item_ids(&only_new), [ids[2]]);
}

// -- the shelf ------------------------------------------------------------------

#[tokio::test]
async fn forgetting_a_fan_refuses_while_its_shelf_holds_records() {
    let fx = fx().await;
    let fan = fx.add_fan(ALICE, false).await["id"].as_i64().unwrap();
    let release = fx.seed_release("Shelved", "Record", Some(fan));

    assert_eq!(fx.delete(&format!("/fans/{fan}")).await.0, 409);
    let (_, body) = fx.get(&format!("/fans/{fan}")).await;
    assert_eq!(body["downloaded"], 1);

    let r = fx.http.delete(format!("{}/fans/{fan}?releases=adopt", fx.base)).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 204);
    let src: Option<i64> = fx.ctx.db.read(move |c| Ok(c.query_row("SELECT source_fan_id FROM releases WHERE id = ?1", [release], |r| r.get(0))?)).unwrap();
    assert_eq!(src, None);
    assert_eq!(fx.q::<i64>("SELECT COUNT(*) FROM fans"), 0);
}

#[tokio::test]
async fn queueing_from_a_fan_files_the_job_under_their_shelf() {
    // The harvest route's queue (`/harvest/items/queue`) is the harvest agent's; this is its
    // fan half: `inbox::queue` with `source_fan_id` and the shelf as the target subdir.
    let fx = fx().await;
    let fan = fx.add_fan(ALICE, false).await;
    let id = fan["id"].as_i64().unwrap();
    members(&fx, id, 2);
    let rows = fx
        .ctx
        .db
        .read(|c| {
            let ids: Vec<i64> =
                c.prepare("SELECT id FROM harvest_items WHERE state = 'new' ORDER BY id")?.query_map([], |r| r.get(0))?.collect::<bc_db::rusqlite::Result<_>>()?;
            Ok(inbox::load_rows(c, &ids).unwrap())
        })
        .unwrap();
    let out = inbox::queue(
        &fx.ctx.db,
        fx.ctx.jobs.store(),
        &rows,
        &QueueOpts {
            allow_unowned: true,
            target_subdir: Some(fan["shelf"].as_str().unwrap().to_string()),
            source_fan_id: Some(id),
            label: Some("alice's wishlist".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out.queued, 2);
    assert_eq!(fx.q::<String>("SELECT label FROM jobs WHERE kind = 'download'"), "alice's wishlist");
    let params: Value = serde_json::from_str(&fx.q::<String>("SELECT params FROM jobs WHERE kind = 'download'")).unwrap();
    assert_eq!(params["source_fan_id"], id);
    assert_eq!(fx.q::<i64>("SELECT COUNT(DISTINCT target_dir) FROM job_items WHERE target_dir = 'fan-alice'"), 1);
}

#[tokio::test]
async fn a_foreign_walk_queues_onto_the_fans_shelf_only_when_asked() {
    let fx = fx().await;
    fx.fake.set_fixed(
        vec![("https://a.bandcamp.com/album/one", "A", "One"), ("https://a.bandcamp.com/album/two", "A", "Two")],
        vec![],
    );
    let id = fx.add_fan(ALICE, false).await["id"].as_i64().unwrap();
    fx.post(&format!("/fans/{id}/walk"), Some(json!({"queue_new": true, "tabs": ["wishlist"]}))).await;
    let walk = fx.wait_for_walk(id).await;
    assert_eq!(walk["queued"], 2, "{walk}");
    let params: Value = serde_json::from_str(&fx.q::<String>("SELECT params FROM jobs WHERE kind = 'download'")).unwrap();
    assert_eq!(params["source_fan_id"], id);
    assert_eq!(fx.q::<String>("SELECT label FROM jobs WHERE kind = 'download'"), "alice's wishlist");
    assert_eq!(fx.q::<String>("SELECT DISTINCT target_dir FROM job_items WHERE target_dir IS NOT NULL"), "fan-alice");
}

#[test]
fn coerce_fan_url_accepts_links_and_usernames() {
    assert_eq!(fans::coerce_fan_url("https://bandcamp.com/alice/wishlist").unwrap(), "https://bandcamp.com/alice");
    assert_eq!(fans::coerce_fan_url("bandcamp.com/alice").unwrap(), "https://bandcamp.com/alice");
    assert_eq!(fans::coerce_fan_url("  bob ").unwrap(), "https://bandcamp.com/bob");
    assert!(fans::coerce_fan_url("").is_err());
    assert!(fans::coerce_fan_url("https://artist.bandcamp.com/album/x").is_err());
    assert_eq!(fans::username_from_url("https://bandcamp.com/alice"), "alice");
}

#[test]
fn coerce_tabs_keeps_walk_order_and_defaults_to_both() {
    let t = |v: &[&str]| fans::coerce_tabs(Some(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>()));
    assert_eq!(fans::coerce_tabs(None), ["wishlist", "collection"]);
    assert_eq!(t(&["collection", "wishlist"]), ["wishlist", "collection"]);
    assert_eq!(t(&["collection"]), ["collection"]);
    assert_eq!(t(&["bogus"]), ["wishlist", "collection"]);
}
