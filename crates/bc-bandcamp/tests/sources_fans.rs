//! `sources::*` fan functions against the fake Bandcamp: `whoami`, `probe_fan`, `peek_fan`,
//! `harvest_collection` (paging, `older_than_token` built from *now*, dedupe, limits) and the
//! shapes a list error arrives in.

mod fakebc;

use std::time::{SystemTime, UNIX_EPOCH};

use bc_bandcamp::HarvestError;
use bc_bandcamp::sources::{self, HarvestEvent};
use fakebc::{Resp, Site, fan_html, fan_item};
use futures::StreamExt;
use serde_json::{Value, json};

const COLLECTION: &str = "/api/fancollection/1/collection_items";
const WISHLIST: &str = "/api/fancollection/1/wishlist_items";
const HIDDEN: &str = "/api/fancollection/1/hidden_items";

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

async fn walk(c: bc_bandcamp::net::BandcampClient, which: &str, fan_id: Option<i64>, url: Option<String>, limit: usize, page: usize, since: Option<String>) -> Vec<Result<HarvestEvent, HarvestError>> {
    sources::harvest_collection(c, fan_id, url, which.into(), limit, page, since).collect().await
}

fn item(slug: &str) -> Value {
    json!({"item_url": format!("https://a.bandcamp.com/album/{slug}"), "item_title": slug, "band_name": "A Band", "tralbum_type": "a"})
}

// -- the token -------------------------------------------------------------------------------------------

#[test]
fn newest_token_is_built_from_now() {
    // `<purchase_ts>:<tralbum_id>:<type>:<index>:` for the *newest* page -- the fan page's own
    // `last_token` points at the oldest item and returns zero rows.
    let token = sources::newest_token();
    let (ts, rest) = token.split_once("::a::").expect("shape `{ts}::a::`");
    assert_eq!(rest, "");
    assert!((now() - ts.parse::<i64>().unwrap()).abs() <= 5);
}

// -- harvest_collection ----------------------------------------------------------------------------------------

#[tokio::test]
async fn a_walk_starts_from_now_and_pages_until_the_list_ends() {
    let site = Site::new("f-walk");
    site.route("POST", COLLECTION, |hit| {
        let j = hit.json();
        let token = j["older_than_token"].as_str().unwrap_or("").to_string();
        if token.ends_with("::a::") {
            Resp::json(&json!({"items": [item("one"), item("two")], "last_token": "tok-2", "more_available": true}))
        } else {
            Resp::json(&json!({"items": [item("two"), item("three")], "last_token": "tok-3", "more_available": false}))
        }
    });
    let c = site.client();
    let events: Vec<_> = walk(c, "collection", Some(77), None, 100, 2, None).await.into_iter().map(Result::unwrap).collect();
    let titles: Vec<_> = events.iter().map(|e| e.release.as_ref().unwrap().title.clone()).collect();
    assert_eq!(titles, ["one", "two", "three"], "a record on two pages is one event");
    assert_eq!(events[2].seen, 3);

    let posts = site.hits_to("POST", COLLECTION);
    assert_eq!(posts.len(), 2);
    let first = posts[0].json();
    let (ts, _) = first["older_than_token"].as_str().unwrap().split_once("::a::").unwrap();
    assert!((now() - ts.parse::<i64>().unwrap()).abs() <= 5, "built from now, not from the fan page");
    assert_eq!((first["fan_id"].clone(), first["count"].clone()), (json!(77), json!(2)));
    assert_eq!(posts[1].json()["older_than_token"], "tok-2");
    assert_eq!(site.count("/alice"), 0, "a known fan id never reads the fan page");
}

