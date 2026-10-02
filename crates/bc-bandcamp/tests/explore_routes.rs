//! The explore routes against a fake Bandcamp: search, discover, band, related, and the two
//! download routes. (The stream proxy has its own file, `explore_stream.rs`.)

mod fakebc;

use axum::Router;
use bc_bandcamp::net::PageKind;
use fakebc::{Resp, Site, TestCtx, discover_row, grid_entry, music_page, q, tralbum_page};
use serde_json::{Value, json};

fn rig(label: &str) -> (Site, TestCtx, Router) {
    let site = Site::new(label);
    let t = fakebc::test_ctx(&site);
    let app = bc_bandcamp::api::explore::router(t.ctx.clone());
    (site, t, app)
}

fn autocomplete(site: &Site, results: Vec<Value>) {
    site.post_json("/api/bcsearch_public_api/1/autocomplete_elastic", json!({"auto": {"results": results}}));
}

// -- search ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn search_keeps_bandcamps_order_honours_limit_and_kind() {
    let (site, _t, app) = rig("search");
    autocomplete(
        &site,
        (0..5).map(|i| json!({"type": "a", "name": format!("Album {i}"), "band_name": "B", "url": format!("https://b.bandcamp.com/album/a{i}"), "id": i})).collect(),
    );
    let (status, body) = fakebc::get_json(&app, "/explore/search?q=burial&kind=album&limit=3").await;
    assert_eq!(status.as_u16(), 200, "{body}");
    let names: Vec<_> = body.as_array().unwrap().iter().map(|h| h["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Album 0", "Album 1", "Album 2"]);
    let posts = site.hits_to("POST", "/api/bcsearch_public_api/1/autocomplete_elastic");
    assert_eq!(posts[0].json()["search_filter"], "a");
    assert_eq!(posts[0].json()["search_text"], "burial");
    // The cookie-less, referer-pinned search the legacy used.
    assert_eq!(posts[0].json()["full_page"], false);
}

#[tokio::test]
async fn search_badges_what_the_shelf_holds() {
    let (site, t, app) = rig("searchbadge");
    let rid = fakebc::owned_release(&t.ctx.db, "Burial", "Untrue");
    fakebc::blacklist_url(&t.ctx.db, "https://x.bandcamp.com/album/thrown-away");
    autocomplete(
        &site,
        vec![
            // Held under no URL at all: matched by (artist, title).
            json!({"type": "a", "name": "Untrue", "band_name": "Burial", "url": "https://burial.bandcamp.com/album/untrue", "id": 1}),
            // A track hit plays just that track: in_library but never a library id.
            json!({"type": "t", "name": "Untrue", "band_name": "Burial", "url": "https://burial.bandcamp.com/track/untrue", "id": 2}),
            json!({"type": "b", "name": "Burial", "url": "https://burial.bandcamp.com", "location": "London", "id": 3}),
            json!({"type": "a", "name": "Thrown Away", "band_name": "X", "url": "https://x.bandcamp.com/album/thrown-away", "id": 4}),
            json!({"type": "a", "name": "Unknown", "band_name": "Nobody", "url": "https://n.bandcamp.com/album/unknown", "id": 5}),
        ],
    );
    let (_, body) = fakebc::get_json(&app, "/explore/search?q=burial").await;
    let hits = body.as_array().unwrap();
    assert_eq!((hits[0]["in_library"].clone(), hits[0]["library_release_id"].clone()), (json!(true), json!(rid)));
    assert_eq!((hits[1]["in_library"].clone(), hits[1]["library_release_id"].clone()), (json!(true), Value::Null));
    assert_eq!(hits[2]["in_library"], false, "a band page is not something the library can own");
    assert_eq!(hits[2]["subtitle"], "London");
    assert_eq!((hits[3]["in_library"].clone(), hits[3]["blacklisted"].clone()), (json!(false), json!(true)));
    assert_eq!((hits[4]["in_library"].clone(), hits[4]["blacklisted"].clone()), (json!(false), json!(false)));
}

