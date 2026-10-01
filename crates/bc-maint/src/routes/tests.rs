//! Route-level tests: an in-process router over a tempfile DB, driven with `tower::ServiceExt`.

use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::lookup::{AlbumInfo, AlbumTrack, BandcampLookup, LookupError};
use crate::testutil::*;

const PAGE: &str = "https://sportinglife.bandcamp.com/album/slam-dunk-vol-i";

struct Fake;

#[async_trait]
impl BandcampLookup for Fake {
    async fn resolve_album_url(&self, url: &str) -> Result<String, LookupError> {
        Ok(if url.contains("/track/hydrate-the-hustle") { PAGE.to_string() } else { url.to_string() })
    }
    async fn fetch_album(&self, url: &str) -> Result<AlbumInfo, LookupError> {
        Ok(AlbumInfo { url: url.into(), title: "x".into(), artist_name: "y".into(), tracks: vec![AlbumTrack::default()], ..Default::default() })
    }
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            b.body(Body::from(v.to_string())).unwrap()
        }
        None => b.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn app(env: &TestEnv, with_lookup: bool) -> Router {
    let lookup: Option<Arc<dyn BandcampLookup>> = if with_lookup { Some(Arc::new(Fake)) } else { None };
    // The loved-streams handlers are not part of `router()` (WS2 mounts them); tests mount both.
    super::router(env.ctx.clone(), lookup.clone()).merge(super::loved_router(env.ctx.clone(), lookup))
}

#[tokio::test]
async fn the_main_router_does_not_register_loved_streams() {
    let env = test_env();
    let only = super::router(env.ctx.clone(), None);
    assert_eq!(call(&only, "GET", "/loved-streams", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&only, "POST", "/loved-streams/download", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&only, "GET", "/labels/1/locate", None).await.0, StatusCode::NOT_FOUND);
}

// ---- cleanup and blacklist routes

const BL: &str = "https://soma-records.bandcamp.com/album/soma-sample-pack-kicks";

#[tokio::test]
async fn the_blacklist_route_round_trips() {
    let env = test_env();
    let app = app(&env, false);
    let (st, added) = call(&app, "POST", "/blacklist", Some(json!({"url": BL, "artist_name": "Soma", "title": "Kicks"}))).await;
    assert_eq!(st, 200);
    assert_eq!(added["reason"], "manual");
    assert_eq!(call(&app, "GET", "/blacklist?q=kicks", None).await.1["total"], 1);
    assert_eq!(call(&app, "GET", "/blacklist?q=nothing", None).await.1["total"], 0);
    let id = added["id"].as_i64().unwrap();
    assert_eq!(call(&app, "DELETE", &format!("/blacklist/{id}"), None).await.0, 204);
    assert_eq!(call(&app, "GET", "/blacklist", None).await.1["total"], 0);
    assert_eq!(call(&app, "DELETE", &format!("/blacklist/{id}"), None).await.0, 404);
}

#[tokio::test]
async fn the_blacklist_route_refuses_an_entry_that_could_match_nothing() {
    let env = test_env();
    let (st, body) = call(&app(&env, false), "POST", "/blacklist", Some(json!({"artist_name": "Only An Artist"}))).await;
    assert_eq!(st, 400);
    assert_eq!(body["status"], 400);
}

#[tokio::test]
async fn cleanup_candidates_route() {
    let env = test_env();
    let kicks = seed_release(&env.db, "Somebody", "Analog Kicks", None, None);
    for n in 1..=3 {
        let t = seed_track(&env.db, kicks, &format!("k{n}"), Some(n));
        exec(&env.db, &format!("UPDATE tracks SET duration_ms=900 WHERE id={t}"));
    }
    let rec = seed_release(&env.db, "Somebody", "A Real Record", None, None);
    let t = seed_track(&env.db, rec, "r", Some(1));
    exec(&env.db, &format!("UPDATE tracks SET duration_ms=380000 WHERE id={t}"));
    let app = app(&env, false);
    let (st, body) = call(&app, "GET", "/cleanup/candidates?max_track_s=60", None).await;
    assert_eq!(st, 200);
    assert_eq!(body["total"], 1);
    assert_eq!(body["max_track_s"], 60);
    assert_eq!(body["items"][0]["release"]["id"], kicks);
    assert_eq!(body["items"][0]["reasons"], json!(["short-tracks"]));
    assert_eq!(body["items"][0]["longest_ms"], 900);
    assert_eq!(body["items"][0]["track_count"], 3);
    assert_eq!(call(&app, "GET", "/cleanup/candidates?max_track_s=600", None).await.1["total"], 2);
    assert_eq!(call(&app, "GET", "/cleanup/candidates?max_track_s=601", None).await.0, 400);
}

// ---- deletes

struct OnDisk {
    folder: std::path::PathBuf,
    release: i64,
}

