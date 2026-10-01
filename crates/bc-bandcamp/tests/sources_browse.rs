//! `sources::*` browse functions against a local fake Bandcamp: search, discover, releases,
//! recommendations, band pages, artist/URL-list harvests, album resolution and the error shapes
//! (Bandcamp reports failures as HTTP 200 with an error body).

mod fakebc;

use bc_bandcamp::HarvestError;
use bc_bandcamp::extract::Tier;
use bc_bandcamp::net::{GetOpts, PageKind};
use bc_bandcamp::sources::{self, DiscoverQuery, HarvestEvent};
use fakebc::{Resp, Site, discover_row, grid_entry, music_page, tralbum_page};
use futures::StreamExt;
use serde_json::{Value, json};

const SEARCH: &str = "/api/bcsearch_public_api/1/autocomplete_elastic";
const DISCOVER: &str = "/api/discover/1/discover_web";

async fn collect(stream: sources::EventStream) -> Vec<Result<HarvestEvent, HarvestError>> {
    stream.collect().await
}

fn release_page(title: &str, minimum: f64) -> String {
    tralbum_page(&json!({"for the curious": "x", "item_type": "album", "id": 9, "artist": "Artist",
        "current": {"title": title, "band_id": 2, "minimum_price": minimum}, "trackinfo": []}))
}

// -- search ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn search_orders_by_bandcamp_applies_the_limit_and_skips_junk() {
    let site = Site::new("s-search");
    site.post_json(
        SEARCH,
        json!({"auto": {"results": [
            {"type": "b", "name": "First", "url": "https://first.bandcamp.com", "id": 1},
            "not an object",
            {"type": "a", "name": "No Url At All"},
            {"type": "a", "name": "Second", "band_name": "B", "url": "https://b.bandcamp.com/album/second?from=x", "id": 2},
            {"type": "t", "name": "Third", "band_name": "B", "item_url_root": "https://b.bandcamp.com", "item_url_path": "/track/third", "id": 3},
            {"type": "f", "name": "Fourth", "url": "https://bandcamp.com/four"},
        ]}}),
    );
    let c = site.client();
    let hits = sources::search(&c, "  query  ", "", 4).await.unwrap();
    assert_eq!(hits.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["First", "Second"], "the limit applies to the raw entries, then junk drops");
    assert_eq!(hits[1].url, "https://b.bandcamp.com/album/second", "tracking query stripped");
    let hits = sources::search(&c, "query", "t", 40).await.unwrap();
    assert_eq!(hits.iter().map(|h| (h.kind.as_str(), h.name.as_str())).collect::<Vec<_>>(), [("artist", "First"), ("album", "Second"), ("track", "Third"), ("fan", "Fourth")]);
    assert_eq!(hits[2].url, "https://b.bandcamp.com/track/third");
    assert_eq!((hits[0].band_id, hits[1].item_id, hits[1].band_id), (Some(1), Some(2), None));

    let posts = site.hits_to("POST", SEARCH);
    assert_eq!(posts[0].json(), json!({"search_text": "query", "search_filter": "", "full_page": false, "fan_id": null}), "trimmed text");
    assert_eq!(posts[1].json()["search_filter"], "t");
}

#[tokio::test]
async fn an_empty_query_costs_no_request_and_search_never_sends_the_cookie() {
    let site = Site::new("s-cookie");
    site.post_json(SEARCH, json!({"auto": {"results": []}}));
    let c = site.client_with(|o| o.cookie = Some("identity=SECRET".into()));
    assert!(sources::search(&c, "   ", "", 10).await.unwrap().is_empty());
    assert!(site.hits().is_empty());
    sources::search(&c, "x", "", 10).await.unwrap();
    // Sending the fan cookie would tie every keystroke in the browse box to the account.
    assert!(site.hits()[0].header("cookie").is_none());
    assert!(sources::search(&c, "x", "", 10).await.unwrap().is_empty(), "no `auto` key reads as no results");
}

