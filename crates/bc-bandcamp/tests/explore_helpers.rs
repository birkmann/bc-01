//! Port of `test_explore.py`: stream extraction, band profiles, search URLs, tag normalisation,
//! recommendations, the player-URL helper and the URL guard.

mod fakebc;

use bc_bandcamp::api::explore::common::{bandcamp_url, proxy_url};
use bc_bandcamp::extract::{HarvestedTrack, parse_band_profile, parse_recommendations, parse_tralbum};
use bc_bandcamp::sources::search_hit;
use bc_bandcamp::urls::norm_tag;
use fakebc::{esc_json, explore_tralbum, tralbum_page};
use serde_json::{Value, json};

const URL: &str = "https://somatic.bandcamp.com/album/grid-failure";

// -- stream URLs ------------------------------------------------------------------------------------

fn tralbum() -> Value {
    // `//host/...` (protocol-relative) on the first track, absolute on the second, like the fixture.
    let mut t = explore_tralbum("https://t4.bcbits.com");
    t["trackinfo"][0]["file"]["mp3-128"] = json!("//t4.bcbits.com/stream/abc/mp3-128/111?token=1");
    t
}

#[test]
fn stream_urls_are_extracted_and_made_absolute() {
    let r = parse_tralbum(&tralbum_page(&tralbum()), URL);
    // Bandcamp emits protocol-relative URLs; an <audio> src of "//..." on a page served over
    // http would silently downgrade the request.
    assert_eq!(r.tracks[0].stream_url.as_deref(), Some("https://t4.bcbits.com/stream/abc/mp3-128/111?token=1"));
    assert_eq!(r.tracks[1].stream_url.as_deref(), Some("https://t4.bcbits.com/stream/def/mp3-128/112?token=2"));
}

#[test]
fn a_track_with_no_file_map_has_no_stream() {
    // The UI has to be able to show an unplayable row, not a broken player.
    let r = parse_tralbum(&tralbum_page(&tralbum()), URL);
    assert_eq!(r.tracks[2].stream_url, None);
}

#[test]
fn an_unknown_encoding_is_not_guessed_at() {
    let mut blob = tralbum();
    blob["trackinfo"] = json!([{"title": "x", "track_id": 9, "file": {"opus-256": "//x/y"}}]);
    assert_eq!(parse_tralbum(&tralbum_page(&blob), URL).tracks[0].stream_url, None);
}

// -- player URLs ------------------------------------------------------------------------------------

#[test]
fn proxy_url_addresses_the_track_not_the_signed_cdn_url() {
    // The signed URL must not reach the browser: it expires, and it is a token.
    let track = HarvestedTrack {
        title: "x".into(),
        bc_track_id: Some(111),
        stream_url: Some("https://t4.bcbits.com/s?token=1".into()),
        ..Default::default()
    };
    let proxied = proxy_url(URL, &track, 0).expect("a stream means a player url");
    assert!(!proxied.contains("token"));
    assert!(!proxied.contains("t4.bcbits.com"));
    assert!(proxied.contains("track=111"));
    assert!(proxied.starts_with("/api/explore/stream?release=https%3A%2F%2Fsomatic.bandcamp.com%2Falbum%2Fgrid-failure"));
}

#[test]
fn proxy_url_falls_back_to_the_index_without_a_track_id() {
    let track = HarvestedTrack { title: "x".into(), stream_url: Some("https://t4.bcbits.com/s".into()), ..Default::default() };
    assert!(proxy_url(URL, &track, 2).unwrap_or_default().contains("track=i2"));
}

#[test]
fn no_stream_means_no_player_url() {
    assert_eq!(proxy_url(URL, &HarvestedTrack { title: "x".into(), ..Default::default() }, 0), None);
}

// -- the URL guard ------------------------------------------------------------------------------------

#[test]
fn non_bandcamp_urls_are_rejected() {
    // Every route hands its input to an HTTP client.
    for raw in [
        "https://evil.example.com/album/x",
        "http://127.0.0.1:8420/api/library/stats",
        "https://bandcamp.com.evil.test/album/x",
        "file:///etc/passwd",
        "",
        "https://evil.com@bandcamp.com/x",
        "https://notbandcamp.com/album/x",
    ] {
        let e = bandcamp_url(raw, "url").expect_err(raw);
        assert_eq!(e.problem.status, 400, "{raw}");
    }
}