#[tokio::test]
async fn a_walk_honours_the_limit_a_resume_token_and_a_stalled_token() {
    let site = Site::new("f-limit");
    site.route("POST", WISHLIST, |hit| {
        let tok = hit.json()["older_than_token"].as_str().unwrap_or("").to_string();
        // Hands the same token back: the list cannot be paged further.
        Resp::json(&json!({"items": [item(&format!("w-{tok}"))], "last_token": tok, "more_available": true}))
    });
    let c = site.client();
    let events = walk(c.clone(), "wishlist", Some(1), None, 100, 10, Some("resume-here".into())).await;
    assert_eq!(events.len(), 1, "a repeated token ends the walk");
    assert_eq!(site.hits_to("POST", WISHLIST)[0].json()["older_than_token"], "resume-here", "since_token replaces the from-now one");

    let serial = std::sync::atomic::AtomicUsize::new(0);
    site.route("POST", HIDDEN, move |hit| {
        let n = hit.json()["count"].as_u64().unwrap();
        let start = serial.fetch_add(n as usize, std::sync::atomic::Ordering::SeqCst);
        let items: Vec<Value> = (0..n as usize).map(|i| item(&format!("h{}", start + i))).collect();
        Resp::json(&json!({"items": items, "last_token": format!("t{start}"), "more_available": true}))
    });
    let events = walk(c, "hidden", Some(1), None, 5, 3, None).await;
    assert_eq!(events.len(), 5);
    let counts: Vec<_> = site.hits_to("POST", HIDDEN).iter().map(|h| h.json()["count"].clone()).collect();
    assert_eq!(counts, [json!(3), json!(2)], "the last page asks for what is left");
}

#[tokio::test]
async fn a_walk_spends_the_embedded_batch_before_any_request() {
    let site = Site::new("f-embedded");
    site.page("/alice", &fan_html(&[fan_item("e1", "E1"), fan_item("e2", "E2"), fan_item("e3", "E3")], &[], Some("x"), (3, 0)));
    site.route("POST", COLLECTION, |_| Resp::json(&json!({"items": [], "more_available": false})));
    let c = site.client();
    let events: Vec<_> = walk(c, "collection", None, Some(site.url("/alice")), 2, 60, None).await.into_iter().map(Result::unwrap).collect();
    assert_eq!(events.iter().map(|e| e.release.as_ref().unwrap().title.clone()).collect::<Vec<_>>(), ["E1", "E2"]);
    assert_eq!(events[0].total, Some(3), "the page's own count");
    assert!(site.hits_to("POST", COLLECTION).is_empty(), "the limit was met by the page alone");
}

#[tokio::test]
async fn a_walk_needs_a_fan_id_or_a_url() {
    let site = Site::new("f-noargs");
    let events = walk(site.client(), "collection", None, None, 10, 10, None).await;
    assert!(matches!(events[0], Err(HarvestError::Other(_))));
}

#[tokio::test]
async fn list_errors_arrive_in_three_shapes() {
    let site = Site::new("f-shapes");
    let c = site.client();
    let first_err = |events: Vec<Result<HarvestEvent, HarvestError>>| events.into_iter().find_map(Result::err).expect("an error");

    // `{"error": true}` and nothing else: a list Bandcamp will not show.
    site.post_json(WISHLIST, json!({"error": true}));
    assert!(matches!(first_err(walk(c.clone(), "wishlist", Some(1), None, 10, 10, None).await), HarvestError::ListUnavailable(_)));
    // …a stated reason is a plain API error…
    site.post_json(WISHLIST, json!({"error": true, "error_message": "internal trouble"}));
    assert!(matches!(first_err(walk(c.clone(), "wishlist", Some(1), None, 10, 10, None).await), HarvestError::Api { unspecified: false, .. }));
    // …and "must be logged in" means the cookie is gone.
    site.post_json(WISHLIST, json!({"error": true, "error_message": "You must be logged in to do that"}));
    assert!(matches!(first_err(walk(c, "wishlist", Some(1), None, 10, 10, None).await), HarvestError::IdentityExpired(_)));
}

// -- whoami / probe / peek -----------------------------------------------------------------------------------------