#[tokio::test]
async fn http_200_error_bodies_become_typed_errors() {
    let site = Site::new("s-errors");
    let c = site.client();
    // The rule the whole fetch layer is built on: errors arrive as 200.
    site.post_json(DISCOVER, json!({"__api_special__": "exception", "error_type": "Endpoints::MissingParamError"}));
    let e = sources::discover_page(&c, &DiscoverQuery::new(), "*", 10).await.unwrap_err();
    assert!(matches!(&e, HarvestError::Api { error_type: Some(t), .. } if t.contains("MissingParam")), "{e:?}");

    site.post_json(DISCOVER, json!({"error": true, "error_message": "nope"}));
    assert!(matches!(sources::discover_page(&c, &DiscoverQuery::new(), "*", 10).await.unwrap_err(), HarvestError::Api { unspecified: false, .. }));
    site.post_json(DISCOVER, json!({"error": true}));
    assert!(matches!(sources::discover_page(&c, &DiscoverQuery::new(), "*", 10).await.unwrap_err(), HarvestError::Api { unspecified: true, .. }));
    site.post_json(DISCOVER, json!({"error": true, "error_message": "You must be logged in"}));
    assert!(matches!(sources::discover_page(&c, &DiscoverQuery::new(), "*", 10).await.unwrap_err(), HarvestError::IdentityExpired(_)));
    site.route("POST", DISCOVER, |_| Resp::ok("<html>not json</html>"));
    let e = sources::discover_page(&c, &DiscoverQuery::new(), "*", 10).await.unwrap_err();
    assert!(e.to_string().contains("non-JSON"), "{e}");
}

// -- discover ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn discover_page_builds_the_payload_and_ends_on_a_repeated_cursor() {
    let site = Site::new("s-discover");
    site.post_json(
        DISCOVER,
        json!({"results": [discover_row("https://a.bandcamp.com/album/x", "X", "A", 1), {"title": "no url"}], "cursor": "c2", "result_count": 27684}),
    );
    let c = site.client();
    let mut q = DiscoverQuery::new();
    q.genre = Some("Electronic".into());
    q.tags = vec!["Dub Techno".into(), "  ".into(), "lo-fi   house".into()];
    q.slice = "top".into();
    q.category_id = 3;
    q.geoname_id = 2643743;
    q.time_facet_id = Some(5);
    let page = sources::discover_page(&c, &q, "", 500).await.unwrap();
    let payload = site.hits_to("POST", DISCOVER)[0].json();
    assert_eq!(payload["tag_norm_names"], json!(["electronic", "dub-techno", "lo-fi-house"]), "genre first, tags normalised, blanks dropped");
    assert_eq!(
        (payload["cursor"].clone(), payload["size"].clone(), payload["slice"].clone(), payload["category_id"].clone(), payload["geoname_id"].clone(), payload["time_facet_id"].clone()),
        (json!("*"), json!(60), json!("top"), json!(3), json!(2643743), json!(5)),
        "an empty cursor starts at `*`; size clamped to Bandcamp's 60"
    );
    assert_eq!(payload["include_result_types"], json!(["a", "s"]));
    assert_eq!((page.cursor.as_deref(), page.total, page.items.len()), (Some("c2"), Some(27684), 1), "rows without a URL drop");
    assert_eq!(page.items[0].url, "https://a.bandcamp.com/album/x", "?from=discover_page stripped");
    assert!(page.items[0].shallow && page.items[0].tags.is_empty());

    // size 0 clamps up to 1.
    sources::discover_page(&c, &DiscoverQuery::new(), "c2", 0).await.unwrap();
    assert_eq!(site.hits_to("POST", DISCOVER)[1].json()["size"], 1);
    // A cursor Bandcamp hands back unchanged means the feed is exhausted.
    assert_eq!(sources::discover_page(&c, &DiscoverQuery::new(), "c2", 10).await.unwrap().cursor, None);
}

