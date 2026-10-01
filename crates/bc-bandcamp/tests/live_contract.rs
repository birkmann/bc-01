//! Live contract tests against the real Bandcamp. **Excluded by default**: run with
//! `cargo test -p bc-bandcamp --features live --test live_contract`.
//!
//! These hit the network, so they do not belong in a normal test run, but they turn API drift
//! from a mystery bug into a red test. Run them weekly. Each assertion pins a fact this codebase
//! depends on (verified 2026-07-28). Targets are stable, well-known public pages and the request
//! volume is a handful, in line with the politeness budget the app itself uses.
#![cfg(feature = "live")]

use std::collections::HashSet;

use bc_bandcamp::HarvestError;
use bc_bandcamp::extract::{self, Tier};
use bc_bandcamp::net::{BandcampClient, ClientOptions, GetOpts, PageKind};
use bc_bandcamp::sources::{self, DiscoverQuery};
use bc_bandcamp::urls::normalise;
use futures::StreamExt;
use scraper::{Html, Selector};
use serde_json::json;

const LABEL: &str = "https://hyperdub.bandcamp.com";

/// Deliberately slower than the app default for a test run.
fn client() -> BandcampClient {
    BandcampClient::new(ClientOptions { rate_per_sec: 0.5, burst: 2, concurrency: 1, ..ClientOptions::default() })
}

/// The rule the whole fetch layer is built on. If Bandcamp ever switches to real status codes
/// this test fails and the body-inspection logic can be simplified -- but until then, removing it
/// would silently turn every error into corrupt data.
#[tokio::test]
async fn api_errors_arrive_as_http_200() {
    let c = client();
    // category_id omitted on purpose.
    let e = c.post_api(sources::DISCOVER_PATH, &json!({}), false, Some("https://bandcamp.com/")).await.unwrap_err();
    match e {
        HarvestError::Api { error_type, .. } => assert!(error_type.unwrap_or_default().contains("MissingParam")),
        other => panic!("expected an API error body, got {other:?}"),
    }
}

#[tokio::test]
async fn unauthenticated_collection_summary_signals_identity() {
    let c = client();
    c.set_cookie(None);
    let e = c.get_api(sources::SUMMARY_PATH, &[], true).await.unwrap_err();
    assert!(matches!(e, HarvestError::IdentityExpired(_)), "{e:?}");
}

/// The rendered head and `data-client-items` are disjoint; both are needed. Bandcamp serves the
/// newest ~16 releases as markup and *the rest* in the attribute. So the assertion that matters
/// is not "the blob is bigger": it is that every rendered item survived into the parsed list.
/// A flip to the CSS tier still means the tail is unreadable and catalogues truncate, so that is
/// pinned too.
#[tokio::test]
async fn music_page_splits_its_grid_into_two_halves() {
    let c = client();
    let body = c.get_html(&format!("{LABEL}/music"), GetOpts::kind(PageKind::Music)).await.unwrap();
    let (items, tier) = extract::parse_music_grid(&body, LABEL);

    // A label's own grid links some items to the artist's subdomain, so the href is absolute as
    // often as it is rooted -- the parser has to take both and so does the expectation.
    let doc = Html::parse_document(&body);
    let li = Selector::parse("ol#music-grid li.music-grid-item").unwrap();
    let a = Selector::parse("a[href]").unwrap();
    let rendered: HashSet<String> = doc
        .select(&li)
        .filter_map(|n| n.select(&a).next())
        .filter_map(|a| a.value().attr("href"))
        .map(|href| normalise(&if href.starts_with("http") { href.to_string() } else { format!("{LABEL}{href}") }))
        .collect();
    let parsed: HashSet<String> = items.iter().map(|i| i.page_url.clone()).collect();

    assert_eq!(tier, Tier::Blob, "fell back to the rendered grid -- catalogues will truncate");
    assert!(!rendered.is_empty(), "the page rendered no grid items at all; the markup moved");
    assert!(rendered.is_subset(&parsed), "the rendered head was dropped -- the union broke");
    assert!(items.len() > rendered.len(), "the attribute carried nothing beyond the rendered page");
    assert!(items.len() > 50);
    assert_eq!(parsed.len(), items.len(), "the two halves overlapped and were not deduped");
    assert!(items.iter().all(|i| i.page_url.starts_with("http")));
    assert!(items.iter().filter_map(|i| i.band_id).collect::<HashSet<_>>().len() > 1, "a label should span several artists");
}