fn album_on_disk(env: &TestEnv, root: i64, music: &std::path::Path, artist: &str, title: &str, n: usize, tags: &[&str], url: Option<&str>) -> OnDisk {
    let folder = music.join(bc_db::util::name_key(artist).replace(' ', "-")).join(bc_db::util::name_key(title).replace(' ', "-"));
    std::fs::create_dir_all(&folder).unwrap();
    let rid = seed_release(&env.db, artist, title, url, None);
    exec(&env.db, &format!("UPDATE releases SET folder_path='{}' WHERE id={rid}", folder.display()));
    for i in 1..=n {
        let t = seed_track(&env.db, rid, &format!("{title} {i}"), Some(i as i64));
        exec(&env.db, &format!("UPDATE tracks SET duration_ms=900 WHERE id={t}"));
        let p = folder.join(format!("{i:02} - track.mp3"));
        std::fs::write(&p, [0xffu8, 0xfb, 0, 0]).unwrap();
        seed_file(&env.db, t, root, p.to_str().unwrap(), p.strip_prefix(music).unwrap().to_str().unwrap(), 4);
        for tag in tags {
            exec(&env.db, &format!(
                "INSERT OR IGNORE INTO tags(name,name_key,kind,track_count) VALUES ('{tag}','{tag}','file',0);
                 UPDATE tags SET track_count=track_count+1 WHERE name_key='{tag}';
                 INSERT INTO track_tags(track_id,tag_id,source,weight) SELECT {t},id,'file',1 FROM tags WHERE name_key='{tag}';"));
        }
        exec(&env.db, &format!("INSERT INTO search_index(track_id,title,artist,album,label,tags) VALUES ({t},'{title}','{artist}','{title}','','')"));
    }
    OnDisk { folder, release: rid }
}

fn music_root(env: &TestEnv) -> (std::path::PathBuf, i64) {
    let music = env.dir.path().join("music");
    std::fs::create_dir_all(&music).unwrap();
    let music = music.canonicalize().unwrap();
    let root = seed_root(&env.db, music.to_str().unwrap(), "library");
    (music, root)
}

#[tokio::test]
async fn bulk_delete_removes_rows_files_and_the_folder() {
    // The sidecar cover is the whole reason folders used to survive a delete.
    let env = test_env();
    let (music, root) = music_root(&env);
    let a = album_on_disk(&env, root, &music, "Producer", "Analog Kicks", 2, &[], None);
    std::fs::write(a.folder.join("cover.jpg"), [0xffu8, 0xd8, 0xff]).unwrap();
    // cached art in both layouts
    let art = env.config.art_dir();
    let files = crate::tidy::art_files(&art, a.release);
    for f in &files {
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, b"x").unwrap();
    }
    exec(&env.db, &format!("INSERT INTO artwork(release_id,version,sizes) VALUES ({},'v1',7)", a.release));
    let (st, body) = call(&app(&env, false), "POST", "/releases/delete", Some(json!({"ids": [a.release]}))).await;
    assert_eq!(st, 200);
    assert_eq!((body["releases"].as_i64(), body["tracks"].as_i64(), body["files"].as_i64()), (Some(1), Some(2), Some(2)));
    assert_eq!(body["errors"], json!([]));
    assert!(!a.folder.exists(), "the sidecar cover must not keep the folder alive");
    assert!(music.exists(), "the root itself is never pruned");
    assert_eq!(q_i64(&env.db, &format!("SELECT COUNT(*) FROM releases WHERE id={}", a.release)), 0);
    assert_eq!(q_i64(&env.db, "SELECT COUNT(*) FROM search_index"), 0, "FTS rows go too");
    assert_eq!(q_i64(&env.db, "SELECT COUNT(*) FROM artwork"), 0);
    assert!(files.iter().all(|f| !f.exists()), "cached art is deleted");
}

#[tokio::test]
async fn bulk_delete_gives_back_the_tag_counts() {
    // Nothing ever decremented tags.track_count, so every delete inflated the tag cloud.
    let env = test_env();
    let (music, root) = music_root(&env);
    let kicks = album_on_disk(&env, root, &music, "Producer", "Kicks", 2, &["techno"], None);
    album_on_disk(&env, root, &music, "Producer", "Keeper", 1, &["techno"], None);
    assert_eq!(q_i64(&env.db, "SELECT track_count FROM tags WHERE name_key='techno'"), 3);
    call(&app(&env, false), "POST", "/releases/delete", Some(json!({"ids": [kicks.release]}))).await;
    assert_eq!(q_i64(&env.db, "SELECT track_count FROM tags WHERE name_key='techno'"), 1);
}

#[tokio::test]
async fn one_bad_release_does_not_abort_the_batch() {
    use std::os::unix::fs::PermissionsExt;
    // A permission-denied file two albums into a sweep must not roll back the ones that worked.
    let env = test_env();
    let (music, root) = music_root(&env);
    let good = album_on_disk(&env, root, &music, "Producer", "Good One", 1, &[], None);
    let doomed = album_on_disk(&env, root, &music, "Producer", "Doomed", 1, &[], None);
    std::fs::set_permissions(&doomed.folder, std::fs::Permissions::from_mode(0o500)).unwrap();
    // root can delete in a read-only dir; skip the assertion then
    let can_write = std::fs::write(doomed.folder.join("probe"), b"x").is_ok();
    let (_, body) = call(&app(&env, false), "POST", "/releases/delete", Some(json!({"ids": [good.release, doomed.release]}))).await;
    std::fs::set_permissions(&doomed.folder, std::fs::Permissions::from_mode(0o700)).unwrap();
    if can_write {
        return;
    }
    assert_eq!(body["releases"], 1);
    let errors = body["errors"].as_array().unwrap();
    assert!(errors.len() == 1 && errors[0].as_str().unwrap().contains("Doomed"), "{errors:?}");
    assert_eq!(q_i64(&env.db, &format!("SELECT COUNT(*) FROM releases WHERE id={}", good.release)), 0);
    assert_eq!(q_i64(&env.db, &format!("SELECT COUNT(*) FROM releases WHERE id={}", doomed.release)), 1, "a failed release keeps its rows");
}

#[tokio::test]
async fn delete_with_blacklist_records_and_blocks() {
    let env = test_env();
    let (music, root) = music_root(&env);
    let url = "https://soma-records.bandcamp.com/album/soma-sample-pack-kicks";
    let a = album_on_disk(&env, root, &music, "Soma Records", "Soma Sample Pack Kicks", 1, &[], Some(url));
    let app = app(&env, false);
    let (_, body) = call(&app, "POST", "/releases/delete", Some(json!({"ids": [a.release], "blacklist": true}))).await;
    assert_eq!(body["blacklisted"], 1);
    let (_, listed) = call(&app, "GET", "/blacklist", None).await;
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["items"][0]["title"], "Soma Sample Pack Kicks");
    assert_eq!(listed["items"][0]["reason"], "cleanup");
    // The point of the whole feature: asking for it again reports it as blacklisted.
    let known = env.db.read(|c| Ok(crate::dedup::find_known(c, &[url.to_string()], None).unwrap())).unwrap();
    assert_eq!(known[&crate::urls::url_key(url)], "blacklist");
    assert_eq!(call(&app, "POST", "/releases/delete", Some(json!({"ids": []}))).await.0, 400);
    assert_eq!(call(&app, "POST", "/releases/delete", Some(json!({"ids": [99999]}))).await.0, 404);
}