#[tokio::test]
async fn search_validates_its_query_and_translates_failures() {
    let (site, _t, app) = rig("searchbad");
    for uri in ["/explore/search", "/explore/search?q=", "/explore/search?q=x&kind=bogus", "/explore/search?q=x&limit=0", "/explore/search?q=x&limit=101"] {
        assert_eq!(fakebc::get(&app, uri).await.0.as_u16(), 422, "{uri}");
    }
    let long = "x".repeat(201);
    assert_eq!(fakebc::get(&app, &format!("/explore/search?q={long}")).await.0.as_u16(), 422);

    site.post_json("/api/bcsearch_public_api/1/autocomplete_elastic", json!({"__api_special__": "exception", "error_type": "Endpoints::MissingParamError"}));
    let (status, body) = fakebc::get_json(&app, "/explore/search?q=x").await;
    assert_eq!(status.as_u16(), 400, "{body}");
    site.post_json("/api/bcsearch_public_api/1/autocomplete_elastic", json!({"error": true, "error_message": "must be logged in"}));
    assert_eq!(fakebc::get(&app, "/explore/search?q=x").await.0.as_u16(), 401, "IdentityExpired -> 401");
}

// -- genres -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn genres_pass_the_discover_vocabulary_through() {
    let (site, t, app) = rig("genres");
    // The facets URL is fixed to https://bandcamp.com, which a plain-HTTP fake cannot serve:
    // seed the page cache like a previous visit would have.
    let blob = json!({"appData": {"initialState": {"genres": [{"slug": "techno"}], "times": []}}});
    let page = format!(r#"<div id="DiscoverApp" data-blob="{}"></div>"#, fakebc::esc_json(&blob));
    t.ctx.client.cache().unwrap().put("https://bandcamp.com/discover/electronic", PageKind::Discover, &page, None).unwrap();
    let (status, body) = fakebc::get_json(&app, "/explore/genres").await;
    assert_eq!(status.as_u16(), 200, "{body}");
    assert_eq!(body["genres"], json!([{"slug": "techno"}]));
    assert!(site.hits().is_empty());
}

// -- discover ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn discover_normalises_tags_pages_by_cursor_and_badges_cards() {
    let (site, t, app) = rig("discover");
    fakebc::owned_release(&t.ctx.db, "Somatic", "Grid Failure");
    site.post_json(
        "/api/discover/1/discover_web",
        json!({"results": [
            discover_row("https://somatic.bandcamp.com/album/grid-failure", "Grid Failure", "Somatic", 1),
            discover_row("https://other.bandcamp.com/album/two", "Two", "Other", 2),
        ], "cursor": "next-1", "result_count": 434991}),
    );
    let uri = format!("/explore/discover?genre=electronic&tags={}&tags=berlin&slice=top&size=24&cursor=*", q("Dub Techno"));
    let (status, body) = fakebc::get_json(&app, &uri).await;
    assert_eq!(status.as_u16(), 200, "{body}");
    assert_eq!((body["cursor"].clone(), body["total"].clone()), (json!("next-1"), json!(434991)));
    assert_eq!(body["items"][0]["in_library"], true, "matched by name");
    assert_eq!(body["items"][1]["in_library"], false);
    // `?from=discover_page` is stripped, the art built from the image id.
    assert_eq!(body["items"][0]["url"], "https://somatic.bandcamp.com/album/grid-failure");
    assert_eq!(body["items"][1]["art_url"], "https://f4.bcbits.com/img/a4000000002_16.jpg");

    let payload = site.hits_to("POST", "/api/discover/1/discover_web")[0].json();
    assert_eq!(payload["tag_norm_names"], json!(["electronic", "dub-techno", "berlin"]));
    assert_eq!((payload["slice"].clone(), payload["size"].clone(), payload["cursor"].clone()), (json!("top"), json!(24), json!("*")));
    assert_eq!((payload["category_id"].clone(), payload["geoname_id"].clone(), payload["time_facet_id"].clone()), (json!(0), json!(0), Value::Null));

    for bad in ["size=0", "size=61", "category_id=x"] {
        assert_eq!(fakebc::get(&app, &format!("/explore/discover?{bad}")).await.0.as_u16(), 422, "{bad}");
    }
}

