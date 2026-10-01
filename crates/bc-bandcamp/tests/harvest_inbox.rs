//! Ports of `test_harvest_queue.py` (queueing inbox items for download, settling, releasing),
//! exercising `harvest::inbox` through the routes, plus the Rust additions: the normalised
//! `harvest_item_tags` table staying in sync, the tag filter / aggregate routes, the fan filters,
//! `q`, and the ignore toggle.

mod harvest_common;

use bc_bandcamp::download::dedup::normalise;
use bc_bandcamp::extract::HarvestedRelease;
use bc_bandcamp::harvest::inbox::{self, UpsertOpts};
use bc_db::rusqlite::params;
use bc_jobs::{Complete, LEASE_SECONDS};
use harvest_common::*;
use serde_json::{Value, json};

const ALBUM: &str = "https://artist.bandcamp.com/album/a-record";
const QUEUE: &str = "/harvest/items/queue";

/// A release shelved off disk (`_shelved`): titled, optionally URL-ed, no artist.
fn shelved(app: &App, title: &str, url: Option<&str>) -> i64 {
    let (title, url) = (title.to_string(), url.map(normalise));
    app.exec(move |t| {
        t.execute(
            "INSERT INTO releases(title, title_key, kind, bandcamp_url, added_at) VALUES (?1, ?2, 'album', ?3, datetime('now'))",
            params![title, title.to_lowercase(), url],
        )?;
        Ok(t.last_insert_rowid())
    })
}

fn settle(app: &App, url: &str, release_id: Option<i64>) -> bool {
    inbox::settle(&app.db, url, release_id).expect("settle")
}

fn job_total(app: &App) -> i64 {
    app.q(|c| Ok(c.query_row("SELECT total FROM jobs WHERE kind = 'download'", [], |r| r.get(0))?))
}

// -- queue ------------------------------------------------------------------------------

/// Nothing owned and nothing free, so the default refuses and reports.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wishlist_items_need_the_unowned_flag() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);
    let (_, body) = app.post(QUEUE, json!({"item_ids": [id]})).await;

    assert_eq!(body["queued"], 0);
    assert_eq!(body["needs_confirmation"], json!([ALBUM]));
    assert_eq!(body["job_id"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unowned_flag_queues_them() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);
    let (_, body) = app.post(QUEUE, json!({"item_ids": [id], "allow_unowned": true})).await;

    assert_eq!(body["queued"], 1);
    assert!(body["job_id"].is_string());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn absent_subdir_groups_under_the_source_label() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);
    app.post(QUEUE, json!({"item_ids": [id], "allow_unowned": true})).await;
    assert_eq!(app.target_dirs(), vec![Some("fan-someone".to_string())]);
}

/// An explicit "" is a choice, not a missing value.
///
/// A harvest into a library that already uses bandcamp-dl's <artist>/<album> layout must land at
/// the root, or artists get duplicated a level down and bandcamp-dl can no longer see that it
/// already has the files.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn empty_subdir_writes_into_the_downloads_root() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);
    app.post(QUEUE, json!({"item_ids": [id], "allow_unowned": true, "target_subdir": ""})).await;
    assert_eq!(app.target_dirs(), vec![None::<String>]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn items_already_in_the_library_are_not_queued() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);
    app.set_state(id, "in_library");

    let (_, body) = app.post(QUEUE, json!({"item_ids": [id], "allow_unowned": true})).await;

    assert_eq!(body["queued"], 0);
    assert_eq!(body["skipped_in_library"], 1);
}

/// The opt-in: a self-contained folder wants the whole list, owned or not.
///
/// `force` must ride along in the job params, or the worker's preflight skips every in-library
/// URL right back out of the job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn include_in_library_queues_them_and_forces_the_download() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);
    app.set_state(id, "in_library");

    let (_, body) = app.post(QUEUE, json!({"item_ids": [id], "allow_unowned": true, "include_in_library": true})).await;

    assert_eq!(body["queued"], 1);
    assert_eq!(body["skipped_in_library"], 0);
    assert_eq!(app.first_job_params(), json!({"source": "harvest", "force": true}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_folder_marks_the_job_flat() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);
    app.post(QUEUE, json!({"item_ids": [id], "allow_unowned": true, "single_folder": true})).await;
    assert_eq!(app.first_job_params(), json!({"source": "harvest", "layout": "flat"}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queueing_flips_the_inbox_state() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);
    app.post(QUEUE, json!({"item_ids": [id], "allow_unowned": true})).await;

    assert_eq!(app.item_state(id), "queued");
    let resolved: Option<String> = app.q(move |c| Ok(c.query_row("SELECT resolved_at FROM harvest_items WHERE id = ?1", [id], |r| r.get(0))?));
    assert!(resolved.is_some());
    assert_eq!(app.get("/harvest/stats").await.1, json!({"queued": 1}));
}

/// The other half of queue(): without it the row says "downloading" forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_takes_a_finished_item_out_of_queued() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);
    let release = shelved(&app, "A Record", None);
    app.post(QUEUE, json!({"item_ids": [id], "allow_unowned": true})).await;
    assert_eq!(app.item_state(id), "queued");

    assert!(settle(&app, ALBUM, Some(release)));

    assert_eq!(app.item_state(id), "in_library");
    let rid: Option<i64> = app.q(move |c| Ok(c.query_row("SELECT release_id FROM harvest_items WHERE id = ?1", [id], |r| r.get(0))?));
    assert_eq!(rid, Some(release));
}