#[tokio::test]
async fn files_outside_every_root_are_never_deleted() {
    let env = test_env();
    let (music, root) = music_root(&env);
    let a = album_on_disk(&env, root, &music, "Producer", "Fine", 1, &[], None);
    let outside = env.dir.path().join("elsewhere");
    std::fs::create_dir_all(&outside).unwrap();
    let victim = outside.join("precious.mp3");
    std::fs::write(&victim, b"keep").unwrap();
    let t = q_i64(&env.db, &format!("SELECT id FROM tracks WHERE release_id={}", a.release));
    exec(&env.db, &format!("UPDATE files SET path='{}' WHERE track_id={t}", victim.display()));
    let app = app(&env, false);
    let (st, body) = call(&app, "DELETE", &format!("/tracks/{t}"), None).await;
    assert_eq!(st, 400);
    assert!(body["detail"].as_str().unwrap().contains("outside every library root"));
    assert!(victim.exists());
    assert_eq!(q_i64(&env.db, &format!("SELECT COUNT(*) FROM tracks WHERE id={t}")), 1, "rows stay when the disk refuses");
    let (_, bulk) = call(&app, "POST", "/releases/delete", Some(json!({"ids": [a.release]}))).await;
    assert_eq!(bulk["releases"], 0);
    assert_eq!(bulk["errors"].as_array().unwrap().len(), 1);
    assert!(victim.exists());
    // a symlink escaping a root is refused too
    let link = music.join("escape");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    exec(&env.db, &format!("UPDATE files SET path='{}' WHERE track_id={t}", link.join("precious.mp3").display()));
    assert_eq!(call(&app, "DELETE", &format!("/tracks/{t}"), None).await.0, 400);
    assert!(victim.exists());
}

#[tokio::test]
async fn delete_track_and_release_routes() {
    let env = test_env();
    let (music, root) = music_root(&env);
    let a = album_on_disk(&env, root, &music, "Producer", "Two Tracks", 2, &["house"], None);
    let app = app(&env, false);
    let t1 = q_i64(&env.db, &format!("SELECT MIN(id) FROM tracks WHERE release_id={}", a.release));
    let (st, body) = call(&app, "DELETE", &format!("/tracks/{t1}"), None).await;
    assert_eq!((st, body), (StatusCode::OK, json!({"tracks": 1, "files": 1})));
    assert_eq!(q_i64(&env.db, "SELECT track_count FROM tags WHERE name_key='house'"), 1);
    assert_eq!(call(&app, "DELETE", &format!("/tracks/{t1}"), None).await.0, 404);
    let (st, body) = call(&app, "DELETE", &format!("/releases/{}", a.release), None).await;
    assert_eq!((st, body), (StatusCode::OK, json!({"tracks": 1, "files": 1})));
    assert!(!a.folder.exists());
    assert_eq!(call(&app, "DELETE", &format!("/releases/{}", a.release), None).await.0, 404);
}

