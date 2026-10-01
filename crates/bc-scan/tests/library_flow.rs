//! Scan-related cases of the legacy `tests/integration/test_library_flow.py`, over the router,
//! with audio synthesised by `ffmpeg` (tests are skipped when it is absent).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_libcore::Ctx;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn run(args: &[&str]) {
    let st = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().expect("ffmpeg");
    assert!(st.success(), "ffmpeg {args:?}");
}

fn make_cover(dir: &Path) -> PathBuf {
    let p = dir.join("cover-src.jpg");
    run(&["-f", "lavfi", "-i", "color=c=0x0099ff:s=96x96", "-frames:v", "1", p.to_str().unwrap()]);
    p
}

#[allow(clippy::too_many_arguments)]
fn write_album(root: &Path, cover: Option<&Path>, artist: &str, album: &str, year: u32, genres: &str, titles: &[&str], label: Option<&str>) {
    let folder = root.join(artist).join(album);
    std::fs::create_dir_all(&folder).unwrap();
    for (n, title) in titles.iter().enumerate() {
        let path = folder.join(format!("{:02} - {title}.mp3", n + 1));
        let mut a: Vec<String> = vec!["-f".into(), "lavfi".into(), "-i".into(), "sine=frequency=300:duration=2".into()];
        if let Some(c) = cover {
            a.extend(["-i".into(), c.to_string_lossy().into_owned(), "-map".into(), "0:a".into(), "-map".into(), "1:v".into(), "-c:v".into(), "copy".into(), "-metadata:s:v".into(), "title=Cover".into(), "-metadata:s:v".into(), "comment=Cover (front)".into()]);
        }
        a.extend(["-c:a".into(), "libmp3lame".into(), "-b:a".into(), "64k".into(), "-id3v2_version".into(), "3".into()]);
        for (k, v) in [("title", title.to_string()), ("artist", artist.into()), ("album", album.into()), ("date", year.to_string()), ("track", (n + 1).to_string()), ("genre", genres.into())] {
            a.extend(["-metadata".into(), format!("{k}={v}")]);
        }
        if let Some(l) = label {
            a.extend(["-metadata".into(), format!("publisher={l}")]);
        }
        a.push(path.to_string_lossy().into_owned());
        let refs: Vec<&str> = a.iter().map(String::as_str).collect();
        run(&refs);
    }
}

struct Env {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    music: PathBuf,
    app: axum::Router,
}

fn setup() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir_all(&music).unwrap();
    let cover = make_cover(dir.path());
    write_album(&music, Some(&cover), "Somatic", "Grid Failure", 2026, "techno;industrial", &["Nocturnal", "Iron Lung"], None);
    write_album(&music, None, "Ferric", "Oxide Bloom", 2025, "electro;breakbeat", &["Oxide", "Rust"], None);
    let mut config = Config::from_env();
    config.data_dir = dir.path().join("data");
    config.library_root = Some(music.clone());
    config.download_dir = dir.path().join("data/downloads");
    let db = Db::open(config.data_dir.join("library.db")).unwrap();
    let ctx = Ctx::new(db, Arc::new(EventBus::new()), config);
    bc_scan::ensure_roots(&ctx).unwrap();
    let app = bc_scan::router(ctx.clone());
    Env { _dir: dir, ctx, music, app }
}