#[test]
fn bandcamp_urls_are_accepted() {
    for raw in [
        "https://somatic.bandcamp.com/album/grid-failure",
        "somatic.bandcamp.com/album/grid-failure",
        "https://bandcamp.com/discover/techno",
        "HTTPS://Somatic.Bandcamp.com/album/grid-failure?from=x",
    ] {
        assert!(bandcamp_url(raw, "url").expect(raw).starts_with("https://"), "{raw}");
    }
    // Normalised on the way in: lower-case host, no tracking query.
    assert_eq!(bandcamp_url("HTTPS://Somatic.Bandcamp.com/album/grid-failure/?from=x", "url").unwrap(), URL);
    assert_eq!(bandcamp_url("  ", "release").unwrap_err().problem.detail.as_deref(), Some("release is required"));
}

#[tokio::test]
async fn the_stream_route_refuses_a_foreign_release() {
    let site = fakebc::Site::new("guard");
    let t = fakebc::test_ctx(&site);
    let app = bc_bandcamp::api::explore::router(t.ctx.clone());
    let (status, _) = fakebc::get_json(&app, &format!("/explore/stream?release={}&track=1", fakebc::q("https://evil.example.com/album/x"))).await;
    assert_eq!(status.as_u16(), 400);
}

// -- search result URLs ---------------------------------------------------------------------------------

#[test]
fn an_absolute_item_path_is_not_joined_to_its_root() {
    // Live data sends `item_url_path` already absolute; joining it to `item_url_root` produced
    // a doubled URL which 404s on every subsequent fetch.
    let hit = search_hit(&json!({
        "type": "a", "name": "Untrue", "band_name": "Burial",
        "item_url_root": "https://burial.bandcamp.com/",
        "item_url_path": "https://burial.bandcamp.com/album/untrue",
    }))
    .expect("a hit");
    assert_eq!(hit.url, "https://burial.bandcamp.com/album/untrue");
    assert_eq!(hit.subtitle, "Burial");
}

#[test]
fn a_relative_item_path_is_still_joined() {
    let hit = search_hit(&json!({
        "type": "a", "name": "Untrue",
        "item_url_root": "https://burial.bandcamp.com/", "item_url_path": "/album/untrue",
    }))
    .expect("a hit");
    assert_eq!(hit.url, "https://burial.bandcamp.com/album/untrue");
}

#[test]
fn release_art_is_rebuilt_rather_than_taken_from_the_response() {
    // Autocomplete's `img` drops the `a` prefix release art needs.
    let hit = search_hit(&json!({
        "type": "a", "name": "Untrue", "url": "https://burial.bandcamp.com/album/untrue",
        "art_id": 3726798017i64, "img": "https://f4.bcbits.com/img/3726798017_3.jpg",
    }))
    .expect("a hit");
    assert_eq!(hit.art_url.as_deref(), Some("https://f4.bcbits.com/img/a3726798017_2.jpg"));
}

#[test]
fn a_band_photo_keeps_its_bare_id() {
    // The `a` prefix is release-art-only; band images 404 with it.
    let hit = search_hit(&json!({"type": "b", "name": "Hyperdub", "url": "https://hyperdub.bandcamp.com", "img_id": 11559624}))
        .expect("a hit");
    assert_eq!(hit.art_url.as_deref(), Some("https://f4.bcbits.com/img/11559624_2.jpg"));
}

#[test]
fn an_entry_without_ids_falls_back_to_the_given_image() {
    let hit = search_hit(&json!({"type": "f", "name": "somefan", "url": "https://bandcamp.com/somefan", "img": "https://f4.bcbits.com/img/fan.jpg"}))
        .expect("a hit");
    assert_eq!(hit.art_url.as_deref(), Some("https://f4.bcbits.com/img/fan.jpg"));
}

#[test]
fn a_label_is_distinguished_from_an_artist() {
    // They share a page shape but not a page: only a label has a roster.
    let label = search_hit(&json!({"type": "b", "name": "Hyperdub", "url": "https://hyperdub.bandcamp.com", "is_label": true})).unwrap();
    assert_eq!(label.kind, "label");
    let artist = search_hit(&json!({"type": "b", "name": "Burial", "url": "https://burial.bandcamp.com", "is_label": false})).unwrap();
    assert_eq!(artist.kind, "artist");
}