#[tokio::test]
async fn delete_label_takes_the_catalogue_and_the_inbox_evidence() {
    let env = test_env();
    let (music, root) = music_root(&env);
    let a = album_on_disk(&env, root, &music, "Producer", "On Label", 1, &[], None);
    let b = album_on_disk(&env, root, &music, "Producer", "Also On Label", 1, &[], None);
    let c = album_on_disk(&env, root, &music, "Producer", "Elsewhere", 1, &[], None);
    exec(&env.db, &format!("INSERT INTO labels(name,name_key) VALUES ('Great Label','great label'); UPDATE releases SET label_id=1 WHERE id IN ({},{});", a.release, b.release));
    exec(&env.db, "INSERT INTO harvest_items(url,url_kind,state,title,artist_name,label_name,tags,in_collection,in_wishlist,is_free_download,is_purchasable,is_preorder,discovered_at) VALUES ('https://x.bandcamp.com/album/z','album','new','t','a','GREAT label','[]',0,0,0,0,0,'x')");
    let app = app(&env, false);
    let (st, body) = call(&app, "DELETE", "/labels/1", None).await;
    assert_eq!(st, 200);
    assert_eq!((body["releases"].as_i64(), body["tracks"].as_i64(), body["files"].as_i64()), (Some(2), Some(2), Some(2)));
    assert_eq!(q_i64(&env.db, "SELECT COUNT(*) FROM labels"), 0);
    assert_eq!(q_i64(&env.db, "SELECT COUNT(*) FROM harvest_items WHERE label_name IS NOT NULL"), 0, "the evidence the startup backfill files from is cleared");
    assert_eq!(q_i64(&env.db, &format!("SELECT COUNT(*) FROM releases WHERE id={}", c.release)), 1);
    assert_eq!(call(&app, "DELETE", "/labels/1", None).await.0, 404);
}

#[tokio::test]
async fn delete_label_keeps_the_label_when_a_release_cannot_go() {
    let env = test_env();
    let (music, root) = music_root(&env);
    let a = album_on_disk(&env, root, &music, "Producer", "Fine", 1, &[], None);
    let outside = env.dir.path().join("elsewhere.mp3");
    std::fs::write(&outside, b"x").unwrap();
    exec(&env.db, &format!("INSERT INTO labels(name,name_key) VALUES ('L','l'); UPDATE releases SET label_id=1 WHERE id={0}; UPDATE files SET path='{1}'", a.release, outside.display()));
    let (st, body) = call(&app(&env, false), "DELETE", "/labels/1", None).await;
    assert_eq!(st, 400);
    assert!(body["detail"].as_str().unwrap().contains("the label was kept"));
    assert_eq!(q_i64(&env.db, "SELECT COUNT(*) FROM labels"), 1);
    assert!(outside.exists());
}

// ---- adopt, fill, scope, snippets

#[tokio::test]
async fn adopt_moves_a_release_or_a_whole_shelf_into_my_library() {
    let env = test_env();
    let fan = seed_fan(&env.db, "alice");
    let shelf = seed_release(&env.db, "Shelf", "shelf album", None, None);
    let other = seed_release(&env.db, "Other", "other album", None, None);
    exec(&env.db, &format!("UPDATE releases SET source_fan_id={fan} WHERE id={shelf}"));
    let app = app(&env, false);
    let mut rx = env.bus.subscribe();
    assert_eq!(call(&app, "POST", "/releases/adopt", Some(json!({"ids": [shelf]}))).await.1, json!({"adopted": 1}));
    assert_eq!(call(&app, "POST", "/releases/adopt", Some(json!({"ids": [shelf]}))).await.1, json!({"adopted": 0}), "idempotent");
    exec(&env.db, &format!("UPDATE releases SET source_fan_id={fan} WHERE id IN ({shelf},{other})"));
    assert_eq!(call(&app, "GET", "/library/scope", None).await.1["foreign_releases"], 2);
    assert_eq!(call(&app, "POST", "/releases/adopt", Some(json!({"fan_id": fan}))).await.1, json!({"adopted": 2}));
    assert_eq!(call(&app, "GET", "/library/scope", None).await.1["foreign_releases"], 0);
    let mut adopted_events = 0;
    while let Ok(ev) = rx.try_recv() {
        if ev.topic == "library.changed" && ev.payload.get("adopted").is_some() {
            adopted_events += 1;
        }
    }
    assert_eq!(adopted_events, 2, "only a real move publishes library.changed");
}

