//! Route-level cases that cross agent boundaries (ports of `test_collectors.py` peek routes and the
//! crate cases of `test_tracklist_route.py`), plus a smoke test that every WS2 router mounts together
//! with the generic job routes without a route-overlap panic.

mod harvest_common;

use bc_bandcamp::net::PageKind;
use harvest_common::*;
use serde_json::{Value, json};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

fn fan_page(item_cache: Value) -> String {
    let blob = json!({
        "fan_data": {"fan_id": 77, "username": "alice", "name": "Alice"},
        "collection_data": {"item_count": 12},
        "wishlist_data": {"item_count": 3},
        "hidden_data": {"item_count": 0},
        "item_cache": item_cache,
    });
    format!(r#"<html><body><div id="pagedata" data-blob="{}"></div></body></html>"#, esc(&blob.to_string()))
}

fn entry(url: &str, title: &str, artist: &str) -> Value {
    json!({"item_url": url, "item_title": title, "band_name": artist, "tralbum_type": "a"})
}

#[tokio::test]
async fn peek_shows_a_fan_without_following_them() {
    let app = app().await;
    // The fan page is `https://bandcamp.com/<user>` (no local server can pose as it): seed the page cache.
    let page = fan_page(json!({
        "collection": {"a1": entry("https://a.bandcamp.com/album/owned", "Owned Album", "Owned Artist")},
        "wishlist": {"a2": entry("https://b.bandcamp.com/album/wanted", "Wanted", "B")},
    }));
    app.ctx.client.cache().expect("page cache").put("https://bandcamp.com/alice", PageKind::Fan, &page, None).unwrap();
    app.release("Owned Artist", "Owned Album", None);

    let (st, body) = app.get("/fans/peek?url=alice").await;
    assert_eq!(st, 200, "{body}");
    assert_eq!((body["username"].as_str(), body["display_name"].as_str(), body["bc_fan_id"].as_i64()), (Some("alice"), Some("Alice"), Some(77)));
    assert_eq!((body["collection_count"].as_i64(), body["wishlist_count"].as_i64()), (Some(12), Some(3)));
    assert!(body["followed_id"].is_null());
    assert_eq!(body["collection"][0]["in_library"], true, "matched by (artist, title) without a URL");
    assert_eq!(body["wishlist"][0]["in_library"], false);
    assert_eq!(app.count("fans"), 0, "a peek writes nothing");

    // Once followed, the peek says so.
    app.exec(|tx| {
        tx.execute(
            "INSERT INTO fans (username, url, is_self, created_at) VALUES ('alice', 'https://bandcamp.com/alice', 0, '2026-01-01 00:00:00')",
            [],
        )?;
        Ok(())
    });
    let (_, again) = app.get("/fans/peek?url=alice").await;
    assert!(!again["followed_id"].is_null());
}

#[tokio::test]
async fn peek_items_route_marks_the_library_and_ends_the_list() {
    let fake = fake_bandcamp(&[]).await;
    fake.api(
        "/api/fancollection/1/wishlist_items",
        json!({
            "items": [
                entry("https://a.bandcamp.com/album/owned", "Owned Album", "Owned Artist"),
                entry("https://b.bandcamp.com/album/wanted", "Wanted", "B"),
            ],
            "last_token": "tok9",
            "more_available": false,
        }),
    );
    let app = app_fake(&fake).await;
    app.release("Owned Artist", "Owned Album", None);

    let (st, body) = app.get("/fans/peek/items?url=alice&which=wishlist&cursor=tok2&fan_id=77&count=40").await;
    assert_eq!(st, 200, "{body}");
    let flags: Vec<bool> = body["items"].as_array().unwrap().iter().map(|i| i["in_library"].as_bool().unwrap()).collect();
    assert_eq!(flags, [true, false]);
    assert!(body["cursor"].is_null(), "nothing behind the last page, so nothing to ask for");
    assert_eq!(body["more"], false);
    assert_eq!(app.count("fans"), 0, "reading a list writes nothing");
    assert!(fake.hits().iter().any(|h| h.ends_with("/api/fancollection/1/wishlist_items")));
}

#[tokio::test]
async fn a_crate_queues_flat_and_unwidened() {
    // What the tracklist review table's Download button sends: the two flags are the whole
    // difference between a crate and an ordinary batch of downloads.
    let app = app().await;
    let (st, job) = app
        .post(
            "/downloads",
            json!({
                "urls": ["https://dftd.bandcamp.com/track/2am-extended-mix", "https://syncrophone.bandcamp.com/track/wired"],
                "target_subdir": "Club Room 418-421",
                "label": "Club Room 418-421",
                "single_folder": true,
                "tracks_only": true,
            }),
        )
        .await;
    assert_eq!(st, 200, "{job}");
    let params = app.first_job_params();
    assert_eq!(params["layout"], "flat", "every file lands in the one folder");
    assert_eq!(params["tracks_only"], true, "no track URL is widened to its album");
    assert_eq!(params["target_subdir"], "Club Room 418-421");
    assert!(app.target_dirs().iter().all(|d| d.as_deref() == Some("Club Room 418-421")));
    let kinds: Vec<String> = app.q(|c| {
        let mut st = c.prepare("SELECT DISTINCT url_kind FROM job_items")?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
    });
    assert_eq!(kinds, ["track"]);
}

#[tokio::test]
async fn an_ordinary_batch_is_unaffected() {
    let app = app().await;
    let (st, _) = app.post("/downloads", json!({"urls": ["https://x.bandcamp.com/album/y"], "target_subdir": "Some"})).await;
    assert_eq!(st, 200);
    let params = app.first_job_params();
    assert!(params.get("layout").is_none());
    assert!(params.get("tracks_only").is_none());
}

#[tokio::test]
async fn every_router_mounts_together_and_answers() {
    // `harvest_common::app` nests `api::router` + `JobsService::router` under /api: building it at all
    // proves there is no route-overlap panic; hit one route per module.
    let app = app().await;
    for path in [
        "/jobs",
        "/downloads/disk",
        "/harvest/stats",
        "/harvest/health",
        "/fans",
        "/follows",
        "/harvest/labels",
        "/harvest/enrich",
        "/harvest/labels/sweep",
        "/harvest/favorites/sweep",
        "/follows/sweep",
    ] {
        let (st, body) = app.get(path).await;
        assert!(st == 200, "{path} -> {st} {body}");
    }
}