#[tokio::test]
async fn harvest_discover_pages_to_the_limit_and_stamps_the_query_tags() {
    let site = Site::new("s-harvestdiscover");
    site.route("POST", DISCOVER, |hit| {
        let j = hit.json();
        let cursor = j["cursor"].as_str().unwrap_or("*").to_string();
        let page: usize = cursor.strip_prefix('p').and_then(|n| n.parse().ok()).unwrap_or(0);
        let size = j["size"].as_u64().unwrap_or(0) as usize;
        let results: Vec<Value> = (0..size).map(|i| discover_row(&format!("https://a.bandcamp.com/album/p{page}-{i}?from=discover_page"), "T", "A", (page * 10 + i) as i64)).collect();
        Resp::json(&json!({"results": results, "cursor": format!("p{}", page + 1), "result_count": 434991}))
    });
    let c = site.client();
    let mut q = DiscoverQuery::new();
    q.tags = vec!["Techno".into()];
    let events = collect(sources::harvest_discover(c, q, 5, 2)).await;
    let events: Vec<_> = events.into_iter().map(Result::unwrap).collect();
    assert_eq!(events.len(), 5);
    let urls: Vec<_> = events.iter().map(|e| e.release.as_ref().unwrap().url.clone()).collect();
    assert_eq!(urls.len(), urls.iter().collect::<std::collections::HashSet<_>>().len(), "no duplicates; ?from= stripped");
    assert!(urls.iter().all(|u| !u.contains('?')));
    // The API returns no per-item tags, but the query's own tags are why the item is here.
    assert!(events.iter().all(|e| e.release.as_ref().unwrap().tags == ["techno"]));
    assert_eq!(events.last().unwrap().total, Some(5), "a hard ceiling, however many results the tag has");
    let sizes: Vec<_> = site.hits_to("POST", DISCOVER).iter().map(|h| h.json()["size"].clone()).collect();
    assert_eq!(sizes, [json!(2), json!(2), json!(1)], "the last page asks only for what is left");
}

#[tokio::test]
async fn harvest_discover_stops_on_an_empty_page_or_a_stalled_cursor() {
    let site = Site::new("s-discoverstall");
    site.post_json(DISCOVER, json!({"results": [discover_row("https://a.bandcamp.com/album/x", "X", "A", 1)], "cursor": "*", "result_count": 9}));
    let events = collect(sources::harvest_discover(site.client(), DiscoverQuery::new(), 50, 10)).await;
    assert_eq!(events.len(), 1, "the cursor never moved");
    site.post_json(DISCOVER, json!({"results": [], "cursor": "z"}));
    assert!(collect(sources::harvest_discover(site.client(), DiscoverQuery::new(), 50, 10)).await.is_empty());
}

#[tokio::test]
async fn discover_facets_come_off_the_discover_page() {
    let site = Site::new("s-facets");
    let c = site.client();
    let blob = json!({"appData": {"initialState": {"genres": [{"slug": "techno"}, {"slug": "house"}], "times": []}}});
    let page = format!(r#"<div id="DiscoverApp" data-blob="{}"></div>"#, fakebc::esc_json(&blob));
    // The facets URL is fixed to https://bandcamp.com: seed the cache the way a previous visit would.
    c.cache().unwrap().put("https://bandcamp.com/discover/electronic", PageKind::Discover, &page, None).unwrap();
    let facets = sources::fetch_discover_facets(&c, "electronic").await.unwrap();
    assert_eq!(facets["genres"].len(), 2);
    assert!(facets["times"].is_empty());
}

// -- releases ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn fetch_release_rides_the_playable_ttl_and_shares_the_page_with_recommendations() {
    let site = Site::new("s-release");
    let page = release_page("Grid Failure", 0.0).replace(
        "</body>",
        r#"<div class="recommendations-container"><ul><li class="recommended-album" data-albumtitle="R" data-artist="A"><a class="album-link" href="https://r.bandcamp.com/album/r?from=x"></a></li></ul></div></body>"#,
    );
    site.page("/album/grid-failure", &page);
    let c = site.client();
    let url = site.url("/album/grid-failure");
    let r = sources::fetch_release(&c, &url).await.unwrap();
    assert_eq!((r.title.as_str(), r.artist_name.as_str(), r.tier, r.is_free_download), ("Grid Failure", "Artist", Tier::Blob, true));
    let recs = sources::fetch_recommendations(&c, &url).await.unwrap();
    assert_eq!(recs[0].page_url, "https://r.bandcamp.com/album/r");
    // The reserved lane reads the same page (and the same cache).
    let r2 = sources::fetch_release_reserved(&c, &url).await.unwrap();
    assert_eq!(r2.title, r.title);
    assert_eq!(site.count("/album/grid-failure"), 1, "one request for release + related strip + reserved resolve");
    assert_eq!(sources::TTL_PLAYABLE.as_secs(), 900);

    let e = sources::fetch_release(&c, &site.url("/album/missing")).await.unwrap_err();
    assert!(e.to_string().contains("not found"), "{e}");
}