async fn call(app: &axum::Router, method: &str, uri: &str) -> (u16, Value) {
    let resp = app.clone().oneshot(Request::builder().method(method).uri(uri).body(Body::empty()).unwrap()).await.unwrap();
    let st = resp.status().as_u16();
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

/// POST /library/scan and wait for the task; returns the results of roots that saw files.
async fn scan(app: &axum::Router) -> Vec<Value> {
    let (st, j) = call(app, "POST", "/library/scan").await;
    assert_eq!(st, 202);
    let job = j["job_id"].as_str().unwrap().to_string();
    for _ in 0..600 {
        let (_, s) = call(app, "GET", &format!("/library/scan/{job}")).await;
        if s["state"] == "done" {
            return s["results"].as_array().unwrap().iter().filter(|r| r["files_seen"].as_i64().unwrap_or(0) > 0).cloned().collect();
        }
        assert_ne!(s["state"], "failed", "{s}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("scan did not finish");
}

fn n(ctx: &Ctx, sql: &str) -> i64 {
    let sql = sql.to_string();
    ctx.db.read(|c| Ok(c.query_row(&sql, [], |r| r.get::<_, i64>(0))?)).unwrap()
}

#[tokio::test]
async fn root_is_auto_registered_and_scan_indexes_everything() {
    if !have_ffmpeg() {
        return;
    }
    let e = setup();
    let (_, roots) = call(&e.app, "GET", "/library/roots").await;
    assert!(roots.as_array().unwrap().iter().any(|r| r["kind"] == "library"));
    let res = scan(&e.app).await;
    let lib = &res[0];
    assert_eq!(lib["files_seen"], 4);
    assert_eq!(lib["files_added"], 4);
    assert_eq!(lib["tracks_added"], 4);
    assert_eq!(lib["errors"].as_array().unwrap().len(), 0, "{lib}");
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM tracks"), 4);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM releases"), 2);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM artists"), 2);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL"), 0);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM tags"), 4, "techno, industrial, electro, breakbeat");
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM tracks WHERE track_no IS NOT NULL"), 4);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM tracks WHERE duration_ms BETWEEN 1500 AND 2600"), 4);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM releases WHERE year IN (2025, 2026)"), 2);
}

#[tokio::test]
async fn rescan_is_incremental() {
    if !have_ffmpeg() {
        return;
    }
    let e = setup();
    scan(&e.app).await;
    let gen0 = e.ctx.db.generation();
    let lib = &scan(&e.app).await[0];
    assert_eq!(lib["files_unchanged"], 4);
    assert_eq!(lib["files_added"], 0);
    assert_eq!(lib["files_updated"], 0);
    // nothing but the root's scan stamp (one write per scanned root: library + downloads)
    assert!(e.ctx.db.generation() - gen0 <= 2);
}

#[tokio::test]
async fn external_edit_is_detected_and_reindexed() {
    if !have_ffmpeg() {
        return;
    }
    let e = setup();
    scan(&e.app).await;
    let target = e.music.join("Somatic/Grid Failure/02 - Iron Lung.mp3");
    let tmp = target.with_extension("tmp.mp3");
    run(&["-i", target.to_str().unwrap(), "-map", "0", "-c", "copy", "-id3v2_version", "3", "-metadata", "title=Iron Lung (Dub)", tmp.to_str().unwrap()]);
    std::fs::rename(&tmp, &target).unwrap();
    let lib = &scan(&e.app).await[0];
    assert_eq!(lib["files_updated"], 1);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM tracks WHERE title = 'Iron Lung (Dub)'"), 1);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM search_index WHERE search_index MATCH '\"dub\"'"), 1);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM tracks"), 4);
}

#[tokio::test]
async fn missing_file_is_marked_not_deleted_and_reappears() {
    if !have_ffmpeg() {
        return;
    }
    let e = setup();
    scan(&e.app).await;
    let victim = e.music.join("Ferric/Oxide Bloom/01 - Oxide.mp3");
    let hidden = victim.with_extension("hidden");
    std::fs::rename(&victim, &hidden).unwrap();
    let lib = &scan(&e.app).await[0];
    assert_eq!(lib["files_missing"], 1);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM tracks"), 4, "the track row must survive");
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM files WHERE missing_since IS NOT NULL"), 1);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM tracks WHERE available = 1"), 3);
    std::fs::rename(&hidden, &victim).unwrap();
    scan(&e.app).await;
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM tracks WHERE available = 1"), 4);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM tracks"), 4);
}

#[tokio::test]
async fn cover_art_is_extracted() {
    if !have_ffmpeg() {
        return;
    }
    let e = setup();
    scan(&e.app).await;
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM artwork"), 1, "only the Somatic release embeds art");
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM releases WHERE cover_path IS NOT NULL"), 1);
    assert_eq!(n(&e.ctx, "SELECT COUNT(*) FROM artwork WHERE source = 'embedded' AND sizes = 7 AND blurhash IS NOT NULL AND width = 96"), 1);
    let path: String = e.ctx.db.read(|c| Ok(c.query_row("SELECT cover_path FROM releases WHERE cover_path IS NOT NULL", [], |r| r.get(0))?)).unwrap();
    assert!(path.ends_with(".webp") && Path::new(&path).exists(), "{path}");
}