/// A wishlist or feed download carries no job-level label, so the label Bandcamp stated on the
/// release page -- held by the inbox row -- files the album the moment it lands, not at the next
/// server restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_files_the_release_under_the_items_stated_label() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);
    let release = shelved(&app, "A Record", None);
    let set_label = |name: &'static str| {
        app.exec(move |t| Ok(t.execute("UPDATE harvest_items SET label_name = ?2 WHERE id = ?1", params![id, name]).map(|_| ())?));
    };
    let release_label = || -> Option<String> {
        app.q(move |c| {
            Ok(c.query_row("SELECT l.name FROM releases r JOIN labels l ON l.id = r.label_id WHERE r.id = ?1", [release], |r| r.get(0)).ok())
        })
    };
    set_label("Life In Patterns");

    assert!(settle(&app, ALBUM, Some(release)));
    assert_eq!(release_label().as_deref(), Some("Life In Patterns"));

    // A label already on the row -- the files' own publisher tag, or a label-page job -- always
    // wins over the inbox's word: settling again under a different name must not re-file it.
    set_label("Some Other Imprint");
    assert!(settle(&app, ALBUM, Some(release)));
    assert_eq!(release_label().as_deref(), Some("Life In Patterns"));
}

/// A re-run that merges no new files still finished; the release is already there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_finds_the_release_by_url_when_the_caller_has_none() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);
    let release = shelved(&app, "A Record", Some(ALBUM));

    assert!(settle(&app, ALBUM, None));

    let rid: Option<i64> = app.q(move |c| Ok(c.query_row("SELECT release_id FROM harvest_items WHERE id = ?1", [id], |r| r.get(0))?));
    assert_eq!(rid, Some(release));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settle_without_a_release_still_leaves_the_queue() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);

    assert!(settle(&app, ALBUM, None));
    assert_eq!(app.item_state(id), "downloaded");
    // A URL the inbox never saw is not an error -- it was pasted by hand.
    assert!(!settle(&app, "https://x.bandcamp.com/album/never-seen", None));
}

/// The bug: the items cascade away and the inbox is left claiming they download.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_a_job_reopens_the_rows_it_never_ran() {
    let app = app().await;
    let ids: Vec<i64> = (0..3).map(|n| app.inbox(&format!("https://artist.bandcamp.com/album/rec-{n}"), "fan-someone", true)).collect();
    let (_, queued) = app.post(QUEUE, json!({"item_ids": ids, "allow_unowned": true})).await;
    let job_id = queued["job_id"].as_str().unwrap().to_string();

    // One got downloaded before the job was abandoned; the other two never ran.
    let store = app.ctx.jobs.store().clone();
    let claimed = store.claim_item("download", 1, LEASE_SECONDS).unwrap().expect("a pending item");
    store.complete_item(claimed.item.id, Complete::msg("ok")).unwrap();
    settle(&app, claimed.item.url.as_deref().unwrap(), None);

    assert_eq!(app.post_empty(&format!("/jobs/{job_id}/cancel")).await.0, 200);
    assert_eq!(app.delete(&format!("/jobs/{job_id}")).await.0, 204);

    let mut states: Vec<String> = ids.iter().map(|i| app.item_state(*i)).collect();
    states.sort();
    assert_eq!(states, vec!["downloaded", "new", "new"]);
    // And the two are queueable again rather than stranded as "downloading".
    assert_eq!(app.get("/harvest/stats").await.1, json!({"new": 2, "downloaded": 1}));
}

