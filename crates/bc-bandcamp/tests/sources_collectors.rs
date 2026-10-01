//! Port of `test_collectors.py`: who bought a record ("supported by") and a fan at a glance.
//!
//! Source-level cases run `sources::*` against the fake Bandcamp; the route case drives the real
//! `/explore/collectors` router. Not ported here (owned by the fans router agent):
//! `test_peek_shows_a_fan_without_following_them` and
//! `test_peek_items_route_marks_the_library_and_ends_the_list` (they exercise `/fans/peek*`).

mod fakebc;

use bc_bandcamp::HarvestError;
use bc_bandcamp::extract::parse_collectors;
use bc_bandcamp::sources;
use fakebc::{Site, collectors_page, fan_html, fan_item};
use futures::StreamExt;
use serde_json::{Value, json};

fn blob() -> Value {
    json!({
        "thumbs": [
            {"fan_id": 5063709, "username": "dumiovoxo", "name": "Dumi", "image_id": 25165605, "token": "1:1787172552:5063709:0:1:0"},
            {"fan_id": 3575534, "username": "exm0nster", "name": "ExM", "image_id": 33616909, "token": "1:1787172175:3575534:0:1:0"},
        ],
        "more_thumbs_available": true,
        "reviews": [
            {"fan_id": 13644698, "username": "sinnatro", "name": "Sinnatro", "why": "This hipnotize my soul",
             "image_id": 46230767, "token": "1:1784630191:13644698:1:1:0", "fav_track_title": "Tempest"},
        ],
        "more_reviews_available": false,
    })
}

#[test]
fn parse_collectors_reads_the_page_blob() {
    let found = parse_collectors(&collectors_page(Some(&blob()), None));
    assert_eq!(found.thumbs.iter().map(|c| c.username.as_str()).collect::<Vec<_>>(), ["dumiovoxo", "exm0nster"]);
    assert_eq!(found.thumbs[0].name, "Dumi");
    assert_eq!(found.thumbs[0].image_id, Some(25165605));
    assert_eq!(found.thumbs[0].url(), "https://bandcamp.com/dumiovoxo");
    assert!(found.more_thumbs && !found.more_reviews);
    let review = &found.reviews[0];
    assert_eq!(review.why.as_deref(), Some("This hipnotize my soul"));
    assert_eq!(review.fav_track.as_deref(), Some("Tempest"));
    assert_eq!(review.token.as_deref(), Some("1:1784630191:13644698:1:1:0"));
}

#[test]
fn a_page_with_no_buyers_parses_to_nothing() {
    let found = parse_collectors(&collectors_page(None, None));
    assert!(found.thumbs.is_empty() && found.reviews.is_empty() && !found.more_thumbs);
}

fn album_site(label: &str, blob: &Value) -> Site {
    let site = Site::new(label);
    site.page("/album/d-rin", &collectors_page(Some(blob), None));
    site
}

#[tokio::test]
async fn fetch_collectors_pages_on_with_the_last_token_up_to_the_limit() {
    let site = album_site("coll", &blob());
    site.post_json(
        "/api/tralbumcollectors/2/thumbs",
        json!({"results": [
            {"fan_id": 1, "username": "third", "name": "Third", "image_id": 3, "token": "t3"},
            {"fan_id": 2, "username": "fourth", "name": "Fourth", "image_id": 4, "token": "t4"},
        ], "more_available": true}),
    );
    let client = site.client();
    let (_release, found) = sources::fetch_collectors(&client, &site.url("/album/d-rin"), 4).await.unwrap();
    assert_eq!(
        found.thumbs.iter().map(|c| c.username.as_str()).collect::<Vec<_>>(),
        ["dumiovoxo", "exm0nster", "third", "fourth"]
    );
    assert!(found.more_thumbs, "the API said there were more and the cap was hit");
    let posts = site.hits_to("POST", "/api/tralbumcollectors/2/thumbs");
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].json(),
        json!({"tralbum_type": "a", "tralbum_id": 503240863, "token": "1:1787172175:3575534:0:1:0", "count": 2})
    );
}

#[tokio::test]
async fn fetch_collectors_within_the_page_costs_no_api_call() {
    let site = album_site("collpage", &blob());
    let client = site.client();
    let (_release, found) = sources::fetch_collectors(&client, &site.url("/album/d-rin"), 2).await.unwrap();
    assert_eq!(found.thumbs.len(), 2);
    assert!(site.hits().iter().all(|h| h.method == "GET"), "no API call");
}

// -- a fan at a glance ------------------------------------------------------------------------------

#[tokio::test]
async fn peek_items_first_page_comes_off_the_fan_page() {
    let site = Site::new("peek1");
    site.page("/alice", &fan_html(&[fan_item("one", "One"), fan_item("two", "Two")], &[], Some("1779387967:324009377:a::"), (3, 1)));
    let client = site.client();
    let page = sources::peek_fan_items(&client, &site.url("/alice"), "collection", None, None, 1).await.unwrap();
    assert_eq!(page.rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(), ["One", "Two"], "the embedded batch comes back whole");
    assert!(site.hits().iter().all(|h| h.method == "GET"), "and costs no API call");
    assert_eq!(page.cursor.as_deref(), Some("1779387967:324009377:a::"), "the tab's token asks for the batch behind it");
    assert_eq!((page.more, page.total), (true, Some(3)), "three in the list, two shown");
}

