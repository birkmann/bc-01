//! `test_artist_expansion.py`: a queued artist or label page becomes the releases it lists.
//!
//! `bandcamp-dl` acts on `/album/` and `/track/` URLs only -- it skips anything else in silence, so
//! a queued `/music` page produced no audio, exited 0, and failed "No audio files were produced."
//! three identical times. The worker expands the page instead, and never hands the downloader a
//! URL it cannot act on.
//!
//! The Python tests monkeypatched `fetch_band_page`; here the same seam is the handler's
//! `BandFetcher` hook (`DownloadDeps::with_band_fetcher`); the last section runs the default path
//! too -- the real `BandcampClient` on `/music`, against a local axum server posing as
//! `lemos.bandcamp.com` (a DNS override whose address carries the port).

mod worker_common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bc_bandcamp::HarvestError;
use bc_bandcamp::download::bcdl::is_downloadable;
use bc_bandcamp::download::worker::{BandFetcher, DownloadDeps, DownloadHandler};
use bc_bandcamp::extract::{BandProfile, GridItem, Tier};
use bc_bandcamp::net::BandcampClient;
use bc_bandcamp::sources::BandPage;
use bc_jobs::{HandlerOutcome, NewItem, NewJob};
use worker_common::*;

const MUSIC_URL: &str = "https://lemos.bandcamp.com/music";
const ROOT_URL: &str = "https://lemos.bandcamp.com";
const ALBUM_URL: &str = "https://lemos.bandcamp.com/album/kontrol-lemos-06";

fn page(titles_and_slugs: &[(&str, &str)], is_label: bool, tier: Tier, host: &str) -> BandPage {
    BandPage {
        profile: BandProfile { url: ROOT_URL.into(), name: "Lemos".into(), is_label, ..Default::default() },
        releases: titles_and_slugs
            .iter()
            .map(|(title, slug)| GridItem {
                page_url: format!("https://{host}/album/{slug}"),
                title: (*title).into(),
                artist: "Lemos".into(),
                item_type: "album".into(),
                ..Default::default()
            })
            .collect(),
        tier,
        ..Default::default()
    }
}

fn simple(titles_and_slugs: &[(&str, &str)]) -> BandPage {
    page(titles_and_slugs, false, Tier::Blob, "lemos.bandcamp.com")
}

fn stub_page(p: BandPage) -> BandFetcher {
    Arc::new(move |_url| {
        let p = p.clone();
        Box::pin(async move { Ok(p) })
    })
}

fn stub_raise(make: fn() -> HarvestError) -> BandFetcher {
    Arc::new(move |_url| Box::pin(async move { Err(make()) }))
}

/// A handler whose band-page fetch is `fetcher`; `with_client` = the worker has a Bandcamp client.
fn handler(env: &Env, fetcher: Option<BandFetcher>, with_client: bool) -> DownloadHandler {
    let client = with_client.then(|| BandcampClient::new(Default::default()));
    let mut deps = DownloadDeps::new(env.db.clone(), env.bus.clone(), env.cfg.clone(), client)
        .with_downloader(bcdl("success"))
        .with_library(Arc::new(FakeLibrary::new(env.db.clone())));
    if let Some(f) = fetcher {
        deps = deps.with_band_fetcher(f);
    }
    DownloadHandler::new(Arc::new(deps))
}

/// A job with `items` (the first is the one claimed): returns the claimed item id.
fn job_claimed(env: &Env, items: Vec<NewItem>) -> (String, i64) {
    let job = env.store().create_job(NewJob::new("download", items).label("test")).expect("job");
    (job.id, env.claim().expect("claim"))
}

fn artist_item(env: &Env, url: &str) -> i64 {
    job_claimed(env, vec![NewItem::url(url, "artist")]).1
}

