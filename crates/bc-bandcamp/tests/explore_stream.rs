//! The stream resolver and the CDN proxy: Range passthrough in both directions, 403 -> re-resolve
//! once, the host allowlist, the in-memory tralbum cache with single-flight, resolving ahead on the
//! reserved lane, and a CDN path that never touches the scraper's token bucket.

mod fakebc;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use bc_bandcamp::api::explore;
use bc_bandcamp::net::Lane;
use bc_bandcamp::sources;
use bc_bandcamp::stream::{StreamConfig, StreamService};
use fakebc::{Resp, Site, TestCtx, explore_tralbum, range_bytes, tralbum_page};

const RELEASE: &str = "/album/grid-failure";

fn audio() -> Vec<u8> {
    (0..300_000usize).map(|i| (i * 31 % 251) as u8).collect()
}

struct Rig {
    bc: Site,
    cdn: Site,
    t: TestCtx,
    svc: Arc<StreamService>,
    app: Router,
    /// How many times the release page was served; also the token the page hands out.
    pages: Arc<AtomicUsize>,
    /// Streams with a token below this answer 403 (their signature "expired").
    valid: Arc<AtomicUsize>,
}

impl Rig {
    fn release_url(&self) -> String {
        self.bc.url(RELEASE)
    }
    fn stream_uri(&self, track: &str) -> String {
        format!("/explore/stream?release={}&track={track}", fakebc::q(&self.release_url()))
    }
}

fn rig(label: &str, tweak: impl FnOnce(&mut StreamConfig)) -> Rig {
    rig_with(label, tweak, |_| {})
}

fn rig_with(label: &str, tweak: impl FnOnce(&mut StreamConfig), client_tweak: impl FnOnce(&mut bc_bandcamp::net::ClientOptions)) -> Rig {
    let bc = Site::new(label);
    let cdn = Site::cdn(label);
    let pages = Arc::new(AtomicUsize::new(0));
    let valid = Arc::new(AtomicUsize::new(1));

    let (p, cdn_base) = (pages.clone(), format!("http://{}", cdn.host));
    bc.route("GET", RELEASE, move |_| {
        let n = p.fetch_add(1, Ordering::SeqCst) + 1;
        let mut t = explore_tralbum(&cdn_base);
        for (i, id) in [(0, 111), (1, 112)] {
            t["trackinfo"][i]["file"]["mp3-128"] = serde_json::json!(format!("{cdn_base}/stream/abc/mp3-128/{id}?token={n}"));
        }
        Resp::html(&tralbum_page(&t))
    });

    let (v, inner) = (valid.clone(), range_bytes(audio(), "audio/mpeg"));
    let cdn_handler = Arc::new(move |hit: &fakebc::Hit| {
        let token: usize = hit.query.strip_prefix("token=").and_then(|t| t.parse().ok()).unwrap_or(0);
        if token < v.load(Ordering::SeqCst) { Resp::status(403) } else { inner(hit) }
    });
    for id in [111, 112] {
        let h = cdn_handler.clone();
        cdn.route("GET", &format!("/stream/abc/mp3-128/{id}"), move |hit| h(hit));
    }

    let t = fakebc::test_ctx_with(&bc, bc.client_with(client_tweak));
    let mut cfg = StreamConfig { allow_plain_http: true, ..StreamConfig::default() };
    tweak(&mut cfg);
    let svc = Arc::new(StreamService::with_config(t.ctx.client.clone(), t.ctx.db.clone(), cfg));
    t.ctx.put(svc.clone());
    let app = explore::router(t.ctx.clone());
    Rig { bc, cdn, t, svc, app, pages, valid }
}

fn header(h: &axum::http::HeaderMap, k: &str) -> String {
    h.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string()
}

// -- the relay --------------------------------------------------------------------------------------

#[tokio::test]
async fn a_whole_stream_is_relayed_with_the_legacy_header_whitelist() {
    let r = rig("whole", |_| {});
    let (status, headers, body) = fakebc::get(&r.app, &r.stream_uri("111")).await;
    assert_eq!(status.as_u16(), 200);
    assert_eq!(body, audio(), "every byte, in order");
    assert_eq!(header(&headers, "content-type"), "audio/mpeg");
    assert_eq!(header(&headers, "content-length"), audio().len().to_string());
    assert_eq!(header(&headers, "cache-control"), "private, max-age=600");
    // Exactly one Accept-Ranges: a duplicate stops Chrome issuing range requests at all.
    assert_eq!(headers.get_all("accept-ranges").iter().count(), 1);
    assert_eq!(header(&headers, "accept-ranges"), "bytes");
    // Only the whitelist is relayed.
    assert!(headers.get("server").is_none() && headers.get("set-cookie").is_none());
}