// -- band profiles ------------------------------------------------------------------------------------

fn band_page() -> String {
    format!(
        r#"<html><body>
  <script data-band="{}"></script>
  <p id="band-name-location"><span class="title">Hyperdub</span>
     <span class="location">London, UK</span></p>
  <div id="bio-text">Music label from South East London</div>
  <div class="band-photo-container"><img src="https://f4.bcbits.com/img/band.jpg"/></div>
  <ol class="artists-grid">
    <li class="artists-grid-item" data-item-id="1">
      <a href="/artist/burial"><div class="artists-grid-name">Burial</div></a>
    </li>
  </ol>
</body></html>"#,
        esc_json(&json!({"id": 42, "name": "Hyperdub"}))
    )
}

#[test]
fn band_profile_reads_identity_and_detects_a_label() {
    let p = parse_band_profile(&band_page(), "https://hyperdub.bandcamp.com/music");
    assert_eq!(p.name, "Hyperdub");
    assert_eq!(p.band_id, Some(42));
    assert_eq!(p.location.as_deref(), Some("London, UK"));
    assert!(p.bio.as_deref().is_some_and(|b| b.starts_with("Music label")));
    assert_eq!(p.image_url.as_deref(), Some("https://f4.bcbits.com/img/band.jpg"));
    assert!(p.is_label);
    // artist_root, not the /music page it was parsed from.
    assert_eq!(p.url, "https://hyperdub.bandcamp.com");
}

fn band_blob_page(blob: &Value) -> String {
    format!(r#"<html><body><script data-band="{}"></script></body></html>"#, esc_json(blob))
}

#[test]
fn a_label_without_a_roster_grid_is_still_a_label() {
    // Many label /music pages render no artists grid -- the roster lives at /artists -- but
    // Bandcamp's own band blob says what the account is.
    let p = parse_band_profile(&band_blob_page(&json!({"id": 7, "name": "Ostgut Ton", "is_label": true})), "https://ostgut.bandcamp.com/music");
    assert!(p.is_label);
}

#[test]
fn an_artist_blob_does_not_make_a_label() {
    // `is_label: false` with no grid stays an artist -- filing a self-released catalogue under a
    // label named after its artist is the thing we never do.
    let p = parse_band_profile(&band_blob_page(&json!({"id": 8, "name": "Burial", "is_label": false})), "https://burial.bandcamp.com/music");
    assert!(!p.is_label);
}

#[test]
fn a_bare_page_still_yields_a_usable_profile() {
    // Themed pages drop half these elements; the header still has to render.
    let p = parse_band_profile("<html><body></body></html>", "https://somatic.bandcamp.com/music");
    assert_eq!(p.name, "somatic");
    assert!(!p.is_label);
    assert!(p.bio.is_none());
}

// -- tag normalisation ----------------------------------------------------------------------------------

#[test]
fn tags_are_normalised_for_discover() {
    // Discover silently returns zero results for an unnormalised tag. Verified live: `dub techno`
    // reported 0 releases, `dub-techno` 27,684.
    for (raw, expected) in [
        ("Dub Techno", "dub-techno"),
        ("dub techno", "dub-techno"),
        ("dub-techno", "dub-techno"),
        ("  Hip-Hop/Rap  ", "hip-hop-rap"),
        ("drum & bass", "drum-bass"),
        ("R&B", "r-b"),
        ("lo-fi   house", "lo-fi-house"),
    ] {
        assert_eq!(norm_tag(raw), expected, "{raw}");
    }
}

// -- related releases -------------------------------------------------------------------------------------

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
    // The tracking query is stripped, so the card dedupes against the library and against the
    // other related sections.
    assert_eq!(first.page_url, "https://houseoftheinternet.bandcamp.com/album/in-my-dreams");
    assert_eq!(first.title, "In My Dreams");
    assert_eq!(first.artist, "Girls of the Internet");
    assert_eq!(first.item_type, "album");
    assert_eq!(first.art_id, Some(4113854720));
    // /track/ URLs are singles, and the art is optional.
    assert_eq!(items[1].item_type, "track");
    assert_eq!(items[1].art_id, None);
}