fn children_urls(env: &Env, parent: i64) -> Vec<(i64, String, String, String)> {
    env.db
        .read(move |c| {
            let mut st = c.prepare("SELECT seq, url, url_kind, status FROM job_items WHERE id != ?1 ORDER BY seq")?;
            Ok(st
                .query_map([parent], |r| Ok((r.get(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .expect("children")
}

// -- expansion ------------------------------------------------------------------------------------

#[tokio::test]
async fn a_music_page_becomes_one_item_per_release() {
    let env = Env::new();
    let h = handler(&env, Some(stub_page(simple(&[("Kontrol", "kontrol-lemos-06"), ("Brazil VA", "brazil-va")]))), true);
    let id = artist_item(&env, MUSIC_URL);

    let outcome = env.run_item(&h, id).await;

    assert!(matches!(outcome, HandlerOutcome::Handled), "{outcome:?}");
    let item = env.item(id);
    assert_eq!(item.status, "skipped", "the page itself downloads nothing");
    assert_eq!(item.message.as_deref(), Some("Lemos: expanded into 2 releases"));
    let job = env.job(&item.job_id);
    assert_eq!((job.total, job.skipped), (3, 1));
    let kids = children_urls(&env, id);
    assert_eq!(
        kids.iter().map(|k| k.1.as_str()).collect::<Vec<_>>(),
        ["https://lemos.bandcamp.com/album/kontrol-lemos-06", "https://lemos.bandcamp.com/album/brazil-va"]
    );
    assert!(kids.iter().all(|k| k.3 == "pending" && k.2 == "album"));
    assert_eq!(kids.iter().map(|k| k.0).collect::<Vec<_>>(), [1, 2], "appended after everything queued");
}

#[tokio::test]
async fn a_bare_band_root_expands_too() {
    // `bandcamp-dl` ignores the root exactly as it ignores `/music`.
    let env = Env::new();
    let h = handler(&env, Some(stub_page(simple(&[("Kontrol", "kontrol-lemos-06")]))), true);
    let id = artist_item(&env, ROOT_URL);

    env.run_item(&h, id).await;

    assert_eq!(env.item(id).message.as_deref(), Some("Lemos: expanded into 1 release"));
}

#[tokio::test]
async fn the_job_stays_claimable_when_the_page_was_its_only_item() {
    // The releases must exist before the stand-in stops being outstanding. Settling first would
    // let the job close, and the claim query only takes from a queued or running one -- the whole
    // discography would be inserted into a completed job and never run.
    let env = Env::new();
    let h = handler(&env, Some(stub_page(simple(&[("Kontrol", "kontrol-lemos-06")]))), true);
    let id = artist_item(&env, MUSIC_URL);

    env.run_item(&h, id).await;

    assert_ne!(env.job(&env.item(id).job_id).status, "completed");
    let claimed = env.claim().expect("the expanded release is claimable");
    assert_eq!(env.item(claimed).url.as_deref(), Some(ALBUM_URL));
}

#[tokio::test]
async fn a_release_already_in_the_job_is_not_queued_twice() {
    // Pasting the album and its band page must not download it twice.
    let env = Env::new();
    let h = handler(&env, Some(stub_page(simple(&[("Kontrol", "kontrol-lemos-06"), ("Brazil VA", "brazil-va")]))), true);
    // The same album, spelt the way a real paste spells it: trailing slash, upper-case host.
    let (_job, id) = job_claimed(
        &env,
        vec![
            NewItem::url(MUSIC_URL, "artist"),
            NewItem::url("https://LEMOS.bandcamp.com/album/kontrol-lemos-06/", "album"),
        ],
    );

    env.run_item(&h, id).await;

    let urls: Vec<String> = env
        .db
        .read(|c| {
            let mut st = c.prepare("SELECT url FROM job_items WHERE status = 'pending' ORDER BY seq")?;
            Ok(st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
        })
        .expect("urls");
    assert_eq!(urls, ["https://LEMOS.bandcamp.com/album/kontrol-lemos-06/", "https://lemos.bandcamp.com/album/brazil-va"]);
}

fn job_params(env: &Env, job_id: &str) -> serde_json::Value {
    serde_json::from_str(&env.job(job_id).params).expect("params json")
}

#[tokio::test]
async fn a_label_page_files_a_single_page_job_under_the_label() {
    let env = Env::new();
    let h = handler(&env, Some(stub_page(page(&[("Kontrol", "kontrol-lemos-06")], true, Tier::Blob, "lemos.bandcamp.com"))), true);
    let (job_id, id) = job_claimed(&env, vec![NewItem::url(MUSIC_URL, "artist")]);

    env.run_item(&h, id).await;

    let params = job_params(&env, &job_id);
    assert_eq!(params["label_name"], "Lemos");
    assert_eq!(params["label_url"], ROOT_URL);
}

#[tokio::test]
async fn a_label_page_pasted_beside_other_urls_labels_nothing() {
    // Params are shared by the job, so a mixed paste must not be mislabelled.
    let env = Env::new();
    let h = handler(&env, Some(stub_page(page(&[("Kontrol", "kontrol-lemos-06")], true, Tier::Blob, "lemos.bandcamp.com"))), true);
    let (job_id, id) = job_claimed(
        &env,
        vec![NewItem::url(MUSIC_URL, "artist"), NewItem::url("https://other.bandcamp.com/album/unrelated", "album")],
    );

    env.run_item(&h, id).await;

    assert!(job_params(&env, &job_id).get("label_name").is_none());
}

#[tokio::test]
async fn a_truncated_grid_still_expands() {
    // A small band renders its whole catalogue and still reports `css`. Refusing the releases we
    // can see, or crying truncation at the user on a complete read, would both be worse than
    // expanding what the page gave us.
    let env = Env::new();
    let h = handler(&env, Some(stub_page(page(&[("Kontrol", "kontrol-lemos-06")], false, Tier::Css, "lemos.bandcamp.com"))), true);
    let id = artist_item(&env, MUSIC_URL);

    env.run_item(&h, id).await;

    assert_eq!(env.item(id).message.as_deref(), Some("Lemos: expanded into 1 release"));
}

#[tokio::test]
async fn a_label_roster_spanning_hosts_expands_whole() {
    // Half of a real label's grid lives on its artists' own subdomains.
    let env = Env::new();
    let mut p = page(&[("Artifacts", "artifacts-lemos-04")], false, Tier::Blob, "fadeface.bandcamp.com");
    p.releases.push(GridItem { page_url: ALBUM_URL.into(), title: "Kontrol".into(), artist: "Lemos".into(), item_type: "album".into(), ..Default::default() });
    let h = handler(&env, Some(stub_page(p)), true);
    let id = artist_item(&env, MUSIC_URL);

    env.run_item(&h, id).await;

    let urls: std::collections::HashSet<String> = children_urls(&env, id).into_iter().map(|k| k.1).collect();
    assert_eq!(urls, ["https://fadeface.bandcamp.com/album/artifacts-lemos-04".to_string(), ALBUM_URL.to_string()].into_iter().collect());
}

// -- when there is nothing to expand into -----------------------------------------------------------

#[tokio::test]
async fn a_page_with_no_releases_is_skipped_not_retried() {
    // The page answered, and the answer was "nothing here" -- not a failure.
    let env = Env::new();
    let h = handler(&env, Some(stub_page(simple(&[]))), true);
    let id = artist_item(&env, MUSIC_URL);

    env.run_item(&h, id).await;

    let item = env.item(id);
    assert_eq!(item.status, "skipped");
    assert_eq!(item.message.as_deref(), Some("Lemos lists no downloadable releases"));
}

#[tokio::test]
async fn a_network_failure_is_retried() {
    let env = Env::new();
    let h = handler(&env, Some(stub_raise(|| HarvestError::other("connection reset"))), true);
    let id = artist_item(&env, MUSIC_URL);

    env.run_item(&h, id).await;

    let item = env.item(id);
    assert_eq!(item.status, "pending", "a bad moment deserves another attempt");
    assert_eq!(item.error_class.as_deref(), Some("network"));
}

#[tokio::test]
async fn a_missing_page_is_not_retried() {
    // Three identical retries against a deterministic 404 is the bug being fixed.
    let env = Env::new();
    let h = handler(&env, Some(stub_raise(|| HarvestError::other("not found (404)"))), true);
    let id = artist_item(&env, MUSIC_URL);

    env.run_item(&h, id).await;

    let item = env.item(id);
    assert_eq!(item.status, "failed");
    assert_eq!(item.error_class.as_deref(), Some("not_found"));
}

#[tokio::test]
async fn an_expired_identity_says_so_once() {
    let env = Env::new();
    let h = handler(&env, Some(stub_raise(|| HarvestError::IdentityExpired("cookie rejected".into()))), true);
    let id = artist_item(&env, MUSIC_URL);

    env.run_item(&h, id).await;

    let item = env.item(id);
    assert_eq!(item.status, "failed");
    assert_eq!(item.error_class.as_deref(), Some("identity"));
    assert!(item.last_error.unwrap_or_default().contains("Settings"));
}

#[tokio::test]
async fn a_clientless_worker_fails_the_item_rather_than_downloading() {
    let env = Env::new();
    let h = handler(&env, None, false);
    let id = artist_item(&env, MUSIC_URL);

    let outcome = env.run_item(&h, id).await;

    assert!(matches!(outcome, HandlerOutcome::Failed { .. }));
    let item = env.item(id);
    assert_eq!(item.status, "failed");
    assert_eq!(item.error_class.as_deref(), Some("no_client"));
}

// -- what is left alone -------------------------------------------------------------------------------

#[tokio::test]
async fn release_urls_are_not_expanded_and_cost_no_request() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c2 = calls.clone();
    let fetcher: BandFetcher = Arc::new(move |_| {
        c2.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(HarvestError::other("a release URL must not be fetched")) })
    });
    let env = Env::new();
    // The downloader fails fast (silent) so the item runs through to the end without a band fetch.
    let h = DownloadHandler::new(Arc::new(
        DownloadDeps::new(env.db.clone(), env.bus.clone(), env.cfg.clone(), Some(BandcampClient::new(Default::default())))
            .with_downloader(bcdl("silent_fail"))
            .with_library(Arc::new(FakeLibrary::new(env.db.clone())))
            .with_band_fetcher(fetcher),
    ));
    for url in [ALBUM_URL, "https://lemos.bandcamp.com/track/one"] {
        let (_job, id) = job_claimed(&env, vec![NewItem::url(url, "album")]);
        env.run_item(&h, id).await;
        assert_ne!(env.item(id).error_class.as_deref(), Some("not_found"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// -- the invariant: bandcamp-dl only ever sees a release URL ---------------------------------------

#[test]
fn is_downloadable_matches_what_bandcamp_dl_acts_on() {
    assert!(is_downloadable(ALBUM_URL));
    assert!(is_downloadable("https://lemos.bandcamp.com/track/one"));
    // Custom domains are release URLs too -- the path shape is the signal.
    assert!(is_downloadable("https://music.example.com/album/thing"));
    assert!(!is_downloadable(MUSIC_URL));
    assert!(!is_downloadable(ROOT_URL));
    assert!(!is_downloadable("https://lemos.bandcamp.com/artists"));
    assert!(!is_downloadable("https://bandcamp.com/someone"));
}

// -- the whole item path, claim to settle ------------------------------------------------------------

#[tokio::test]
async fn running_a_band_page_item_expands_instead_of_spawning_bandcamp_dl() {
    // Covers the wiring the method tests cannot: that the expansion runs early enough that no
    // staging directory, preflight or subprocess happens for an item which does not stand for a
    // release. The downloader is a binary that does not exist: it must never be started.
    let env = Env::new();
    let h = DownloadHandler::new(Arc::new(
        DownloadDeps::new(env.db.clone(), env.bus.clone(), env.cfg.clone(), Some(BandcampClient::new(Default::default())))
            .with_downloader(bcdl_with("/nonexistent/bandcamp-dl", "success"))
            .with_library(Arc::new(FakeLibrary::new(env.db.clone())))
            .with_band_fetcher(stub_page(simple(&[("Kontrol", "kontrol-lemos-06")]))),
    ));
    let id = artist_item(&env, MUSIC_URL);

    env.run_item(&h, id).await;

    let item = env.item(id);
    assert_eq!(item.status, "skipped");
    assert_eq!(item.message.as_deref(), Some("Lemos: expanded into 1 release"));
    assert_eq!(env.job(&item.job_id).total, 2);
    assert!(!env.downloads().join(".staging").exists(), "no staging dir for a page");
}

#[tokio::test]
async fn running_an_unexpandable_page_says_why_once() {
    // A URL neither expandable nor downloadable is explained, not retried. This is the case the old
    // code turned into "No audio files were produced." three times over.
    let env = Env::new();
    let h = handler(&env, None, true);
    let (_job, id) = job_claimed(&env, vec![NewItem::url("https://lemos.bandcamp.com/artists", "artist")]);

    env.run_item(&h, id).await;

    let item = env.item(id);
    assert_eq!(item.status, "failed", "no amount of retrying changes the answer");
    assert_eq!(item.error_class.as_deref(), Some("unsupported_url"));
    assert!(item.last_error.unwrap_or_default().contains("/album/ and /track/"));
}

// -- widening a track to its album (the one page fetch before a download) ------------------------------

fn resolver(f: fn(&str) -> bc_bandcamp::Result<String>) -> bc_bandcamp::download::worker::AlbumResolver {
    Arc::new(move |u| {
        let r = f(&u);
        Box::pin(async move { r })
    })
}

fn widening_handler(env: &Env, r: bc_bandcamp::download::worker::AlbumResolver) -> DownloadHandler {
    DownloadHandler::new(Arc::new(
        DownloadDeps::new(env.db.clone(), env.bus.clone(), env.cfg.clone(), None)
            .with_downloader(bcdl("success"))
            .with_library(Arc::new(FakeLibrary::new(env.db.clone())))
            .with_album_resolver(r),
    ))
}

const TRACK: &str = "https://somatic.bandcamp.com/track/opening-drift";

#[tokio::test]
async fn a_track_url_is_widened_to_its_album_and_written_back() {
    let env = Env::new();
    let h = widening_handler(&env, resolver(|_| Ok(GRID.to_string())));
    let (_job, id) = job_claimed(&env, vec![NewItem::url(TRACK, "track")]);

    env.run_item(&h, id).await;

    let item = env.item(id);
    assert_eq!(item.status, "done");
    assert_eq!(item.url.as_deref(), Some(GRID), "the queue shows the record it actually fetched");
    assert_eq!(item.url_kind.as_deref(), Some("album"));
    assert_eq!(count_files(&env.downloads().join("Somatic").join("Grid Failure"), ".mp3"), 4);
}

#[tokio::test]
async fn tracks_only_jobs_keep_their_track_urls() {
    let env = Env::new();
    let h = widening_handler(&env, resolver(|_| panic!("a tracks_only job must not resolve albums")));
    env.store()
        .create_job(NewJob::new("download", vec![NewItem::url(TRACK, "track")]).params(serde_json::json!({"tracks_only": true})))
        .expect("job");
    let id = env.claim().expect("claim");

    env.run_item(&h, id).await;

    assert_eq!(env.item(id).url.as_deref(), Some(TRACK));
}

#[tokio::test]
async fn a_failed_widening_falls_back_to_the_track() {
    let env = Env::new();
    let h = widening_handler(&env, resolver(|_| Err(HarvestError::other("boom"))));
    let (_job, id) = job_claimed(&env, vec![NewItem::url(TRACK, "track")]);

    env.run_item(&h, id).await;

    assert_eq!(env.item(id).url.as_deref(), Some(TRACK), "still beats failing the item");
    assert_eq!(env.item(id).status, "done");
}

// -- the default path: the real client against a local "Bandcamp" ------------------------------------------

mod real_client {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::{Html, IntoResponse};
    use bc_bandcamp::net::ClientOptions;
    use serde_json::json;

    /// `http`, so the DNS override (which carries the local port) applies.
    const HTTP_MUSIC: &str = "http://lemos.bandcamp.com/music";

    fn esc(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    fn music_page(is_label: bool, slugs: &[&str]) -> String {
        let band = json!({"id": 7, "name": "Lemos", "is_label": is_label});
        let items: Vec<serde_json::Value> = slugs
            .iter()
            .enumerate()
            .map(|(i, slug)| json!({"id": 10 + i, "band_id": 7, "artist": "Lemos", "title": slug, "type": "album",
                                    "page_url": format!("/album/{slug}?label=1&tab=music")}))
            .collect();
        format!(
            r#"<html><body><script data-band="{}"></script><ol id="music-grid" data-client-items="{}"></ol></body></html>"#,
            esc(&band.to_string()),
            esc(&serde_json::Value::Array(items).to_string())
        )
    }

    async fn handler(env: &Env, status: StatusCode, html: String) -> DownloadHandler {
        let app = Router::new().fallback(move || {
            let html = html.clone();
            async move { if status == StatusCode::OK { Html(html).into_response() } else { status.into_response() } }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = BandcampClient::new(ClientOptions {
            resolve: vec![("lemos.bandcamp.com".into(), addr)],
            rate_per_sec: 1000.0,
            burst: 1000,
            backoff_scale: 0.001,
            ..Default::default()
        });
        DownloadHandler::new(Arc::new(
            DownloadDeps::new(env.db.clone(), env.bus.clone(), env.cfg.clone(), Some(client))
                .with_downloader(bcdl_with("/nonexistent/bandcamp-dl", "success"))
                .with_library(Arc::new(FakeLibrary::new(env.db.clone()))),
        ))
    }

    #[tokio::test]
    async fn a_label_music_page_is_fetched_parsed_and_expanded() {
        let env = Env::new();
        let h = handler(&env, StatusCode::OK, music_page(true, &["kontrol-lemos-06", "brazil-va"])).await;
        let (job_id, id) = job_claimed(&env, vec![NewItem::url(HTTP_MUSIC, "artist")]);

        env.run_item(&h, id).await;

        let item = env.item(id);
        assert_eq!(item.status, "skipped");
        assert_eq!(item.message.as_deref(), Some("Lemos: expanded into 2 releases"));
        let urls: Vec<String> = children_urls(&env, id).into_iter().map(|k| k.1).collect();
        assert_eq!(urls, ["http://lemos.bandcamp.com/album/kontrol-lemos-06", "http://lemos.bandcamp.com/album/brazil-va"]);
        // A single-page label job files its releases under the label.
        let params = job_params(&env, &job_id);
        assert_eq!((params["label_name"].as_str(), params["label_url"].as_str()), (Some("Lemos"), Some("http://lemos.bandcamp.com")));
    }

    #[tokio::test]
    async fn a_404_page_fails_not_found_without_retries() {
        let env = Env::new();
        let h = handler(&env, StatusCode::NOT_FOUND, String::new()).await;
        let id = artist_item(&env, HTTP_MUSIC);

        env.run_item(&h, id).await;

        let item = env.item(id);
        assert_eq!((item.status.as_str(), item.error_class.as_deref()), ("failed", Some("not_found")));
    }

    #[tokio::test]
    async fn a_server_error_is_a_retried_network_failure() {
        let env = Env::new();
        let h = handler(&env, StatusCode::INTERNAL_SERVER_ERROR, String::new()).await;
        let id = artist_item(&env, HTTP_MUSIC);

        env.run_item(&h, id).await;

        let item = env.item(id);
        assert_eq!((item.status.as_str(), item.error_class.as_deref()), ("pending", Some("network")));
    }
}