#[tokio::test]
async fn a_cdn_that_omits_accept_ranges_and_type_gets_the_defaults() {
    let r = rig("defaults", |_| {});
    r.cdn.route("GET", "/stream/abc/mp3-128/111", |_| Resp::ok(b"abc".to_vec()));
    let (status, headers, body) = fakebc::get(&r.app, &r.stream_uri("111")).await;
    assert_eq!((status.as_u16(), body.as_slice()), (200, b"abc".as_slice()));
    assert_eq!(header(&headers, "accept-ranges"), "bytes");
    assert_eq!(header(&headers, "content-type"), "audio/mpeg");
}

#[tokio::test]
async fn range_is_passed_through_both_ways() {
    let r = rig("range", |_| {});
    let req = axum::extract::Request::builder()
        .uri(r.stream_uri("111"))
        .header("range", "bytes=100-199")
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, headers, body) = fakebc::call(&r.app, req).await;
    assert_eq!(status.as_u16(), 206);
    assert_eq!(body, audio()[100..200]);
    assert_eq!(header(&headers, "content-range"), format!("bytes 100-199/{}", audio().len()));
    assert_eq!(header(&headers, "content-length"), "100");
    assert_eq!(header(&headers, "accept-ranges"), "bytes");
    // Forwarded verbatim; rewriting it is how seeking breaks.
    let seen = r.cdn.hits_to("GET", "/stream/abc/mp3-128/111");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].header("range").as_deref(), Some("bytes=100-199"));

    // An open-ended seek near the end.
    let req = axum::extract::Request::builder()
        .uri(r.stream_uri("111"))
        .header("range", "bytes=299990-")
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, headers, body) = fakebc::call(&r.app, req).await;
    assert_eq!(status.as_u16(), 206);
    assert_eq!(body, audio()[299_990..]);
    assert_eq!(header(&headers, "content-range"), "bytes 299990-299999/300000");
}

#[tokio::test]
async fn an_unsatisfiable_range_is_relayed_as_416() {
    let r = rig("range416", |_| {});
    let req = axum::extract::Request::builder()
        .uri(r.stream_uri("111"))
        .header("range", "bytes=999999-")
        .body(axum::body::Body::empty())
        .unwrap();
    let (status, headers, _) = fakebc::call(&r.app, req).await;
    assert_eq!(status.as_u16(), 416);
    assert_eq!(header(&headers, "content-range"), "bytes */300000");
}

#[tokio::test]
async fn a_403_drops_the_signature_re_resolves_once_and_retries() {
    let r = rig("reresolve", |_| {});
    // Resolve (page #1, token 1), then the CDN expires that signature.
    assert!(r.svc.resolve_stream(&r.release_url(), "111", false).await.unwrap().url.ends_with("token=1"));
    r.valid.store(2, Ordering::SeqCst);

    let (status, _, body) = fakebc::get(&r.app, &r.stream_uri("111")).await;
    assert_eq!(status.as_u16(), 200, "seek must not kill playback");
    assert_eq!(body, audio());
    assert_eq!(r.pages.load(Ordering::SeqCst), 2, "the page was re-fetched exactly once");
    let tokens: Vec<_> = r.cdn.hits_to("GET", "/stream/abc/mp3-128/111").iter().map(|h| h.query.clone()).collect();
    assert_eq!(tokens, ["token=1", "token=2"], "the stale signature, then the fresh one");
}

#[tokio::test]
async fn a_403_that_persists_is_a_404_not_a_loop() {
    let r = rig("persist403", |_| {});
    r.valid.store(1000, Ordering::SeqCst); // nothing the page hands out is ever valid
    let (status, body) = fakebc::get_json(&r.app, &r.stream_uri("111")).await;
    assert_eq!(status.as_u16(), 404);
    assert_eq!(body["detail"], "Bandcamp returned 403 for this stream");
    assert_eq!(r.pages.load(Ordering::SeqCst), 2, "one re-resolve, no more");
}

#[tokio::test]
async fn an_upstream_error_is_a_404_naming_the_status() {
    let r = rig("upstream500", |_| {});
    r.cdn.route("GET", "/stream/abc/mp3-128/111", |_| Resp::status(500));
    let (status, body) = fakebc::get_json(&r.app, &r.stream_uri("111")).await;
    assert_eq!(status.as_u16(), 404);
    assert_eq!(body["detail"], "Bandcamp returned 500 for this stream");
}

// -- the allowlist ----------------------------------------------------------------------------------