#[tokio::test]
async fn whoami_needs_a_cookie_and_reads_the_summary() {
    let site = Site::new("f-whoami");
    site.route("GET", "/api/fan/2/collection_summary", |hit| {
        if hit.header("cookie").is_some() {
            Resp::json(&json!({"fan_id": 0, "collection_summary": {"fan_id": 77, "username": "alice", "url": "https://bandcamp.com/alice"}}))
        } else {
            Resp::json(&json!({"error": true, "error_message": "must be logged in"}))
        }
    });
    let anonymous = site.client();
    let e = sources::whoami(&anonymous).await.unwrap_err();
    assert!(matches!(e, HarvestError::IdentityExpired(ref m) if m.contains("no identity cookie")), "{e:?}");
    assert!(site.hits().is_empty(), "no cookie, no request");

    // Cookies only travel to Bandcamp hosts; the fake pretends to be one.
    let c = site.client_with(|o| o.cookie = Some("identity=SECRET".into()));
    let who = sources::whoami(&c).await.unwrap();
    assert_eq!((who.fan_id, who.username.as_deref(), who.url.as_deref()), (Some(77), Some("alice"), Some("https://bandcamp.com/alice")));

    // A cookie the server no longer honours: the 200 + "must be logged in" shape.
    site.route("GET", "/api/fan/2/collection_summary", |_| Resp::json(&json!({"error": true, "error_message": "must be logged in"})));
    assert!(matches!(sources::whoami(&c).await.unwrap_err(), HarvestError::IdentityExpired(_)));
}

#[tokio::test]
async fn probe_fan_reports_the_tab_asked_for() {
    let site = Site::new("f-probe");
    site.page("/alice", &fan_html(&[], &[], Some("x"), (412, 88)));
    let c = site.client();
    let p = sources::probe_fan(&c, &site.url("/alice")).await.unwrap();
    assert_eq!((p.kind.as_str(), p.total_hint, p.requires_auth, p.label.as_str()), ("collection", Some(412), false, "alice-collection"));
    assert_eq!(p.params["fan_id"], json!(77));
    let p = sources::probe_fan(&c, &site.url("/alice/wishlist")).await.unwrap();
    assert_eq!((p.kind.as_str(), p.total_hint), ("wishlist", Some(88)), "the tab lives in the URL; only the base page is fetched");
    assert_eq!(site.count("/alice"), 1);
    assert_eq!(site.count("/alice/wishlist"), 0);
    let p = sources::probe_fan(&c, &format!("{}?tab=hidden", site.url("/alice"))).await.unwrap();
    assert!(p.requires_auth, "the hidden tab always needs a cookie");
}

#[tokio::test]
async fn peek_fan_reads_both_shelves_off_one_page() {
    let site = Site::new("f-peekfan");
    site.page("/alice", &fan_html(&[fan_item("one", "One")], &[fan_item("w", "Wanted"), fan_item("w", "Wanted")], Some("x"), (12, 3)));
    let (page, owned, wished) = sources::peek_fan(&site.client(), &site.url("/alice")).await.unwrap();
    assert_eq!((page.fan_id, page.username.as_str(), page.collection_count, page.wishlist_count), (77, "alice", 12, 3));
    assert_eq!((owned.len(), wished.len()), (1, 1), "a record listed twice is one card");
}

#[test]
fn collection_items_become_shallow_releases() {
    let r = sources::release_from_collection_item(&json!({
        "url_hints": {"custom_domain": null, "subdomain": "somatic", "slug": "grid-failure", "item_type": "a"},
        "album_title": "Grid Failure", "band_name": "Somatic", "tralbum_type": "a", "tralbum_id": 55, "item_art_id": 4000000001i64,
        "label": "Hyperdub", "is_preorder": true, "is_purchasable": false,
    }))
    .expect("built from url_hints");
    assert_eq!((r.title.as_str(), r.artist_name.as_str(), r.bc_item_id, r.label_name.as_deref()), ("Grid Failure", "Somatic", Some(55), Some("Hyperdub")));
    assert_eq!((r.is_preorder, r.is_purchasable, r.item_type.as_str()), (true, false, "album"));
    assert!(sources::release_from_collection_item(&json!({"item_title": "no url"})).is_none());
    let t = sources::release_from_collection_item(&json!({"item_url": "https://a.bandcamp.com/track/t", "item_title": "T", "tralbum_type": "t"})).unwrap();
    assert_eq!((t.item_type.as_str(), t.is_purchasable), ("track", true));
}