#[tokio::test]
async fn peek_items_pages_on_with_the_cursor() {
    let site = Site::new("peek2");
    site.page("/alice", &fan_html(&[], &[], Some("x"), (3, 1)));
    site.post_json(
        "/api/fancollection/1/wishlist_items",
        json!({"items": [
            {"item_url": "https://a.bandcamp.com/album/three", "item_title": "Three", "band_name": "A Band", "tralbum_type": "a"},
            {"item_url": "https://a.bandcamp.com/album/three", "item_title": "Three", "band_name": "A Band", "tralbum_type": "a"},
        ], "last_token": "tok3", "more_available": false}),
    );
    let client = site.client();
    let page = sources::peek_fan_items(&client, &site.url("/alice"), "wishlist", Some(77), Some("tok2"), 60).await.unwrap();
    assert_eq!(page.rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(), ["Three"], "a record listed twice is one card");
    assert_eq!((page.cursor.as_deref(), page.more, page.total), (Some("tok3"), false, None));
    let posts = site.hits_to("POST", "/api/fancollection/1/wishlist_items");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].json(), json!({"fan_id": 77, "older_than_token": "tok2", "count": 60}));
    assert_eq!(site.count("/alice"), 0, "a known fan id skips re-reading the page");
}

#[tokio::test]
async fn peek_items_without_a_token_falls_back_to_the_newest() {
    // An embedded item with no cursor of its own still leaves a way onwards -- from the newest,
    // which the reader dedupes by URL.
    let site = Site::new("peek3");
    site.page("/alice", &fan_html(&[fan_item("one", "One")], &[], None, (3, 1)));
    let client = site.client();
    let page = sources::peek_fan_items(&client, &site.url("/alice"), "collection", None, None, 60).await.unwrap();
    assert!(page.cursor.as_deref().is_some_and(|c| c.ends_with("::a::")));
    assert!(page.more);
}

// -- lists Bandcamp will not serve ---------------------------------------------------------------------

/// A fan page that reads, over a list API that answers `{"error": true}` -- what Bandcamp sends
/// for a wishlist set to private.
fn refusing_site(label: &str, counts: (i64, i64)) -> Site {
    let site = Site::new(label);
    site.page("/alice", &fan_html(&[], &[], Some("x"), counts));
    for path in ["/api/fancollection/1/wishlist_items", "/api/fancollection/1/collection_items"] {
        site.post_json(path, json!({"error": true}));
    }
    site
}

#[tokio::test]
async fn a_list_the_page_says_is_empty_is_never_asked_for() {
    // Some accounts answer a page-of-my-empty-wishlist with a bare error, so the walk that
    // already knows the list is empty must not ask.
    let site = refusing_site("empty", (3, 0));
    let client = site.client();
    let events: Vec<_> =
        sources::harvest_collection(client, None, Some(site.url("/alice")), "wishlist".into(), 500, 60, None).collect().await;
    assert!(events.is_empty());
    assert!(site.hits().iter().all(|h| h.method == "GET"));
}

#[tokio::test]
async fn a_refused_list_reads_as_unavailable_not_as_a_bare_error() {
    let site = refusing_site("refused", (3, 2));
    let client = site.client();
    let events: Vec<_> =
        sources::harvest_collection(client, None, Some(site.url("/alice")), "wishlist".into(), 500, 60, None).collect().await;
    let err = events.into_iter().find_map(Result::err).expect("the walk fails");
    assert!(matches!(err, HarvestError::ListUnavailable(_)), "{err:?}");
    assert!(err.to_string().contains("wishlist"));
}

#[tokio::test]
async fn peeking_a_refused_list_says_so() {
    let site = refusing_site("peekrefused", (3, 2));
    let client = site.client();
    let err = sources::peek_fan_items(&client, &site.url("/alice"), "wishlist", Some(77), Some("tok2"), 60).await.unwrap_err();
    assert!(matches!(err, HarvestError::ListUnavailable(_)), "{err:?}");
}

// -- the route ------------------------------------------------------------------------------------------

#[tokio::test]
async fn collectors_route_marks_who_is_already_followed() {
    let site = album_site("collroute", &blob());
    // The default limit (80) is above the two embedded buyers, so the route pages on once; an
    // empty page ends it.
    site.post_json("/api/tralbumcollectors/2/thumbs", json!({"results": [], "more_available": false}));
    let t = fakebc::test_ctx(&site);
    t.ctx
        .db
        .write(|tx| {
            tx.execute("INSERT INTO fans(username, url, is_self, created_at) VALUES ('Sinnatro', 'https://bandcamp.com/sinnatro', 0, datetime('now'))", [])?;
            Ok(())
        })
        .unwrap();
    let app = bc_bandcamp::api::explore::router(t.ctx.clone());

    let (status, body) = fakebc::get_json(&app, &format!("/explore/collectors?url={}", fakebc::q(&site.url("/album/d-rin")))).await;
    assert_eq!(status.as_u16(), 200, "{body}");
    assert_eq!(body["more"], json!(true));
    // Reviewers first, each buyer once.
    let names: Vec<_> = body["supporters"].as_array().unwrap().iter().map(|s| s["username"].as_str().unwrap()).collect();
    assert_eq!(names, ["sinnatro", "dumiovoxo", "exm0nster"]);
    let sinnatro = &body["supporters"][0];
    assert!(!sinnatro["followed_id"].is_null());
    assert_eq!(sinnatro["why"], "This hipnotize my soul");
    assert_eq!(sinnatro["fav_track"], "Tempest");
    assert_eq!(sinnatro["image_url"], "https://f4.bcbits.com/img/0046230767_50.jpg");
    assert!(body["supporters"][1]["followed_id"].is_null());
    let reviews: Vec<_> = body["reviews"].as_array().unwrap().iter().map(|s| s["username"].as_str().unwrap()).collect();
    assert_eq!(reviews, ["sinnatro"]);
}
