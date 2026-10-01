//! Route-level tests (oneshot against an in-process router over a tempfile DB) and the ported
//! legacy export/playlist-make tests.

use std::io::Cursor;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use bc_core::EventBus;
use bc_db::Db;
use bc_db::util::name_key;
use bc_libcore::{ApiResult, Ctx, Scope};
use bc_types::library::TrackQuery;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::TrackResolver;

// ---------------------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------------------

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A trivial stand-in for the library's filter engine: loved, favourites (pinned artists), tags,
/// artist/label/release, missing, ordered newest first.
struct TestResolver;

impl TrackResolver for TestResolver {
    fn resolve_ids(&self, c: &bc_db::rusqlite::Connection, q: &TrackQuery, _s: &Scope, limit: Option<i64>) -> ApiResult<Vec<i64>> {
        let mut w = vec!["1=1".to_string()];
        if q.loved == Some(true) {
            w.push("t.loved = 1".into());
        }
        if q.favorites == Some(true) {
            w.push("t.artist_id IN (SELECT artist_id FROM favorites WHERE artist_id IS NOT NULL)".into());
        }
        if q.missing == Some(false) {
            w.push("t.available = 1".into());
        }
        if let Some(a) = q.artist_id {
            w.push(format!("t.artist_id = {a}"));
        }
        if let Some(l) = q.label_id {
            w.push(format!("t.release_id IN (SELECT id FROM releases WHERE label_id = {l})"));
        }
        for tag in &q.tags {
            w.push(format!(
                "EXISTS (SELECT 1 FROM track_tags tt JOIN tags g ON g.id = tt.tag_id WHERE tt.track_id = t.id AND g.name_key = {})",
                lit(&name_key(tag))
            ));
        }
        if let Some(s) = &q.q {
            w.push(format!("t.title LIKE {}", lit(&format!("%{s}%"))));
        }
        let sql = format!(
            "SELECT t.id FROM tracks t WHERE {} ORDER BY t.added_at DESC, t.id {}",
            w.join(" AND "),
            limit.map(|l| format!("LIMIT {l}")).unwrap_or_default()
        );
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

struct Fix {
    db: Db,
    ctx: Ctx,
    app: Router,
    dir: tempfile::TempDir,
}

#[derive(Clone, Default)]
struct Opts {
    label: Option<&'static str>,
    tags: Vec<&'static str>,
    bpm: Option<f64>,
    camelot: Option<&'static str>,
    energy: Option<f64>,
    loved: bool,
    missing: bool,
    duration_ms: Option<i64>,
    cover: bool,
    year: Option<i64>,
}

impl Fix {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        db.write(|t| {
            t.execute("INSERT INTO library_roots(path,kind,watch,enabled) VALUES ('/music','library',0,1)", [])?;
            Ok(())
        })
        .unwrap();
        let ctx = Ctx::new(db.clone(), Arc::new(EventBus::new()), bc_core::Config::from_env());
        let app = crate::router(ctx.clone(), Arc::new(TestResolver));
        Fix { db, ctx, app, dir }
    }

    /// A track on its own release; the file path is `/music/{id}.mp3` (not on disk) unless
    /// `add_real` is used.
    fn add(&self, title: &str, artist: &str, o: Opts) -> i64 {
        let (title, artist) = (title.to_string(), artist.to_string());
        self.db
            .write(move |t| {
                let aid = match t.query_row("SELECT id FROM artists WHERE name = ?1", [&artist], |r| r.get::<_, i64>(0)) {
                    Ok(id) => id,
                    Err(_) => {
                        t.execute(
                            "INSERT INTO artists(name,name_key,created_at) VALUES (?1,?2,'2020-01-01 00:00:00.000000')",
                            [&artist, &name_key(&artist)],
                        )?;
                        t.last_insert_rowid()
                    }
                };
                let lid: Option<i64> = match o.label {
                    None => None,
                    Some(l) => Some(match t.query_row("SELECT id FROM labels WHERE name = ?1", [l], |r| r.get::<_, i64>(0)) {
                        Ok(id) => id,
                        Err(_) => {
                            t.execute("INSERT INTO labels(name,name_key) VALUES (?1,?2)", [l, &name_key(l)])?;
                            t.last_insert_rowid()
                        }
                    }),
                };
                let rtitle = format!("{title} EP");
                t.execute(
                    "INSERT INTO releases(title,title_key,artist_id,label_id,kind,year,added_at,cover_path)
                     VALUES (?1,?2,?3,?4,'album',?5,'2020-01-01 00:00:00.000000',?6)",
                    bc_db::rusqlite::params![rtitle, name_key(&rtitle), aid, lid, o.year, o.cover.then_some("/c.jpg")],
                )?;
                let rid = t.last_insert_rowid();
                if o.cover {
                    t.execute("INSERT INTO artwork(release_id,version) VALUES (?1,'v1')", [rid])?;
                }
                t.execute(
                    "INSERT INTO tracks(release_id,artist_id,title,title_key,duration_ms,loved,play_count,skip_count,added_at,is_snippet)
                     VALUES (?1,?2,?3,?4,?5,?6,0,0,'2020-01-01 00:00:00.000000',0)",
                    bc_db::rusqlite::params![rid, aid, title, name_key(&title), o.duration_ms.unwrap_or(300_000), o.loved],
                )?;
                let tid = t.last_insert_rowid();
                t.execute(
                    "INSERT INTO files(track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at,missing_since)
                     VALUES (?1,1,?2,?3,'mp3',1,1,'x','x',?4)",
                    bc_db::rusqlite::params![
                        tid,
                        format!("/music/{tid}.mp3"),
                        format!("{tid}.mp3"),
                        o.missing.then_some("2020-01-02 00:00:00.000000")
                    ],
                )?;
                if o.bpm.is_some() || o.camelot.is_some() || o.energy.is_some() {
                    t.execute(
                        "INSERT INTO analysis(track_id,analyzer_version,backend,status,analyzed_at,bpm,camelot,energy,beat_offset_ms,loudness_lufs)
                         VALUES (?1,1,'e','ok','x',?2,?3,?4,12.5,-9.0)",
                        bc_db::rusqlite::params![tid, o.bpm, o.camelot, o.energy],
                    )?;
                }
                for name in &o.tags {
                    let gid = match t.query_row("SELECT id FROM tags WHERE name = ?1", [name], |r| r.get::<_, i64>(0)) {
                        Ok(id) => id,
                        Err(_) => {
                            t.execute(
                                "INSERT INTO tags(name,name_key,kind,track_count) VALUES (?1,?2,'genre',0)",
                                [*name, &name_key(name)],
                            )?;
                            t.last_insert_rowid()
                        }
                    };
                    t.execute("INSERT INTO track_tags(track_id,tag_id,source,weight) VALUES (?1,?2,'bandcamp',1)", [tid, gid])?;
                }
                Ok(tid)
            })
            .unwrap()
    }

    fn simple(&self, title: &str) -> i64 {
        self.add(title, "Someone", Opts::default())
    }

    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut b = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let resp = self.app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let (parts, body) = resp.into_parts();
        (parts.status, parts.headers, body.collect().await.unwrap().to_bytes().to_vec())
    }

    async fn json(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (s, _, b) = self.call(method, uri, body).await;
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    async fn ok(&self, method: &str, uri: &str, body: Option<Value>) -> Value {
        let (s, v) = self.json(method, uri, body).await;
        assert!(s.is_success(), "{method} {uri} -> {s}: {v}");
        v
    }

    fn q<T: Send + 'static>(&self, f: impl FnOnce(&bc_db::rusqlite::Connection) -> T + Send + 'static) -> T {
        self.db.read(|c| Ok(f(c))).unwrap()
    }
}

fn ids_of(v: &Value) -> Vec<i64> {
    v["items"].as_array().unwrap().iter().map(|t| t["id"].as_i64().unwrap()).collect()
}

fn names_in_zip(blob: &[u8]) -> Vec<String> {
    let mut z = zip::ZipArchive::new(Cursor::new(blob.to_vec())).unwrap();
    (0..z.len()).map(|i| z.by_index(i).unwrap().name().to_string()).collect()
}

// ---------------------------------------------------------------------------------------
// test_playlist_make.py (saving a view, bulk adds) -- `/similar` belongs to the recommender
// ---------------------------------------------------------------------------------------

async fn save(f: &Fix, body: Value) -> Value {
    f.ok("POST", "/playlists/from-tracks", Some(body)).await
}