#[tokio::test]
async fn scope_and_snippet_settings_round_trip_and_publish() {
    let env = test_env();
    let app = app(&env, false);
    let mut rx = env.bus.subscribe();
    assert_eq!(call(&app, "GET", "/library/scope", None).await.1, json!({"unified": false, "foreign_releases": 0}));
    assert_eq!(call(&app, "PUT", "/library/scope", Some(json!({"unified": true}))).await.1["unified"], true);
    assert_eq!(call(&app, "GET", "/library/scope", None).await.1["unified"], true);
    // Snippets: off until asked for, with the counts next to the switch.
    let rid = seed_release(&env.db, "a", "b", None, None);
    seed_track(&env.db, rid, "Regrets [SNIPPET]", None);
    env.db.write(|t| Ok(crate::snippets::backfill_tx(t).unwrap())).unwrap();
    assert_eq!(call(&app, "GET", "/library/snippets", None).await.1, json!({"hidden": false, "snippet_tracks": 1, "snippet_releases": 1}));
    assert_eq!(call(&app, "PUT", "/library/snippets", Some(json!({"hidden": true}))).await.1["hidden"], true);
    assert_eq!(call(&app, "GET", "/library/snippets", None).await.1["hidden"], true);
    let mut topics = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        topics.push((ev.topic, ev.payload));
    }
    assert!(topics.contains(&("library.changed".into(), json!({"scope": "all"}))));
    assert!(topics.contains(&("library.changed".into(), json!({"snippets": "hidden"}))));
}

#[tokio::test]
async fn fill_routes() {
    let env = test_env();
    let fan = seed_fan(&env.db, "yassinepeixoto");
    let url = "https://a.bandcamp.com/album/mutual-rytm";
    let r = seed_release(&env.db, "A", "Mutual Rytm", Some(url), None);
    exec(&env.db, &format!("UPDATE releases SET source_fan_id={fan}, expected_track_count=12 WHERE id={r}"));
    seed_track(&env.db, r, "t1", Some(1));
    let unlinked = seed_release(&env.db, "A", "Unlinked", None, None);
    seed_track(&env.db, unlinked, "t", Some(1));
    let app = app(&env, false);
    let (st, body) = call(&app, "POST", &format!("/releases/{r}/fill"), None).await;
    assert_eq!(st, 200);
    assert_eq!((body["detail"].as_str(), body["url"].as_str()), (Some("queued"), Some(url)));
    let (_, again) = call(&app, "POST", &format!("/releases/{r}/fill"), None).await;
    assert_eq!((again["detail"].as_str(), &again["job_id"]), (Some("already queued"), &body["job_id"]));
    assert_eq!(call(&app, "POST", &format!("/releases/{unlinked}/fill"), None).await.0, 409);
    assert_eq!(call(&app, "POST", "/releases/99999/fill", None).await.0, 404);
    // fill-all sees the same release already covered by the unfinished fill
    let (st, all) = call(&app, "POST", "/releases/fill", None).await;
    assert_eq!(st, 200);
    assert_eq!((all["missing"].as_i64(), all["queued"].as_i64(), all["already_queued"].as_i64()), (Some(1), Some(0), Some(1)));
}

#[tokio::test]
async fn availability_route_fetches_once_then_serves_the_cache() {
    let env = test_env();
    let r = seed_release(&env.db, "A", "Ghost EP", Some("https://a.bandcamp.com/album/ghost-ep"), None);
    let unlinked = seed_release(&env.db, "A", "Unlinked", None, None);
    let app = app(&env, true);
    let (st, first) = call(&app, "GET", &format!("/releases/{r}/availability"), None).await;
    assert_eq!(st, 200);
    assert_eq!((first["fetched"].as_bool(), first["is_preorder"].as_bool()), (Some(true), Some(false)));
    assert_eq!(first["tracks"].as_array().unwrap().len(), 1);
    let (_, again) = call(&app, "GET", &format!("/releases/{r}/availability"), None).await;
    assert_eq!(again["fetched"].as_bool(), Some(false), "a fresh row answers from the DB");
    assert_eq!(call(&app, "GET", &format!("/releases/{unlinked}/availability"), None).await.1, Value::Null);
    assert_eq!(call(&app, "GET", "/releases/99999/availability", None).await.0, 404);
    // A pre-order whose date has passed is stale after the hour: it is read again.
    exec(&env.db, &format!("UPDATE release_availability SET is_preorder=1, release_date='2000-01-01', checked_at=datetime('now','-2 hours') WHERE release_id={r}"));
    let (_, third) = call(&app, "GET", &format!("/releases/{r}/availability"), None).await;
    assert_eq!(third["fetched"].as_bool(), Some(true));
    // Without the client the stale row still answers.
    exec(&env.db, &format!("UPDATE release_availability SET checked_at=datetime('now','-2 days') WHERE release_id={r}"));
    let (st, stale) = call(&super::router(env.ctx.clone(), None), "GET", &format!("/releases/{r}/availability"), None).await;
    assert_eq!((st, stale["fetched"].as_bool()), (StatusCode::OK, Some(false)));
}

// ---- strays