#[tokio::test]
async fn only_https_bcbits_hosts_are_ever_requested() {
    // Production config: plain http is refused even for a bcbits host.
    let r = rig("allowlist", |c| c.allow_plain_http = false);
    let (status, body) = fakebc::get_json(&r.app, &r.stream_uri("111")).await;
    assert_eq!(status.as_u16(), 400);
    assert_eq!(body["detail"], "stream URL must be an https bcbits.com address");
    assert!(r.cdn.hits().is_empty(), "nothing was sent");

    // A foreign host, whatever the scheme.
    let r = rig("allowlist2", |_| {});
    let evil = Site::on("evil", "example.com");
    evil.route("GET", "/x", |_| Resp::ok(b"pwned".to_vec()));
    r.bc.route("GET", RELEASE, {
        let url = evil.url("/x");
        move |_| {
            let mut t = explore_tralbum("http://x.bcbits.com");
            t["trackinfo"][0]["file"]["mp3-128"] = serde_json::json!(url.clone());
            Resp::html(&tralbum_page(&t))
        }
    });
    let (status, _) = fakebc::get_json(&r.app, &r.stream_uri("111")).await;
    assert_eq!(status.as_u16(), 400);
    assert!(evil.hits().is_empty());

    // `bcbits.com` itself and look-alikes do not pass the suffix test.
    for host in ["bcbits.com", "evilbcbits.com"] {
        let r = rig("allowlist3", |_| {});
        r.bc.route("GET", RELEASE, {
            let url = format!("http://{host}/x");
            move |_| {
                let mut t = explore_tralbum("http://x.bcbits.com");
                t["trackinfo"][0]["file"]["mp3-128"] = serde_json::json!(url.clone());
                Resp::html(&tralbum_page(&t))
            }
        });
        let (status, _) = fakebc::get_json(&r.app, &r.stream_uri("111")).await;
        assert_eq!(status.as_u16(), 400, "{host}");
    }
}

#[tokio::test]
async fn a_redirect_off_the_allowlist_does_not_make_an_open_proxy() {
    let r = rig("redirect", |_| {});
    let evil = Site::on("evil", "example.com");
    evil.route("GET", "/x", |_| Resp::ok(b"pwned".to_vec()));
    let target = evil.url("/x");
    r.cdn.route("GET", "/stream/abc/mp3-128/111", move |_| Resp::status(302).header("location", &target));
    let (status, body) = fakebc::get_json(&r.app, &r.stream_uri("111")).await;
    assert_eq!(status.as_u16(), 400, "{body}");
}

// -- track addressing -----------------------------------------------------------------------------------

#[tokio::test]
async fn tracks_are_addressed_by_id_or_index_and_missing_ones_are_404() {
    let r = rig("addressing", |_| {});
    let by_id = r.svc.resolve_stream(&r.release_url(), "112", false).await.unwrap();
    let by_index = r.svc.resolve_stream(&r.release_url(), "i1", false).await.unwrap();
    assert_eq!(by_id.url, by_index.url);
    assert!(by_id.url.contains("/112?"));

    // Not on the release / no stream at all: 404s through the route.
    for key in ["999", "i9", "113", "i2"] {
        let (status, body) = fakebc::get_json(&r.app, &r.stream_uri(key)).await;
        assert_eq!(status.as_u16(), 404, "{key}: {body}");
    }
    let (_, body) = fakebc::get_json(&r.app, &r.stream_uri("i2")).await;
    assert!(body["detail"].as_str().unwrap().contains("no stream"), "{body}");
}

// -- the cache --------------------------------------------------------------------------------------------

#[tokio::test]
async fn range_requests_never_re_parse_the_page() {
    let r = rig("noreparse", |_| {});
    for range in ["bytes=0-9", "bytes=10-19", "bytes=20-29", "bytes=300-399"] {
        let req = axum::extract::Request::builder().uri(r.stream_uri("111")).header("range", range).body(axum::body::Body::empty()).unwrap();
        assert_eq!(fakebc::call(&r.app, req).await.0.as_u16(), 206);
    }
    assert_eq!(r.pages.load(Ordering::SeqCst), 1, "one page fetch for four ranges");
    assert_eq!(r.svc.cached_releases(), 1);
}

