//! Port of `test_tracklist_route.py`: uploading tracklists and matching their rows over HTTP.
//!
//! Not ported here: `test_a_crate_queues_flat_and_unwidened` and
//! `test_an_ordinary_batch_is_unaffected` exercise `POST /downloads` (`single_folder`,
//! `tracks_only`), which the download-route agent owns.

mod fakebc;

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use fakebc::{Resp, Site};
use serde_json::{Value, json};

const CLUBROOM_418: &str = "#,Start,End,Artist,Title,Label
1,00:06:03,00:10:16,Luke Alessi,Sex Machine,Coffee Cola
2,00:22:29,00:29:10,Boogie Vice,2AM (Extended Mix),DFTD
3,00:34:26,00:40:36,Roman Flügel,Tippex In My Eye (Dance Mix),Phantasy Sound
";

const CLUBROOM_421: &str = "#,Start,End,Artist,Title,Label
1,00:01:12,00:04:30,Boogie Vice,2AM (Extended Mix),DFTD
2,00:14:34,00:21:21,Dj Honesty,Wired,Syncrophone
";

fn rig(label: &str) -> (Site, fakebc::TestCtx, Router) {
    let site = Site::new(label);
    let t = fakebc::test_ctx(&site);
    let app = bc_bandcamp::api::tracklists::router(t.ctx.clone());
    (site, t, app)
}

/// `(filename, bytes)` parts of a multipart `files` upload.
fn upload(name: &str, text: &str) -> (String, Vec<u8>) {
    (name.to_string(), text.as_bytes().to_vec())
}

/// What a real export sends: UTF-8 read as cp1252, written back as UTF-8.
fn mangled(name: &str, text: &str) -> (String, Vec<u8>) {
    let reread: String = text
        .bytes()
        .map(|b| {
            assert!(!(0x80..0xA0).contains(&b), "fixture uses only latin-1-compatible cp1252 bytes");
            b as char
        })
        .collect();
    (name.to_string(), reread.into_bytes())
}