#[tokio::test]
async fn album_page_blob_and_canary() {
    let c = client();
    let body = c.get_html(&format!("{LABEL}/music"), GetOpts::kind(PageKind::Music)).await.unwrap();
    let (items, _) = extract::parse_music_grid(&body, LABEL);
    let album = items.iter().find(|i| i.item_type == "album").expect("an album on the label's grid");

    let page = c.get_html(&album.page_url, GetOpts::kind(PageKind::Album)).await.unwrap();
    assert_eq!(extract::tralbum_has_canary(&page), Some(true), "the 'for the curious' key vanished -- the data-tralbum format changed");
    let release = extract::parse_tralbum(&page, &album.page_url);

    assert_eq!(release.tier, Tier::Blob);
    assert!(!release.missing.iter().any(|m| m == "canary"));
    assert!(!release.title.is_empty());
    assert!(!release.artist_name.is_empty());
    assert!(release.band_id.is_some());
    assert!(!release.tracks.is_empty(), "trackinfo should not be empty for an album");
}

/// The /artists page uses single-quoted attributes.
#[tokio::test]
async fn label_roster_is_parseable() {
    let events: Vec<_> = sources::harvest_label_roster(client(), LABEL.into()).collect().await;
    let roster: Vec<_> = events.into_iter().filter_map(|e| e.ok().and_then(|e| e.artist)).collect();
    assert!(roster.len() > 5);
    assert!(roster.iter().all(|a| a.url.starts_with("http")));
    assert!(roster.iter().any(|a| !a.name.is_empty()));
}

#[tokio::test]
async fn discover_api_paginates() {
    let mut q = DiscoverQuery::new();
    q.tags = vec!["techno".into()];
    let events: Vec<_> = sources::harvest_discover(client(), q, 4, 2).collect().await;
    let seen: Vec<_> = events.into_iter().filter_map(|e| e.ok().and_then(|e| e.release)).collect();
    assert_eq!(seen.len(), 4);
    // ?from=discover_page must be stripped, or the inbox holds duplicates.
    assert!(seen.iter().all(|r| !r.url.contains('?')));
    assert_eq!(seen.iter().map(|r| r.url.clone()).collect::<HashSet<_>>().len(), seen.len());
}

#[tokio::test]
async fn discover_reports_a_result_count_and_a_cursor() {
    let mut q = DiscoverQuery::new();
    q.tags = vec!["techno".into()];
    let page = sources::discover_page(&client(), &q, "*", 2).await.unwrap();
    assert!(page.total.unwrap_or(0) > 0, "result_count vanished from the discover response");
    assert!(page.cursor.is_some());
    assert_eq!(page.items.len(), 2);
}

/// The facet vocabulary rides on `div#DiscoverApp[data-blob]`, not a `<script>`.
#[tokio::test]
async fn discover_facets_are_available() {
    let c = client();
    let body = c.get_html("https://bandcamp.com/discover/electronic", GetOpts::kind(PageKind::Discover)).await.unwrap();
    let doc = Html::parse_document(&body);
    let sel = Selector::parse("div#DiscoverApp[data-blob]").unwrap();
    assert!(doc.select(&sel).next().is_some(), "the facet blob moved off div#DiscoverApp");
    let facets = sources::fetch_discover_facets(&c, "electronic").await.unwrap();
    assert!(facets.get("genres").is_some_and(|g| g.len() > 10), "genre vocabulary missing from the discover page");
}