#[tokio::test]
async fn resolving_is_single_flight_per_release() {
    let r = rig("flight", |_| {});
    r.bc.route("GET", RELEASE, {
        let pages = r.pages.clone();
        move |_| {
            std::thread::sleep(Duration::from_millis(150)); // a slow page: everyone piles up
            let n = pages.fetch_add(1, Ordering::SeqCst) + 1;
            let mut t = explore_tralbum("http://x.bcbits.com");
            t["trackinfo"][0]["file"]["mp3-128"] = serde_json::json!(format!("http://x.bcbits.com/s?token={n}"));
            Resp::html(&tralbum_page(&t))
        }
    });
    let url = r.release_url();
    let calls = (0..8).map(|_| r.svc.resolve_stream(&url, "111", false));
    let results = futures::future::join_all(calls).await;
    assert!(results.iter().all(|x| x.is_ok()));
    assert_eq!(r.pages.load(Ordering::SeqCst), 1, "eight callers, one fetch");

    // Concurrent forced refreshes share one re-resolve.
    let calls = (0..6).map(|_| r.svc.resolve_stream(&url, "111", true));
    let urls: Vec<_> = futures::future::join_all(calls).await.into_iter().map(|x| x.unwrap().url).collect();
    assert_eq!(r.pages.load(Ordering::SeqCst), 2, "six refreshes, one more fetch");
    assert!(urls.iter().all(|u| u == &urls[0] && u.ends_with("token=2")));
}

#[tokio::test]
async fn expires_in_counts_down_from_the_ttl() {
    let r = rig("expires", |c| c.ttl = Duration::from_secs(60));
    let a = r.svc.resolve_stream(&r.release_url(), "111", false).await.unwrap();
    assert!(a.expires_in > Duration::from_secs(55) && a.expires_in <= Duration::from_secs(60), "{:?}", a.expires_in);
    tokio::time::sleep(Duration::from_millis(60)).await;
    let b = r.svc.resolve_stream(&r.release_url(), "111", false).await.unwrap();
    assert!(b.expires_in < a.expires_in);
}

