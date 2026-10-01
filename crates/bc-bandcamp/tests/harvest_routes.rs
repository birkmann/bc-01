//! Route-level behaviours of `api/harvest.rs` that need a Bandcamp to talk to: resolve, identity
//! (the cookie is never returned), health, label resolution as a job -- against the local fake.

mod harvest_common;

use harvest_common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve_answers_without_the_network_for_lists_albums_and_discover() {
    let app = app().await;
    let r = |input: &str| {
        let input = input.to_string();
        let app = &app;
        async move { app.post("/harvest/resolve", json!({"input": input})).await }
    };

    assert_eq!(r("   ").await.0, 400);
    let (_, one) = r("https://a.bandcamp.com/album/x?from=y").await;
    assert_eq!((one["kind"].as_str(), one["url"].as_str(), one["total_hint"].as_i64()), (Some("url_list"), Some("https://a.bandcamp.com/album/x"), Some(1)));
    assert_eq!(one["detail"], "single album");
    let (_, list) = r("https://a.bandcamp.com/album/x\nhttps://a.bandcamp.com/track/y\njunk").await;
    assert_eq!((list["kind"].as_str(), list["total_hint"].as_i64(), list["label"].as_str()), (Some("url_list"), Some(2), Some("pasted list")));
    assert_eq!(list["detail"], "2 album/track URLs");
    let (_, d) = r("https://bandcamp.com/discover/techno").await;
    assert_eq!(d["kind"], "discover");
    assert_eq!(d["params"]["tags"], json!(["techno"]));
    assert_eq!(r("https://bandcamp.com/login").await.0, 400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resolve_probes_an_artist_page_with_one_fetch() {
    let fake = fake_bandcamp(&["x.bandcamp.com", "gone.bandcamp.com"]).await;
    let app = app_fake(&fake).await;
    fake.page("x.bandcamp.com", "/music", &music_page(&[("/album/a", "X", "A"), ("/album/b", "X", "B")]));

    let origin = fake.origin("x.bandcamp.com");
    let (status, body) = app.post("/harvest/resolve", json!({"input": origin})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!((body["kind"].as_str(), body["total_hint"].as_i64()), (Some("artist"), Some(2)));
    assert_eq!(fake.hits(), vec!["x.bandcamp.com/music"]);

    // An unreachable page is a 400 that says so.
    let (status, body) = app.post("/harvest/resolve", json!({"input": fake.origin("gone.bandcamp.com")})).await;
    assert_eq!(status, 400, "{body}");
    assert!(body["detail"].as_str().unwrap().contains("could not reach that source"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identity_is_stored_verified_and_never_returned() {
    let fake = fake_bandcamp(&[]).await;
    let app = app_fake(&fake).await;
    let secret = "identity=SECRETVALUE123456; client_id=abc";

    let (_, none) = app.get("/harvest/identity").await;
    assert_eq!((none["configured"].as_bool(), none["detail"].as_str()), (Some(false), Some("No Bandcamp cookie stored.")));

    // Not a cookie with an identity in it.
    assert_eq!(app.put("/harvest/identity", json!({"cookie": "client_id=abc"})).await.0, 400);

    // Rejected by Bandcamp (HTTP 200 with an error body, as it really does it).
    fake.api("/api/fan/2/collection_summary", json!({"error": true, "error_message": "must be logged in"}));
    let (status, body) = app.put("/harvest/identity", json!({"cookie": format!("Cookie: {secret}")})).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!((body["configured"].as_bool(), body["valid"].as_bool()), (Some(true), Some(false)));
    assert_eq!(body["fingerprint"], "\u{2026}123456");

    // Accepted.
    fake.api("/api/fan/2/collection_summary", json!({"fan_id": 7, "collection_summary": {"fan_id": 7, "username": "me", "url": "https://bandcamp.com/me"}}));
    let (_, body) = app.put("/harvest/identity", json!({"cookie": secret})).await;
    assert_eq!((body["valid"].as_bool(), body["username"].as_str(), body["fan_id"].as_i64()), (Some(true), Some("me"), Some(7)));
    assert!(!body.to_string().contains("SECRETVALUE"), "the cookie must never be returned");
    let (_, again) = app.get("/harvest/identity").await;
    assert_eq!(again["valid"], json!(true));
    assert!(!again.to_string().contains("SECRETVALUE"));
    // The client sent it; the file fallback holds it with 0600.
    assert_eq!(fake.cookies().last().map(String::as_str), Some(secret));
    let path = app.ctx.cookies.cookie_path();
    assert!(path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    // Forgotten: the file goes and the client stops sending it.
    assert_eq!(app.delete("/harvest/identity").await.0, 204);
    assert!(!path.exists());
    assert_eq!(app.get("/harvest/identity").await.1["configured"], json!(false));
    app.ctx.reload_cookie();
    assert!(!fake.client.has_cookie());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn health_reports_limiter_cache_counters_and_extract_tiers() {
    let fake = fake_bandcamp(&["x.bandcamp.com", "gone.bandcamp.com"]).await;
    let app = app_fake(&fake).await;
    fake.page("x.bandcamp.com", "/music", &music_page(&[]));
    for (i, tier) in ["blob", "blob", "css"].into_iter().enumerate() {
        let tier = tier.to_string();
        let url = format!("https://x.bandcamp.com/album/t{i}");
        app.exec(move |t| {
            t.execute(
                "INSERT INTO harvest_items(url, url_kind, state, title, artist_name, tags, extract_tier, in_collection, in_wishlist, \
                 is_free_download, is_purchasable, is_preorder, discovered_at) VALUES (?1,'album','new','T','A','[]',?2,0,0,0,1,0,datetime('now'))",
                bc_db::rusqlite::params![url, tier],
            )?;
            Ok(())
        });
    }
    app.post("/harvest/resolve", json!({"input": fake.origin("x.bandcamp.com")})).await;
    app.post("/harvest/resolve", json!({"input": fake.origin("gone.bandcamp.com")})).await;

    let (status, h) = app.get("/harvest/health").await;
    assert_eq!(status, 200);
    assert_eq!(h["extract_tiers"], json!({"blob": 2, "css": 1}));
    assert_eq!(h["blob_ratio"], json!(0.667));
    assert!(h["rate_limit"]["current_rate"].as_f64().unwrap() > 0.0);
    assert_eq!(h["rate_limit"]["penalised_for_s"], json!(0.0));
    assert!(h["requests"].as_i64().unwrap() >= 2);
    assert!(h["errors"].is_number() && h["cache_hits"].is_number());
    assert!(h["last_errors"].as_array().unwrap().len() <= 5);
    assert!(h["cache"]["entries"].is_number());
}

/// `POST /harvest/labels/resolve` is a `202` + a `sweep` job; a second press while it runs is a 400.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn label_resolution_is_a_job_and_cannot_be_doubled() {
    let app = app().await;
    let dcg = "https://detroitclassicgallery.bandcamp.com";
    app.album_item(&format!("{dcg}/album/a"), "E110101", "A", None, "in_library");
    app.album_item(&format!("{dcg}/album/b"), "Sol Ortega", "B", None, "in_library");
    // A page source that is slow, so the second press arrives while the first runs.
    struct Slow;
    #[async_trait::async_trait]
    impl bc_bandcamp::harvest::labels::PageSource for Slow {
        async fn page(&self, _u: &str, _k: bc_bandcamp::net::PageKind, _t: Option<std::time::Duration>) -> bc_bandcamp::error::Result<String> {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            Ok(album_html("E110101", "A", "Detroit Classic Gallery"))
        }
        async fn search(&self, _q: &str, _k: &str, _l: usize) -> bc_bandcamp::error::Result<Vec<bc_bandcamp::sources::SearchHit>> {
            Ok(vec![])
        }
    }
    app.ctx.expect::<bc_bandcamp::harvest::labels::LabelResolver>().set_source(std::sync::Arc::new(Slow));

    let (status, started) = app.post_empty("/harvest/labels/resolve").await;
    assert_eq!((status, started["running"].as_bool()), (202, Some(true)));
    assert_eq!(app.post_empty("/harvest/labels/resolve").await.0, 400);
    let done = app.poll("/harvest/labels", |s| s["phase"] == "done").await;
    assert_eq!((done["resolved"].as_i64(), done["labelled"].as_i64()), (Some(1), Some(2)));
    assert_eq!(app.get("/jobs?kind=sweep").await.1["total"], 1);
}