// -- bands ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn fetch_band_page_reads_a_catalogue_in_one_request_and_a_roster_only_for_labels() {
    let site = Site::new("s-band");
    site.page("/music", &music_page(&json!({"id": 5, "name": "Hyperdub", "is_label": true}), Some(&[grid_entry("a", "A", "X", 1)])));
    let c = site.client();
    // A label whose /artists page 404s is still a browsable label.
    let page = sources::fetch_band_page(&c, &site.url("/album/whatever"), true).await.unwrap();
    assert_eq!((page.profile.is_label, page.releases.len(), page.roster.len(), page.tier), (true, 1, 0, Tier::Blob));
    assert_eq!(site.count("/music"), 1);
    assert_eq!(site.count("/artists"), 1);

    site.page("/artists", r#"<ol class="artists-grid"><li class="artists-grid-item" data-item-id="1"><a href="/artist/burial"><div class="artists-grid-name">Burial</div></a></li></ol>"#);
    let page = sources::fetch_band_page(&c, &site.url("/"), true).await.unwrap();
    assert_eq!(page.roster.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["Burial"], "a 404 is not cached, so the roster arrives once the page exists");
    let page = sources::fetch_band_page(&c, &site.url("/"), false).await.unwrap();
    assert!(page.roster.is_empty(), "with_roster=false never reads /artists");

    let events = collect(sources::harvest_label_roster(c, site.url("/"))).await;
    let names: Vec<_> = events.into_iter().map(|e| e.unwrap().artist.unwrap().name).collect();
    assert_eq!(names, ["Burial"]);
}

#[tokio::test]
async fn probe_artist_counts_the_grid_and_flags_a_truncated_one() {
    let site = Site::new("s-probe");
    site.page("/music", &music_page(&json!({"id": 5, "name": "Solo", "is_label": false}), None));
    let probe = sources::probe_artist(&site.client(), &site.url("/")).await.unwrap();
    assert_eq!((probe.kind.as_str(), probe.total_hint, probe.auth_ok), ("artist", Some(0), true));
    assert!(probe.detail.contains("grid fallback"), "{}", probe.detail);
}

// -- harvests ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn harvest_artist_shallow_reads_one_page_full_reads_every_release() {
    let site = Site::new("s-artist");
    site.page("/music", &music_page(&json!({"id": 5, "name": "Band"}), Some(&[grid_entry("one", "One", "Band", 1), grid_entry("two", "Two", "Band", 2), grid_entry("three", "Three", "Band", 3)])));
    site.page("/album/one", &release_page("One (full)", 0.0));
    site.page("/album/two", &release_page("Two (full)", 5.0));
    // /album/three is a dead page: one event with an error, the run goes on.
    let c = site.client();

    let shallow: Vec<_> = collect(sources::harvest_artist(c.clone(), site.url("/"), "shallow".into(), None)).await.into_iter().map(Result::unwrap).collect();
    assert_eq!(shallow.len(), 3);
    assert!(shallow.iter().all(|e| e.release.as_ref().unwrap().shallow), "list data only");
    assert_eq!(site.count("/album/one"), 0, "shallow costs one request in total");
    assert_eq!((shallow[0].seen, shallow[0].total, shallow[2].cursor.as_deref()), (1, Some(3), Some("3")));
    assert!(shallow[0].release.as_ref().unwrap().art_url.is_some(), "art built from the grid's art id");

    let limited: Vec<_> = collect(sources::harvest_artist(c.clone(), site.url("/"), "shallow".into(), Some(2))).await;
    assert_eq!(limited.len(), 2);

    let full: Vec<_> = collect(sources::harvest_artist(c, site.url("/"), "full".into(), None)).await.into_iter().map(Result::unwrap).collect();
    assert_eq!(full.len(), 3);
    assert_eq!(full[0].release.as_ref().unwrap().title, "One (full)");
    assert!(!full[0].release.as_ref().unwrap().shallow);
    assert!(full[2].release.is_none() && full[2].error.as_deref().unwrap().contains("not found"), "{:?}", full[2].error);
}