async fn tracks_in(f: &Fix, pid: i64) -> Vec<i64> {
    ids_of(&f.ok("GET", &format!("/playlists/{pid}/tracks"), None).await)
}

async fn playlist_of(f: &Fix, ids: &[i64], name: &str) -> i64 {
    let created = f.ok("POST", "/playlists", Some(json!({"name": name}))).await;
    let pid = created["id"].as_i64().unwrap();
    if !ids.is_empty() {
        f.ok("POST", &format!("/playlists/{pid}/tracks"), Some(json!({"track_ids": ids}))).await;
    }
    pid
}

#[tokio::test]
async fn the_loved_shelf_is_saved_as_a_playlist() {
    let f = Fix::new();
    let one = f.add("one", "Someone", Opts { loved: true, ..Default::default() });
    let two = f.add("two", "Someone", Opts { loved: true, ..Default::default() });
    f.simple("cold");

    let body = save(&f, json!({"loved": true})).await;

    assert_eq!(body["name"], "Loved");
    assert_eq!(body["track_count"], 2);
    assert_eq!(body["duration_ms"], 600_000);
    let got: std::collections::HashSet<i64> = tracks_in(&f, body["id"].as_i64().unwrap()).await.into_iter().collect();
    assert_eq!(got, [one, two].into_iter().collect());
}

