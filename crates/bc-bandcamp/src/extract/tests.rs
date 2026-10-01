//! Ports of `test_harvest_extract.py` plus the pure-extractor cases of `test_collectors.py`
//! and `test_explore.py`. Fixtures are synthetic pages replicating the *structure* verified
//! live against Bandcamp on 2026-07-28, not captured pages.

use serde_json::{Value, json};

use super::*;

/// Python `html.escape` (quote=True).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

fn album_page(tralbum: Option<&Value>, band: Option<&Value>, ld: Option<&Value>, css_only: bool) -> String {
    let mut parts = vec!["<html><head><title>Grid Failure | Somatic</title></head><body>".to_string()];
    if let (Some(t), false) = (tralbum, css_only) {
        parts.push(format!(r#"<script data-tralbum="{}"></script>"#, esc(&t.to_string())));
    }
    if let (Some(b), false) = (band, css_only) {
        parts.push(format!(r#"<script data-band="{}"></script>"#, esc(&b.to_string())));
    }
    if let Some(l) = ld {
        parts.push(format!(r#"<script type="application/ld+json">{l}</script>"#));
    }
    parts.push(
        concat!(
            r#"<h2 class="trackTitle">Grid Failure</h2>"#,
            r#"<span itemprop="byArtist"><a>Somatic</a></span>"#,
            r#"<div class="tralbum-tags"><a class="tag">techno</a><a class="tag">industrial</a></div>"#,
            r#"<div id="tralbumArt"><img src="https://f4.bcbits.com/img/a1_16.jpg"/></div>"#,
            "</body></html>"
        )
        .to_string(),
    );
    parts.concat()
}

fn full_tralbum() -> Value {
    json!({
        CANARY_KEY: "bandcamp's own note to scrapers",
        "item_type": "album",
        "id": 2196852739_i64,
        "art_id": 2761330920_i64,
        "artist": "Somatic",
        "album_release_date": "17 Jul 2026 00:00:00 GMT",
        "defaultPrice": 9.0,
        "freeDownloadPage": null,
        "is_preorder": false,
        "current": {
            "title": "Grid Failure",
            "band_id": 2482507458_i64,
            "about": "notes",
            "credits": "mastered somewhere",
            "minimum_price": 7.0,
            "purchase_url": "https://example.test/buy",
            "private": false,
        },
        "trackinfo": [
            {"title": "Nocturnal Transit", "track_num": 1, "duration": 401.5,
             "title_link": "/track/nocturnal-transit", "track_id": 111, "has_lyrics": false},
            {"title": "Iron Lung", "track_num": 2, "duration": 362.0,
             "title_link": "/track/iron-lung", "track_id": 112},
        ],
    })
}

fn band() -> Value {
    json!({"id": 2482507458_i64, "name": "Somatic"})
}

fn ld() -> Value {
    json!({
        "@type": "MusicAlbum",
        "name": "Grid Failure",
        "byArtist": {"@type": "MusicGroup", "name": "Somatic"},
        "publisher": {"@type": "MusicGroup", "name": "Vault Sector"},
        "keywords": ["Electronic", "techno", "industrial"],
        "datePublished": "17 Jul 2026 00:00:00 GMT",
    })
}

const URL: &str = "https://somatic.bandcamp.com/album/grid-failure";

// ---------------------------------------------------------------------------
// The ladder
// ---------------------------------------------------------------------------

#[test]
fn blob_tier_extracts_everything() {
    let r = parse_tralbum(&album_page(Some(&full_tralbum()), Some(&band()), Some(&ld()), false), URL);

    assert_eq!(r.tier, Tier::Blob);
    assert!(r.missing.is_empty());
    assert_eq!(r.title, "Grid Failure");
    assert_eq!(r.artist_name, "Somatic");
    assert_eq!(r.bc_item_id, Some(2196852739));
    assert_eq!(r.band_id, Some(2482507458));
    assert_eq!(r.release_date.as_deref(), Some("2026-07-17"));
    assert_eq!(r.track_count(), 2);
    assert_eq!(r.tracks[0].title, "Nocturnal Transit");
    assert!((r.tracks[0].duration_sec.unwrap() - 401.5).abs() < 1e-9);
    assert_eq!(r.tracks[0].url.as_deref(), Some("https://somatic.bandcamp.com/track/nocturnal-transit"));
    assert_eq!(r.label_name.as_deref(), Some("Vault Sector"));
}

#[test]
fn falls_back_to_jsonld_when_the_blob_is_gone() {
    let r = parse_tralbum(&album_page(None, None, Some(&ld()), false), URL);

    assert_eq!(r.tier, Tier::JsonLd);
    assert!(r.missing.contains(&"data-tralbum".to_string()));
    assert_eq!(r.title, "Grid Failure");
    assert_eq!(r.artist_name, "Somatic");
    assert_eq!(r.tags, vec!["Electronic", "techno", "industrial"]);
}

#[test]
fn falls_back_to_css_when_all_json_is_gone() {
    let r = parse_tralbum(&album_page(None, None, None, false), URL);

    assert_eq!(r.tier, Tier::Css);
    assert_eq!(r.title, "Grid Failure");
    assert_eq!(r.artist_name, "Somatic");
    assert_eq!(r.tags, vec!["techno", "industrial"]);
    assert_eq!(r.art_url.as_deref(), Some("https://f4.bcbits.com/img/a1_16.jpg"));
}

#[test]
fn missing_canary_is_flagged() {
    let mut without = full_tralbum();
    without.as_object_mut().unwrap().remove(CANARY_KEY);
    let page = album_page(Some(&without), Some(&band()), Some(&ld()), false);
    let r = parse_tralbum(&page, URL);
    assert!(r.missing.contains(&"canary".to_string()));
    assert_eq!(tralbum_has_canary(&page), Some(false));

    let ok = album_page(Some(&full_tralbum()), Some(&band()), Some(&ld()), false);
    assert_eq!(tralbum_has_canary(&ok), Some(true));
    assert_eq!(tralbum_has_canary(&album_page(None, None, None, false)), None);
}

#[test]
fn self_published_release_reports_no_label() {
    let mut ld1 = ld();
    ld1["publisher"] = json!({"name": "Kode9"});
    let mut tr = full_tralbum();
    tr["artist"] = json!("kode9, burial");
    assert_eq!(parse_tralbum(&album_page(Some(&tr), Some(&band()), Some(&ld1), false), URL).label_name, None);

    let mut exact = ld();
    exact["publisher"] = json!({"name": "SOMATIC"});
    assert_eq!(
        parse_tralbum(&album_page(Some(&full_tralbum()), Some(&band()), Some(&exact), false), URL).label_name,
        None
    );
}

#[test]
fn free_download_is_detected() {
    let mut free = full_tralbum();
    free["current"]["minimum_price"] = json!(0.0);
    assert!(parse_tralbum(&album_page(Some(&free), Some(&band()), Some(&ld()), false), URL).is_free_download);
    assert!(
        !parse_tralbum(&album_page(Some(&full_tralbum()), Some(&band()), Some(&ld()), false), URL).is_free_download
    );
}

#[test]
fn track_url_is_inferred_from_the_page_url() {
    let r = parse_tralbum(
        &album_page(Some(&full_tralbum()), Some(&band()), Some(&ld()), false),
        "https://somatic.bandcamp.com/track/x",
    );
    assert_eq!(r.item_type, "track");
}

#[test]
fn tier_strings_are_the_legacy_persisted_ones() {
    assert_eq!(Tier::Blob.as_str(), "blob");
    assert_eq!(Tier::JsonLd.as_str(), "jsonld");
    assert_eq!(Tier::Css.as_str(), "css");
    assert_eq!(serde_json::to_string(&Tier::JsonLd).unwrap(), "\"jsonld\"");
    assert_eq!(Tier::parse("json_ld"), Some(Tier::JsonLd));
}

#[test]
fn entities_in_blob_attributes_are_decoded_exactly_once() {
    let tr = json!({CANARY_KEY: "x", "item_type": "album", "artist": "A & B",
        "current": {"title": "Fish &amp; Chips <3 \"quoted\""}});
    let r = parse_tralbum(&album_page(Some(&tr), None, None, false), URL);
    assert_eq!(r.artist_name, "A & B");
    assert_eq!(r.title, "Fish &amp; Chips <3 \"quoted\"");
}

#[test]
fn artist_parts_splits_credits() {
    let p = artist_parts("Kode9, Burial & Foo feat. Bar with Baz");
    for n in ["kode9", "burial", "foo", "baz"] {
        assert!(p.contains(n), "{n} in {p:?}");
    }
}

// ---------------------------------------------------------------------------
// Dates
// ---------------------------------------------------------------------------

#[test]
fn parse_bc_date_cases() {
    for (raw, expected) in [
        ("17 Jul 2026 00:00:00 GMT", Some("2026-07-17")),
        ("17 Jul 2026", Some("2026-07-17")),
        ("2026-07-17", Some("2026-07-17")),
        ("2026-07-17T10:00:00Z", Some("2026-07-17")),
        ("", None),
        ("not a date", None),
    ] {
        assert_eq!(parse_bc_date(Some(raw)).as_deref(), expected, "{raw:?}");
    }
    assert_eq!(parse_bc_date(None), None);
    // Epoch seconds, as Python's isdigit branch.
    assert_eq!(parse_bc_date(Some("1784246400")).as_deref(), Some("2026-07-17"));
}

// ---------------------------------------------------------------------------
// /music grid
// ---------------------------------------------------------------------------

/// One rendered grid entry, as the live page writes it. Only the first rows carry a real
/// `src` (the rest are lazy, placeholder gif + `data-original`), and the per-item artist is
/// a span *inside* the title cell.
fn grid_item(n: usize) -> String {
    let art = if n < 4 {
        format!(r#"<img src="https://f4.bcbits.com/img/a{}_2.jpg" alt="" />"#, 900 + n)
    } else {
        format!(
            r#"<img class="lazy" src="/img/0.gif" data-original="https://f4.bcbits.com/img/a{}_2.jpg" alt="">"#,
            900 + n
        )
    };
    let over = if n.is_multiple_of(2) { r#"<br><span class="artist-override"> Someone </span>"# } else { "" };
    format!(
        r#"<li class="music-grid-item" data-item-id="album-{n}" data-band-id="7"><a href="/album/rendered-{n}"><div class="art">{art}</div><p class="title"> Rendered {n} {over}</p></a></li>"#
    )
}

fn grid_page(entries: Option<&Value>, rendered: usize) -> String {
    let attr = entries.map(|e| format!(r#" data-client-items="{}""#, esc(&e.to_string()))).unwrap_or_default();
    let lis: String = (0..rendered).map(grid_item).collect();
    format!(r#"<html><body><ol id="music-grid"{attr}>{lis}</ol></body></html>"#)
}

const ROOT: &str = "https://a.bandcamp.com";

#[test]
fn music_grid_unions_the_rendered_head_with_the_attribute_tail() {
    let entries: Vec<Value> = (0..50)
        .map(|n| {
            json!({"id": 1000 + n, "band_id": 500 + n, "art_id": 900 + n, "artist": format!("Artist {n}"),
                "title": format!("Release {n}"), "type": "album",
                "page_url": format!("https://a.bandcamp.com/album/release-{n}?label=1&tab=music")})
        })
        .collect();
    let (items, tier) = parse_music_grid(&grid_page(Some(&Value::Array(entries)), 16), ROOT);

    assert_eq!(tier, Tier::Blob);
    assert_eq!(items.len(), 66, "the head is not an alternative to the tail; it is the rest of it");
    let head: Vec<&str> = items[..16].iter().map(|i| i.page_url.as_str()).collect();
    let want: Vec<String> = (0..16).map(|n| format!("https://a.bandcamp.com/album/rendered-{n}")).collect();
    assert_eq!(head, want.iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(items[0].art_id, Some(900), "the recovered head keeps its lazy cover");
    assert_eq!(items[16].page_url, "https://a.bandcamp.com/album/release-0");
    assert_eq!(items[16].band_id, Some(500));
    assert_eq!(items[16].artist, "Artist 0");
}

#[test]
fn music_grid_falls_back_to_the_rendered_dom() {
    let (items, tier) = parse_music_grid(&grid_page(None, 3), ROOT);
    assert_eq!(tier, Tier::Css);
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].page_url, "https://a.bandcamp.com/album/rendered-0");
    assert_eq!(items[0].item_type, "album");
}

#[test]
fn music_grid_dedupes_a_release_that_sits_in_both_halves() {
    let entries = json!([{"id": 1, "band_id": 9, "art_id": 5, "artist": "Someone Else", "title": "Rendered 1",
        "type": "album", "page_url": "/album/rendered-1?label=1&tab=music"}]);
    let (items, tier) = parse_music_grid(&grid_page(Some(&entries), 3), ROOT);

    assert_eq!(tier, Tier::Blob);
    assert_eq!(items.len(), 3, "one release, not two");
    assert_eq!(items[1].page_url, "https://a.bandcamp.com/album/rendered-1");
    assert_eq!(items[1].art_id, Some(901), "the rendered row keeps its own lazy cover");
    assert_eq!(items[1].artist, "Someone Else");
}

#[test]
fn music_grid_reads_an_empty_attribute_as_a_complete_catalogue() {
    let (items, tier) = parse_music_grid(&grid_page(Some(&json!([])), 3), ROOT);
    assert_eq!(tier, Tier::Blob);
    assert_eq!(items.len(), 3);
}

#[test]
fn music_grid_reads_the_attribute_when_the_page_renders_nothing() {
    let entries: Vec<Value> =
        (0..3).map(|n| json!({"id": n, "title": format!("Release {n}"), "type": "album", "page_url": format!("/album/release-{n}")})).collect();
    let (items, tier) = parse_music_grid(&grid_page(Some(&Value::Array(entries)), 0), ROOT);
    assert_eq!(tier, Tier::Blob);
    let got: Vec<&str> = items.iter().map(|i| i.page_url.as_str()).collect();
    assert_eq!(
        got,
        vec![
            "https://a.bandcamp.com/album/release-0",
            "https://a.bandcamp.com/album/release-1",
            "https://a.bandcamp.com/album/release-2"
        ]
    );
}

#[test]
fn rendered_grid_recovers_cover_art_including_the_lazy_rows() {
    let (items, _) = parse_music_grid(&grid_page(None, 6), ROOT);
    let ids: Vec<Option<i64>> = items.iter().map(|i| i.art_id).collect();
    assert_eq!(ids, vec![Some(900), Some(901), Some(902), Some(903), Some(904), Some(905)]);
    assert!(items.iter().all(|i| i.art_url.is_none()), "an id is enough; no URL kept");
}

#[test]
fn rendered_grid_keeps_the_artist_out_of_the_title() {
    let (items, _) = parse_music_grid(&grid_page(None, 2), ROOT);
    assert_eq!(items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), ["Rendered 0", "Rendered 1"]);
    assert_eq!(items.iter().map(|i| i.artist.as_str()).collect::<Vec<_>>(), ["Someone", ""]);
}

#[test]
fn rendered_grid_falls_back_to_the_scraped_url_when_no_id_is_in_it() {
    let page = concat!(
        r#"<html><body><ol id="music-grid">"#,
        r#"<li class="music-grid-item" data-item-id="album-1" data-band-id="7">"#,
        r#"<a href="/album/x"><div class="art">"#,
        r#"<img src="https://f4.bcbits.com/img/reshaped.jpg" alt="" /></div>"#,
        "<p class='title'>X</p></a></li></ol></body></html>"
    );
    let (items, _) = parse_music_grid(page, ROOT);
    assert_eq!(items[0].art_id, None);
    assert_eq!(items[0].art_url.as_deref(), Some("https://f4.bcbits.com/img/reshaped.jpg"));
}

#[test]
fn music_grid_handles_relative_urls() {
    let entries = json!([{"id": 1, "title": "X", "artist": "Y", "type": "track", "page_url": "/track/x"}]);
    let (items, _) = parse_music_grid(&grid_page(Some(&entries), 0), ROOT);
    assert_eq!(items[0].page_url, "https://a.bandcamp.com/track/x");
    assert_eq!(items[0].item_type, "track");
}

// ---------------------------------------------------------------------------
// Label roster -- note the single-quoted attributes
// ---------------------------------------------------------------------------

const ROSTER_HTML: &str = concat!(
    "<html><body><ol class='editable-grid artists-grid'>",
    "<li class='artists-grid-item' data-item-id='3934775866'>",
    "<a href='https://djmanny1.bandcamp.com?label=1&amp;tab=artists'>",
    "<div class='artists-grid-name'>Dj Manny</div></a>",
    "<div class='artists-grid-location'>Chicago, Illinois</div></li>",
    "<li class='artists-grid-item' data-item-id='123'>",
    "<a href='https://other.bandcamp.com'><div class='artists-grid-name'>Other</div></a></li>",
    "</ol></body></html>"
);

#[test]
fn roster_parses_single_quoted_attributes() {
    let roster = parse_roster(ROSTER_HTML, "https://label.bandcamp.com");
    assert_eq!(roster.len(), 2);
    assert_eq!(roster[0].name, "Dj Manny");
    assert_eq!(roster[0].band_id, Some(3934775866));
    assert_eq!(roster[0].location.as_deref(), Some("Chicago, Illinois"));
    assert_eq!(roster[0].url, "https://djmanny1.bandcamp.com");
    assert_eq!(roster[1].location, None);
}

#[test]
fn label_detection() {
    assert!(looks_like_label(ROSTER_HTML));
    assert!(!looks_like_label("<html><body><ol id='music-grid'></ol></body></html>"));
}

// ---------------------------------------------------------------------------
// Fan page
// ---------------------------------------------------------------------------

#[test]
fn fan_page_blob() {
    let blob = json!({
        "fan_data": {"fan_id": 10236, "username": "someone", "name": "Some One"},
        "collection_data": {"item_count": 412, "last_token": "1301406176:1500067389:a:2:"},
        "wishlist_data": {"item_count": 88},
        "hidden_data": {"item_count": 3},
        "item_cache": {"a123": {"item_title": "X"}},
    });
    let page = format!(
        r#"<html><body><script id="pagedata" data-blob="{}"></script></body></html>"#,
        esc(&blob.to_string())
    );
    let fan = parse_fan_page(&page).unwrap();
    assert_eq!(fan.fan_id, 10236);
    assert_eq!(fan.username, "someone");
    assert_eq!(fan.display_name, "Some One");
    assert_eq!(fan.collection_count, 412);
    assert_eq!(fan.wishlist_count, 88);
    assert_eq!(fan.hidden_count, 3);
    assert_eq!(Value::Object(fan.item_cache.clone()), json!({"a123": {"item_title": "X"}}));
    assert_eq!(fan.last_tokens.get("collection").map(String::as_str), Some("1301406176:1500067389:a:2:"));
    assert!(!fan.last_tokens.contains_key("wishlist"));
}

#[test]
fn fan_page_without_a_blob_raises() {
    let err = parse_fan_page("<html><body>nothing here</body></html>").unwrap_err();
    assert!(matches!(err, crate::HarvestError::Extraction(_)));
}

fn fan_page_html() -> String {
    let blob = json!({
        "fan_data": {"fan_id": 10236, "username": "someone", "name": "Some One"},
        "collection_data": {"item_count": 9},
        "wishlist_data": {"item_count": 19},
        "hidden_data": {"item_count": 2},
        "item_cache": {
            "collection": {"a1": {"item_title": "Owned", "item_url": "https://x.bandcamp.com/album/owned"}},
            "wishlist": {"a2": {"item_title": "Wanted", "item_url": "https://y.bandcamp.com/album/wanted"}},
            "hidden": {},
            "followers": {"nope": {}},
        },
    });
    format!(r#"<html><body><div id="pagedata" data-blob="{}"></div></body></html>"#, esc(&blob.to_string()))
}

#[test]
fn fan_blob_is_read_from_a_div_not_a_script() {
    let fan = parse_fan_page(&fan_page_html()).unwrap();
    assert_eq!(fan.fan_id, 10236);
    assert_eq!(fan.collection_count, 9);
    assert_eq!(fan.wishlist_count, 19);
    assert_eq!(fan.hidden_count, 2);
}

#[test]
fn cached_items_selects_the_right_tab() {
    let fan = parse_fan_page(&fan_page_html()).unwrap();
    let collection = fan.cached_items("collection");
    let wishlist = fan.cached_items("wishlist");
    assert_eq!(collection.len(), 1);
    assert_eq!(wishlist.len(), 1);
    assert_eq!(collection[0]["item_title"], "Owned");
    assert_eq!(wishlist[0]["item_title"], "Wanted");
    assert!(fan.cached_items("hidden").is_empty());
    assert!(fan.cached_items("nonexistent").is_empty());
}

#[test]
fn cached_items_returns_records_not_keys() {
    for item in parse_fan_page(&fan_page_html()).unwrap().cached_items("wishlist") {
        assert!(item.is_object(), "must be the record, not the item key");
        assert!(item.get("item_url").is_some());
    }
}

#[test]
fn cached_items_keep_the_pages_own_order() {
    // item_cache is newest-first; sorting keys would reorder "a9" after "a10".
    let blob = json!({"fan_data": {"fan_id": 1, "username": "u"},
        "item_cache": {"collection": {"a9": {"n": 1}, "a10": {"n": 2}, "a2": {"n": 3}}}});
    let page = format!(r#"<div id="pagedata" data-blob="{}"></div>"#, esc(&blob.to_string()));
    let fan = parse_fan_page(&page).unwrap();
    let order: Vec<i64> = fan.cached_items("collection").iter().map(|v| v["n"].as_i64().unwrap()).collect();
    assert_eq!(order, vec![1, 2, 3]);
    assert_eq!(fan.display_name, "u");
}

// ---------------------------------------------------------------------------
// test_collectors.py (pure extract cases)
// ---------------------------------------------------------------------------

fn collectors_blob() -> Value {
    json!({
        "thumbs": [
            {"fan_id": 5063709, "username": "dumiovoxo", "name": "Dumi", "image_id": 25165605,
             "token": "1:1787172552:5063709:0:1:0"},
            {"fan_id": 3575534, "username": "exm0nster", "name": "ExM", "image_id": 33616909,
             "token": "1:1787172175:3575534:0:1:0"},
        ],
        "more_thumbs_available": true,
        "reviews": [
            {"fan_id": 13644698, "username": "sinnatro", "name": "Sinnatro", "why": "This hipnotize my soul",
             "image_id": 46230767, "token": "1:1784630191:13644698:1:1:0", "fav_track_title": "Tempest"},
        ],
        "more_reviews_available": false,
    })
}

fn collectors_page(blob: Option<&Value>) -> String {
    let tr = json!({"id": 503240863, "item_type": "album", "artist": "Phil Berg",
        "current": {"title": "Dārin"}, "trackinfo": [], "for the curious": "x"});
    let mut parts = vec![format!(r#"<script data-tralbum="{}"></script>"#, esc(&tr.to_string()))];
    if let Some(b) = blob {
        parts.push(format!(r#"<div id="collectors-data" data-blob="{}"></div>"#, esc(&b.to_string())));
    }
    format!("<html><body>{}</body></html>", parts.concat())
}

#[test]
fn parse_collectors_reads_the_page_blob() {
    let found = parse_collectors(&collectors_page(Some(&collectors_blob())));
    assert_eq!(found.thumbs.iter().map(|c| c.username.as_str()).collect::<Vec<_>>(), ["dumiovoxo", "exm0nster"]);
    assert!(found.thumbs[0].name == "Dumi" && found.thumbs[0].image_id == Some(25165605));
    assert_eq!(found.thumbs[0].url(), "https://bandcamp.com/dumiovoxo");
    assert!(found.more_thumbs && !found.more_reviews);
    let review = &found.reviews[0];
    assert_eq!(review.why.as_deref(), Some("This hipnotize my soul"));
    assert_eq!(review.fav_track.as_deref(), Some("Tempest"));
    assert_eq!(review.token.as_deref(), Some("1:1784630191:13644698:1:1:0"));
}

#[test]
fn a_page_with_no_buyers_parses_to_nothing() {
    let found = parse_collectors(&collectors_page(None));
    assert!(found.thumbs.is_empty() && found.reviews.is_empty() && !found.more_thumbs);
}

#[test]
fn collectors_from_results_skips_rows_without_a_username() {
    let rows = json!([{"fan_id": 1, "name": "No User"}, "junk", {"username": "x", "name": "  ", "why": "  "}]);
    let got = collectors_from_results(Some(&rows));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].name, "x");
    assert_eq!(got[0].why, None);
    assert!(collectors_from_results(Some(&json!({"a": 1}))).is_empty());
    assert!(collectors_from_results(None).is_empty());
}

// ---------------------------------------------------------------------------
// test_explore.py (pure extract cases)
// ---------------------------------------------------------------------------

fn explore_tralbum() -> Value {
    json!({
        "for the curious": "note",
        "item_type": "album",
        "id": 1,
        "artist": "Somatic",
        "current": {"title": "Grid Failure", "band_id": 2, "minimum_price": 0.0},
        "trackinfo": [
            {"title": "Nocturnal Transit", "track_num": 1, "track_id": 111,
             "file": {"mp3-128": "//t4.bcbits.com/stream/abc/mp3-128/111?token=1"}},
            {"title": "Iron Lung", "track_num": 2, "track_id": 112,
             "file": {"mp3-128": "https://t4.bcbits.com/stream/def/mp3-128/112?token=2"}},
            {"title": "Unreleased", "track_num": 3, "track_id": 113},
        ],
    })
}

fn tralbum_page(t: &Value) -> String {
    format!(r#"<html><body><script data-tralbum="{}"></script></body></html>"#, esc(&t.to_string()))
}

#[test]
fn stream_urls_are_extracted_and_made_absolute() {
    let r = parse_tralbum(&tralbum_page(&explore_tralbum()), URL);
    assert_eq!(r.tracks[0].stream_url.as_deref(), Some("https://t4.bcbits.com/stream/abc/mp3-128/111?token=1"));
    assert_eq!(r.tracks[1].stream_url.as_deref(), Some("https://t4.bcbits.com/stream/def/mp3-128/112?token=2"));
}

#[test]
fn a_track_with_no_file_map_has_no_stream() {
    let r = parse_tralbum(&tralbum_page(&explore_tralbum()), URL);
    assert_eq!(r.tracks[2].stream_url, None);
}

#[test]
fn an_unknown_encoding_is_not_guessed_at() {
    let mut blob = explore_tralbum();
    blob["trackinfo"] = json!([{"title": "x", "track_id": 9, "file": {"opus-256": "//x/y"}}]);
    assert_eq!(parse_tralbum(&tralbum_page(&blob), URL).tracks[0].stream_url, None);
}

fn band_page() -> String {
    r#"
<html><body>
  <script data-band="{band}"></script>
  <p id="band-name-location"><span class="title">Hyperdub</span>
     <span class="location">London, UK</span></p>
  <div id="bio-text">Music label from South East London</div>
  <div class="band-photo-container"><img src="https://f4.bcbits.com/img/band.jpg"/></div>
  <ol class="artists-grid">
    <li class="artists-grid-item" data-item-id="1">
      <a href="/artist/burial"><div class="artists-grid-name">Burial</div></a>
    </li>
  </ol>
</body></html>
"#
    .replace("{band}", &esc(&json!({"id": 42, "name": "Hyperdub"}).to_string()))
}

#[test]
fn band_profile_reads_identity_and_detects_a_label() {
    let p = parse_band_profile(&band_page(), "https://hyperdub.bandcamp.com/music");
    assert_eq!(p.name, "Hyperdub");
    assert_eq!(p.band_id, Some(42));
    assert_eq!(p.location.as_deref(), Some("London, UK"));
    assert!(p.bio.as_deref().unwrap().starts_with("Music label"));
    assert_eq!(p.image_url.as_deref(), Some("https://f4.bcbits.com/img/band.jpg"));
    assert!(p.is_label);
    assert_eq!(p.url, "https://hyperdub.bandcamp.com");
}

fn band_blob_page(blob: &Value) -> String {
    r#"<html><body><script data-band="{band}"></script></body></html>"#.replace("{band}", &esc(&blob.to_string()))
}

#[test]
fn a_label_without_a_roster_grid_is_still_a_label() {
    let p = parse_band_profile(
        &band_blob_page(&json!({"id": 7, "name": "Ostgut Ton", "is_label": true})),
        "https://ostgut.bandcamp.com/music",
    );
    assert!(p.is_label);
}

#[test]
fn an_artist_blob_does_not_make_a_label() {
    let p = parse_band_profile(
        &band_blob_page(&json!({"id": 8, "name": "Burial", "is_label": false})),
        "https://burial.bandcamp.com/music",
    );
    assert!(!p.is_label);
}

#[test]
fn a_bare_page_still_yields_a_usable_profile() {
    let p = parse_band_profile("<html><body></body></html>", "https://somatic.bandcamp.com/music");
    assert_eq!(p.name, "somatic");
    assert!(!p.is_label);
    assert_eq!(p.bio, None);
}

#[test]
fn band_profile_collects_links_and_falls_back_to_og_description() {
    let page = r#"<html><head><meta property="og:description" content="About us"></head><body>
        <ol id="band-links"><li><a href="https://x.test">Site</a></li><li><a href="">Empty</a></li></ol></body></html>"#;
    let p = parse_band_profile(page, "https://z.bandcamp.com");
    assert_eq!(p.bio.as_deref(), Some("About us"));
    assert_eq!(p.links.len(), 1);
    assert_eq!(p.links[0]["label"], "Site");
    assert_eq!(p.links[0]["url"], "https://x.test");
}

const RECS_PAGE: &str = r#"
<html><body><div class="recommendations-container">
  <ul class="horizontal">
    <li class="recommended-album footer-bcw"
        id="id-27120331"
        data-albumid="27120331"
        data-albumtitle="In My Dreams"
        data-artist="Girls of the Internet">
      <img class="album-art" src="https://f4.bcbits.com/img/a4113854720_1x1_120.jpg">
      <a class="album-link" href="https://houseoftheinternet.bandcamp.com/album/in-my-dreams?from=footer-bcw-a1">
        <span class="release-title">In My Dreams</span>
      </a>
    </li>
    <li class="recommended-album footer-nn" data-albumid="99" data-albumtitle="A Single"
        data-artist="Someone">
      <a class="album-link" href="https://someone.bandcamp.com/track/a-single?from=footer-nn-a2"></a>
    </li>
    <!-- A malformed entry must drop itself, not the strip. -->
    <li class="recommended-album" data-albumtitle="No Link"></li>
  </ul>
</div></body></html>
"#;

#[test]
fn recommendations_are_read_off_the_release_page() {
    let items = parse_recommendations(RECS_PAGE);
    assert_eq!(items.len(), 2);
    let first = &items[0];
    assert_eq!(first.page_url, "https://houseoftheinternet.bandcamp.com/album/in-my-dreams");
    assert_eq!(first.title, "In My Dreams");
    assert_eq!(first.artist, "Girls of the Internet");
    assert_eq!(first.item_type, "album");
    assert_eq!(first.art_id, Some(4113854720));
    assert_eq!(items[1].item_type, "track");
    assert_eq!(items[1].art_id, None);
    assert_eq!(items[1].bc_item_id, Some(99));
}

// ---------------------------------------------------------------------------
// Remaining extract.py surface (no Python test): facets, track album, url shim.
// ---------------------------------------------------------------------------

#[test]
fn discover_facets_try_selectors_in_order() {
    let blob = json!({"appData": {"initialState": {"genres": [{"slug": "techno"}], "times": [], "junk": [1]}}});
    let page = format!(r#"<div id="DiscoverApp" data-blob="{}"></div>"#, esc(&blob.to_string()));
    let facets = parse_discover_facets(&page);
    assert_eq!(facets["genres"], vec![json!({"slug": "techno"})]);
    assert!(facets["times"].is_empty());
    assert!(!facets.contains_key("junk"));
    assert!(parse_discover_facets("<html></html>").is_empty());
    // Moved to a bare [data-blob] element: still found.
    let moved = format!(r#"<section data-blob="{}"></section>"#, esc(&blob.to_string()));
    assert!(parse_discover_facets(&moved).contains_key("genres"));
}

#[test]
fn track_album_from_blob_and_css_floor() {
    let tr = json!({"item_type": "track", "album_url": "/album/the-record"});
    assert_eq!(
        parse_track_album(&tralbum_page(&tr), "https://a.bandcamp.com/track/t?from=x").as_deref(),
        Some("https://a.bandcamp.com/album/the-record")
    );
    let single = json!({"item_type": "track"});
    assert_eq!(parse_track_album(&tralbum_page(&single), "https://a.bandcamp.com/track/t"), None);
    let album = json!({"item_type": "album"});
    assert_eq!(
        parse_track_album(&tralbum_page(&album), "https://a.bandcamp.com/album/z/").as_deref(),
        Some("https://a.bandcamp.com/album/z")
    );
    let css = r#"<h3 class="albumTitle"><a href="/album/from-css">x</a></h3><a href="/album/sidebar">s</a>"#;
    assert_eq!(
        parse_track_album(css, "https://a.bandcamp.com/track/t").as_deref(),
        Some("https://a.bandcamp.com/album/from-css")
    );
    assert_eq!(parse_track_album(r#"<a href="/album/sidebar">s</a>"#, "https://a.bandcamp.com/track/t"), None);
}

#[test]
fn url_shim_matches_the_python_helpers() {
    use crate::urls::*;
    assert_eq!(normalise("HTTPS://A.Bandcamp.com:443/album/x/?from=y&b=2&a=1&utm_source=z"), "https://a.bandcamp.com/album/x?a=1&b=2");
    assert_eq!(artist_root("https://Hyper.bandcamp.com/music"), "https://hyper.bandcamp.com");
    assert_eq!(display_name("https://somatic.bandcamp.com/music"), "somatic");
    assert_eq!(display_name("https://bandcamp.com/discover/techno/x?tags=dub  techno"), "discover-techno-x-dub-techno");
    assert_eq!(display_name("https://bandcamp.com/someone"), "fan-someone");
    assert_eq!(display_name("https://custom.example.com/"), "custom-example-com");
    assert_eq!(build_art_url(Some(5)).as_deref(), Some("https://f4.bcbits.com/img/a5_16.jpg"));
    assert_eq!(build_art_url(None), None);
}

#[test]
fn a_preorder_marks_the_tracks_without_a_file_as_not_available() {
    let mut t = full_tralbum();
    t["is_preorder"] = json!(true);
    t["album_release_date"] = json!("09 Oct 2026 00:00:00 GMT");
    t["trackinfo"] = json!([
        {"title": "Ghost", "track_num": 1, "duration": 300.0, "file": {"mp3-128": "https://t4.bcbits.com/stream/x/mp3-128/1"}},
        {"title": "Extrude", "track_num": 2, "duration": 280.0, "file": null},
        {"title": "Quadrant", "track_num": 3, "duration": null, "file": null},
        {"title": "Destroy To Create", "track_num": 4, "file": null},
    ]);
    let r = parse_tralbum(&album_page(Some(&t), Some(&band()), Some(&ld()), false), URL);
    assert!(r.is_preorder);
    let a = r.availability();
    assert_eq!(a.release_date.as_deref(), Some("2026-10-09"));
    assert!(a.is_preorder);
    assert_eq!(a.tracks.iter().map(|t| t.available).collect::<Vec<_>>(), vec![true, false, false, false]);
    assert_eq!(a.unreleased().map(|t| t.title.as_str()).collect::<Vec<_>>(), vec!["Extrude", "Quadrant", "Destroy To Create"]);
    assert_eq!(a.tracks[1].duration_sec, Some(280.0));
    assert_eq!(a.tracks[2].duration_sec, None);
}