/// Only unrun items are reopened -- a completed one must not be re-offered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_item_survives_the_job_being_deleted() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);
    let (_, queued) = app.post(QUEUE, json!({"item_ids": [id], "allow_unowned": true})).await;
    let job_id = queued["job_id"].as_str().unwrap().to_string();

    let release = shelved(&app, "A Record", Some(ALBUM));
    let store = app.ctx.jobs.store().clone();
    let claimed = store.claim_item("download", 1, LEASE_SECONDS).unwrap().expect("a pending item");
    store.complete_item(claimed.item.id, Complete { message: Some("ok".into()), release_id: Some(release), ..Default::default() }).unwrap();
    settle(&app, claimed.item.url.as_deref().unwrap(), Some(release));

    assert_eq!(app.delete(&format!("/jobs/{job_id}")).await.0, 204);
    assert_eq!(app.item_state(id), "in_library");
}

/// The sticky in_wishlist flag, not source_kind -- see list_items.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn items_can_be_filtered_to_the_wishlist() {
    let app = app().await;
    let wished = app.inbox(ALBUM, "fan-someone", true);
    app.inbox("https://label.bandcamp.com/album/other", "fan-someone", false);

    let (_, page) = app.get("/harvest/items?in_wishlist=true&state=all").await;
    let ids: Vec<i64> = page["items"].as_array().unwrap().iter().map(|i| i["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![wished]);
    assert_eq!(page["total"], 1);
}

/// "Queue everything missing" must not sweep up other harvests' rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_matching_can_be_scoped_to_the_wishlist() {
    let app = app().await;
    app.inbox(ALBUM, "fan-someone", true);
    app.inbox("https://label.bandcamp.com/album/other", "fan-someone", false);

    let (_, body) = app.post(QUEUE, json!({"all_matching": true, "in_wishlist": true, "allow_unowned": true})).await;

    assert_eq!(body["queued"], 1);
    assert_eq!(app.target_dirs(), vec![Some("fan-someone".to_string())]);
}

