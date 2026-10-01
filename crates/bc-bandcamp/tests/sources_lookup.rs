//! `BcLookup`: `bc_maint::BandcampLookup` over the real client.

mod fakebc;

use bc_bandcamp::lookup::BcLookup;
use bc_maint::lookup::BandcampLookup;
use fakebc::{Site, tralbum_page};
use serde_json::json;

#[tokio::test]
async fn resolves_a_track_to_its_album_and_fetches_an_album() {
    let site = Site::new("lookup");
    site.page(
        "/track/t",
        &tralbum_page(&json!({"for the curious": "x", "item_type": "track", "id": 3, "artist": "A", "album_url": "/album/parent", "current": {"title": "T"}, "trackinfo": []})),
    );
    site.page(
        "/album/parent",
        &tralbum_page(&json!({"for the curious": "x", "item_type": "album", "id": 4, "artist": "Somatic",
            "current": {"title": "Parent", "release_date": "03 Mar 2020 00:00:00 GMT", "about": "About text", "credits": "Credits text", "band_id": 2},
            "trackinfo": [{"title": "One", "track_num": 1, "track_id": 1}, {"title": "Two", "track_num": 2, "track_id": 2}]})),
    );
    let lookup = BcLookup::new(site.client());
    assert_eq!(lookup.resolve_album_url(&site.url("/track/t")).await.unwrap(), site.url("/album/parent"));
    assert_eq!(lookup.resolve_album_url(&site.url("/album/parent")).await.unwrap(), site.url("/album/parent"));

    let album = lookup.fetch_album(&site.url("/album/parent")).await.unwrap();
    assert_eq!((album.title.as_str(), album.artist_name.as_str()), ("Parent", "Somatic"));
    assert_eq!(album.release_date.as_deref(), Some("2020-03-03"));
    assert_eq!((album.about.as_deref(), album.credits.as_deref()), (Some("About text"), Some("Credits text")));
    assert_eq!(album.tracks.iter().map(|t| (t.title.as_str(), t.track_num)).collect::<Vec<_>>(), [("One", Some(1)), ("Two", Some(2))]);
}

#[tokio::test]
async fn a_dead_page_is_a_lookup_error_not_a_panic() {
    let site = Site::new("lookup-dead");
    let lookup = BcLookup::new(site.client());
    let e = lookup.fetch_album(&site.url("/album/gone")).await.unwrap_err();
    assert!(e.to_string().contains("not found"), "{e}");
    assert!(lookup.resolve_album_url(&site.url("/track/gone")).await.is_err());
}
