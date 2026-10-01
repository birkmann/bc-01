//! Ports of test_library_scope (read half), test_favorites, test_label_edit (patch + size), test_release_shuffle,
//! plus filter/sort/paging behaviour of the track engine.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use serde_json::json;

fn titles(v: &serde_json::Value) -> Vec<String> {
    let mut t: Vec<String> = v["items"].as_array().unwrap().iter().map(|r| r["title"].as_str().unwrap().to_string()).collect();
    t.sort();
    t
}

// ------------------------------------------------------------------ test_library_scope (read half)

#[tokio::test]
async fn listings_hide_the_shelf_by_default_and_show_it_on_request() {
    let a = app();
    seed_scope(&a);
    assert_eq!(titles(&a.get("/releases").await), ["mine album", "other album"]);
    assert_eq!(titles(&a.get("/releases?scope=all").await), ["mine album", "other album", "shelf album"]);
    let shelf = a.get("/releases?source_fan_id=1").await;
    assert_eq!(titles(&shelf), ["shelf album"]);
    assert_eq!(shelf["items"][0]["source_fan_id"], 1);

    let tracks = a.get("/tracks").await;
    assert_eq!(titles(&tracks), ["mine 1", "mine 2", "other 1", "other 2"]);
    assert_eq!(tracks["total"], 4);

    let artists: Vec<_> = a.get("/artists").await["items"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(artists, ["My Artist", "Other Artist"]);
    let labels: Vec<String> = {
        let mut v: Vec<String> = a.get("/labels").await["items"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap().to_string()).collect();
        v.sort();
        v
    };
    assert_eq!(labels, ["My Label", "Other Label"]);
    let mut tags: Vec<String> = a.get("/tags").await.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
    tags.sort();
    assert_eq!(tags, ["mytag", "othertag"]);
    let facets = a.get("/facets").await;
    assert_eq!(facets["tags"].as_array().unwrap().len(), 2);
    assert_eq!(facets["total"], 4);

    let stats = a.get("/library/stats").await;
    assert_eq!((stats["tracks"].as_i64(), stats["releases"].as_i64(), stats["artists"].as_i64(), stats["tags"].as_i64()), (Some(4), Some(2), Some(2), Some(2)));
    assert_eq!(stats["total_bytes"], 4000);

    let top = a.get("/history/top").await;
    assert!(top["items"].as_array().unwrap().iter().all(|i| !i["track"]["title"].as_str().unwrap().starts_with("shelf")));
    let ids: std::collections::BTreeSet<i64> = a.get("/releases/ids").await.as_array().unwrap().iter().map(|r| r["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, [1, 2].into());
}

#[tokio::test]
async fn the_unified_switch_shows_everything_everywhere() {
    let a = app();
    seed_scope(&a);
    a.sql("INSERT INTO settings(key,value,updated_at) VALUES ('library.unified','1','x')");
    assert_eq!(titles(&a.get("/releases").await), ["mine album", "other album", "shelf album"]);
    assert_eq!(a.get("/tracks").await["total"], 6);
    assert_eq!(a.get("/artists").await["items"].as_array().unwrap().len(), 3);
    assert_eq!(a.get("/library/stats").await["releases"], 3);
    // explicit beats the default, both ways
    assert_eq!(titles(&a.get("/releases?scope=mine").await), ["mine album", "other album"]);
}

#[tokio::test]
async fn direct_reads_by_id_are_never_scoped() {
    let a = app();
    seed_scope(&a);
    let (s, _) = a.call(Method::GET, "/releases/3", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(titles(&a.get("/tracks?release_id=3").await), ["shelf 1", "shelf 2"]);
    let (s, _) = a.call(Method::GET, "/tracks/5", None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn continuation_follows_the_scoped_listing() {
    let a = app();
    seed_scope(&a);
    let after_mine = a.get("/releases/1/next?sort=title&order=asc").await;
    assert_eq!(after_mine["title"], "other album");
    let after_other = a.get("/releases/2/next?sort=title&order=asc").await;
    assert!(after_other.is_null(), "the shelf album would be next alphabetically");
}

#[tokio::test]
async fn artist_and_label_detail_count_only_what_is_shown() {
    let a = app();
    seed_scope(&a);
    a.sql("INSERT INTO releases(id,title,title_key,artist_id,label_id,kind,year,added_at,source_fan_id) VALUES (9,'mine but shelved','mine but shelved',1,1,'album',2023,'x',1)");
    assert_eq!(a.get("/artists/1").await["release_count"], 1);
    assert_eq!(a.get("/artists/1?scope=all").await["release_count"], 2);
    assert_eq!(a.get("/labels/1").await["release_count"], 1);
}

#[tokio::test]
async fn a_library_with_no_shelves_pays_nothing() {
    let a = app();
    use bc_libcore::Scope;
    use bc_types::ScopeMode;
    a.ctx.read(|c| {
        assert!(!Scope::resolve(c, None, None).unwrap().filtered());
        assert!(!Scope::resolve(c, Some(ScopeMode::Mine), None).unwrap().filtered());
        assert!(matches!(Scope::resolve(c, None, Some(3)).unwrap().mode, bc_libcore::scope::Mode::Fan(3)));
        Ok(())
    })
    .unwrap();
}

#[tokio::test]
async fn loving_a_track_adopts_its_release() {
    let a = app();
    seed_scope(&a);
    let (s, _) = a.call(Method::POST, "/tracks/5/love", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(a.scalar("SELECT COUNT(*) FROM releases WHERE id=3 AND source_fan_id IS NULL"), 1);
    // and the bulk form
    a.sql("UPDATE releases SET source_fan_id = 1 WHERE id = 3");
    let (s, v) = a.call(Method::POST, "/tracks/love", Some(json!({"track_ids": [6], "loved": true}))).await;
    assert_eq!((s, v["changed"].as_i64()), (StatusCode::OK, Some(1)));
    assert_eq!(a.scalar("SELECT COUNT(*) FROM releases WHERE id=3 AND source_fan_id IS NULL"), 1);
    // assign is idempotent: loving again changes nothing
    let (_, v) = a.call(Method::POST, "/tracks/love", Some(json!({"track_ids": [6], "loved": true}))).await;
    assert_eq!(v["changed"], 0);
}

// ------------------------------------------------------------------ test_favorites

fn seed_fav(a: &App) {
    a.sql(
        "INSERT INTO artists(id,name,name_key,created_at) VALUES (1,'Kaipe','kaipe','x');
         INSERT INTO labels(id,name,name_key) VALUES (1,'Trax','trax');
         INSERT INTO releases(id,title,title_key,artist_id,label_id,kind,cover_path,added_at) VALUES (1,'Psycho','psycho',1,1,'album','/covers/psycho.jpg','x');
         INSERT INTO tracks(id,release_id,artist_id,title,title_key,loved,play_count,skip_count,added_at) VALUES (1,1,1,'A1','a1',0,0,0,'x');
         INSERT INTO tags(id,name,name_key,kind,track_count) VALUES (1,'Hardgroove','hardgroove','genre',12);",
    );
}

#[tokio::test]
async fn favorites_start_empty_and_pins_list_as_cards() {
    let a = app();
    assert_eq!(a.get("/favorites").await, json!({"artists": [], "labels": [], "tags": []}));
    seed_fav(&a);
    assert_eq!(a.call(Method::PUT, "/favorites/artist/1", None).await.0, StatusCode::NO_CONTENT);
    let body = a.get("/favorites").await;
    assert_eq!(body["artists"][0]["name"], "Kaipe");
    assert_eq!((body["artists"][0]["release_count"].as_i64(), body["artists"][0]["track_count"].as_i64()), (Some(1), Some(1)));
    assert!(body["artists"][0]["art_url"].is_string(), "the card carries the artist's cover");
    assert_eq!(a.call(Method::PUT, "/favorites/label/1", None).await.0, StatusCode::NO_CONTENT);
    let body = a.get("/favorites").await;
    assert_eq!(body["labels"][0]["name"], "Trax");
    assert_eq!(body["labels"][0]["release_count"], 1);
    assert_eq!(body["labels"][0]["art_urls"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn tags_are_pinned_by_name_case_insensitively() {
    let a = app();
    seed_fav(&a);
    assert_eq!(a.call(Method::PUT, "/favorites/tag?name=hardgroove", None).await.0, StatusCode::NO_CONTENT);
    let body = a.get("/favorites").await;
    assert_eq!(body["tags"][0]["name"], "Hardgroove");
    assert_eq!(body["tags"][0]["track_count"], 12);
    assert_eq!(a.call(Method::DELETE, "/favorites/tag?name=HARDGROOVE", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(a.get("/favorites").await["tags"], json!([]));
}

#[tokio::test]
async fn pinning_twice_keeps_one_row_and_unknown_is_404() {
    let a = app();
    seed_fav(&a);
    for _ in 0..2 {
        assert_eq!(a.call(Method::PUT, "/favorites/artist/1", None).await.0, StatusCode::NO_CONTENT);
    }
    assert_eq!(a.get("/favorites").await["artists"].as_array().unwrap().len(), 1);
    assert_eq!(a.call(Method::PUT, "/favorites/artist/999", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(a.call(Method::PUT, "/favorites/label/999", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(a.call(Method::PUT, "/favorites/tag?name=nope", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(a.call(Method::PUT, "/favorites/release/1", None).await.0, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn unpinning_is_idempotent_and_newest_pin_comes_first() {
    let a = app();
    seed_fav(&a);
    a.call(Method::PUT, "/favorites/label/1", None).await;
    for _ in 0..2 {
        assert_eq!(a.call(Method::DELETE, "/favorites/label/1", None).await.0, StatusCode::NO_CONTENT);
    }
    assert_eq!(a.call(Method::DELETE, "/favorites/label/999", None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(a.get("/favorites").await["labels"], json!([]));
    a.sql("INSERT INTO artists(id,name,name_key,created_at) VALUES (2,'Genex','genex','x')");
    a.call(Method::PUT, "/favorites/artist/1", None).await;
    std::thread::sleep(std::time::Duration::from_millis(5));
    a.call(Method::PUT, "/favorites/artist/2", None).await;
    let names: Vec<String> = a.get("/favorites").await["artists"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap().into()).collect();
    assert_eq!(names, ["Genex", "Kaipe"]);
}

#[tokio::test]
async fn removing_the_entity_drops_its_pin() {
    let a = app();
    seed_fav(&a);
    a.call(Method::PUT, "/favorites/tag?name=Hardgroove", None).await;
    a.sql("DELETE FROM tags WHERE id = 1");
    assert_eq!(a.get("/favorites").await["tags"], json!([]));
}

#[tokio::test]
async fn favorites_filter_is_the_pool_behind_the_card() {
    let a = app();
    a.sql(
        "INSERT INTO library_roots(id,path,kind,watch,enabled) VALUES (1,'/music','library',0,1);
         INSERT INTO artists(id,name,name_key,created_at) VALUES (1,'Kaipe','kaipe','x'),(2,'Other','other','x');
         INSERT INTO labels(id,name,name_key) VALUES (1,'Trax','trax'),(2,'Elsewhere','elsewhere');
         INSERT INTO tags(id,name,name_key,kind,track_count) VALUES (1,'Hardgroove','hardgroove','genre',1);
         INSERT INTO releases(id,title,title_key,artist_id,label_id,kind,added_at) VALUES (1,'Kaipe LP','kaipe lp',1,2,'album','x'),(2,'Other on Trax','other on trax',2,1,'album','x'),(3,'Stray EP','stray ep',2,2,'album','x');
         INSERT INTO tracks(id,release_id,artist_id,title,title_key,loved,play_count,skip_count,added_at) VALUES
            (1,1,1,'By Kaipe','by kaipe',0,0,0,'x'),(2,2,2,'On Trax','on trax',0,0,0,'x'),(3,3,2,'Tagged','tagged',0,0,0,'x'),(4,3,2,'Left out','left out',0,0,0,'x');
         INSERT INTO files(track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at) VALUES
            (1,1,'/music/1.flac','1.flac','flac',1,1,'x','x'),(2,1,'/music/2.flac','2.flac','flac',1,1,'x','x'),(3,1,'/music/3.flac','3.flac','flac',1,1,'x','x'),(4,1,'/music/4.flac','4.flac','flac',1,1,'x','x');
         INSERT INTO track_tags(track_id,tag_id,source,weight) VALUES (3,1,'bandcamp',1);",
    );
    assert_eq!(titles(&a.get("/tracks?favorites=true&limit=500").await), Vec::<String>::new(), "nothing pinned, nothing to play");
    a.call(Method::PUT, "/favorites/artist/1", None).await;
    assert_eq!(titles(&a.get("/tracks?favorites=true").await), ["By Kaipe"]);
    a.call(Method::PUT, "/favorites/label/1", None).await;
    a.call(Method::PUT, "/favorites/tag?name=hardgroove", None).await;
    assert_eq!(titles(&a.get("/tracks?favorites=true").await), ["By Kaipe", "On Trax", "Tagged"]);
    assert_eq!(titles(&a.get("/tracks?favorites=true&sort=random").await), ["By Kaipe", "On Trax", "Tagged"]);
    assert_eq!(titles(&a.get("/tracks").await), ["By Kaipe", "Left out", "On Trax", "Tagged"]);
}

// ------------------------------------------------------------------ test_label_edit (patch + size)

fn seed_label(a: &App, id: i64, name: &str, url: Option<&str>, releases: i64) {
    let key = bc_db::util::name_key(name);
    let url_sql = url.map(|u| format!("'{u}'")).unwrap_or_else(|| "NULL".into());
    let mut sql = format!("INSERT INTO labels(id,name,name_key,bandcamp_url) VALUES ({id},'{name}','{key}',{url_sql});");
    for n in 0..releases {
        sql += &format!("INSERT INTO releases(title,title_key,label_id,kind,added_at) VALUES ('{name} {n}','{key} {n}',{id},'album','x');");
    }
    sql += &format!(
        "INSERT INTO harvest_items(url,url_kind,state,title,artist_name,label_name,tags,in_collection,in_wishlist,is_free_download,is_purchasable,is_preorder,discovered_at)
         VALUES ('https://{}.bandcamp.com/album/a{id}','album','in_library','A Record','Someone','{name}','[]',0,0,0,0,0,'x');",
        key.replace(' ', "")
    );
    a.sql(&sql);
}

#[tokio::test]
async fn rename_updates_the_label_and_its_harvest_evidence() {
    let a = app();
    seed_label(&a, 1, "SK_eleven", None, 1);
    let (s, body) = a.call(Method::PATCH, "/labels/1", Some(json!({"name": "SK Eleven"}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((body["id"].as_i64(), body["name"].as_str()), (Some(1), Some("SK Eleven")));
    assert_eq!(a.scalar("SELECT COUNT(*) FROM harvest_items WHERE label_name = 'SK Eleven'"), 1);
    assert_eq!(a.scalar("SELECT COUNT(DISTINCT label_name) FROM harvest_items"), 1);
}

#[tokio::test]
async fn rename_onto_an_existing_label_merges_the_shelves() {
    let a = app();
    seed_label(&a, 1, "Ostgut Ton", None, 2);
    seed_label(&a, 2, "Ostgutton", Some("https://ostgut.bandcamp.com"), 1);
    let (_, body) = a.call(Method::PATCH, "/labels/2", Some(json!({"name": "Ostgut Ton"}))).await;
    assert_eq!(body["id"], 1);
    assert_eq!(body["release_count"], 3);
    assert_eq!(body["bandcamp_url"], "https://ostgut.bandcamp.com", "the URL survives the merge");
    assert_eq!(a.scalar("SELECT COUNT(*) FROM labels WHERE id = 2"), 0);
    assert_eq!(a.scalar("SELECT COUNT(*) FROM releases WHERE label_id = 1"), 3);
    assert_eq!(a.scalar("SELECT COUNT(DISTINCT label_name) FROM harvest_items WHERE label_name = 'Ostgut Ton'"), 1);
}

#[tokio::test]
async fn url_can_be_set_cleared_and_never_stolen() {
    let a = app();
    seed_label(&a, 1, "Alpha", None, 1);
    seed_label(&a, 2, "Beta", Some("https://beta.bandcamp.com"), 1);
    let (_, body) = a.call(Method::PATCH, "/labels/1", Some(json!({"bandcamp_url": "https://Alpha.bandcamp.com/"}))).await;
    assert_eq!(body["bandcamp_url"], "https://alpha.bandcamp.com");
    let (s, conflict) = a.call(Method::PATCH, "/labels/1", Some(json!({"bandcamp_url": "https://beta.bandcamp.com"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(conflict["detail"].as_str().unwrap().contains("Beta"));
    let (_, cleared) = a.call(Method::PATCH, "/labels/1", Some(json!({"bandcamp_url": ""}))).await;
    assert!(cleared["bandcamp_url"].is_null());
    assert_eq!(a.scalar("SELECT COUNT(*) FROM labels WHERE id = 2 AND bandcamp_url = 'https://beta.bandcamp.com'"), 1);
}

#[tokio::test]
async fn patch_missing_label_is_404() {
    let a = app();
    assert_eq!(a.call(Method::PATCH, "/labels/9999", Some(json!({"name": "X"}))).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn size_sums_every_file_without_inflating_the_track_count() {
    let a = app();
    seed_label(&a, 1, "Planet Rhythm", None, 1);
    seed_label(&a, 2, "Bare", None, 1);
    a.sql(
        "INSERT INTO library_roots(id,path,kind,watch,enabled) VALUES (1,'/music','library',0,1);
         INSERT INTO tracks(id,release_id,title,title_key,loved,play_count,skip_count,added_at) VALUES (1,1,'t0','t0',0,0,0,'x'),(2,1,'t1','t1',0,0,0,'x');
         INSERT INTO files(track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at) VALUES
            (1,1,'/music/a-0.flac','a-0.flac','flac',1000,1,'x','x'),(1,1,'/music/a-1.flac','a-1.flac','flac',500,1,'x','x'),(2,1,'/music/b-0.flac','b-0.flac','flac',2000,1,'x','x');",
    );
    let listing = a.get("/labels").await;
    let find = |n: &str| listing["items"].as_array().unwrap().iter().find(|r| r["name"] == n).unwrap().clone();
    assert_eq!(find("Planet Rhythm")["size_bytes"], 3500);
    assert_eq!(find("Planet Rhythm")["track_count"], 2);
    assert_eq!(find("Bare")["size_bytes"], 0);
    let detail = a.get("/labels/1").await;
    assert_eq!((detail["size_bytes"].as_i64(), detail["track_count"].as_i64()), (Some(3500), Some(2)));
    assert_eq!(a.get("/labels/2").await["size_bytes"], 0);
}

// ------------------------------------------------------------------ test_release_shuffle

fn seed_shuffle(a: &App) {
    let mut sql = String::new();
    for n in 0..60 {
        sql += &format!(
            "INSERT INTO releases(id,title,title_key,kind,added_at) VALUES ({id},'Record {n}','record {n}','album','x');
             INSERT INTO tracks(release_id,title,title_key,track_no,loved,play_count,skip_count,added_at) VALUES ({id},'t{n}','t{n}',1,0,0,0,'x');",
            id = n + 1
        );
    }
    a.sql(&sql);
}

async fn page(a: &App, q: &str) -> Vec<i64> {
    a.get(&format!("/releases?{q}")).await["items"].as_array().unwrap().iter().map(|r| r["id"].as_i64().unwrap()).collect()
}

#[tokio::test]
async fn a_seed_pages_the_whole_listing_exactly_once() {
    let a = app();
    seed_shuffle(&a);
    let mut seen = vec![];
    for offset in (0..60).step_by(12) {
        seen.extend(page(&a, &format!("sort=random&seed=99&offset={offset}&limit=12")).await);
    }
    assert_eq!(seen.len(), 60);
    let uniq: std::collections::BTreeSet<_> = seen.iter().collect();
    assert_eq!(uniq.len(), 60, "no record is dealt twice");
}

#[tokio::test]
async fn same_seed_same_page_different_seed_different_shuffle_and_it_is_not_id_order() {
    let a = app();
    seed_shuffle(&a);
    let first = page(&a, "sort=random&seed=99&offset=0&limit=12").await;
    assert_eq!(page(&a, "sort=random&seed=99&offset=0&limit=12").await, first);
    assert_ne!(page(&a, "sort=random&seed=99&offset=12&limit=12").await, first);
    assert_ne!(page(&a, "sort=random&seed=1234&offset=0&limit=12").await, first);
    let drawn = page(&a, "sort=random&seed=99&offset=0&limit=60").await;
    let mut sorted = drawn.clone();
    sorted.sort();
    assert_ne!(drawn, sorted, "a sorted first page means the hash never wrapped");
}

#[tokio::test]
async fn without_a_seed_the_listing_still_rerolls() {
    let a = app();
    seed_shuffle(&a);
    let mut draws = std::collections::HashSet::new();
    for _ in 0..6 {
        draws.insert(page(&a, "sort=random&offset=0&limit=12").await);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(draws.len() > 1, "an unseeded random listing must still re-roll");
}

// ------------------------------------------------------------------ track engine: filters, sorts, paging

#[tokio::test]
async fn track_filters_sorts_and_unlimited_paging() {
    let a = app();
    a.sql(
        "INSERT INTO library_roots(id,path,kind,watch,enabled) VALUES (1,'/m','library',0,1);
         INSERT INTO artists(id,name,name_key,created_at) VALUES (1,'Zed','zed','x'),(2,'Amy','amy','x');
         INSERT INTO releases(id,title,title_key,artist_id,kind,year,added_at) VALUES (1,'R1','r1',1,'album',2019,'2020-01-01 00:00:00'),(2,'R2','r2',2,'album',2023,'2021-01-01 00:00:00');
         INSERT INTO tags(id,name,name_key,kind,track_count) VALUES (1,'Dub Techno','dub techno','genre',2);",
    );
    let mut sql = String::new();
    for i in 1..=30 {
        let rel = if i % 2 == 0 { 2 } else { 1 };
        let art = if rel == 1 { 1 } else { 2 };
        sql += &format!(
            "INSERT INTO tracks(id,release_id,artist_id,title,title_key,track_no,duration_ms,loved,play_count,skip_count,added_at) VALUES ({i},{rel},{art},'Track {i:02}','track {i:02}',{i},{d},{l},{p},0,'2021-02-{i:02} 00:00:00');
             INSERT INTO files(track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at) VALUES ({i},1,'/m/{i}.mp3','{i}.mp3','mp3',1,1,'x','x');",
            d = i * 1000, l = (i % 5 == 0) as i32, p = i % 4
        );
    }
    sql += "INSERT INTO track_tags(track_id,tag_id,source,weight) VALUES (3,1,'f',1),(4,1,'f',1);
            INSERT INTO analysis(track_id,analyzer_version,backend,status,analyzed_at,bpm,camelot) VALUES (5,1,'e','ok','x',128.0,'8A'),(6,1,'e','ok','x',140.0,'9A');";
    a.sql(&sql);
    // total + playtime are over the whole filter, not the page
    let p = a.get("/tracks?limit=5").await;
    assert_eq!((p["total"].as_i64(), p["items"].as_array().unwrap().len(), p["total_duration_ms"].as_i64()), (Some(30), 5, Some((1..=30).sum::<i64>() * 1000)));
    // paging beyond the old 500 cap's spirit: offset/limit are exact and total order holds
    let mut all = vec![];
    for off in (0..30).step_by(7) {
        for t in a.get(&format!("/tracks?sort=title&order=asc&offset={off}&limit=7")).await["items"].as_array().unwrap() {
            all.push(t["id"].as_i64().unwrap());
        }
    }
    assert_eq!(all, (1..=30).collect::<Vec<_>>());
    assert_eq!(a.get("/tracks?sort=artist&order=asc&limit=2").await["items"][0]["artist"]["name"], "Amy");
    assert_eq!(a.get("/tracks?sort=year&order=desc&limit=1").await["items"][0]["release"]["year"], 2023);
    assert_eq!(a.get("/tracks?sort=duration&order=desc&limit=1").await["items"][0]["id"], 30);
    assert_eq!(a.get("/tracks?sort=bpm&order=desc&limit=1").await["items"][0]["bpm"], 140.0);
    assert_eq!(a.get("/tracks?bpm_min=130").await["total"], 1);
    assert_eq!(a.get("/tracks?camelot=8a").await["items"][0]["id"], 5);
    assert_eq!(a.get("/tracks?loved=true").await["total"], 6);
    assert_eq!(a.get("/tracks?played=false").await["total"], 7);
    // tag filter on name_key: case and spacing insensitive; unknown tag = empty, not an error
    assert_eq!(a.get("/tracks?tags=DUB%20TECHNO").await["total"], 2);
    assert_eq!(a.get("/tracks?tags=nope").await["total"], 0);
    assert_eq!(a.get("/tracks?tags=dub%20techno&tags=nope").await["total"], 0);
    assert_eq!(a.get("/tracks?artist_id=2").await["total"], 15);
    assert_eq!(a.get("/tracks?label_id=999").await["total"], 0);
    assert_eq!(a.get("/tracks?release_ids=1&release_ids=2").await["total"], 30);
    assert_eq!(a.get("/tracks?added_after=2021-02-20").await["total"], 11);
    assert_eq!(a.get("/tracks?added_before=2021-02-11").await["total"], 10);
    assert_eq!(a.get("/tracks?year_min=2020").await["total"], 15);
    // FTS: relevance default with q, prefix on the last term
    a.sql("INSERT INTO search_index(track_id,title,artist,album,label,tags) SELECT t.id, t.title, '', '', '', '' FROM tracks t");
    let hit = a.get("/tracks?q=track%2007").await;
    assert_eq!(hit["total"], 1);
    assert_eq!(hit["items"][0]["id"], 7);
    assert_eq!(a.get("/tracks?q=trac&sort=title&order=asc&limit=1").await["items"][0]["id"], 1);
    // seeded track shuffle is stable and pageable
    let one = a.get("/tracks?sort=random&seed=5&limit=10").await;
    assert_eq!(a.get("/tracks?sort=random&seed=5&limit=10").await["items"], one["items"]);
    let mut seen = std::collections::HashSet::new();
    for off in (0..30).step_by(10) {
        for t in a.get(&format!("/tracks?sort=random&seed=5&limit=10&offset={off}")).await["items"].as_array().unwrap() {
            assert!(seen.insert(t["id"].as_i64().unwrap()));
        }
    }
    assert_eq!(seen.len(), 30);
    // bad date -> 400/422 problem, not a 500
    let (s, _) = a.call(Method::GET, "/tracks?added_after=garbage", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // rating setter
    let (s, t) = a.call(Method::PUT, "/tracks/3/rating", Some(json!({"rating": 4}))).await;
    assert_eq!((s, t["rating"].as_i64()), (StatusCode::OK, Some(4)));
    assert_eq!(a.call(Method::PUT, "/tracks/3/rating", Some(json!({"rating": 9}))).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn history_play_and_top_and_reset() {
    let a = app();
    seed_scope(&a);
    a.sql("DELETE FROM play_history; UPDATE tracks SET play_count = 0, last_played_at = NULL");
    for _ in 0..3 {
        assert_eq!(a.call(Method::POST, "/history/play", Some(json!({"track_id": 1, "ms_played": 1000, "completed": true}))).await.0, StatusCode::NO_CONTENT);
    }
    a.call(Method::POST, "/history/play", Some(json!({"track_id": 2, "ms_played": 10, "skipped": true}))).await;
    assert_eq!(a.call(Method::POST, "/history/play", Some(json!({"track_id": 999}))).await.0, StatusCode::NOT_FOUND);
    let top = a.get("/history/top?days=30&limit=5").await;
    assert_eq!((top["items"].as_array().unwrap().len(), top["items"][0]["plays"].as_i64()), (1, Some(3)), "a skip is not a listen");
    assert_eq!(a.scalar("SELECT skip_count FROM tracks WHERE id = 2"), 1);
    assert_eq!(a.get("/history/recent?limit=10").await.as_array().unwrap().len(), 4);
    let (_, r) = a.call(Method::DELETE, "/history?days=7", None).await;
    assert_eq!((r["events"].as_i64(), r["tracks"].as_i64()), (Some(4), Some(2)));
    assert_eq!(a.scalar("SELECT play_count FROM tracks WHERE id = 1"), 0);
    let (_, r) = a.call(Method::DELETE, "/history", None).await;
    assert_eq!(r["events"], 0);
}

#[tokio::test]
async fn releases_related_next_and_artists_detail() {
    let a = app();
    seed_scope(&a);
    a.sql("INSERT INTO releases(id,title,title_key,artist_id,label_id,kind,year,added_at) VALUES (10,'mine two','mine two',1,1,'album',2020,'2026-02-01 00:00:00');
           INSERT INTO tracks(id,release_id,artist_id,title,title_key,loved,play_count,skip_count,added_at) VALUES (100,10,1,'m2','m2',0,0,0,'x');
           INSERT INTO track_tags(track_id,tag_id,source,weight) VALUES (100,1,'f',1);");
    let groups = a.get("/releases/1/related").await;
    let kinds: Vec<_> = groups.as_array().unwrap().iter().map(|g| g["kind"].as_str().unwrap().to_string()).collect();
    assert!(kinds.contains(&"artist".to_string()), "{groups}");
    assert!(groups[0]["items"].as_array().unwrap().iter().all(|r| r["id"] != 1), "never repeats the release itself");
    let detail = a.get("/artists/1").await;
    assert_eq!(detail["release_count"], 2);
    assert_eq!(detail["labels"][0]["name"], "My Label");
    assert!(a.get("/artists/1/related").await["similar_artists"].as_array().is_some());
    assert!(a.get("/labels/random").await["id"].is_i64());
    let shuffle = a.get("/labels/shuffle?limit=10").await;
    assert!(shuffle["items"].as_array().unwrap().len() <= 10 && !shuffle["items"].as_array().unwrap().is_empty());
    assert_eq!(a.get("/library/home?seed=3").await["seed"], 3);
}

#[tokio::test]
async fn artists_can_be_renamed_and_collisions_are_409() {
    let a = app();
    a.sql("INSERT INTO artists(id,name,name_key,created_at) VALUES (1,'Kaipe','kaipe','x'),(2,'Genex','genex','x');
           INSERT INTO releases(id,title,title_key,artist_id,kind,added_at) VALUES (1,'R','r',1,'album','x');
           INSERT INTO tracks(id,release_id,artist_id,title,title_key,loved,play_count,skip_count,added_at) VALUES (1,1,1,'t','t',0,0,0,'x');");
    let (s, body) = a.call(Method::PATCH, "/artists/1", Some(json!({"name": "  KAIPE (live)  "}))).await;
    assert_eq!((s, body["name"].as_str()), (StatusCode::OK, Some("KAIPE (live)")));
    assert_eq!(a.scalar("SELECT COUNT(*) FROM tracks WHERE artist_key = 'kaipe live'"), 1, "denormalised sort key follows");
    let (s, _) = a.call(Method::PATCH, "/artists/1", Some(json!({"name": "genex"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    // a case-only change of its own name is fine
    let (s, _) = a.call(Method::PATCH, "/artists/2", Some(json!({"name": "GENEX"}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(a.call(Method::PATCH, "/artists/2", Some(json!({"name": " "}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(a.call(Method::PATCH, "/artists/99", Some(json!({"name": "X"}))).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn search_short_queries_and_pushed_limit_keep_bm25_order() {
    let a = app();
    let mut sql = String::new();
    for i in 1..=40 {
        let title = if i == 7 { "dub dub dub".to_string() } else if i % 2 == 0 { format!("dub mix {i}") } else { format!("Another {i}") };
        sql += &format!("INSERT INTO tracks(id,title,title_key,loved,play_count,skip_count,added_at) VALUES ({i},'{title}','{}',0,0,0,'x');
                         INSERT INTO files(track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at) VALUES ({i},1,'/m/{i}','{i}','mp3',1,1,'x','x');
                         INSERT INTO search_index(track_id,title,artist,album,label,tags) VALUES ({i},'{title}','','','','');", title.to_lowercase());
    }
    a.sql("INSERT INTO library_roots(id,path,kind,watch,enabled) VALUES (1,'/m','library',0,1);");
    a.sql(&sql);
    // best match first, total is the real match count, paging continues the same order
    let p1 = a.get("/tracks?q=dub&limit=5").await;
    assert_eq!(p1["items"][0]["id"], 7);
    assert_eq!(p1["total"], 21);
    let p2 = a.get("/tracks?q=dub&limit=5&offset=5").await;
    let ids: Vec<i64> = p1["items"].as_array().unwrap().iter().chain(p2["items"].as_array().unwrap()).map(|t| t["id"].as_i64().unwrap()).collect();
    let uniq: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(uniq.len(), 10, "pages do not overlap");
    // a single character means "title starts with"
    let one = a.get("/tracks?q=a").await;
    assert_eq!(one["total"], 19);
    assert!(one["items"].as_array().unwrap().iter().all(|t| t["title"].as_str().unwrap().to_lowercase().starts_with('a')));
    // extra filters take the capped path and still agree
    let f = a.get("/tracks?q=dub&loved=false&limit=3").await;
    assert_eq!((f["total"].as_i64(), f["items"][0]["id"].as_i64()), (Some(21), Some(7)));
}
