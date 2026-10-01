//! `POST /library/import`: the importer as a tracked task into the live database.

mod common;

use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use common::*;
use serde_json::json;

fn legacy_db(dir: &std::path::Path) -> std::path::PathBuf {
    let p = dir.join("legacy.db");
    let c = bc_db::rusqlite::Connection::open(&p).unwrap();
    c.execute_batch(include_str!("../../bc-db/migrations/0001_legacy.sql")).unwrap();
    c.execute_batch(
        "INSERT INTO library_roots(id,path,kind,watch,enabled) VALUES (1,'/m','library',0,1);
         INSERT INTO artists(id,name,name_key,created_at) VALUES (1,'Old Artist','old artist','2020-01-01');
         INSERT INTO releases(id,title,title_key,artist_id,kind,year,added_at) VALUES (1,'Old Record','old record',1,'album',2019,'2020-01-01');
         INSERT INTO tracks(id,release_id,artist_id,title,title_key,loved,play_count,skip_count,added_at,is_snippet) VALUES (1,1,1,'Old Track','old track',1,2,0,'2020-01-01',0);
         INSERT INTO files(track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at) VALUES (1,1,'/m/a.mp3','a.mp3','mp3',5,1,'x','x');
         INSERT INTO search_index(track_id,title,artist,album,label,tags) VALUES (1,'Old Track','Old Artist','Old Record','','');",
    )
    .unwrap();
    p
}

async fn wait(a: &App, id: &str) -> serde_json::Value {
    let t = Instant::now();
    loop {
        let (_, v) = a.call(Method::GET, &format!("/library/tasks/{id}"), None).await;
        if v["state"] != "running" {
            return v;
        }
        assert!(t.elapsed() < Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn import_into_an_empty_library_then_refuse_without_force() {
    let a = app();
    let src = legacy_db(a.dir.path());
    let (s, v) = a.call(Method::POST, "/library/import", Some(json!({"from": src.to_string_lossy()}))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let task = wait(&a, v["job_id"].as_str().unwrap()).await;
    assert_eq!(task["state"], "done", "{task}");
    assert_eq!(task["result"]["ok"], true, "{}", task["result"]);
    let tracks = a.get("/tracks").await;
    assert_eq!(tracks["total"], 1);
    assert_eq!(tracks["items"][0]["title"], "Old Track");
    assert_eq!(a.get("/tracks?q=old").await["total"], 1, "FTS survived the restore");
    // schema is current again after the restore (v2 triggers work)
    assert_eq!(a.scalar("SELECT available FROM tracks WHERE id = 1"), 1);

    let (s, v) = a.call(Method::POST, "/library/import", Some(json!({"from": src.to_string_lossy()}))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    let (s, v) = a.call(Method::POST, "/library/import", Some(json!({"from": src.to_string_lossy(), "force": true}))).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let task = wait(&a, v["job_id"].as_str().unwrap()).await;
    assert_eq!(task["state"], "done", "{task}");
    assert!(std::fs::read_dir(a.dir.path()).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("pre-import")), "safety copy kept");
}

#[tokio::test]
async fn a_missing_source_is_a_400() {
    let a = app();
    let (s, _) = a.call(Method::POST, "/library/import", Some(json!({"from": "/nonexistent/library.db"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