async fn parse(app: &Router, files: Vec<(String, Vec<u8>)>) -> (u16, Value) {
    let boundary = "----bcrusttestboundary";
    let mut body: Vec<u8> = Vec::new();
    for (name, data) in files {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{name}\"\r\nContent-Type: text/csv\r\n\r\n").as_bytes());
        body.extend_from_slice(&data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let req = Request::builder()
        .method("POST")
        .uri("/tracklists/parse")
        .header("content-type", format!("multipart/form-data; boundary={boundary}"))
        .body(Body::from(body))
        .unwrap();
    let (status, _, bytes) = fakebc::call(app, req).await;
    (status.as_u16(), serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

// -- parsing an upload --------------------------------------------------------------------------------

#[tokio::test]
async fn several_files_merge_into_one_deduped_crate() {
    let (_s, _t, app) = rig("tl-merge");
    let (status, body) = parse(
        &app,
        vec![mangled("clubroom-418-with-anja-schneider.csv", CLUBROOM_418), upload("club-room-no-421.csv", CLUBROOM_421)],
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let rows: Vec<_> = body["files"].as_array().unwrap().iter().map(|f| f["rows"].clone()).collect();
    assert_eq!(rows, [json!(3), json!(2)]);
    assert_eq!(body["duplicates"], 1, "2AM is played in both sets");
    assert_eq!(body["rows"].as_array().unwrap().len(), 4);
    assert_eq!(body["rows"][2]["artist"], "Roman Flügel", "the mojibake must be repaired");
    assert_eq!(body["suggested_title"], "", "two files name nothing");
}

#[tokio::test]
async fn one_file_prefills_the_crate_name() {
    let (_s, _t, app) = rig("tl-name");
    let (_, body) = parse(&app, vec![upload("clubroom-418-with-anja-schneider.csv", CLUBROOM_418)]).await;
    assert_eq!(body["suggested_title"], "clubroom-418-with-anja-schneider");
}

#[tokio::test]
async fn one_unreadable_file_does_not_lose_the_others() {
    let (_s, _t, app) = rig("tl-partial");
    let (status, body) = parse(&app, vec![upload("notes.csv", "Start,End\n1,2\n"), upload("clubroom-418.csv", CLUBROOM_418)]).await;
    assert_eq!(status, 200, "{body}");
    assert!(!body["files"][0]["error"].is_null());
    assert!(body["files"][1]["error"].is_null());
    assert_eq!(body["rows"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn an_upload_of_nothing_readable_is_a_bad_request() {
    let (_s, _t, app) = rig("tl-unreadable");
    let (status, body) = parse(&app, vec![upload("notes.csv", "Start,End\n1,2\n")]).await;
    assert_eq!(status, 400);
    assert!(!body["files"].as_array().expect("the per-file reasons ride along").is_empty());
}

#[tokio::test]
async fn more_rows_than_one_pass_can_search_is_refused() {
    // 500 rows is already twenty minutes of rate-limited requests.
    let (_s, _t, app) = rig("tl-big");
    let rows: Vec<String> = (0..600).map(|i| format!("Artist {i},Title {i}")).collect();
    let (status, body) = parse(&app, vec![upload("big.csv", &format!("Artist,Title\n{}\n", rows.join("\n")))]).await;
    assert_eq!(status, 400);
    assert!(body["detail"].as_str().unwrap().contains("smaller batches"), "{body}");
}

#[tokio::test]
async fn no_files_and_too_many_files_are_bad_requests() {
    let (_s, _t, app) = rig("tl-limits");
    assert_eq!(parse(&app, vec![]).await.0, 400);
    let many: Vec<_> = (0..21).map(|i| upload(&format!("{i}.csv"), "Artist,Title\na,b\n")).collect();
    let (status, body) = parse(&app, many).await;
    assert_eq!(status, 400);
    assert!(body["detail"].as_str().unwrap().contains("Too many files"));
}

// -- matching a row ---------------------------------------------------------------------------------------

/// Answers track/all searches with `hits`, nothing for the other filters; records the queries.
fn fake_search(site: &Site, hits: Vec<Value>) -> Arc<Mutex<Vec<(String, String)>>> {
    let asked = Arc::new(Mutex::new(Vec::new()));
    let a = asked.clone();
    site.route("POST", "/api/bcsearch_public_api/1/autocomplete_elastic", move |hit| {
        let j = hit.json();
        let (text, filter) = (j["search_text"].as_str().unwrap_or("").to_string(), j["search_filter"].as_str().unwrap_or("").to_string());
        a.lock().unwrap().push((text, filter.clone()));
        let results = if matches!(filter.as_str(), "t" | "") { hits.clone() } else { vec![] };
        Resp::json(&json!({"auto": {"results": results}}))
    });
    asked
}

#[tokio::test]
async fn a_matched_row_comes_back_ranked_and_pre_selected() {
    let (site, _t, app) = rig("tl-match");
    fake_search(
        &site,
        vec![
            json!({"type": "t", "name": "2AM (Extended Mix)", "band_name": "Boogie Vice", "url": "https://dftd.bandcamp.com/track/2am-extended-mix", "id": 1}),
            json!({"type": "t", "name": "4AM", "band_name": "Nobody", "url": "https://x.bandcamp.com/track/4am", "id": 2}),
        ],
    );
    let (status, body) =
        fakebc::post_json(&app, "/tracklists/match", &json!({"artist": "Boogie Vice", "title": "2AM (Extended Mix)", "label": "DFTD"})).await;
    assert_eq!(status.as_u16(), 200, "{body}");
    assert_eq!(body["best_index"], 0);
    assert_eq!(body["candidates"][0]["url"], "https://dftd.bandcamp.com/track/2am-extended-mix");
    assert_eq!(body["candidates"][0]["tier"], "strong");
    // Flattened onto the hit, like the legacy CandidateOut(SearchHitOut).
    assert_eq!(body["candidates"][0]["kind"], "track");
    assert_eq!(body["candidates"][0]["in_library"], false);
    assert_eq!(body["candidates"][1]["tier"], "weak");
    assert_eq!(body["searches"], 1);
}

#[tokio::test]
async fn candidates_carry_the_library_badges() {
    let (site, t, app) = rig("tl-badge");
    fakebc::owned_release(&t.ctx.db, "Boogie Vice", "2AM (Extended Mix)");
    fakebc::blacklist_url(&t.ctx.db, "https://x.bandcamp.com/track/thrown-away");
    fake_search(
        &site,
        vec![
            json!({"type": "t", "name": "2AM (Extended Mix)", "band_name": "Boogie Vice", "url": "https://dftd.bandcamp.com/track/2am-extended-mix", "id": 1}),
            json!({"type": "t", "name": "Thrown Away", "band_name": "X", "url": "https://x.bandcamp.com/track/thrown-away", "id": 2}),
        ],
    );
    let (_, body) = fakebc::post_json(&app, "/tracklists/match", &json!({"artist": "Boogie Vice", "title": "2AM (Extended Mix)"})).await;
    let c = body["candidates"].as_array().unwrap();
    let by = |name: &str| c.iter().find(|x| x["name"] == name).unwrap().clone();
    assert_eq!(by("2AM (Extended Mix)")["in_library"], true, "a crate may re-download, but the user is told");
    assert_eq!((by("Thrown Away")["in_library"].clone(), by("Thrown Away")["blacklisted"].clone()), (json!(false), json!(true)));
}

#[tokio::test]
async fn a_row_nothing_answers_returns_no_candidates() {
    let (site, _t, app) = rig("tl-none");
    fake_search(&site, vec![]);
    let (status, body) = fakebc::post_json(&app, "/tracklists/match", &json!({"artist": "Nobody At All", "title": "Nothing"})).await;
    assert_eq!(status.as_u16(), 200, "{body}");
    assert_eq!(body, json!({"query": "Nobody At All Nothing", "candidates": [], "best_index": null, "searches": 2}));
}

#[tokio::test]
async fn a_hand_typed_query_is_searched_but_the_row_is_still_what_scores() {
    // The point of the override: a row looked up by hand comes back with a real confidence,
    // comparable to every other row's, rather than a placeholder the UI would have to invent.
    let (site, _t, app) = rig("tl-query");
    let asked = fake_search(&site, vec![json!({"type": "t", "name": "Lifetimes", "band_name": "Slam", "url": "https://soma.bandcamp.com/track/lifetimes", "id": 1})]);
    let (_, body) = fakebc::post_json(&app, "/tracklists/match", &json!({"artist": "Slam", "title": "Lifetimes", "query": "soma lifetimes 1997"})).await;
    let asked: Vec<String> = asked.lock().unwrap().iter().map(|(q, _)| q.clone()).collect();
    assert_eq!(asked, ["soma lifetimes 1997"], "the guessed query must not be used as well");
    assert_eq!(body["query"], "soma lifetimes 1997");
    assert_eq!(body["candidates"][0]["score"], 1.0, "scored against the row, not the query");
}

#[tokio::test]
async fn an_empty_row_is_rejected_before_a_request_is_spent() {
    let (site, _t, app) = rig("tl-empty");
    let (status, _) = fakebc::post_json(&app, "/tracklists/match", &json!({"artist": "", "title": "x"})).await;
    assert_eq!(status.as_u16(), 422);
    let (status, _) = fakebc::post_json(&app, "/tracklists/match", &json!({"artist": "a", "title": "x", "query": ""})).await;
    assert_eq!(status.as_u16(), 422);
    assert!(site.hits().is_empty(), "no search was spent");
}

#[tokio::test]
async fn a_failing_search_is_translated() {
    let (site, _t, app) = rig("tl-fail");
    site.post_json("/api/bcsearch_public_api/1/autocomplete_elastic", json!({"__api_special__": "exception", "error_type": "Endpoints::MissingParamError"}));
    let (status, _) = fakebc::post_json(&app, "/tracklists/match", &json!({"artist": "a", "title": "x"})).await;
    assert_eq!(status.as_u16(), 400);
}