#[tokio::test]
async fn an_entry_past_its_ttl_is_fetched_again() {
    let r = rig("ttl", |c| c.ttl = Duration::from_millis(120));
    r.svc.resolve_stream(&r.release_url(), "111", false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The page cache would serve the old HTML (15 min TTL); the in-memory entry is what expired,
    // and a plain resolve re-reads through the page cache, so force the network to see it.
    let again = r.svc.resolve_stream(&r.release_url(), "111", true).await.unwrap();
    assert!(again.url.ends_with("token=2"), "{}", again.url);
}

// -- resolving ahead -------------------------------------------------------------------------------------------

#[tokio::test]
async fn warm_resolves_in_the_background() {
    let r = rig("warm", |_| {});
    r.svc.warm(&r.release_url());
    tokio::time::timeout(Duration::from_secs(5), async {
        while r.svc.cached_releases() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("warm-up finished");
    assert_eq!(r.pages.load(Ordering::SeqCst), 1);
    // Pressing play now costs no page fetch.
    r.svc.resolve_stream(&r.release_url(), "111", false).await.unwrap();
    assert_eq!(r.pages.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_refresher_re_resolves_used_entries_at_80_percent_of_the_ttl() {
    let r = rig("refresher", |c| {
        c.ttl = Duration::from_millis(500);
        c.tick = Duration::from_millis(40);
    });
    let first = r.svc.resolve_stream(&r.release_url(), "111", false).await.unwrap();
    assert!(first.url.ends_with("token=1"));
    r.svc.start_refresher();
    r.svc.start_refresher(); // idempotent

    // Before 80 % (400 ms) nothing is re-fetched.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(r.pages.load(Ordering::SeqCst), 1);

    // After it, the entry is renewed without anyone asking, so the next resolve is a memory hit
    // that already carries the fresh signature.
    tokio::time::timeout(Duration::from_secs(5), async {
        while r.pages.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("refreshed ahead of expiry");
    let renewed = r.svc.resolve_stream(&r.release_url(), "111", false).await.unwrap();
    assert!(renewed.url.ends_with("token=2"), "{}", renewed.url);
    assert!(renewed.expires_in > Duration::from_millis(300), "a fresh signature: {:?}", renewed.expires_in);
}

#[tokio::test]
async fn idle_entries_are_evicted_not_refreshed() {
    let r = rig("idle", |c| {
        c.ttl = Duration::from_millis(100);
        c.active_window = Duration::from_millis(50);
    });
    r.svc.resolve_stream(&r.release_url(), "111", false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    r.svc.refresh_due().await;
    assert_eq!(r.svc.cached_releases(), 0);
    assert_eq!(r.pages.load(Ordering::SeqCst), 1, "no refresh for something nobody is playing");
}

#[tokio::test]
async fn streams_never_queue_behind_the_scrapers_token_bucket() {
    // The normal lane is starved (one token, then ~100 s per request); stream resolution rides the
    // reserved lane and the audio bytes ride the unthrottled CDN client.
    let r = rig_with("lanes", |_| {}, |o| {
        o.rate_per_sec = 0.01;
        o.burst = 1;
    });
    r.bc.page("/album/other", "<html></html>");
    sources::fetch_release(&r.t.ctx.client, &r.bc.url("/album/other")).await.unwrap(); // drains the bucket

    let work = async {
        for track in ["111", "i1", "112"] {
            let (status, _, body) = fakebc::get(&r.app, &r.stream_uri(track)).await;
            assert_eq!(status.as_u16(), 200);
            assert_eq!(body, audio());
        }
        r.svc.resolve_stream(&r.release_url(), "111", true).await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(10), work).await.expect("the reserved lane is never starved by the crawler");
    assert!(r.t.ctx.client.limiter(Lane::Normal).rate() <= 0.01 + f64::EPSILON);
}

// -- the engine API and the release route -------------------------------------------------------------------------

#[tokio::test]
async fn release_tracks_carry_ids_and_proxy_urls_never_signed_ones() {
    let r = rig("tracks", |_| {});
    let out = r.svc.release_tracks(&r.release_url()).await.unwrap();
    assert_eq!(out.title, "Grid Failure");
    assert_eq!(out.tracks.len(), 3);
    assert_eq!(out.tracks[0].bc_track_id, Some(111));
    let proxied = out.tracks[0].stream_url.as_deref().unwrap();
    assert!(proxied.starts_with("/api/explore/stream?release=http%3A%2F%2F") && proxied.ends_with("&track=111"), "{proxied}");
    assert!(!proxied.contains("token"));
    assert_eq!(out.tracks[2].stream_url, None, "an unplayable row has no player URL");
    assert_eq!(out.band_url.as_deref(), Some(format!("http://{}", r.bc.host).as_str()));
    // …and the same fetch warmed the stream cache.
    r.svc.resolve_stream(&r.release_url(), "111", false).await.unwrap();
    assert_eq!(r.pages.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_release_route_writes_tags_back_to_the_inbox_row_and_badges_the_library() {
    let r = rig("relroute", |_| {});
    let url = r.release_url();
    r.bc.route("GET", RELEASE, {
        let cdn = format!("http://{}", r.cdn.host);
        move |_| {
            let page = tralbum_page(&explore_tralbum(&cdn))
                .replace("</body>", r#"<a class="tag">dub techno</a><a class="tag">berlin</a></body>"#);
            Resp::html(&page)
        }
    });
    let key = bc_bandcamp::urls::normalise(&url);
    let (k2, rid) = (key.clone(), {
        r.t.ctx
            .db
            .write(move |tx| {
                tx.execute("INSERT INTO artists(name, name_key, created_at) VALUES ('Somatic', 'somatic', datetime('now'))", [])?;
                let aid = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO releases(title, title_key, kind, artist_id, added_at) VALUES ('Grid Failure', 'grid failure', 'album', ?1, datetime('now'))",
                    [aid],
                )?;
                let rid = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO harvest_items(url, url_kind, state, title, artist_name, tags, in_collection, in_wishlist, is_free_download, is_purchasable, is_preorder, discovered_at) \
                     VALUES (?1, 'album', 'new', 'Grid Failure', 'Somatic', '[]', 0, 0, 0, 1, 0, datetime('now'))",
                    [&key],
                )?;
                Ok(rid)
            })
            .unwrap()
    });

    let (status, body) = fakebc::get_json(&r.app, &format!("/explore/release?url={}", fakebc::q(&url))).await;
    assert_eq!(status.as_u16(), 200, "{body}");
    let tags: Vec<String> = body["tags"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect();
    assert!(!tags.is_empty(), "the page carries tags: {body}");
    assert_eq!(body["in_library"], true, "matched by (artist, title) without a URL");
    assert_eq!(body["library_release_id"], rid);

    let (stored, normalised): (String, i64) = r
        .t
        .ctx
        .db
        .read(move |c| {
            Ok((
                c.query_row("SELECT tags FROM harvest_items WHERE url = ?1", [&k2], |x| x.get::<_, String>(0))?,
                c.query_row("SELECT count(*) FROM harvest_item_tags", [], |x| x.get::<_, i64>(0))?,
            ))
        })
        .unwrap();
    assert_eq!(serde_json::from_str::<Vec<String>>(&stored).unwrap(), tags, "tags written back while in hand");
    assert_eq!(normalised as usize, tags.len(), "and the tag table stays in step");
}