#[tokio::test]
async fn discover_ends_when_the_cursor_repeats() {
    let (site, _t, app) = rig("discoverend");
    site.post_json("/api/discover/1/discover_web", json!({"results": [discover_row("https://a.bandcamp.com/album/x", "X", "A", 1)], "cursor": "same", "result_count": 1}));
    let (_, body) = fakebc::get_json(&app, "/explore/discover?cursor=same").await;
    assert_eq!(body["cursor"], Value::Null, "a repeated cursor means the feed is exhausted");
}

// -- spotlight ----------------------------------------------------------------------------------------------

fn seller(band: &str, url: &str, credit: &str, image: Option<i64>) -> Value {
    json!({"item_url": format!("{url}/album/x?from=discover_page"), "title": "X", "band_name": band, "album_artist": credit,
           "band_url": format!("{url}?from=discover_page"), "band_location": "Berlin, Germany",
           "band_image": image.map(|i| json!({"image_id": i})), "primary_image": {"image_id": 99}})
}

#[tokio::test]
async fn spotlight_splits_sellers_into_artists_and_labels_and_caches() {
    let (site, _t, app) = rig("spotlight");
    site.post_json(
        "/api/discover/1/discover_web",
        json!({"results": [
            seller("Mogwai", "https://mogwai.bandcamp.com", "", Some(11)),
            seller("Hyperdub", "https://hyperdub.bandcamp.com", "Burial", Some(12)),
            seller("doseone", "https://doseone.bandcamp.com", "doseone, Fatboi Sharif & steel tipped dove", None),
            seller("Godspeed You! Black Emperor", "https://gybe.bandcamp.com", "Godspeed You Black Emperor!", None),
            // A page photo that is really a cover (`is_art`) takes the cover URL shape.
            {"band_name": "BoC", "band_url": "https://boc.bandcamp.com", "band_image": {"image_id": 31, "is_art": true}},
            // A second record from the same page: counted, not repeated; its own credit is enough to make a label.
            seller("Hyperdub", "https://hyperdub.bandcamp.com", "", Some(12)),
            seller("Cut Outs", "https://cutouts.bandcamp.com", "V/A", None),
            {"title": "no band"},
        ], "cursor": "c", "result_count": 6}),
    );
    let (status, body) = fakebc::get_json(&app, "/explore/spotlight").await;
    assert_eq!(status.as_u16(), 200, "{body}");
    let names = |k: &str| body[k].as_array().unwrap().iter().map(|b| b["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(names("artists"), ["Mogwai", "doseone", "Godspeed You! Black Emperor", "BoC"], "a collaboration naming the page, or the same name punctuated differently, stays an artist");
    assert_eq!(names("labels"), ["Hyperdub", "Cut Outs"], "feed order kept");
    assert_eq!(body["labels"][0]["url"], "https://hyperdub.bandcamp.com", "?from= stripped");
    assert_eq!(body["labels"][0]["releases"], 2);
    assert_eq!(body["artists"][0]["image_url"], "https://f4.bcbits.com/img/11_23.jpg", "the page's own photo");
    assert_eq!(body["artists"][1]["image_url"], "https://f4.bcbits.com/img/a99_23.jpg", "else the record's cover");
    assert_eq!(body["artists"][3]["image_url"], "https://f4.bcbits.com/img/a31_23.jpg");
    assert_eq!(body["artists"][0]["location"], "Berlin, Germany");

    let payload = site.hits_to("POST", "/api/discover/1/discover_web")[0].json();
    assert_eq!((payload["slice"].clone(), payload["tag_norm_names"].clone()), (json!("top"), json!([])), "best-selling, all genres");

    fakebc::get_json(&app, "/explore/spotlight").await;
    assert_eq!(site.count("/api/discover/1/discover_web"), 1, "a revisit is served from the cache");
    fakebc::get_json(&app, "/explore/spotlight?genre=Jazz").await;
    assert_eq!(site.hits_to("POST", "/api/discover/1/discover_web")[1].json()["tag_norm_names"], json!(["jazz"]), "each genre is its own entry");
}

// -- band -------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn band_shows_a_label_with_its_roster_and_badged_releases() {
    let (site, t, app) = rig("band");
    fakebc::owned_release(&t.ctx.db, "Someone", "Owned One");
    site.page(
        "/music",
        &music_page(&json!({"id": 42, "name": "Hyperdub", "is_label": true}), Some(&[grid_entry("owned-one", "Owned One", "Someone", 1), grid_entry("two", "Two", "", 2)])),
    );
    site.page(
        "/artists",
        r#"<ol class="artists-grid"><li class="artists-grid-item" data-item-id="1"><a href="/artist/burial"><div class="artists-grid-name">Burial</div></a></li></ol>"#,
    );
    let (status, body) = fakebc::get_json(&app, &format!("/explore/band?url={}", q(&site.url("/")))).await;
    assert_eq!(status.as_u16(), 200, "{body}");
    assert_eq!((body["kind"].clone(), body["name"].clone(), body["truncated"].clone()), (json!("label"), json!("Hyperdub"), json!(false)));
    assert_eq!(body["roster"][0]["name"], "Burial");
    let releases = body["releases"].as_array().unwrap();
    assert_eq!(releases.len(), 2);
    assert_eq!(releases[0]["in_library"], true);
    // The grid omits the artist on a single-artist page; the band's own name stands in.
    assert_eq!(releases[1]["artist_name"], "Hyperdub");
    assert_eq!(releases[1]["art_url"], "https://f4.bcbits.com/img/a4000000002_16.jpg");
}

#[tokio::test]
async fn band_flags_a_truncated_grid_and_keeps_artists_rosterless() {
    let (site, _t, app) = rig("bandtrunc");
    site.page("/music", &music_page(&json!({"id": 1, "name": "Solo", "is_label": false}), None));
    let (_, body) = fakebc::get_json(&app, &format!("/explore/band?url={}", q(&site.url("/")))).await;
    assert_eq!((body["kind"].clone(), body["truncated"].clone()), (json!("artist"), json!(true)));
    assert_eq!(body["roster"], json!([]));
    assert_eq!(site.count("/artists"), 0, "only a label pays for the second fetch");
    // The SSRF guard.
    assert_eq!(fakebc::get(&app, "/explore/band?url=https%3A%2F%2Fevil.example.com").await.0.as_u16(), 400);
    assert_eq!(fakebc::get(&app, "/explore/band").await.0.as_u16(), 422);
}

// -- related ---------------------------------------------------------------------------------------------------------

fn related_page(site: &Site) -> String {
    let recs = |slug: &str, title: &str| {
        format!(
            r#"<li class="recommended-album" data-albumid="1" data-albumtitle="{title}" data-artist="R"><a class="album-link" href="https://r.bandcamp.com/album/{slug}?from=footer-bcw-a1"></a></li>"#
        )
    };
    let tralbum = json!({"for the curious": "x", "item_type": "album", "id": 1, "artist": "Somatic",
        "current": {"title": "Grid Failure", "band_id": 2}, "trackinfo": []});
    let _ = site;
    tralbum_page(&tralbum).replace(
        "</body>",
        &format!(
            r#"<a class="tag">dub techno</a><a class="tag">berlin</a><div class="recommendations-container"><ul class="horizontal">{}{}{}{}</ul></div></body>"#,
            recs("r1", "R1"),
            recs("r2", "R2"),
            // Also in the catalogue below: shown once, here.
            r#"<li class="recommended-album" data-albumid="8" data-albumtitle="M1" data-artist="S"><a class="album-link" href="HOST/album/m1"></a></li>"#.replace("HOST", &site.url("")),
            // Also the release itself: must not recommend itself.
            r#"<li class="recommended-album" data-albumid="9" data-albumtitle="Self" data-artist="S"><a class="album-link" href="HOST/album/grid-failure"></a></li>"#.replace("HOST", &site.url("")),
        ),
    )
}

fn related_site(label: &str) -> (Site, TestCtx, Router) {
    let (site, t, app) = rig(label);
    site.page("/album/grid-failure", &related_page(&site));
    site.page(
        "/music",
        &music_page(
            &json!({"id": 2, "name": "Somatic", "is_label": false}),
            Some(&[grid_entry("grid-failure", "Grid Failure", "", 1), grid_entry("m1", "More One", "", 2), grid_entry("m2", "More Two", "", 3)]),
        ),
    );
    site.route("POST", "/api/discover/1/discover_web", |hit| {
        let tag = hit.json()["tag_norm_names"][0].as_str().unwrap_or("").to_string();
        Resp::json(&json!({"results": [
            discover_row(&format!("https://t.bandcamp.com/album/{tag}-1"), &format!("{tag} one"), "T", 10),
            discover_row("https://b.bandcamp.com/album/shared", "Shared", "B", 11),
        ], "cursor": format!("c-{tag}"), "result_count": 99}))
    });
    (site, t, app)
}

#[tokio::test]
async fn related_gathers_three_kinds_of_neighbour_and_scans_each_record_once() {
    let (site, t, app) = related_site("related");
    fakebc::owned_release(&t.ctx.db, "T", "dub-techno one");
    let (status, body) = fakebc::get_json(&app, &format!("/explore/related?url={}", q(&site.url("/album/grid-failure")))).await;
    assert_eq!(status.as_u16(), 200, "{body}");
    let sections = body["sections"].as_array().unwrap();
    let keys: Vec<_> = sections.iter().map(|s| s["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["recommended", "band", "tag:dub-techno", "tag:berlin"]);

    let urls = |s: &Value| -> Vec<String> { s["items"].as_array().unwrap().iter().map(|i| i["url"].as_str().unwrap().to_string()).collect() };
    // The release itself is nobody's neighbour.
    assert_eq!(urls(&sections[0])[..2], ["https://r.bandcamp.com/album/r1", "https://r.bandcamp.com/album/r2"]);
    assert_eq!(sections[0]["title"], "You may also like");
    assert_eq!(urls(&sections[0]).len(), 3);
    // m1 already shown above; grid-failure is the release.
    assert_eq!(urls(&sections[1]), [site.url("/album/m2")]);
    assert_eq!((sections[1]["title"].clone(), sections[1]["url"].clone(), sections[1]["total"].clone()), (json!("More from Somatic"), json!(site.url("")), json!(3)));
    // `shared` shows once, under the first tag that has it.
    assert_eq!(urls(&sections[2]), ["https://t.bandcamp.com/album/dub-techno-1", "https://b.bandcamp.com/album/shared"]);
    assert_eq!(urls(&sections[3]), ["https://t.bandcamp.com/album/berlin-1"]);
    assert_eq!((sections[2]["tag"].clone(), sections[2]["cursor"].clone(), sections[2]["total"].clone()), (json!("dub-techno"), json!("c-dub-techno"), json!(99)));
    // One library lookup, badges on every section.
    assert_eq!(sections[2]["items"][0]["in_library"], true);
    assert_eq!(sections[2]["items"][1]["in_library"], false);
    // The release page was fetched once for release + recommendations.
    assert_eq!(site.count("/album/grid-failure"), 1);
    // slice defaults to "top".
    assert_eq!(site.hits_to("POST", "/api/discover/1/discover_web")[0].json()["slice"], "top");
}

#[tokio::test]
async fn a_failing_section_drops_only_itself() {
    let (site, _t, app) = related_site("relfail");
    site.route("GET", "/music", |_| Resp::status(500)); // the catalogue fails
    site.route("POST", "/api/discover/1/discover_web", |hit| {
        // …and so does the first tag's feed (HTTP 200 + error body, as Bandcamp does).
        if hit.json()["tag_norm_names"][0] == "dub-techno" {
            Resp::json(&json!({"error": true, "error_message": "rate limited"}))
        } else {
            Resp::json(&json!({"results": [discover_row("https://t.bandcamp.com/album/berlin-1", "b", "T", 10)], "cursor": "c", "result_count": 1}))
        }
    });
    let (status, body) = fakebc::get_json(&app, &format!("/explore/related?url={}", q(&site.url("/album/grid-failure")))).await;
    assert_eq!(status.as_u16(), 200, "{body}");
    let keys: Vec<_> = body["sections"].as_array().unwrap().iter().map(|s| s["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["recommended", "tag:berlin"], "a rate-limited feed must not blank the page");
}

#[tokio::test]
async fn related_options_choose_the_neighbours() {
    let (site, _t, app) = related_site("relopts");
    let base = format!("/explore/related?url={}", q(&site.url("/album/grid-failure")));
    // include_band=false never touches /music.
    let (_, body) = fakebc::get_json(&app, &format!("{base}&include_band=false&tag_limit=1")).await;
    let keys: Vec<_> = body["sections"].as_array().unwrap().iter().map(|s| s["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["recommended", "tag:dub-techno"]);
    assert_eq!(site.count("/music"), 0);
    // Caller-named tags replace the release's own; tag_limit=0 means none; slice is forwarded.
    let (_, body) = fakebc::get_json(&app, &format!("{base}&include_band=false&tags=house&slice=new")).await;
    let keys: Vec<_> = body["sections"].as_array().unwrap().iter().map(|s| s["key"].as_str().unwrap()).collect();
    assert_eq!(keys, ["recommended", "tag:house"]);
    let (_, body) = fakebc::get_json(&app, &format!("{base}&include_band=false&tag_limit=0")).await;
    assert_eq!(body["sections"].as_array().unwrap().len(), 1);
    assert!(site.hits_to("POST", "/api/discover/1/discover_web").iter().any(|h| h.json()["slice"] == "new"));
    // Limits: size 6..=60, tag_limit 0..=6.
    for bad in ["size=5", "size=61", "tag_limit=7", "include_band=maybe"] {
        assert_eq!(fakebc::get(&app, &format!("{base}&{bad}")).await.0.as_u16(), 422, "{bad}");
    }
}

// -- downloads ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn download_queues_a_job_and_skips_what_the_library_holds() {
    let (site, t, app) = rig("download");
    let db = &t.ctx.db;
    // Known by URL (a done download), blacklisted, and held by name only.
    let known = site.url("/album/known");
    let blocked = site.url("/album/blocked");
    let fresh = site.url("/album/fresh");
    let rid = fakebc::owned_release(db, "A", "Known");
    db.write({
        let (known, rid) = (bc_bandcamp::urls::normalise(&known), rid);
        move |tx| {
            tx.execute("INSERT INTO jobs(id, kind, status, priority, params, total, completed, failed, skipped, cancel_requested, created_at) VALUES ('old','download','completed',0,'{}',1,1,0,0,0,datetime('now'))", [])?;
            tx.execute(
                "INSERT INTO job_items(job_id, seq, status, url, url_kind, attempts, max_attempts, progress, release_id) VALUES ('old',0,'done',?1,'album',1,3,1.0,?2)",
                bc_db::rusqlite::params![known, rid],
            )?;
            Ok(())
        }
    })
    .unwrap();
    fakebc::blacklist_url(db, &bc_bandcamp::urls::normalise(&blocked));

    let (status, job) = fakebc::post_json(
        &app,
        "/explore/download",
        &json!({"urls": [known, blocked, fresh, fresh.clone() + "?from=x"], "target_subdir": "My/Crate", "label_name": " Hyperdub ", "label_url": "https://hyperdub.bandcamp.com", "source_fan_id": null}),
    )
    .await;
    assert_eq!(status.as_u16(), 200, "{job}");
    assert_eq!(job["kind"], "download");
    assert_eq!(job["label"], "4 from Explore", "explore always labels its jobs; the count is of what was sent");
    assert_eq!((job["total"].clone(), job["skipped"].clone(), job["status"].clone()), (json!(3), json!(2), json!("queued")), "duplicates collapse; known ones are created but skipped");

    let id = job["id"].as_str().unwrap();
    let items = fakebc::job_items(db, id);
    let by_url = |u: &str| items.iter().find(|i| i.2.as_deref() == Some(bc_bandcamp::urls::normalise(u).as_str())).unwrap().clone();
    assert_eq!(by_url(&known).0, "skipped");
    assert_eq!(by_url(&known).1.as_deref(), Some("Already in library — skipped"));
    assert_eq!(by_url(&blocked).1.as_deref(), Some("Blacklisted — skipped"));
    assert_eq!(by_url(&fresh).0, "pending");
    assert_eq!(by_url(&fresh).3.as_deref(), Some("My_Crate"));
    let stored = t.ctx.jobs.store().get_job(id).unwrap().unwrap();
    let params = stored.params_json();
    assert_eq!((params["target_subdir"].clone(), params["force"].clone()), (json!("My_Crate"), json!(false)));
    assert_eq!((params["label_name"].clone(), params["label_url"].clone()), (json!("Hyperdub"), json!("https://hyperdub.bandcamp.com")));
    assert!(params.get("source_fan_id").is_none());
}

#[tokio::test]
async fn download_with_a_fan_shelf_and_default_label_and_everything_known() {
    let (site, t, app) = rig("download2");
    let u = site.url("/album/a");
    let (_, job) = fakebc::post_json(&app, "/explore/download", &json!({"urls": [u], "source_fan_id": 7})).await;
    assert_eq!(job["label"], "1 from Explore");
    let stored = t.ctx.jobs.store().get_job(job["id"].as_str().unwrap()).unwrap().unwrap();
    assert_eq!(stored.params_json()["source_fan_id"], 7);
    assert!(stored.params_json()["target_subdir"].is_null());

    // Everything known: the job exists as a record, is already settled, and never runs.
    fakebc::blacklist_url(&t.ctx.db, &bc_bandcamp::urls::normalise(&u));
    let (status, job) = fakebc::post_json(&app, "/explore/download", &json!({"urls": [u]})).await;
    assert_eq!(status.as_u16(), 200);
    assert_eq!((job["status"].clone(), job["skipped"].clone()), (json!("completed"), json!(1)));
}

#[tokio::test]
async fn download_refuses_bad_input() {
    let (_site, _t, app) = rig("downloadbad");
    let (s, b) = fakebc::post_json(&app, "/explore/download", &json!({"urls": []})).await;
    assert_eq!((s.as_u16(), b["detail"].clone()), (400, json!("no releases to queue")));
    let (s, _) = fakebc::post_json(&app, "/explore/download", &json!({"urls": ["https://evil.example.com/album/x"]})).await;
    assert_eq!(s.as_u16(), 400, "SSRF guard");
    // A band page is not something bandcamp-dl can download.
    let (s, b) = fakebc::post_json(&app, "/explore/download", &json!({"urls": ["https://a.bandcamp.com/music"]})).await;
    assert_eq!(s.as_u16(), 400);
    assert!(b["detail"].as_str().unwrap().contains("No valid Bandcamp album or track URLs"), "{b}");
    let many: Vec<String> = (0..501).map(|i| format!("https://a.bandcamp.com/album/a{i}")).collect();
    assert_eq!(fakebc::post_json(&app, "/explore/download", &json!({"urls": many})).await.0.as_u16(), 422);
}

fn free_page(minimum: f64) -> String {
    tralbum_page(&json!({"for the curious": "x", "item_type": "album", "id": 1, "artist": "A",
        "current": {"title": "T", "band_id": 2, "minimum_price": minimum}, "trackinfo": []}))
}

fn catalogue_site(label: &str, is_label: bool, n: usize) -> (Site, TestCtx, Router) {
    let (site, t, app) = rig(label);
    let tail: Vec<Value> = (1..=n).map(|i| grid_entry(&format!("r{i}"), &format!("Release {i}"), "Artist", i as i64)).collect();
    site.page("/music", &music_page(&json!({"id": 5, "name": "Some Label", "is_label": is_label}), Some(&tail)));
    (site, t, app)
}

#[tokio::test]
async fn catalog_queues_the_discography_minus_the_shelf_as_one_job() {
    let (site, t, app) = catalogue_site("catalog", true, 4);
    fakebc::owned_release(&t.ctx.db, "Artist", "Release 2");
    let (status, out) = fakebc::post_json(&app, "/explore/download/catalog", &json!({"url": site.url("/")})).await;
    assert_eq!(status.as_u16(), 200, "{out}");
    assert_eq!((out["band"].clone(), out["found"].clone(), out["queued"].clone(), out["skipped_in_library"].clone()), (json!("Some Label"), json!(4), json!(3), json!(1)));
    assert_eq!((out["truncated"].clone(), out["detail"].clone()), (json!(false), json!("")));
    assert_eq!(out["job"]["label"], "Some Label — full catalogue");
    let id = out["job"]["id"].as_str().unwrap();
    let items = fakebc::job_items(&t.ctx.db, id);
    assert_eq!(items.len(), 3);
    assert!(items.iter().all(|i| i.3.as_deref() == Some("Some Label")), "filed under the band's folder");
    let params = t.ctx.jobs.store().get_job(id).unwrap().unwrap().params_json();
    assert_eq!((params["label_name"].clone(), params["label_url"].clone()), (json!("Some Label"), json!(site.url(""))), "a label's catalogue names the label");

    // An artist's catalogue names no label, and a custom folder wins.
    let (site, _t, app) = catalogue_site("catalogartist", false, 2);
    let (_, out) = fakebc::post_json(&app, "/explore/download/catalog", &json!({"url": site.url("/"), "target_subdir": "Custom"})).await;
    let params = _t.ctx.jobs.store().get_job(out["job"]["id"].as_str().unwrap()).unwrap().unwrap().params_json();
    assert!(params.get("label_name").is_none());
    assert_eq!(params["target_subdir"], "Custom");
}

#[tokio::test]
async fn catalog_limit_truncates_and_says_so() {
    let (site, _t, app) = catalogue_site("cataloglimit", false, 5);
    let (_, out) = fakebc::post_json(&app, "/explore/download/catalog", &json!({"url": site.url("/"), "limit": 2})).await;
    assert_eq!((out["found"].clone(), out["queued"].clone(), out["truncated"].clone()), (json!(2), json!(2), json!(true)));
    assert_eq!(out["detail"], "the discography grid was truncated — some releases may be missing");
    for bad in [json!({"url": site.url("/"), "limit": 0}), json!({"url": site.url("/"), "limit": 2001})] {
        assert_eq!(fakebc::post_json(&app, "/explore/download/catalog", &bad).await.0.as_u16(), 422);
    }
}

#[tokio::test]
async fn catalog_free_only_probes_each_release() {
    let (site, _t, app) = catalogue_site("catalogfree", false, 3);
    site.page("/album/r1", &free_page(0.0));
    site.page("/album/r2", &free_page(7.0));
    // r3 cannot be probed: dropped, not counted as paid.
    let (_, out) = fakebc::post_json(&app, "/explore/download/catalog", &json!({"url": site.url("/"), "free_only": true})).await;
    assert_eq!((out["queued"].clone(), out["skipped_not_free"].clone()), (json!(1), json!(1)), "{out}");
    assert_eq!(site.count("/album/r1") + site.count("/album/r2") + site.count("/album/r3"), 3);
}

#[tokio::test]
async fn catalog_of_a_shelf_already_held_queues_nothing_but_files_the_label() {
    let (site, t, app) = catalogue_site("catalogheld", true, 2);
    let r1 = fakebc::owned_release(&t.ctx.db, "Artist", "Release 1");
    let r2 = fakebc::owned_release(&t.ctx.db, "Artist", "Release 2");
    let (status, out) = fakebc::post_json(&app, "/explore/download/catalog", &json!({"url": site.url("/")})).await;
    assert_eq!(status.as_u16(), 200, "{out}");
    assert_eq!((out["queued"].clone(), out["skipped_in_library"].clone(), out["job"].clone()), (json!(0), json!(2), Value::Null));
    assert!(out["detail"].as_str().unwrap().starts_with("everything here is already in the library"), "{out}");
    assert!(out["detail"].as_str().unwrap().contains("filed 2 under Some Label"), "{out}");
    let labelled: i64 = t
        .ctx
        .db
        .read(move |c| Ok(c.query_row("SELECT count(*) FROM releases WHERE id IN (?1, ?2) AND label_id IS NOT NULL", [r1, r2], |r| r.get(0))?))
        .unwrap();
    assert_eq!(labelled, 2, "a re-run of the button labels a library downloaded before labels were recorded");

    // An empty catalogue.
    let (site, _t, app) = catalogue_site("catalogempty", false, 0);
    let (_, out) = fakebc::post_json(&app, "/explore/download/catalog", &json!({"url": site.url("/")})).await;
    assert_eq!((out["queued"].clone(), out["detail"].clone()), (json!(0), json!("nothing to queue")));
}