/// A state filter of "new" alone would exclude the very rows the flag is for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_matching_with_include_in_library_brings_those_rows_along() {
    let app = app().await;
    app.inbox(ALBUM, "fan-someone", true);
    let owned = app.inbox("https://artist.bandcamp.com/album/owned", "fan-someone", true);
    app.set_state(owned, "in_library");

    let (_, body) = app
        .post(QUEUE, json!({"all_matching": true, "in_wishlist": true, "allow_unowned": true, "include_in_library": true}))
        .await;

    assert_eq!(body["queued"], 2);
    assert_eq!(body["skipped_in_library"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_matching_queues_every_new_item() {
    let app = app().await;
    for n in 0..3 {
        app.inbox(&format!("https://artist.bandcamp.com/album/rec-{n}"), "fan-someone", true);
    }

    let (_, body) = app.post(QUEUE, json!({"all_matching": true, "state": "new", "allow_unowned": true, "target_subdir": ""})).await;

    assert_eq!(body["queued"], 3);
    assert_eq!(job_total(&app), 3);
    assert_eq!(app.first_job_params(), json!({"source": "harvest"}));
}

// -- Rust additions ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queue_with_nothing_matching_is_a_400_and_wakes_nothing() {
    let app = app().await;
    assert_eq!(app.post(QUEUE, json!({"item_ids": [12345]})).await.0, 400);
    assert_eq!(app.post(QUEUE, json!({"all_matching": true})).await.0, 400);
}

/// `fan_id` / `tab` restrict `all_matching` through `fan_items`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_matching_can_be_scoped_to_one_fans_tab() {
    let app = app().await;
    let on_wish = app.inbox("https://artist.bandcamp.com/album/w", "fan-x", true);
    let on_coll = app.inbox("https://artist.bandcamp.com/album/c", "fan-x", false);
    app.inbox("https://artist.bandcamp.com/album/none", "fan-x", false);
    let fan = add_fan(&app, "x");
    fan_item(&app, fan, on_wish, "wishlist", 0);
    fan_item(&app, fan, on_coll, "collection", 0);

    let (_, body) = app.post(QUEUE, json!({"all_matching": true, "fan_id": fan, "tab": "wishlist", "allow_unowned": true})).await;
    assert_eq!(body["queued"], 1);
    assert_eq!(app.item_state(on_wish), "queued");
    assert_eq!(app.item_state(on_coll), "new");

    let (_, body) = app.post(QUEUE, json!({"all_matching": true, "fan_id": fan, "allow_unowned": true})).await;
    assert_eq!(body["queued"], 1, "tab=all is both lists; the wishlist row is already queued");
    assert_eq!(app.item_state(on_coll), "queued");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ignore_toggles_and_404s() {
    let app = app().await;
    let id = app.inbox(ALBUM, "fan-someone", true);

    let (status, body) = app.post_empty(&format!("/harvest/items/{id}/ignore")).await;
    assert_eq!((status, body["state"].as_str()), (200, Some("ignored")));
    let (_, body) = app.post_empty(&format!("/harvest/items/{id}/ignore")).await;
    assert_eq!(body["state"], "new");
    assert_eq!(app.post_empty("/harvest/items/99999/ignore").await.0, 404);
}

fn add_fan(app: &App, username: &str) -> i64 {
    let username = username.to_string();
    app.exec(move |t| {
        t.execute(
            "INSERT INTO fans(username, url, is_self, created_at) VALUES (?1, ?2, 0, datetime('now'))",
            params![username, format!("https://bandcamp.com/{username}")],
        )?;
        Ok(t.last_insert_rowid())
    })
}

fn fan_item(app: &App, fan: i64, item: i64, tab: &str, position: i64) {
    let tab = tab.to_string();
    app.exec(move |t| {
        t.execute(
            "INSERT INTO fan_items(fan_id, item_id, tab, position, first_seen_at, last_seen_at) VALUES (?1,?2,?3,?4,datetime('now'),datetime('now'))",
            params![fan, item, tab, position],
        )?;
        Ok(())
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn items_by_fan_come_in_list_order_with_their_tabs() {
    let app = app().await;
    let a = app.inbox("https://x.bandcamp.com/album/a", "fan-x", true);
    let b = app.inbox("https://x.bandcamp.com/album/b", "fan-x", true);
    let other = app.inbox("https://x.bandcamp.com/album/other", "fan-y", true);
    let fan = add_fan(&app, "x");
    fan_item(&app, fan, a, "wishlist", 5);
    fan_item(&app, fan, a, "collection", 9);
    fan_item(&app, fan, b, "wishlist", 2);

    let (_, page) = app.get(&format!("/harvest/items?fan_id={fan}")).await;
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.iter().map(|i| i["id"].as_i64().unwrap()).collect::<Vec<_>>(), vec![b, a], "list order, earlier position first");
    assert_eq!(page["total"], 2, "an item on both lists is still one row");
    assert_eq!(items[1]["position"], 5, "the earlier of its two positions");
    assert_eq!(items[1]["tabs"], json!(["wishlist", "collection"]));
    assert!(!items.iter().any(|i| i["id"] == json!(other)));

    let (_, page) = app.get(&format!("/harvest/items?fan_id={fan}&tab=collection")).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["id"], json!(a));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn q_matches_title_or_artist_substring_and_limits_are_validated() {
    let app = app().await;
    app.inbox_with("https://x.bandcamp.com/album/1", "Deep Space", "Alpha", "label", "x", false, &[]);
    app.inbox_with("https://x.bandcamp.com/album/2", "Other", "Spacey Beta", "label", "x", false, &[]);
    app.inbox_with("https://x.bandcamp.com/album/3", "Nope", "Gamma", "label", "x", false, &[]);

    let (_, page) = app.get("/harvest/items?q=SPACE").await;
    assert_eq!(page["total"], 2);
    assert_eq!(app.get("/harvest/items?q=100%25").await.1["total"], 0, "a % in q is literal");
    assert_eq!(app.get("/harvest/items?limit=0").await.0, 422);
    assert_eq!(app.get("/harvest/items?limit=501").await.0, 422);
    assert_eq!(app.get("/harvest/items?limit=1").await.1["items"].as_array().unwrap().len(), 1);
}

// -- the normalised tag table -----------------------------------------------------------

fn tag_rows(app: &App, item: i64) -> Vec<(String, String)> {
    app.q(move |c| {
        let mut st = c.prepare("SELECT tag_key, tag FROM harvest_item_tags WHERE item_id = ?1 ORDER BY tag_key")?;
        Ok(st.query_map([item], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?)
    })
}

fn upsert(app: &App, release: HarvestedRelease) -> i64 {
    app.exec(move |t| {
        let u = inbox::upsert(
            t,
            &release,
            &UpsertOpts { source_kind: "label", source_label: "x", label_name: None, in_collection: false, in_wishlist: false, claim_source: true },
            None,
        )
        .map_err(|e| bc_db::DbError::Other(e.to_string()))?;
        Ok(u.id)
    })
}

fn release(url: &str, tags: &[&str], shallow: bool) -> HarvestedRelease {
    HarvestedRelease {
        url: url.into(),
        title: "T".into(),
        artist_name: "A".into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        shallow,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upsert_keeps_the_tag_table_in_sync() {
    let app = app().await;
    let url = "https://x.bandcamp.com/album/t";
    let id = upsert(&app, release(url, &["Deep House", "deep  house", "Techno"], false));
    // Folded keys collapse spellings; the first spelling is kept.
    assert_eq!(tag_rows(&app, id), vec![("deep house".to_string(), "Deep House".to_string()), ("techno".to_string(), "Techno".to_string())]);

    // A shallow record's stamp never overwrites the page's own tags...
    upsert(&app, release(url, &["Ambient"], true));
    assert_eq!(tag_rows(&app, id).len(), 2);
    // ...but a full fetch replaces them, and the stale rows go.
    upsert(&app, release(url, &["Ambient", "Dub"], false));
    assert_eq!(tag_rows(&app, id), vec![("ambient".to_string(), "Ambient".to_string()), ("dub".to_string(), "Dub".to_string())]);
    // The JSON column agrees.
    let json: String = app.q(move |c| Ok(c.query_row("SELECT tags FROM harvest_items WHERE id = ?1", [id], |r| r.get(0))?));
    assert_eq!(serde_json::from_str::<Vec<String>>(&json).unwrap(), vec!["Ambient", "Dub"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tag_filter_needs_every_tag_and_folds_case() {
    let app = app().await;
    let a = app.inbox_with("https://x.bandcamp.com/album/a", "A", "X", "label", "x", false, &["Techno", "Deep House"]);
    let b = app.inbox_with("https://x.bandcamp.com/album/b", "B", "X", "label", "x", false, &["techno"]);
    let c = app.inbox_with("https://x.bandcamp.com/album/c", "C", "X", "label", "x", false, &["house"]);
    let ids = |q: &str| {
        let q = q.to_string();
        let app = &app;
        async move {
            let (_, page) = app.get(&format!("/harvest/items?{q}")).await;
            let mut v: Vec<i64> = page["items"].as_array().unwrap().iter().map(|i| i["id"].as_i64().unwrap()).collect();
            v.sort();
            v
        }
    };

    assert_eq!(ids("tags=techno").await, vec![a, b]);
    assert_eq!(ids("tags=TECHNO&tags=deep+house").await, vec![a], "every tag must be present");
    assert_eq!(ids("tags=house").await, vec![c], "\"house\" must not also match \"deep house\"");
    assert_eq!(ids("tags=techno&tags=house").await, Vec::<i64>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tag_aggregate_counts_per_folded_key_hottest_first() {
    let app = app().await;
    app.inbox_with("https://x.bandcamp.com/album/a", "A", "X", "label", "x", false, &["Techno", "Deep House"]);
    app.inbox_with("https://x.bandcamp.com/album/b", "B", "X", "wishlist", "y", false, &["techno"]);
    let c = app.inbox_with("https://x.bandcamp.com/album/c", "C", "X", "label", "x", false, &["house"]);
    app.set_state(c, "in_library");

    let (_, tags) = app.get("/harvest/tags").await;
    // state defaults to "new": the in-library row's tag is not offered. `min(tag)` per key.
    assert_eq!(tags, json!([{"tag": "Techno", "count": 2}, {"tag": "Deep House", "count": 1}]));

    let (_, tags) = app.get("/harvest/tags?state=all").await;
    assert_eq!(tags.as_array().unwrap().len(), 3);
    let (_, tags) = app.get("/harvest/tags?source_kind=wishlist").await;
    assert_eq!(tags, json!([{"tag": "techno", "count": 1}]));
    let (_, tags) = app.get("/harvest/tags?source_label=x&limit=1").await;
    assert_eq!(tags.as_array().unwrap().len(), 1);
    assert_eq!(app.get("/harvest/tags?limit=0").await.0, 422);

    // Fan slices go through fan_items.
    let fan = add_fan(&app, "someone");
    let ids: Vec<i64> = app.q(|c| {
        let mut st = c.prepare("SELECT id FROM harvest_items WHERE title = 'B'")?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
    });
    fan_item(&app, fan, ids[0], "wishlist", 0);
    let (_, tags) = app.get(&format!("/harvest/tags?fan_id={fan}&tab=wishlist")).await;
    assert_eq!(tags, json!([{"tag": "techno", "count": 1}]));
    let (_, tags) = app.get(&format!("/harvest/tags?fan_id={fan}&tab=collection")).await;
    assert_eq!(tags, json!([]));
}