#[tokio::test]
async fn strays_routes_and_the_missing_client() {
    let env = test_env();
    let r = seed_release(&env.db, "Chl\u{e4}r", "Greedy Man", Some("https://m.bandcamp.com/track/greedy-man"), None);
    seed_track(&env.db, r, "Greedy Man", Some(5));
    let orphan = seed_release(&env.db, "X", "Orphan", None, None);
    seed_track(&env.db, orphan, "Orphan", Some(7));
    let no_client = app(&env, false);
    let (st, body) = call(&no_client, "GET", "/releases/strays", None).await;
    assert_eq!(st, 200);
    assert_eq!((body["total"].as_i64(), body["resolvable"].as_i64()), (Some(2), Some(1)));
    assert_eq!(body["items"][0]["release_id"], r);
    assert_eq!(body["items"][0]["track_no"], 5);
    assert_eq!(call(&no_client, "GET", "/releases/strays?limit=1", None).await.1["items"].as_array().unwrap().len(), 1);
    for (m, p) in [("POST", "/releases/strays/merge"), ("GET", "/releases/strays/merge"), ("DELETE", "/releases/strays/merge")] {
        let (st, body) = call(&no_client, m, p, Some(json!({})).filter(|_| m == "POST")).await;
        assert_eq!(st, 409, "{m} {p}");
        assert_eq!(body["detail"], "the Bandcamp client is not available");
    }
    // With a client: start, status, done.
    let with = app(&env, true);
    let (st, state) = call(&with, "POST", "/releases/strays/merge", Some(json!({"ids": [orphan]}))).await;
    assert_eq!(st, 202);
    assert_eq!(state["phase"], "running");
    for _ in 0..200 {
        let (_, s) = call(&with, "GET", "/releases/strays/merge", None).await;
        if s["running"] == false {
            assert_eq!(s["phase"], "done");
            assert_eq!(s["unresolved"], 1);
            let (_, stopped) = call(&with, "DELETE", "/releases/strays/merge", None).await;
            assert_eq!(stopped["phase"], "done", "stopping a finished sweep changes nothing");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("sweep never finished");
}

// ---- loved streams

async fn love(app: &Router, over: Value) -> (StatusCode, Value) {
    let mut body = json!({"page_url": PAGE, "track_key": "12345", "bc_track_id": 12345, "title": "Hydrate The Hustle",
        "artist_name": "Sporting Life", "release_title": "Slam Dunk Vol. I", "duration_ms": 263000});
    for (k, v) in over.as_object().unwrap() {
        body[k] = v.clone();
    }
    call(app, "POST", "/loved-streams", Some(body)).await
}

#[tokio::test]
async fn loved_stream_routes() {
    let env = test_env();
    let app = app(&env, false);
    let (st, body) = love(&app, json!({})).await;
    assert_eq!(st, 200);
    assert!(body["stream_url"].as_str().unwrap().starts_with("/api/explore/stream?release="));
    assert!(body["stream_url"].as_str().unwrap().contains("track=12345"));
    assert!(!body["stream_url"].as_str().unwrap().contains("bcbits"));
    assert_eq!(call(&app, "GET", "/loved-streams", None).await.1.as_array().unwrap().len(), 1);
    // same track twice is one row
    let (_, again) = love(&app, json!({})).await;
    assert_eq!(again["id"], body["id"]);
    assert_eq!(call(&app, "GET", "/loved-streams", None).await.1.as_array().unwrap().len(), 1);
    // unlove
    let q = format!("/loved-streams?page_url={}&track_key=12345", PAGE);
    assert_eq!(call(&app, "DELETE", &q, None).await.0, 204);
    assert_eq!(call(&app, "GET", "/loved-streams", None).await.1, json!([]));
    assert_eq!(call(&app, "DELETE", &q, None).await.0, 404);
    // only bandcamp pages
    assert_eq!(love(&app, json!({"page_url": "https://evil.example.com/x"})).await.0, 400);
    assert_eq!(love(&app, json!({"track_key": ""})).await.0, 400);
    // positional keys
    let (_, pos) = love(&app, json!({"track_key": "i2", "bc_track_id": null, "track_index": 2})).await;
    assert!(pos["stream_url"].as_str().unwrap().contains("track=i2"));
    assert_eq!((&pos["bc_track_id"], &pos["track_index"]), (&Value::Null, &json!(2)));
}

#[tokio::test]
async fn auto_download_is_off_until_asked_for_and_queues_a_new_love() {
    let env = test_env();
    let app = app(&env, true);
    assert_eq!(call(&app, "GET", "/loved-streams/auto", None).await.1["enabled"], false);
    assert_eq!(call(&app, "PUT", "/loved-streams/auto", Some(json!({"enabled": true}))).await.1["enabled"], true);
    assert_eq!(call(&app, "GET", "/loved-streams/auto", None).await.1["enabled"], true);
    // A newly loved stream queues its album; re-loving it does not queue again.
    love(&app, json!({})).await;
    assert_eq!(q_i64(&env.db, "SELECT COUNT(*) FROM jobs"), 1);
    assert_eq!(q_str(&env.db, "SELECT url FROM job_items").as_deref(), Some(PAGE));
    love(&app, json!({})).await;
    assert_eq!(q_i64(&env.db, "SELECT COUNT(*) FROM jobs"), 1);
    call(&app, "PUT", "/loved-streams/auto", Some(json!({"enabled": false}))).await;
    assert_eq!(call(&app, "GET", "/loved-streams/auto", None).await.1["enabled"], false);
}

fn add_release(env: &TestEnv, url: &str, tracks: &[&str]) -> i64 {
    let r = seed_release(&env.db, "Sporting Life", "Slam Dunk Vol. I", Some(url), None);
    for (i, t) in tracks.iter().enumerate() {
        seed_track(&env.db, r, t, Some(i as i64 + 1));
    }
    r
}

#[tokio::test]
async fn downloading_with_nothing_loved_is_not_an_error() {
    let env = test_env();
    let (st, body) = call(&app(&env, true), "POST", "/loved-streams/download", None).await;
    assert_eq!(st, 200);
    assert_eq!((body["queued"].as_i64(), body["already_owned"].as_i64(), &body["job_id"]), (Some(0), Some(0), &Value::Null));
    // and without a client the route says so
    assert_eq!(call(&app(&env, false), "POST", "/loved-streams/download", None).await.0, 409);
}

#[tokio::test]
async fn a_stream_already_in_the_library_converts_instead_of_downloading() {
    let env = test_env();
    let app = app(&env, true);
    love(&app, json!({})).await;
    add_release(&env, PAGE, &["Hydrate The Hustle"]);
    let mut rx = env.bus.subscribe();
    let (_, body) = call(&app, "POST", "/loved-streams/download", None).await;
    assert_eq!((body["already_owned"].as_i64(), body["queued"].as_i64(), &body["job_id"]), (Some(1), Some(0), &Value::Null));
    assert_eq!(call(&app, "GET", "/loved-streams", None).await.1, json!([]));
    assert_eq!(q_i64(&env.db, "SELECT loved FROM tracks"), 1);
    let mut got = false;
    while let Ok(ev) = rx.try_recv() {
        got |= ev.topic == "loved.reconciled";
    }
    assert!(got, "loved.reconciled is published");
}

#[tokio::test]
async fn a_track_page_stream_adopts_through_its_resolved_album() {
    let env = test_env();
    let app = app(&env, true);
    add_release(&env, PAGE, &["Naomi", "Hydrate The Hustle"]);
    love(&app, json!({"page_url": "https://sportinglife.bandcamp.com/track/hydrate-the-hustle", "release_title": "Hydrate The Hustle"})).await;
    let (_, body) = call(&app, "POST", "/loved-streams/download", None).await;
    assert_eq!((body["already_owned"].as_i64(), body["queued"].as_i64(), body["skipped"].as_i64(), body["resolved"].as_i64()), (Some(1), Some(0), Some(0), Some(1)));
    assert_eq!(call(&app, "GET", "/loved-streams", None).await.1, json!([]));
    assert_eq!(q_str(&env.db, "SELECT title FROM tracks WHERE loved=1").as_deref(), Some("Hydrate The Hustle"));
}

#[tokio::test]
async fn a_partial_album_fills_instead_of_skipping() {
    // The album row is here but the loved track is not on it: queued as a force fill, and the
    // stream stays until the fill lands and reconciles it.
    let env = test_env();
    let app = app(&env, true);
    add_release(&env, PAGE, &["Naomi", "Slam Dunk"]);
    love(&app, json!({})).await; // loves "Hydrate The Hustle", which the local copy lacks
    let (_, body) = call(&app, "POST", "/loved-streams/download", None).await;
    assert_eq!((body["already_owned"].as_i64(), body["queued"].as_i64()), (Some(0), Some(1)));
    let job_id = body["job_id"].as_str().unwrap().to_string();
    assert!(q_str(&env.db, &format!("SELECT label FROM jobs WHERE id='{job_id}'")).unwrap().starts_with("fill:"));
    assert!(q_str(&env.db, &format!("SELECT params FROM jobs WHERE id='{job_id}'")).unwrap().contains("\"force\":true"));
    assert_eq!(call(&app, "GET", "/loved-streams", None).await.1.as_array().unwrap().len(), 1, "the stream is still a stream");
    // Pressing again rides the unfinished fill rather than queueing a second.
    let (_, again) = call(&app, "POST", "/loved-streams/download", None).await;
    assert_eq!((again["queued"].as_i64(), again["job_id"].as_str()), (Some(1), Some(job_id.as_str())));
    assert_eq!(q_i64(&env.db, "SELECT COUNT(*) FROM jobs"), 1);
}

#[tokio::test]
async fn an_unowned_album_is_queued_once_and_a_blacklisted_one_is_skipped() {
    let env = test_env();
    let app = app(&env, true);
    love(&app, json!({})).await;
    let (_, body) = call(&app, "POST", "/loved-streams/download", None).await;
    assert_eq!((body["queued"].as_i64(), body["detail"].as_str()), (Some(1), Some("queued")));
    assert_eq!(q_str(&env.db, "SELECT label FROM jobs").unwrap(), "loved: 1 album(s)");
    assert_eq!(q_i64(&env.db, "SELECT priority FROM jobs"), 90);
    assert_eq!(q_str(&env.db, "SELECT source FROM job_items").as_deref(), Some("loved"));
    // blacklisted
    exec(&env.db, "DELETE FROM jobs");
    call(&app, "POST", "/blacklist", Some(json!({"url": PAGE, "artist_name": "Sporting Life", "title": "Slam Dunk Vol. I"}))).await;
    let (_, again) = call(&app, "POST", "/loved-streams/download", None).await;
    assert_eq!((again["queued"].as_i64(), again["skipped"].as_i64(), again["detail"].as_str()), (Some(0), Some(1), Some("nothing left to download")));
}

// ---- move

async fn wait_task(env: &TestEnv, id: &str) -> crate::routes::tests::Task {
    for _ in 0..400 {
        if let Some(t) = env.jobs.get(id)
            && t.state != "running"
        {
            return t;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("move task never finished");
}

type Task = bc_libcore::TaskInfo;

#[tokio::test(flavor = "multi_thread")]
async fn move_is_a_tracked_task_that_publishes_progress() {
    let env = test_env();
    let old = env.dir.path().join("old").join("music");
    let folder = old.join("Somatic").join("Grid Failure");
    std::fs::create_dir_all(&folder).unwrap();
    let root = seed_root(&env.db, old.to_str().unwrap(), "library");
    let rid = seed_release(&env.db, "Somatic", "Grid Failure", None, None);
    for n in 1..=3 {
        let rel = format!("Somatic/Grid Failure/0{n} - track.mp3");
        std::fs::write(old.join(&rel), vec![1u8; 100]).unwrap();
        let t = seed_track(&env.db, rid, &format!("t{n}"), Some(n));
        seed_file(&env.db, t, root, old.join(&rel).to_str().unwrap(), &rel, 100);
    }
    let target_parent = env.dir.path().join("new");
    std::fs::create_dir_all(&target_parent).unwrap();
    let target = target_parent.join("music");
    let app = app(&env, false);
    let mut rx = env.bus.subscribe();

    let (st, plan) = call(&app, "POST", &format!("/library/roots/{root}/move/plan"), Some(json!({"target_path": target}))).await;
    assert_eq!(st, 200);
    assert_eq!((plan["file_count"].as_i64(), plan["ok"].clone(), plan["fits"].clone()), (Some(3), json!(true), json!(true)));
    let (_, inside) = call(&app, "POST", &format!("/library/roots/{root}/move/plan"), Some(json!({"target_path": old.join("nested")}))).await;
    assert_eq!(inside["ok"], false);
    assert!(inside["warnings"].to_string().contains("inside the folder"));

    // A blocked move is refused up front and changes nothing.
    let (st, body) = call(&app, "POST", &format!("/library/roots/{root}/move"), Some(json!({"target_path": env.dir.path().join("nope/deeper")}))).await;
    assert_eq!(st, 400);
    assert!(body["detail"].as_str().unwrap().contains("does not exist"));
    assert_eq!(q_str(&env.db, &format!("SELECT path FROM library_roots WHERE id={root}")).unwrap(), old.to_str().unwrap());
    assert_eq!(call(&app, "POST", "/library/roots/999/move/plan", Some(json!({"target_path": target}))).await.0, 400);

    let (st, accepted) = call(&app, "POST", &format!("/library/roots/{root}/move"), Some(json!({"target_path": target}))).await;
    assert_eq!(st, 202);
    let job_id = accepted["job_id"].as_str().unwrap().to_string();
    let task = wait_task(&env, &job_id).await;
    assert_eq!(task.state, "done", "{task:?}");
    assert_eq!(task.kind, "move");
    let result = task.result.unwrap();
    assert_eq!((result["moved"].as_i64(), result["total"].as_i64(), result["new_path"].as_str()), (Some(3), Some(3), target.to_str()));
    assert_eq!(result["errors"], json!([]));
    assert_eq!(q_str(&env.db, &format!("SELECT path FROM library_roots WHERE id={root}")).unwrap(), target.to_str().unwrap());
    assert!(target.join("Somatic/Grid Failure/01 - track.mp3").is_file());
    assert!(!old.join("Somatic/Grid Failure").exists());

    let mut progress = Vec::new();
    let mut changed = false;
    while let Ok(ev) = rx.try_recv() {
        if ev.topic == "library.changed" && ev.payload["moved_root"] == root {
            changed = true;
        }
        if ev.topic == "library.move.progress" {
            progress.push(ev.payload);
        }
    }
    assert!(changed, "library.changed {{moved_root}} is published");
    assert!(!progress.is_empty() && progress.len() <= 4, "coalesced to at most ~4/s: {} events", progress.len());
    let last = progress.last().unwrap();
    assert_eq!((last["done"].clone(), last["moved"].as_i64(), last["root_id"].as_i64()), (json!(true), Some(3), Some(root)));
    assert_eq!(progress[0]["moved"], 1, "the first file is always reported");
}