#[tokio::test]
async fn the_favourites_pool_is_saved_as_a_playlist() {
    let f = Fix::new();
    let mine = f.add("mine", "Pinned", Opts::default());
    f.add("theirs", "Other", Opts::default());
    f.db
        .write(|t| {
            t.execute(
                "INSERT INTO favorites(artist_id, created_at) SELECT id, 'x' FROM artists WHERE name = 'Pinned'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

    let body = save(&f, json!({"favorites": true})).await;

    assert_eq!(body["name"], "Favourites");
    assert_eq!(tracks_in(&f, body["id"].as_i64().unwrap()).await, vec![mine]);
}

#[tokio::test]
async fn a_saved_view_leaves_out_files_that_are_gone() {
    let f = Fix::new();
    let here = f.add("here", "Someone", Opts { loved: true, ..Default::default() });
    f.add("gone", "Someone", Opts { loved: true, missing: true, ..Default::default() });

    let pid = save(&f, json!({"loved": true})).await["id"].as_i64().unwrap();
    assert_eq!(tracks_in(&f, pid).await, vec![here]);
}

#[tokio::test]
async fn saving_the_same_view_twice_numbers_the_second() {
    let f = Fix::new();
    f.add("one", "Someone", Opts { loved: true, ..Default::default() });

    assert_eq!(save(&f, json!({"loved": true})).await["name"], "Loved");
    assert_eq!(save(&f, json!({"loved": true})).await["name"], "Loved 2");
    assert_eq!(save(&f, json!({"loved": true})).await["name"], "Loved 3");
}

#[tokio::test]
async fn a_saved_view_holds_all_of_it_and_not_a_page() {
    let f = Fix::new();
    for i in 0..120 {
        f.add(&format!("t{i}"), "Someone", Opts { loved: true, ..Default::default() });
    }
    assert_eq!(save(&f, json!({"loved": true})).await["track_count"], 120);
}

#[tokio::test]
async fn a_ceiling_is_still_available_to_a_caller_that_wants_one() {
    let f = Fix::new();
    for i in 0..10 {
        f.add(&format!("t{i}"), "Someone", Opts { loved: true, ..Default::default() });
    }
    assert_eq!(save(&f, json!({"loved": true, "limit": 4})).await["track_count"], 4);
}

#[tokio::test]
async fn a_given_name_is_used_as_given() {
    let f = Fix::new();
    f.add("one", "Someone", Opts { loved: true, ..Default::default() });
    assert_eq!(save(&f, json!({"loved": true, "name": "  Friday  "})).await["name"], "Friday");
}

#[tokio::test]
async fn saving_a_view_with_nothing_in_it_is_a_400() {
    let f = Fix::new();
    let (s, _) = f.json("POST", "/playlists/from-tracks", Some(json!({"loved": true}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn adding_a_pile_of_tracks_at_once_keeps_their_order() {
    let f = Fix::new();
    let ids: Vec<i64> = (0..60).map(|i| f.simple(&format!("t{i}"))).collect();
    let pid = playlist_of(&f, &ids[..10], "Source").await;

    let added = f.ok("POST", &format!("/playlists/{pid}/tracks"), Some(json!({"track_ids": &ids[10..]}))).await;

    assert_eq!(added["added"], 50);
    assert_eq!(added["total"], 60);
    assert_eq!(tracks_in(&f, pid).await, ids);
}

#[tokio::test]
async fn tracks_dropped_in_at_a_point_keep_their_order_too() {
    let f = Fix::new();
    let (head, tail) = (f.simple("head"), f.simple("tail"));
    let (a, b) = (f.simple("a"), f.simple("b"));
    let pid = playlist_of(&f, &[head, tail], "P").await;

    f.ok("POST", &format!("/playlists/{pid}/tracks"), Some(json!({"track_ids": [a, b], "at_index": 1}))).await;

    assert_eq!(tracks_in(&f, pid).await, vec![head, a, b, tail]);
}

// ---------------------------------------------------------------------------------------
// Playlist CRUD
// ---------------------------------------------------------------------------------------

#[tokio::test]
async fn playlist_crud_list_counts_and_cascade() {
    let f = Fix::new();
    let a = f.add("a", "X", Opts { duration_ms: Some(1000), cover: true, ..Default::default() });
    let b = f.add("b", "X", Opts { duration_ms: Some(2000), ..Default::default() });
    let p1 = playlist_of(&f, &[a, b, a], "  Zed ").await;
    let p2 = playlist_of(&f, &[], "Alpha").await;

    let list = f.ok("GET", "/playlists", None).await;
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["Alpha", "Zed"], "sorted by name, trimmed");
    let zed = &list[1];
    assert_eq!(zed["id"], p1);
    assert_eq!(zed["track_count"], 3);
    assert_eq!(zed["duration_ms"], 4000);
    assert_eq!(zed["kind"], "manual");
    assert_eq!(zed["art_url"], "/api/art/release/1?size=thumb&v=v1");
    assert!(zed["created_at"].as_str().unwrap().ends_with('Z'));
    assert_eq!(list[0]["track_count"], 0);

    // the same track held twice: removing one place removes one place
    let page = f.ok("GET", &format!("/playlists/{p1}/tracks"), None).await;
    assert_eq!(ids_of(&page), vec![a, b, a]);
    assert_eq!(page["total"], 3);
    let item_ids: Vec<i64> = page["items"].as_array().unwrap().iter().map(|t| t["item_id"].as_i64().unwrap()).collect();
    assert_eq!(item_ids.iter().collect::<std::collections::HashSet<_>>().len(), 3);
    let (s, _, _) = f.call("DELETE", &format!("/playlists/{p1}/tracks/{}", item_ids[0]), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(tracks_in(&f, p1).await, vec![b, a]);
    // an item of another playlist is a 404
    let (s, _) = f.json("DELETE", &format!("/playlists/{p2}/tracks/{}", item_ids[1]), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // delete cascades to the items, the tracks stay
    let (s, _, _) = f.call("DELETE", &format!("/playlists/{p1}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(f.q(|c| c.query_row("SELECT COUNT(*) FROM playlist_items", [], |r| r.get::<_, i64>(0)).unwrap()), 0);
    assert_eq!(f.q(|c| c.query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get::<_, i64>(0)).unwrap()), 2);
    for (m, u) in [("DELETE", format!("/playlists/{p1}")), ("GET", format!("/playlists/{p1}/tracks")), ("GET", format!("/playlists/{p1}/export"))] {
        assert_eq!(f.json(m, &u, None).await.0, StatusCode::NOT_FOUND, "{m} {u}");
    }
}

#[tokio::test]
async fn create_validates_and_untitled_default() {
    let f = Fix::new();
    let p = f.ok("POST", "/playlists", Some(json!({"name": "   "}))).await;
    assert_eq!(p["name"], "Untitled");
    assert_eq!(f.json("POST", "/playlists", Some(json!({"name": "x", "kind": "weird"}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.json("POST", "/playlists", Some(json!({"name": "x", "kind": "smart"}))).await.0, StatusCode::BAD_REQUEST);
    // malformed body: problem+json 400, not axum's text
    let (s, h, _) = f.call("POST", "/playlists", Some(json!({"nope": 1}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(h.get("content-type").is_some());
    // empty add and unknown tracks are 400, unknown playlist 404
    let pid = p["id"].as_i64().unwrap();
    assert_eq!(f.json("POST", &format!("/playlists/{pid}/tracks"), Some(json!({"track_ids": []}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.json("POST", &format!("/playlists/{pid}/tracks"), Some(json!({"track_ids": [999]}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.json("POST", "/playlists/999/tracks", Some(json!({"track_ids": [1]}))).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn patch_renames_and_describes() {
    let f = Fix::new();
    let pid = playlist_of(&f, &[], "Old").await;
    let p = f.ok("PATCH", &format!("/playlists/{pid}"), Some(json!({"name": " New ", "description": "about"}))).await;
    assert_eq!(p["name"], "New");
    assert_eq!(p["description"], "about");
    assert_eq!(f.json("PATCH", &format!("/playlists/{pid}"), Some(json!({"name": " "}))).await.0, StatusCode::BAD_REQUEST);
    // rules only make sense on a smart playlist
    assert_eq!(f.json("PATCH", &format!("/playlists/{pid}"), Some(json!({"rules": {"loved": true}}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.json("PATCH", "/playlists/999", Some(json!({"name": "x"}))).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn move_uses_the_index_without_the_dragged_item() {
    let f = Fix::new();
    let ids: Vec<i64> = (0..4).map(|i| f.simple(&format!("t{i}"))).collect();
    let pid = playlist_of(&f, &ids, "P").await;
    let page = f.ok("GET", &format!("/playlists/{pid}/tracks"), None).await;
    let item = |n: usize| page["items"][n]["item_id"].as_i64().unwrap();

    // drag the first to index 2 of the list without it: [1,2,0,3]
    let mv = f.ok("POST", &format!("/playlists/{pid}/tracks/{}/move", item(0)), Some(json!({"to_index": 2}))).await;
    assert!(mv["position"].as_f64().is_some());
    assert_eq!(tracks_in(&f, pid).await, vec![ids[1], ids[2], ids[0], ids[3]]);
    // to the very front and past the end
    f.ok("POST", &format!("/playlists/{pid}/tracks/{}/move", item(3)), Some(json!({"to_index": 0}))).await;
    assert_eq!(tracks_in(&f, pid).await, vec![ids[3], ids[1], ids[2], ids[0]]);
    f.ok("POST", &format!("/playlists/{pid}/tracks/{}/move", item(3)), Some(json!({"to_index": 99}))).await;
    assert_eq!(tracks_in(&f, pid).await, vec![ids[1], ids[2], ids[0], ids[3]]);
    assert_eq!(f.json("POST", &format!("/playlists/{pid}/tracks/9999/move"), Some(json!({"to_index": 0}))).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn repeated_inserts_at_one_spot_renormalise_and_stay_ordered() {
    let f = Fix::new();
    let ids: Vec<i64> = (0..70).map(|i| f.simple(&format!("t{i}"))).collect();
    let pid = playlist_of(&f, &ids[..2], "P").await;
    // 68 single inserts, each between the head and whatever sits at index 1 (halving the gap
    // every time -- way past float resolution).
    let mut expected = vec![ids[0], ids[1]];
    for &t in &ids[2..] {
        f.ok("POST", &format!("/playlists/{pid}/tracks"), Some(json!({"track_ids": [t], "at_index": 1}))).await;
        expected.insert(1, t);
    }
    assert_eq!(tracks_in(&f, pid).await, expected);
    let positions: Vec<f64> = f.q(move |c| {
        let mut st = c.prepare("SELECT position FROM playlist_items WHERE playlist_id = ?1 ORDER BY position, id").unwrap();
        st.query_map([pid], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    });
    assert!(positions.windows(2).all(|w| w[1] - w[0] > 1e-6), "no collapsed gaps");
}

#[tokio::test]
async fn smart_playlist_evaluates_its_saved_filter_live() {
    let f = Fix::new();
    let loved = f.add("l1", "A", Opts { loved: true, duration_ms: Some(1000), ..Default::default() });
    f.simple("cold");
    let p = f
        .ok("POST", "/playlists", Some(json!({"name": "Hot", "kind": "smart", "rules": {"loved": true}})))
        .await;
    let pid = p["id"].as_i64().unwrap();
    assert_eq!(p["kind"], "smart");
    assert_eq!(p["rules"]["loved"], true);
    assert_eq!(p["track_count"], 1);
    assert_eq!(tracks_in(&f, pid).await, vec![loved]);
    let page = f.ok("GET", &format!("/playlists/{pid}/tracks"), None).await;
    assert!(page["items"][0]["item_id"].is_null());

    // live: a newly loved track shows up without touching the playlist
    let two = f.simple("two");
    f.db.write(move |t| { t.execute("UPDATE tracks SET loved = 1 WHERE id = ?1", [two])?; Ok(()) }).unwrap();
    assert_eq!(tracks_in(&f, pid).await.len(), 2);
    let list = f.ok("GET", "/playlists", None).await;
    assert_eq!(list[0]["track_count"], 2);
    // nothing to add to
    assert_eq!(f.json("POST", &format!("/playlists/{pid}/tracks"), Some(json!({"track_ids": [two]}))).await.0, StatusCode::BAD_REQUEST);
    // rules can be edited
    let p = f.ok("PATCH", &format!("/playlists/{pid}"), Some(json!({"rules": {"loved": false}}))).await;
    assert_eq!(p["rules"]["loved"], false);
}

#[tokio::test]
async fn playlist_exports_m3u8_csv_zip() {
    let f = Fix::new();
    let real_dir = f.dir.path().join("audio");
    std::fs::create_dir_all(&real_dir).unwrap();
    let a = f.add("Noon", "Yuku", Opts { tags: vec!["dub", "techno"], bpm: Some(128.0), camelot: Some("8A"), year: Some(2021), duration_ms: Some(61_500), ..Default::default() });
    let b = f.add("Gone", "Yuku", Opts { missing: true, ..Default::default() });
    let pa = real_dir.join("a.mp3");
    std::fs::write(&pa, b"AAAA").unwrap();
    let pa_s = pa.to_string_lossy().to_string();
    f.db.write(move |t| { t.execute("UPDATE files SET path = ?1 WHERE track_id = ?2", bc_db::rusqlite::params![pa_s, a])?; Ok(()) }).unwrap();
    let pid = playlist_of(&f, &[a, b], "Mix: \"A/B\"").await;

    let (s, h, body) = f.call("GET", &format!("/playlists/{pid}/export"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h["content-type"].to_str().unwrap().starts_with("audio/x-mpegurl"));
    assert!(h["content-disposition"].to_str().unwrap().contains("Mix_ _A_B_.m3u8"));
    let m3u = String::from_utf8(body).unwrap();
    assert_eq!(m3u, format!("#EXTM3U\n#EXTINF:61,Yuku - Noon\n{}\n", real_dir.join("a.mp3").display()), "missing file left out");

    let (s, h, body) = f.call("GET", &format!("/playlists/{pid}/export?format=csv"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h["content-type"].to_str().unwrap().starts_with("text/csv"));
    let csv = String::from_utf8(body).unwrap();
    let lines: Vec<&str> = csv.split("\r\n").collect();
    assert_eq!(lines[0], "title,artist,album,year,duration_seconds,bpm,camelot,energy,rating,loved,play_count,tags,added_at,path");
    assert!(lines[1].starts_with("Noon,Yuku,Noon EP,2021,62,128.0,8A,,,no,0,dub; techno,2020-01-01T00:00:00,"), "{}", lines[1]);
    assert!(lines[2].starts_with("Gone,Yuku,"), "csv keeps the missing file");

    let (s, h, body) = f.call("GET", &format!("/playlists/{pid}/export?format=zip"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["content-type"], "application/zip");
    assert_eq!(names_in_zip(&body), vec!["Mix_ _A_B_/01 Yuku - Noon.mp3"]);
    assert_eq!(f.json("GET", &format!("/playlists/{pid}/export?format=bogus"), None).await.0, StatusCode::UNPROCESSABLE_ENTITY);
}

// ---------------------------------------------------------------------------------------
// DJ sets
// ---------------------------------------------------------------------------------------

async fn new_set(f: &Fix, body: Value) -> Value {
    f.ok("POST", "/sets", Some(body)).await
}

#[tokio::test]
async fn set_create_empty_and_from_playlist() {
    let f = Fix::new();
    let a = f.add("A", "Art", Opts { bpm: Some(126.0), camelot: Some("8A"), energy: Some(0.64), duration_ms: Some(200_000), ..Default::default() });
    let b = f.add("B", "Art", Opts { bpm: Some(128.0), camelot: Some("9A"), duration_ms: Some(180_000), ..Default::default() });
    let pid = playlist_of(&f, &[b, a], "Src").await;

    let empty = new_set(&f, json!({"name": "  Friday ", "venue": "Club", "target_minutes": 60})).await;
    assert_eq!(empty["name"], "Friday");
    assert_eq!(empty["venue"], "Club");
    assert_eq!(empty["status"], "draft");
    assert_eq!(empty["items"], json!([]));
    assert_eq!(empty["summary"]["track_count"], 0);
    assert_eq!(empty["summary"]["target_ms"], 3_600_000);
    assert_eq!(empty["pool_sources"], json!([]));

    let d = new_set(&f, json!({"name": "", "from_playlist_id": pid, "pool_sources": [{"kind": "loved"}, {"kind": "tag", "tag": "dub", "name": "Dub"}]})).await;
    assert_eq!(d["name"], "Untitled set");
    assert_eq!(d["items"].as_array().unwrap().len(), 2);
    assert_eq!(d["items"][0]["title"], "B");
    assert_eq!(d["items"][1]["title"], "A");
    assert_eq!(d["items"][0]["art_url"], Value::Null);
    assert_eq!(d["pool_sources"].as_array().unwrap().len(), 2);
    assert_eq!(d["pool_sources"][1]["tag"], "dub");
    // a dead playlist seeds nothing
    assert_eq!(new_set(&f, json!({"name": "x", "from_playlist_id": 999})).await["items"], json!([]));
    // a source missing its reference is refused
    assert_eq!(f.json("POST", "/sets", Some(json!({"name": "x", "pool_sources": [{"kind": "tag"}]}))).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn set_items_freeze_a_snapshot_and_survive_a_deleted_track() {
    let f = Fix::new();
    let a = f.add("Alpha", "Art", Opts { bpm: Some(126.0), camelot: Some("8A"), energy: Some(0.64), duration_ms: Some(200_000), cover: true, ..Default::default() });
    let b = f.simple("Beta");
    let set = new_set(&f, json!({"name": "S"})).await;
    let sid = set["id"].as_i64().unwrap();

    let d = f.ok("POST", &format!("/sets/{sid}/items"), Some(json!({"track_ids": [a, b]}))).await;
    let items = d["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["track_id"], a);
    assert_eq!(items[0]["title"], "Alpha");
    assert_eq!(items[0]["artist"], "Art");
    assert_eq!(items[0]["bpm"], 126.0);
    assert_eq!(items[0]["camelot"], "8A");
    assert_eq!(items[0]["art_url"], "/api/art/release/1?size=thumb&v=v1");
    assert_eq!(items[0]["missing"], false);
    assert_eq!(items[0]["beat_offset_ms"], 12.5);
    assert_eq!(items[0]["loudness_lufs"], -9.0);
    // energy comes from the item (or its snapshot: round(0.64 * 10) = 6)
    assert_eq!(items[0]["energy"], 6);

    // the snapshot is in the row
    let snap: Value = serde_json::from_str(
        &f.q(move |c| c.query_row("SELECT snapshot FROM dj_set_items WHERE set_id = ?1 AND track_id = ?2", [sid, a], |r| r.get::<_, String>(0)).unwrap()),
    )
    .unwrap();
    assert_eq!(snap["title"], "Alpha");
    assert_eq!(snap["artist"], "Art");
    assert_eq!(snap["duration_ms"], 200_000);
    assert_eq!(snap["bpm"], 126.0);
    assert_eq!(snap["camelot"], "8A");
    assert_eq!(snap["energy"], 6);
    assert_eq!(snap["beat_offset_ms"], 12.5);

    // delete the track: the slot survives from its snapshot
    f.db.write(move |t| { t.execute("DELETE FROM tracks WHERE id = ?1", [a])?; Ok(()) }).unwrap();
    let d = f.ok("GET", &format!("/sets/{sid}"), None).await;
    let first = &d["items"][0];
    assert_eq!(first["track_id"], Value::Null);
    assert_eq!(first["title"], "Alpha");
    assert_eq!(first["artist"], "Art");
    assert_eq!(first["duration_ms"], 200_000);
    assert_eq!(first["bpm"], 126.0);
    assert_eq!(first["camelot"], "8A");
    assert_eq!(first["missing"], true);
    assert_eq!(first["art_url"], "/api/art/release/1?size=thumb&v=v1", "cover frozen in the snapshot");
    assert_eq!(first["beat_offset_ms"], 12.5);
    assert_eq!(d["items"][1]["missing"], false);

    // a track whose file is gone is flagged too
    f.db.write(move |t| { t.execute("UPDATE files SET missing_since = 'x' WHERE track_id = ?1", [b])?; Ok(()) }).unwrap();
    let d = f.ok("GET", &format!("/sets/{sid}"), None).await;
    assert_eq!(d["items"][1]["missing"], true);
    assert_eq!(d["items"][1]["title"], "Beta");
}

#[tokio::test]
async fn set_item_edit_move_delete_and_errors() {
    let f = Fix::new();
    let ids: Vec<i64> = (0..3).map(|i| f.add(&format!("t{i}"), "Art", Opts { bpm: Some(120.0), camelot: Some("8A"), duration_ms: Some(240_000), ..Default::default() })).collect();
    let sid = new_set(&f, json!({"name": "S"})).await["id"].as_i64().unwrap();
    let d = f.ok("POST", &format!("/sets/{sid}/items"), Some(json!({"track_ids": ids}))).await;
    let item = |d: &Value, n: usize| d["items"][n]["id"].as_i64().unwrap();
    let i0 = item(&d, 0);

    let d = f
        .ok("PATCH", &format!("/sets/{sid}/items/{i0}"), Some(json!({
            "cue_in_ms": 10_000, "cue_out_ms": 200_000, "tempo_adjust_pct": 4.0, "key_lock": false,
            "transition_type": "blend", "transition_beats": 16, "transition_notes": "bass swap", "energy": 7
        })))
        .await;
    let first = &d["items"][0];
    assert_eq!(first["cue_in_ms"], 10_000);
    assert_eq!(first["cue_out_ms"], 200_000);
    assert_eq!(first["tempo_adjust_pct"], 4.0);
    assert_eq!(first["key_lock"], false);
    assert_eq!(first["transition_type"], "blend");
    assert_eq!(first["transition_beats"], 16);
    assert_eq!(first["transition_notes"], "bass swap");
    assert_eq!(first["energy"], 7);
    assert_eq!(first["effective_bpm"], 124.8);
    // cue span (190s) at +4 % plays 182.692s
    assert_eq!(first["played_ms"], 182_692);
    // pitching without key lock moves the key: effective key differs from the stored one
    assert_ne!(first["effective_camelot"], first["camelot"]);

    // an explicit null clears a cue; absent fields stay put
    let d = f.ok("PATCH", &format!("/sets/{sid}/items/{i0}"), Some(json!({"cue_in_ms": null}))).await;
    assert_eq!(d["items"][0]["cue_in_ms"], Value::Null);
    assert_eq!(d["items"][0]["cue_out_ms"], 200_000);
    assert_eq!(d["items"][0]["key_lock"], false);

    for bad in [json!({"energy": 0}), json!({"energy": 11}), json!({"cue_in_ms": "soon"})] {
        assert_eq!(f.json("PATCH", &format!("/sets/{sid}/items/{i0}"), Some(bad)).await.0, StatusCode::BAD_REQUEST);
    }
    assert_eq!(f.json("PATCH", &format!("/sets/{sid}/items/99999"), Some(json!({"energy": 3}))).await.0, StatusCode::NOT_FOUND);

    // move: first item to index 2 of the list without it
    let d = f.ok("POST", &format!("/sets/{sid}/items/{i0}/move"), Some(json!({"to_index": 2}))).await;
    let titles: Vec<&str> = d["items"].as_array().unwrap().iter().map(|i| i["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["t1", "t2", "t0"]);
    assert_eq!(d["items"][2]["index"], 2);
    assert_eq!(f.json("POST", &format!("/sets/{sid}/items/99999/move"), Some(json!({"to_index": 0}))).await.0, StatusCode::NOT_FOUND);

    // delete
    let d = f.ok("DELETE", &format!("/sets/{sid}/items/{i0}"), None).await;
    assert_eq!(d["items"].as_array().unwrap().len(), 2);
    assert_eq!(f.json("DELETE", &format!("/sets/{sid}/items/{i0}"), None).await.0, StatusCode::NOT_FOUND);
    // an item of another set is not reachable through this one
    let other = new_set(&f, json!({"name": "O"})).await["id"].as_i64().unwrap();
    let j = item(&d, 0);
    assert_eq!(f.json("DELETE", &format!("/sets/{other}/items/{j}"), None).await.0, StatusCode::NOT_FOUND);
    // add errors
    assert_eq!(f.json("POST", &format!("/sets/{sid}/items"), Some(json!({"track_ids": []}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.json("POST", &format!("/sets/{sid}/items"), Some(json!({"track_ids": [987654]}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.json("POST", "/sets/999/items", Some(json!({"track_ids": [1]}))).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn add_items_at_an_index_keeps_batch_order() {
    let f = Fix::new();
    let (head, tail, a, b) = (f.simple("head"), f.simple("tail"), f.simple("a"), f.simple("b"));
    let sid = new_set(&f, json!({"name": "S"})).await["id"].as_i64().unwrap();
    f.ok("POST", &format!("/sets/{sid}/items"), Some(json!({"track_ids": [head, tail]}))).await;
    let d = f.ok("POST", &format!("/sets/{sid}/items"), Some(json!({"track_ids": [a, b], "at_index": 1}))).await;
    let got: Vec<i64> = d["items"].as_array().unwrap().iter().map(|i| i["track_id"].as_i64().unwrap()).collect();
    assert_eq!(got, vec![head, a, b, tail]);
}

#[tokio::test]
async fn set_detail_has_summary_timing_and_transitions() {
    let f = Fix::new();
    let a = f.add("A", "X", Opts { bpm: Some(128.0), camelot: Some("8A"), duration_ms: Some(300_000), ..Default::default() });
    let b = f.add("B", "Y", Opts { bpm: Some(130.0), camelot: Some("9A"), duration_ms: Some(240_000), ..Default::default() });
    let c = f.add("C", "Z", Opts { bpm: Some(90.0), camelot: Some("2B"), duration_ms: Some(120_000), ..Default::default() });
    let sid = new_set(&f, json!({"name": "S", "target_minutes": 5})).await["id"].as_i64().unwrap();
    let d = f.ok("POST", &format!("/sets/{sid}/items"), Some(json!({"track_ids": [a, b, c]}))).await;
    let i1 = d["items"][1]["id"].as_i64().unwrap();
    let d = f.ok("PATCH", &format!("/sets/{sid}/items/{i1}"), Some(json!({"transition_beats": 32}))).await;

    // reference values computed by bc_music::setmath on the same slots
    let slot = |n: usize, bpm: f64, cam: &str, dur: i64, beats: Option<i64>| bc_music::setmath::Slot {
        index: n, bpm: Some(bpm), camelot: Some(cam.into()), duration_ms: Some(dur), transition_beats: beats, ..Default::default()
    };
    let slots = vec![slot(0, 128.0, "8A", 300_000, None), slot(1, 130.0, "9A", 240_000, Some(32)), slot(2, 90.0, "2B", 120_000, None)];
    let want = bc_music::setmath::summarise(&slots, Some(5));
    let want_starts = bc_music::setmath::start_times(&slots);

    assert_eq!(d["summary"]["track_count"], 3);
    assert_eq!(d["summary"]["played_ms"], want.played_ms);
    assert_eq!(d["summary"]["overlap_ms"], want.overlap_ms);
    assert!(want.overlap_ms > 0);
    assert_eq!(d["summary"]["total_ms"], want.total_ms);
    assert_eq!(d["summary"]["target_ms"], 300_000);
    assert_eq!(d["summary"]["over_target_ms"], want.over_target_ms().unwrap());
    assert_eq!(d["summary"]["bpm_min"], 90.0);
    assert_eq!(d["summary"]["bpm_max"], 130.0);
    assert_eq!(d["summary"]["avg_bpm"], bc_music::setmath::round_to((128.0 + 130.0 + 90.0) / 3.0, 1));
    for (n, start) in want_starts.iter().enumerate() {
        assert_eq!(d["items"][n]["start_ms"], *start, "start of slot {n}");
    }
    assert_eq!(d["items"][1]["start_ms"].as_i64().unwrap(), 300_000 - want.overlap_ms);
    let trans = d["transitions"].as_array().unwrap();
    assert_eq!(trans.len(), 2);
    assert_eq!(trans[0]["from_index"], 0);
    assert_eq!(trans[0]["to_index"], 1);
    assert_eq!(trans[0]["ok"], true, "8A -> 9A at +1.6 % is a fine join: {}", trans[0]);
    assert_eq!(trans[1]["ok"], false, "2B at 90 bpm after 9A at 130 is not");
    assert_eq!(d["summary"]["problem_transitions"], 1);
    assert_eq!(trans[0]["overlap_ms"], want.overlap_ms);
}

#[tokio::test]
async fn set_update_list_cards_and_delete() {
    let f = Fix::new();
    let a = f.add("A", "X", Opts { bpm: Some(120.0), duration_ms: Some(200_000), cover: true, ..Default::default() });
    let b = f.add("B", "Y", Opts { bpm: Some(130.0), duration_ms: Some(100_000), cover: true, ..Default::default() });
    let c = f.add("C", "Z", Opts { duration_ms: Some(50_000), ..Default::default() });
    let s1 = new_set(&f, json!({"name": "One", "venue": "V", "pool_sources": [{"kind": "loved"}]})).await["id"].as_i64().unwrap();
    let s2 = new_set(&f, json!({"name": "Two"})).await["id"].as_i64().unwrap();
    let d = f.ok("POST", &format!("/sets/{s1}/items"), Some(json!({"track_ids": [a, b, c]}))).await;
    // cue out before cue in clamps to zero; cue out shortens
    let i_a = d["items"][0]["id"].as_i64().unwrap();
    let i_b = d["items"][1]["id"].as_i64().unwrap();
    f.ok("PATCH", &format!("/sets/{s1}/items/{i_a}"), Some(json!({"cue_in_ms": 20_000, "cue_out_ms": 120_000}))).await;
    f.ok("PATCH", &format!("/sets/{s1}/items/{i_b}"), Some(json!({"cue_in_ms": 90_000, "cue_out_ms": 10_000}))).await;
    // a deleted track still counts from its snapshot
    f.db.write(move |t| { t.execute("DELETE FROM tracks WHERE id = ?1", [c])?; Ok(()) }).unwrap();

    let list = f.ok("GET", "/sets", None).await;
    let cards = list.as_array().unwrap();
    assert_eq!(cards.len(), 2);
    let card = cards.iter().find(|c| c["id"] == s1).unwrap();
    assert_eq!(card["track_count"], 3);
    assert_eq!(card["est_duration_ms"], 100_000 + 50_000);
    assert_eq!(card["avg_bpm"], 125.0);
    assert_eq!(card["pool_source_count"], 1);
    assert_eq!(card["art_urls"].as_array().unwrap().len(), 2);
    assert_eq!(card["status"], "draft");
    assert!(card["updated_at"].as_str().unwrap().ends_with('Z'));
    let empty = cards.iter().find(|c| c["id"] == s2).unwrap();
    assert_eq!(empty["track_count"], 0);
    assert_eq!(empty["avg_bpm"], Value::Null);
    assert_eq!(empty["art_urls"], json!([]));

    // update: fields, explicit null clears, bad status refused, pool [] clears
    let d = f.ok("PATCH", &format!("/sets/{s1}"), Some(json!({"name": "Renamed", "status": "ready", "notes": "n", "event_date": "2026-01-01", "target_minutes": 90, "venue": null, "pool_sources": []}))).await;
    assert_eq!(d["name"], "Renamed");
    assert_eq!(d["status"], "ready");
    assert_eq!(d["notes"], "n");
    assert_eq!(d["event_date"], "2026-01-01");
    assert_eq!(d["target_minutes"], 90);
    assert_eq!(d["venue"], Value::Null);
    assert_eq!(d["pool_sources"], json!([]));
    let d = f.ok("PATCH", &format!("/sets/{s1}"), Some(json!({"notes": "n2"}))).await;
    assert_eq!(d["status"], "ready", "untouched");
    assert_eq!(f.json("PATCH", &format!("/sets/{s1}"), Some(json!({"status": "bogus"}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.json("PATCH", "/sets/999", Some(json!({"name": "x"}))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(f.json("GET", "/sets/999", None).await.0, StatusCode::NOT_FOUND);

    // delete cascades
    let (s, _, _) = f.call("DELETE", &format!("/sets/{s1}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(f.q(|c| c.query_row("SELECT COUNT(*) FROM dj_set_items", [], |r| r.get::<_, i64>(0)).unwrap()), 0);
    assert_eq!(f.json("DELETE", &format!("/sets/{s1}"), None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn set_tracks_and_exports() {
    let f = Fix::new();
    let dir = f.dir.path().join("audio");
    std::fs::create_dir_all(&dir).unwrap();
    let a = f.add("Noon", "Yuku", Opts::default());
    let b = f.add("Gone", "Yuku", Opts { missing: true, ..Default::default() });
    let c = f.add("Dusk", "Yuku", Opts::default());
    for (t, n) in [(a, "a.mp3"), (c, "c.flac")] {
        let p = dir.join(n);
        std::fs::write(&p, n.as_bytes()).unwrap();
        let ps = p.to_string_lossy().to_string();
        f.db.write(move |x| { x.execute("UPDATE files SET path = ?1 WHERE track_id = ?2", bc_db::rusqlite::params![ps, t])?; Ok(()) }).unwrap();
    }
    let sid = new_set(&f, json!({"name": "Set: one"})).await["id"].as_i64().unwrap();
    f.ok("POST", &format!("/sets/{sid}/items"), Some(json!({"track_ids": [a, b, c]}))).await;

    // playable tracks only, in order
    let page = f.ok("GET", &format!("/sets/{sid}/tracks"), None).await;
    assert_eq!(ids_of(&page), vec![a, c]);
    assert_eq!(page["total"], 2);

    let (s, h, body) = f.call("GET", &format!("/sets/{sid}/export"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h["content-disposition"].to_str().unwrap().contains("Set_ one.m3u8"));
    let m3u = String::from_utf8(body).unwrap();
    assert_eq!(m3u.lines().filter(|l| l.starts_with("#EXTINF")).count(), 2);
    assert!(m3u.contains("Yuku - Noon") && m3u.contains("Yuku - Dusk") && !m3u.contains("Gone"));

    let (_, _, body) = f.call("GET", &format!("/sets/{sid}/export?format=csv"), None).await;
    assert_eq!(String::from_utf8(body).unwrap().split("\r\n").filter(|l| !l.is_empty()).count(), 4, "header + 3 tracks, csv keeps the missing one");

    let (s, h, body) = f.call("GET", &format!("/sets/{sid}/export?format=zip"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h["content-type"], "application/zip");
    assert_eq!(names_in_zip(&body), vec!["Set_ one/01 Yuku - Noon.mp3", "Set_ one/02 Yuku - Dusk.flac"]);

    assert_eq!(f.json("GET", &format!("/sets/{sid}/export?format=mp3"), None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.json("GET", "/sets/999/export", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(f.json("GET", "/sets/999/tracks", None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mutations_publish_invalidations() {
    let f = Fix::new();
    let mut rx = f.ctx.bus.subscribe();
    let pid = playlist_of(&f, &[], "P").await;
    let sid = new_set(&f, json!({"name": "S"})).await["id"].as_i64().unwrap();
    let mut seen = vec![];
    while let Ok(ev) = rx.try_recv() {
        seen.push((ev.payload["entity"].as_str().unwrap_or("").to_string(), ev.payload["ids"].clone()));
    }
    assert!(seen.contains(&("playlist".into(), json!([pid]))), "{seen:?}");
    assert!(seen.contains(&("set".into(), json!([sid]))), "{seen:?}");
}

#[tokio::test]
async fn list_endpoints_do_not_scale_queries_with_row_count() {
    // 150 sets and 150 playlists with items: one pass each, comfortably fast.
    let f = Fix::new();
    let ids: Vec<i64> = (0..5).map(|i| f.add(&format!("t{i}"), "A", Opts { bpm: Some(120.0), cover: true, ..Default::default() })).collect();
    let ids2 = ids.clone();
    f.db
        .write(move |t| {
            for n in 0..150 {
                t.execute("INSERT INTO playlists(name,kind,created_at,updated_at) VALUES (?1,'manual','2020-01-01 00:00:00.000000','2020-01-01 00:00:00.000000')", [format!("p{n}")])?;
                let pid = t.last_insert_rowid();
                t.execute("INSERT INTO dj_sets(name,status,created_at,updated_at) VALUES (?1,'draft','2020-01-01 00:00:00.000000','2020-01-01 00:00:00.000000')", [format!("s{n}")])?;
                let sid = t.last_insert_rowid();
                for (i, tid) in ids2.iter().enumerate() {
                    t.execute("INSERT INTO playlist_items(playlist_id,track_id,position,added_at) VALUES (?1,?2,?3,'x')", bc_db::rusqlite::params![pid, tid, (i + 1) as f64 * 1024.0])?;
                    t.execute("INSERT INTO dj_set_items(set_id,track_id,position,tempo_adjust_pct,key_lock,snapshot) VALUES (?1,?2,?3,0,1,'{}')", bc_db::rusqlite::params![sid, tid, (i + 1) as f64 * 1024.0])?;
                }
            }
            Ok(())
        })
        .unwrap();
    let t0 = std::time::Instant::now();
    let l = f.ok("GET", "/playlists", None).await;
    let s = f.ok("GET", "/sets", None).await;
    assert_eq!(l.as_array().unwrap().len(), 150);
    assert_eq!(s.as_array().unwrap().len(), 150);
    assert_eq!(s[0]["track_count"], 5);
    assert_eq!(s[0]["art_urls"].as_array().unwrap().len(), 4);
    assert!(t0.elapsed().as_secs() < 5, "{:?}", t0.elapsed());
}

// ---------------------------------------------------------------------------------------
// test_zip_export.py
// ---------------------------------------------------------------------------------------

mod zip_export {
    use super::*;
    use crate::export::{self, CHUNK};
    use std::io::Read;
    use std::path::Path;

    fn make(dir: &Path, name: &str, data: &[u8]) -> String {
        let p = dir.join(name);
        std::fs::write(&p, data).unwrap();
        p.to_string_lossy().into_owned()
    }
    fn item(t: &str, a: &str, p: String) -> (String, String, String) {
        (t.into(), a.into(), p)
    }
    fn arcnames(e: &[(String, std::path::PathBuf)]) -> Vec<String> {
        e.iter().map(|x| x.0.clone()).collect()
    }

    #[test]
    fn entries_number_and_sanitize() {
        let d = tempfile::tempdir().unwrap();
        let items = vec![
            item("Sirgblot [99C005]", "West Code", make(d.path(), "a.mp3", b"a")),
            item("Trip / Speeder: \"live\"", "xyiz", make(d.path(), "b.flac", b"b")),
        ];
        assert_eq!(
            arcnames(&export::track_entries(&items, None)),
            vec!["01 West Code - Sirgblot [99C005].mp3", "02 xyiz - Trip _ Speeder_ _live_.flac"]
        );
    }

    #[test]
    fn entries_nest_flat_under_one_folder() {
        let d = tempfile::tempdir().unwrap();
        let items = vec![item("Noon", "Yuku", make(d.path(), "a.mp3", b"a")), item("First Light", "Yuku", make(d.path(), "b.mp3", b"b"))];
        assert_eq!(
            arcnames(&export::track_entries(&items, Some("Späti \"Rec\""))),
            vec!["Späti _Rec_/01 Yuku - Noon.mp3", "Späti _Rec_/02 Yuku - First Light.mp3"]
        );
    }

    #[test]
    fn entries_skip_missing_files() {
        let d = tempfile::tempdir().unwrap();
        let items = vec![
            item("Here", "A", make(d.path(), "here.mp3", b"x")),
            item("Gone", "B", d.path().join("gone.mp3").to_string_lossy().into_owned()),
            item("Pathless", "C", String::new()),
        ];
        let e = export::track_entries(&items, None);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].0, "01 A - Here.mp3");
    }

    #[test]
    fn entries_without_artist_and_with_wide_numbering() {
        let d = tempfile::tempdir().unwrap();
        let items: Vec<_> = (0..101).map(|i| item(&format!("t{i}"), "", make(d.path(), &format!("{i}.MP3"), b"x"))).collect();
        let e = export::track_entries(&items, None);
        assert_eq!(e[0].0, "001 t0.mp3", "width grows with the count; extension lowercased");
        assert_eq!(e[100].0, "101 t100.mp3");
    }

    #[test]
    fn stream_zip_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let payloads: Vec<(String, Vec<u8>)> = (1u8..=3).map(|i| (format!("track{i}"), vec![i; 3 * CHUNK / 2])).collect();
        let items: Vec<_> = payloads.iter().map(|(t, data)| item(t, "Artist", make(d.path(), &format!("{t}.mp3"), data))).collect();
        let entries = export::track_entries(&items, None);

        let mut blob = Vec::new();
        export::write_zip(&mut blob, &entries).unwrap();
        let mut z = zip::ZipArchive::new(Cursor::new(blob)).unwrap();
        let names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().to_string()).collect();
        assert_eq!(names, vec!["01 Artist - track1.mp3", "02 Artist - track2.mp3", "03 Artist - track3.mp3"]);
        for (i, (_, data)) in payloads.iter().enumerate() {
            let mut f = z.by_index(i).unwrap();
            // Stored, not deflated: audio is already compressed.
            assert_eq!(f.compression(), zip::CompressionMethod::Stored);
            let mut got = Vec::new();
            f.read_to_end(&mut got).unwrap();
            assert_eq!(&got, data);
        }
    }

    #[test]
    fn stream_zip_skips_file_deleted_after_listing() {
        let d = tempfile::tempdir().unwrap();
        let keep = make(d.path(), "keep.mp3", b"k");
        let doomed = make(d.path(), "doomed.mp3", b"d");
        let entries = export::track_entries(&[item("Keep", "", keep), item("Doomed", "", doomed.clone())], None);
        std::fs::remove_file(doomed).unwrap();

        let mut blob = Vec::new();
        export::write_zip(&mut blob, &entries).unwrap();
        let mut z = zip::ZipArchive::new(Cursor::new(blob)).unwrap();
        assert_eq!(z.len(), 1);
        let mut f = z.by_name("01 Keep.mp3").unwrap();
        let mut got = Vec::new();
        f.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"k");
    }

    #[tokio::test]
    async fn zip_response_streams_the_archive_in_chunks() {
        let d = tempfile::tempdir().unwrap();
        // 6 MiB across three files: more than the channel can hold, so the body must be streamed
        let items: Vec<_> = (0..3).map(|i| item(&format!("t{i}"), "A", make(d.path(), &format!("{i}.mp3"), &vec![i as u8; 2 << 20]))).collect();
        let resp = export::zip_response(export::track_entries(&items, Some("Dir")), "Dir");
        assert_eq!(resp.headers()["content-type"], "application/zip");
        let mut body = resp.into_body();
        let (mut frames, mut total) = (0, 0usize);
        let mut blob = Vec::new();
        while let Some(frame) = body.frame().await {
            if let Some(data) = frame.unwrap().data_ref() {
                frames += 1;
                total += data.len();
                blob.extend_from_slice(data);
            }
        }
        assert!(frames > 6, "streamed in many chunks, not one buffer ({frames})");
        assert!(total > 6 << 20);
        assert_eq!(names_in_zip(&blob), vec!["Dir/1 A - t0.mp3".replace("1 A", "01 A"), "Dir/02 A - t1.mp3".into(), "Dir/03 A - t2.mp3".into()]);
    }

    #[test]
    fn zip64_entry_and_directory_read_back() {
        // a file declared (by its metadata) as 5 GiB takes the ZIP64 path; the bytes are small
        let mut sink = Vec::new();
        let mut zw = export::ZipStream::new(&mut sink);
        zw.add_stored("big.bin", &mut Cursor::new(b"hello zip64".to_vec()), 5 << 30, (0, 33)).unwrap();
        zw.add_stored("small.txt", &mut Cursor::new(b"small".to_vec()), 5, (0, 33)).unwrap();
        zw.finish().unwrap();
        let mut z = zip::ZipArchive::new(Cursor::new(sink)).unwrap();
        assert_eq!(z.len(), 2);
        let mut got = String::new();
        z.by_name("big.bin").unwrap().read_to_string(&mut got).unwrap();
        assert_eq!(got, "hello zip64");
        got.clear();
        z.by_name("small.txt").unwrap().read_to_string(&mut got).unwrap();
        assert_eq!(got, "small");
    }

    #[test]
    fn many_entries_use_the_zip64_directory() {
        let mut sink = Vec::new();
        let mut zw = export::ZipStream::new(&mut sink);
        for i in 0..70_000 {
            zw.add_stored(&format!("{i}"), &mut Cursor::new(Vec::new()), 0, (0, 33)).unwrap();
        }
        zw.finish().unwrap();
        let z = zip::ZipArchive::new(Cursor::new(sink)).unwrap();
        assert_eq!(z.len(), 70_000);
    }

    #[test]
    fn an_external_unzipper_agrees() {
        let d = tempfile::tempdir().unwrap();
        let items = vec![item("Ünï", "Ärtist", make(d.path(), "a.mp3", &vec![7u8; 3 << 20])), item("Two", "B", make(d.path(), "b.flac", b"bb"))];
        let out = d.path().join("out.zip");
        let mut f = std::fs::File::create(&out).unwrap();
        export::write_zip(&mut f, &export::track_entries(&items, Some("Dir"))).unwrap();
        drop(f);
        match std::process::Command::new("unzip").arg("-tq").arg(&out).output() {
            Ok(o) => assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stdout)),
            Err(_) => eprintln!("unzip not installed; skipped"),
        }
    }

    #[test]
    fn attachment_headers_handle_awkward_names() {
        let h = export::attachment_headers("Späti \"Nights\".zip");
        let d = h["content-disposition"].to_str().unwrap();
        assert!(d.starts_with("attachment;"));
        assert!(!d.contains('\n'));
        assert!(d.contains("filename*=UTF-8''Sp%C3%A4ti%20%22Nights%22.zip"), "{d}");
        assert!(d.contains("filename=\"Sp?ti 'Nights'.zip\""), "{d}");
    }

    #[test]
    fn safe_component_rules() {
        assert_eq!(export::safe_name("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(export::safe_name("  ..x.. "), "x");
        assert_eq!(export::safe_name(" . "), "untitled");
        assert_eq!(export::safe_component("", "fb"), "fb");
        assert_eq!(export::safe_name(&"x".repeat(400)).chars().count(), 150);
        assert_eq!(export::safe_name("tab\there"), "tab_here");
    }
}

// ---------------------------------------------------------------------------------------
// test_export_naming.py (the naming and the zip contents; the /tracks/export route itself is the
// library crate's, it calls these functions)
// ---------------------------------------------------------------------------------------

mod export_naming {
    use super::*;
    use crate::export;
    use bc_types::library::ExportFormat;
    use axum::http::header;

    const LABEL: &str = "Späti \"Rec\"";

    struct Lib {
        f: Fix,
        artist: i64,
        label: i64,
        sundial: i64,
    }

    /// One label, one artist, two records -- the older one second in the table.
    fn library() -> Lib {
        let f = Fix::new();
        let audio = f.dir.path().join("audio");
        std::fs::create_dir_all(&audio).unwrap();
        let rec = |title: &str, year: i64, titles: &[&str]| {
            for (n, t) in titles.iter().enumerate() {
                let id = f.add(t, "Yuku", Opts { label: Some(LABEL), year: Some(year), ..Default::default() });
                let path = audio.join(format!("{title} {}.mp3", n + 1));
                std::fs::write(&path, format!("{title}{}", n + 1)).unwrap();
                let ps = path.to_string_lossy().to_string();
                let (title, no) = (title.to_string(), n as i64 + 1);
                f.db
                    .write(move |c| {
                        // the helper made one release per track: gather them on one record
                        let rid: i64 = match c.query_row("SELECT id FROM releases WHERE title = ?1", [&title], |r| r.get(0)) {
                            Ok(r) => r,
                            Err(_) => {
                                c.execute("UPDATE releases SET title = ?1, title_key = ?2 WHERE id = (SELECT release_id FROM tracks WHERE id = ?3)", bc_db::rusqlite::params![title, name_key(&title), id])?;
                                c.query_row("SELECT release_id FROM tracks WHERE id = ?1", [id], |r| r.get(0))?
                            }
                        };
                        c.execute("UPDATE tracks SET release_id = ?1, track_no = ?2 WHERE id = ?3", bc_db::rusqlite::params![rid, no, id])?;
                        c.execute("UPDATE files SET path = ?1 WHERE track_id = ?2", bc_db::rusqlite::params![ps, id])?;
                        Ok(())
                    })
                    .unwrap();
            }
        };
        rec("Sundial", 2021, &["Noon", "Dusk"]);
        rec("Dawnfall", 2019, &["First Light"]);
        let (artist, label, sundial) = f.q(|c| {
            (
                c.query_row("SELECT id FROM artists WHERE name = 'Yuku'", [], |r| r.get(0)).unwrap(),
                c.query_row("SELECT id FROM labels LIMIT 1", [], |r| r.get(0)).unwrap(),
                c.query_row("SELECT id FROM releases WHERE title = 'Sundial'", [], |r| r.get(0)).unwrap(),
            )
        });
        Lib { f, artist, label, sundial }
    }

    /// What `/tracks/export` does with a filter: name, listening order (a record, artist or
    /// label comes out by year, title, disc, track), then the response.
    fn export_of(l: &Lib, q: TrackQuery, format: ExportFormat) -> ApiResult<(String, axum::response::Response)> {
        let by_record = q.release_id.is_some() || !q.release_ids.is_empty() || q.artist_id.is_some() || q.label_id.is_some();
        l.f.db
            .read_with::<_, bc_libcore::ApiError>(|c| {
                let sql = if by_record {
                    "SELECT t.id FROM tracks t LEFT JOIN releases r ON r.id = t.release_id WHERE (?1 = 0 OR t.artist_id = ?1) AND (?2 = 0 OR r.label_id = ?2) AND (?3 = 0 OR r.id = ?3) ORDER BY r.year, r.title_key, t.disc_no, t.track_no, t.id"
                } else {
                    "SELECT t.id FROM tracks t ORDER BY t.added_at DESC, t.id"
                };
                let mut st = c.prepare(sql)?;
                let ids: Vec<i64> = if by_record {
                    st.query_map([q.artist_id.unwrap_or(0), q.label_id.unwrap_or(0), q.release_id.unwrap_or(0)], |r| r.get(0))?.collect::<Result<_, _>>()?
                } else {
                    st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
                };
                let name = export::export_name(c, &q)?;
                let tracks = export::load_export_tracks(c, &ids)?;
                Ok((name.clone(), export::export_response(format, &tracks, &name)?))
            })
    }

    fn filename(resp: &axum::response::Response) -> String {
        let d = resp.headers()[header::CONTENT_DISPOSITION].to_str().unwrap();
        d.split("filename*=UTF-8''").nth(1).unwrap().to_string()
    }
    fn quote(s: &str) -> String {
        s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"_.-~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
    }
    async fn body(resp: axum::response::Response) -> Vec<u8> {
        resp.into_body().collect().await.unwrap().to_bytes().to_vec()
    }

    #[tokio::test]
    async fn album_zip_is_named_after_the_record() {
        let l = library();
        let (_, resp) = export_of(&l, TrackQuery { release_id: Some(l.sundial), ..Default::default() }, ExportFormat::Zip).unwrap();
        assert_eq!(filename(&resp), quote("Yuku - Sundial.zip"));
        assert_eq!(names_in_zip(&body(resp).await), vec!["Yuku - Sundial/01 Yuku - Noon.mp3", "Yuku - Sundial/02 Yuku - Dusk.mp3"]);
    }

    #[tokio::test]
    async fn label_zip_gathers_the_catalogue_in_one_flat_folder() {
        let l = library();
        let (_, resp) = export_of(&l, TrackQuery { label_id: Some(l.label), ..Default::default() }, ExportFormat::Zip).unwrap();
        assert_eq!(filename(&resp), quote("Späti _Rec_.zip"));
        // flat: two records, no per-album subfolder, oldest first and each record in track order
        assert_eq!(
            names_in_zip(&body(resp).await),
            vec!["Späti _Rec_/01 Yuku - First Light.mp3", "Späti _Rec_/02 Yuku - Noon.mp3", "Späti _Rec_/03 Yuku - Dusk.mp3"]
        );
    }

    #[tokio::test]
    async fn artist_zip_is_named_after_the_artist() {
        let l = library();
        let (_, resp) = export_of(&l, TrackQuery { artist_id: Some(l.artist), ..Default::default() }, ExportFormat::Zip).unwrap();
        assert_eq!(filename(&resp), quote("Yuku.zip"));
        assert!(names_in_zip(&body(resp).await).iter().all(|n| n.starts_with("Yuku/")));
    }

    #[tokio::test]
    async fn view_exports_fall_back_to_naming_the_filter() {
        let l = library();
        let name = |q: TrackQuery, fmt| filename(&export_of(&l, q, fmt).unwrap().1);
        assert_eq!(name(TrackQuery::default(), ExportFormat::Zip), quote("Tracks.zip"));
        assert_eq!(name(TrackQuery { q: Some("Noon".into()), ..Default::default() }, ExportFormat::M3u8), quote("Search - Noon.m3u8"));
        assert_eq!(name(TrackQuery { loved: Some(true), ..Default::default() }, ExportFormat::M3u8), quote("Loved.m3u8"));
        assert_eq!(name(TrackQuery { favorites: Some(true), ..Default::default() }, ExportFormat::M3u8), quote("Favourites.m3u8"));
        assert_eq!(name(TrackQuery { tags: vec!["dub".into()], ..Default::default() }, ExportFormat::Csv), quote("dub.csv"));
        assert_eq!(name(TrackQuery { tags: vec!["a".into(), "b".into()], ..Default::default() }, ExportFormat::Csv), quote("a, b.csv"));
        assert_eq!(name(TrackQuery { release_ids: vec![1, 2], ..Default::default() }, ExportFormat::Csv), quote("2 albums.csv"));
        // the other formats name the record the same way -- one view, three shapes
        assert_eq!(name(TrackQuery { release_id: Some(l.sundial), ..Default::default() }, ExportFormat::M3u8), quote("Yuku - Sundial.m3u8"));
        // a vanished record falls through to what is left
        assert_eq!(name(TrackQuery { release_id: Some(9999), loved: Some(true), ..Default::default() }, ExportFormat::M3u8), quote("Loved.m3u8"));
    }

    #[test]
    fn zip_without_a_single_file_on_disk_is_refused() {
        let f = Fix::new();
        f.simple("nowhere");
        let tracks = f.q(|c| export::load_export_tracks(c, &[1]).unwrap());
        let err = export::export_zip(&tracks, "Tracks").unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn export_rows_and_formats() {
        let f = Fix::new();
        let a = f.add("Comma, \"Quote\"", "Ünï", Opts { loved: true, tags: vec!["x"], ..Default::default() });
        let rows = f.q(move |c| export::load_export_tracks(c, &[a, a, 999]).unwrap());
        assert_eq!(rows.len(), 2, "duplicates kept in order, unknown skipped");
        assert_eq!(rows[0].artist, "Ünï");
        assert_eq!(rows[0].path, format!("/music/{a}.mp3"));
        assert!(!rows[0].missing);
        let csv = export::csv(&rows[..1]);
        assert!(csv.contains("\"Comma, \"\"Quote\"\"\",Ünï,"), "{csv}");
        assert!(csv.contains(",300,,,,,yes,0,x,2020-01-01T00:00:00,"), "{csv}");
        let m = export::m3u8(&rows[..1]);
        assert_eq!(m, format!("#EXTM3U\n#EXTINF:300,Ünï - Comma, \"Quote\"\n/music/{a}.mp3\n"));
        assert_eq!(export::m3u8(&[]), "#EXTM3U\n");
    }
}

/// `BC_PLIST_REAL_DB=/path/to/copy-of-legacy.db cargo test -p bc-plist real_library -- --ignored --nocapture`
#[tokio::test]
#[ignore]
async fn real_library_timings() {
    let Ok(path) = std::env::var("BC_PLIST_REAL_DB") else { return };
    let db = Db::open(path).unwrap();
    let ctx = Ctx::new(db.clone(), Arc::new(EventBus::new()), bc_core::Config::from_env());
    let app = crate::router(ctx.clone(), Arc::new(TestResolver));
    let f = Fix { db, ctx, app, dir: tempfile::tempdir().unwrap() };
    for uri in ["/playlists", "/sets"] {
        let t0 = std::time::Instant::now();
        let v = f.ok("GET", uri, None).await;
        println!("{uri}: {} rows in {:?}", v.as_array().unwrap().len(), t0.elapsed());
        for row in v.as_array().unwrap().iter().take(3) {
            let id = row["id"].as_i64().unwrap();
            let u = if uri == "/sets" { format!("/sets/{id}") } else { format!("/playlists/{id}/tracks") };
            let t0 = std::time::Instant::now();
            let d = f.ok("GET", &u, None).await;
            println!("  {u}: {:?} ({} bytes)", t0.elapsed(), d.to_string().len());
        }
    }
}
