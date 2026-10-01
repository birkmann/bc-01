//! Port of tests/integration/test_library_flow.py: scan a folder, index it, serve it, stream it.
//! Fixtures: tag-free sine files from bc-media, tagged here with lofty.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use bc_db::Db;
use bc_library::LibraryService;
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{Accessor, ItemKey, Tag, TagExt, TagType};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Flow {
    dir: tempfile::TempDir,
    svc: LibraryService,
    router: axum::Router,
    music: PathBuf,
}

fn jpeg() -> Vec<u8> {
    let img = image::RgbImage::from_pixel(64, 64, image::Rgb([0, 153, 255]));
    let mut buf = std::io::Cursor::new(vec![]);
    img.write_to(&mut buf, image::ImageFormat::Jpeg).unwrap();
    buf.into_inner()
}

#[allow(clippy::too_many_arguments)]
fn write_album(root: &Path, artist: &str, album: &str, year: i32, genres: &[&str], titles: &[&str], art: bool, label: Option<&str>) {
    let folder = root.join(artist).join(album);
    std::fs::create_dir_all(&folder).unwrap();
    for (i, title) in titles.iter().enumerate() {
        let p = folder.join(format!("{:02} - {title}.mp3", i + 1));
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("../bc-media/tests/fixtures/sine.mp3"), &p).unwrap();
        tag(&p, title, artist, album, year, i as u32 + 1, genres, art, label);
    }
}

#[allow(clippy::too_many_arguments)]
fn tag(p: &Path, title: &str, artist: &str, album: &str, year: i32, no: u32, genres: &[&str], art: bool, label: Option<&str>) {
    let mut t = Tag::new(TagType::Id3v2);
    t.set_title(title.into());
    t.set_artist(artist.into());
    t.set_album(album.into());
    t.set_track(no);
    t.insert_text(ItemKey::RecordingDate, year.to_string());
    for g in genres {
        t.push(lofty::tag::TagItem::new(ItemKey::Genre, lofty::tag::ItemValue::Text((*g).into())));
    }
    if let Some(l) = label {
        t.insert_text(ItemKey::Label, l.into());
    }
    if art {
        t.push_picture(Picture::unchecked(jpeg()).pic_type(PictureType::CoverFront).mime_type(MimeType::Jpeg).build());
    }
    t.save_to_path(p, WriteOptions::default()).unwrap();
}

fn flow() -> Flow {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    write_album(&music, "Somatic", "Grid Failure", 2026, &["techno", "industrial"], &["Nocturnal", "Iron Lung"], true, None);
    write_album(&music, "Ferric", "Oxide Bloom", 2025, &["electro", "breakbeat"], &["Oxide", "Rust"], false, None);
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let mut cfg = bc_core::Config::from_env();
    cfg.data_dir = dir.path().join("data");
    cfg.library_root = Some(music.clone());
    cfg.download_dir = dir.path().join("dl-none");
    std::fs::create_dir_all(cfg.art_dir()).unwrap();
    let svc = LibraryService::new(db, Arc::new(bc_core::EventBus::new()), cfg);
    let router = svc.router();
    Flow { dir, svc, router, music }
}