#[tokio::test]
async fn harvest_url_list_dedupes_skips_comments_and_ignores_non_releases() {
    let site = Site::new("s-urllist");
    site.page("/album/a", &release_page("A (full)", 0.0));
    let c = site.client();
    let (a, b) = (site.url("/album/a"), site.url("/track/b"));
    let text = format!(
        "# a comment\n// another\n\n{a}\n{a}/\n{a}?from=x\n{b}, {a}\n{}\nnot a url\nhttps://x.bandcamp.com/music\nhttps://bandcamp.com/somefan\n",
        site.url("/album/c")
    );
    let events: Vec<_> = collect(sources::harvest_url_list(c.clone(), text.clone(), "shallow".into(), None)).await.into_iter().map(Result::unwrap).collect();
    let urls: Vec<_> = events.iter().map(|e| e.release.as_ref().unwrap().url.clone()).collect();
    assert_eq!(urls, [a.clone(), b.clone(), site.url("/album/c")], "deduped by canonical URL, first sighting wins, order kept");
    let kinds: Vec<_> = events.iter().map(|e| e.release.as_ref().unwrap().item_type.clone()).collect();
    assert_eq!(kinds, ["album", "track", "album"]);
    assert_eq!(events[2].total, Some(3));
    assert!(site.hits().is_empty(), "shallow touches no page");

    let limited = collect(sources::harvest_url_list(c.clone(), text.clone(), "shallow".into(), Some(2))).await;
    assert_eq!(limited.len(), 2);

    // Bare `host/path` input is coerced.
    let bare = text.replace("http://", "");
    assert_eq!(collect(sources::harvest_url_list(c.clone(), bare, "shallow".into(), None)).await.len(), 3);

    // Full depth fetches each page; a dead one becomes an error event, not an aborted run.
    let full: Vec<_> = collect(sources::harvest_url_list(c, text, "full".into(), None)).await.into_iter().map(Result::unwrap).collect();
    assert_eq!(full[0].release.as_ref().unwrap().title, "A (full)");
    assert!(full[1].error.is_some() && full[2].error.is_some());
}

// -- resolving an album ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn resolve_album_url_follows_a_track_to_its_album_and_leaves_the_rest_alone() {
    let site = Site::new("s-resolve");
    let c = site.client();
    // An album URL is returned unchanged and costs no request.
    let album = site.url("/album/x");
    assert_eq!(sources::resolve_album_url(&c, &format!("{album}/?from=y")).await.unwrap(), album);
    assert!(site.hits().is_empty());

    let in_album = tralbum_page(&json!({"for the curious": "x", "item_type": "track", "id": 3, "artist": "A",
        "album_url": "/album/parent", "current": {"title": "T", "band_id": 2}, "trackinfo": []}));
    site.page("/track/in-album", &in_album);
    site.page("/track/single", &release_page("Single", 0.0));
    assert_eq!(sources::resolve_album_url(&c, &site.url("/track/in-album")).await.unwrap(), site.url("/album/parent"));
    assert_eq!(sources::resolve_album_url(&c, &site.url("/track/single")).await.unwrap(), site.url("/track/single"), "a standalone single is its own album");
    assert!(sources::resolve_album_url(&c, &site.url("/track/gone")).await.is_err());
}

#[test]
fn shallow_records_are_built_from_list_data_alone() {
    let r = sources::shallow(
        "https://a.bandcamp.com/album/x/?from=y",
        sources::Shallow { title: "T", artist: "A", art_id: Some(4000000001), is_free: true, tags: vec!["x".into()], ..Default::default() },
    );
    assert_eq!((r.url.as_str(), r.item_type.as_str(), r.shallow, r.tier, r.is_free_download), ("https://a.bandcamp.com/album/x", "album", true, Tier::Blob, true));
    assert_eq!(r.art_url.as_deref(), Some("https://f4.bcbits.com/img/a4000000001_16.jpg"));
    // Used by the cookie-free client option.
    let _ = GetOpts::default();
}