impl Flow {
    async fn call(&self, m: Method, uri: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut req = Request::builder().method(m).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let b = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let resp = self.router.clone().oneshot(req.body(b).unwrap()).await.unwrap();
        let (st, h) = (resp.status(), resp.headers().clone());
        (st, h, to_bytes(resp.into_body(), 64 << 20).await.unwrap().to_vec())
    }
    async fn json(&self, m: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (s, _, b) = self.call(m, uri, body, &[]).await;
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }
    /// POST /library/scan, wait for the task, return the first ScanResult.
    async fn scan(&self) -> Value {
        let (s, v) = self.json(Method::POST, "/library/scan", None).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{v}");
        let id = v["job_id"].as_str().unwrap().to_string();
        let t = Instant::now();
        loop {
            let (_, st) = self.json(Method::GET, &format!("/library/scan/{id}"), None).await;
            if st["state"] == "done" {
                return st["results"].as_array().unwrap().iter().find(|r| r["files_seen"].as_i64().unwrap_or(0) > 0).cloned().unwrap_or(Value::Null);
            }
            assert_ne!(st["state"], "failed", "{st}");
            assert!(t.elapsed() < Duration::from_secs(60));
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
}

async fn started() -> Flow {
    let f = flow();
    f.svc.start().await;
    f
}

#[tokio::test]
async fn root_is_auto_registered_and_scan_indexes_everything() {
    let f = started().await;
    let (_, roots) = f.json(Method::GET, "/library/roots", None).await;
    assert!(roots.as_array().unwrap().iter().any(|r| r["kind"] == "library"));
    let rep = f.scan().await;
    assert_eq!((rep["files_seen"].as_i64(), rep["files_added"].as_i64(), rep["tracks_added"].as_i64()), (Some(4), Some(4), Some(4)));
    assert_eq!(rep["errors"], json!([]));
    let (_, stats) = f.json(Method::GET, "/library/stats", None).await;
    assert_eq!((stats["tracks"].as_i64(), stats["releases"].as_i64(), stats["artists"].as_i64(), stats["missing_files"].as_i64()), (Some(4), Some(2), Some(2), Some(0)));
    drop(f.dir);
}

#[tokio::test]
async fn rescan_is_incremental() {
    let f = started().await;
    f.scan().await;
    let again = f.scan().await;
    assert_eq!((again["files_unchanged"].as_i64(), again["files_added"].as_i64(), again["files_updated"].as_i64()), (Some(4), Some(0), Some(0)));
}

#[tokio::test]
async fn external_edit_is_detected_and_reindexed() {
    let f = started().await;
    f.scan().await;
    let target = walk(&f.music).into_iter().find(|p| p.to_string_lossy().contains("Iron Lung")).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    tag(&target, "Iron Lung (Dub)", "Somatic", "Grid Failure", 2026, 2, &["techno", "industrial"], true, None);
    let rep = f.scan().await;
    assert_eq!(rep["files_updated"], 1);
    let (_, hit) = f.json(Method::GET, "/tracks?q=Dub", None).await;
    assert_eq!(hit["total"], 1);
    assert_eq!(hit["items"][0]["title"], "Iron Lung (Dub)");
    let (_, old) = f.json(Method::GET, "/tracks?q=%22Iron%20Lung%22", None).await;
    assert_eq!(old["total"], 1, "still one row for the track (not duplicated)");
}

#[tokio::test]
async fn a_missing_file_is_marked_not_deleted_and_returns_when_restored() {
    let f = started().await;
    f.scan().await;
    let target = walk(&f.music).into_iter().find(|p| p.to_string_lossy().contains("Rust")).unwrap();
    let saved = std::fs::read(&target).unwrap();
    std::fs::remove_file(&target).unwrap();
    let rep = f.scan().await;
    assert_eq!(rep["files_missing"], 1);
    let (_, stats) = f.json(Method::GET, "/library/stats", None).await;
    assert_eq!(stats["missing_files"], 1);
    let (_, tracks) = f.json(Method::GET, "/tracks", None).await;
    assert_eq!(tracks["total"], 3, "tracks without a live file leave the default listing");
    let (_, all) = f.json(Method::GET, "/tracks?missing=true", None).await;
    assert_eq!(all["total"], 1);
    std::fs::write(&target, saved).unwrap();
    f.scan().await;
    let (_, stats) = f.json(Method::GET, "/library/stats", None).await;
    assert_eq!(stats["missing_files"], 0);
    assert_eq!(f.json(Method::GET, "/tracks", None).await.1["total"], 4);
}

#[tokio::test]
async fn art_is_extracted_streams_are_ranged_and_tags_become_facets() {
    let f = started().await;
    f.scan().await;
    let (_, rels) = f.json(Method::GET, "/releases?sort=title&order=asc", None).await;
    let grid = rels["items"].as_array().unwrap().iter().find(|r| r["title"] == "Grid Failure").unwrap().clone();
    let art = grid["art_url"].as_str().expect("embedded art extracted");
    assert!(art.starts_with("/api/art/release/"));
    let art_path = art.trim_start_matches("/api");
    let (s, h, body) = f.call(Method::GET, art_path, None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h[header::CONTENT_TYPE].to_str().unwrap().starts_with("image/"));
    assert!(h[header::CACHE_CONTROL].to_str().unwrap().contains("immutable"));
    assert!(!body.is_empty());
    let ferric = rels["items"].as_array().unwrap().iter().find(|r| r["title"] == "Oxide Bloom").unwrap();
    assert!(ferric["art_url"].is_null());

    let (_, tracks) = f.json(Method::GET, "/tracks?limit=1", None).await;
    let stream = tracks["items"][0]["stream_url"].as_str().unwrap().trim_start_matches("/api").to_string();
    let (s, h, full) = f.call(Method::GET, &stream, None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::ACCEPT_RANGES], "bytes");
    let (s, h, part) = f.call(Method::GET, &stream, None, &[("range", "bytes=0-99")]).await;
    assert_eq!(s, StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.len(), 100);
    assert!(h[header::CONTENT_RANGE].to_str().unwrap().starts_with("bytes 0-99/"));
    let (s, _, tail) = f.call(Method::GET, &stream, None, &[("range", "bytes=-50")]).await;
    assert_eq!((s, tail.len()), (StatusCode::PARTIAL_CONTENT, 50));
    assert_eq!(&tail[..], &full[full.len() - 50..]);
    let (s, _, _) = f.call(Method::GET, &stream, None, &[("range", "bytes=999999999-")]).await;
    assert_eq!(s, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(f.call(Method::GET, "/stream/99999", None, &[]).await.0, StatusCode::NOT_FOUND);

    let (_, tags) = f.json(Method::GET, "/tags", None).await;
    let names: Vec<&str> = tags.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for n in ["techno", "industrial", "electro", "breakbeat"] {
        assert!(names.iter().any(|x| x.eq_ignore_ascii_case(n)), "{n} in {names:?}");
    }
    assert_eq!(f.json(Method::GET, "/tracks?tags=TECHNO", None).await.1["total"], 2);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[tokio::test]
async fn exports_m3u8_csv_zip_for_a_filtered_view() {
    let f = started().await;
    f.scan().await;
    let (s, h, body) = f.call(Method::GET, "/tracks/export?tags=techno&format=m3u8", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h[header::CONTENT_TYPE].to_str().unwrap().contains("mpegurl"));
    let text = String::from_utf8(body).unwrap();
    assert!(text.starts_with("#EXTM3U") && text.matches("#EXTINF").count() == 2, "{text}");
    let (s, _, csv) = f.call(Method::GET, "/tracks/export?format=csv", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(String::from_utf8(csv).unwrap().lines().count(), 5);
    let (s, h, zip) = f.call(Method::GET, "/tracks/export?release_id=1&format=zip", None, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert!(h[header::CONTENT_DISPOSITION].to_str().unwrap().contains(".zip"));
    assert_eq!(&zip[..2], b"PK");
}

#[tokio::test]
async fn a_metadata_write_is_not_seen_as_an_external_edit_by_the_scanner() {
    let f = started().await;
    f.scan().await;
    f.svc.ctx().db.write(|t| {
        t.execute("INSERT INTO analysis(track_id,analyzer_version,backend,status,analyzed_at,bpm,bpm_confidence,key_root,key_mode,camelot,key_confidence,energy)
                   SELECT id,1,'t','ok','x',128.0,0.9,9,'minor','8A',0.8,0.5 FROM tracks", [])?;
        Ok(())
    })
    .unwrap();
    let (s, v) = f.json(Method::POST, "/metadata/write", Some(json!({"dry_run": false, "confirm": true}))).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let id = v["job_id"].as_str().unwrap().to_string();
    let t = Instant::now();
    while f.svc.ctx().jobs.get(&id).map(|j| j.state == "running").unwrap_or(false) {
        assert!(t.elapsed() < Duration::from_secs(30));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let rep = f.scan().await;
    assert_eq!((rep["files_unchanged"].as_i64(), rep["files_updated"].as_i64()), (Some(4), Some(0)), "the re-stamp makes our own write invisible");
}
