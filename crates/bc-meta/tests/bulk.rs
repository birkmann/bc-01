//! Port of tests/integration/test_metadata_bulk.py (bulk tag writing end to end through the router and the
//! task host). Fixtures: tag-free sine files from bc-media.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use bc_db::Db;
use bc_libcore::Ctx;
use bc_media::tags::read_tags;
use serde_json::{Value, json};
use tower::ServiceExt;

const TITLES: [&str; 3] = ["Grid Failure", "Vault Pressure", "Null Route"];

struct App {
    dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
    paths: Vec<PathBuf>,
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../bc-media/tests/fixtures/sine.mp3")
}

fn app() -> App {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music/Somatic/Vault");
    std::fs::create_dir_all(&music).unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let mut cfg = bc_core::Config::from_env();
    cfg.data_dir = dir.path().join("data");
    let ctx = Ctx::new(db, Arc::new(bc_core::EventBus::new()), cfg);
    let mut paths = vec![];
    let mut sql = format!(
        "INSERT INTO library_roots(id,path,kind,watch,enabled) VALUES (1,'{}','library',0,1);
         INSERT INTO artists(id,name,name_key,created_at) VALUES (1,'Somatic','somatic','x');
         INSERT INTO releases(id,title,title_key,artist_id,kind,added_at) VALUES (1,'Vault','vault',1,'album','x');",
        dir.path().join("music").display()
    );
    for (i, title) in TITLES.iter().enumerate() {
        let n = i as i64 + 1;
        let p = music.join(format!("{n:02} - {title}.mp3"));
        std::fs::copy(fixture(), &p).unwrap();
        // title tag so we can prove nothing else is disturbed
        let mut f = lofty::read_from_path(&p).unwrap();
        use lofty::config::WriteOptions;
        use lofty::file::TaggedFileExt;
        use lofty::tag::{Accessor, Tag, TagExt, TagType};
        let mut tag = Tag::new(TagType::Id3v2);
        tag.set_title(title.to_string());
        tag.set_artist("Somatic".into());
        tag.save_to_path(&p, WriteOptions::default()).unwrap();
        let _ = &mut f;
        let _ = f.primary_tag();
        sql += &format!(
            "INSERT INTO tracks(id,release_id,artist_id,title,title_key,track_no,loved,play_count,skip_count,added_at) VALUES ({n},1,1,'{title}','{title}',{n},0,0,0,'x');
             INSERT INTO files(id,track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at) VALUES ({n},{n},1,'{p}','x','mp3',1,1,'x','x');
             INSERT INTO analysis(track_id,analyzer_version,backend,status,analyzed_at,bpm,bpm_confidence,key_root,key_mode,camelot,key_confidence,energy,replaygain_gain,true_peak_db)
                  VALUES ({n},1,'test','ok','x',{bpm},0.9,9,'minor','8A',0.8,0.55,-3.42,-0.5);",
            p = p.display(),
            bpm = 128.0 + i as f64
        );
        paths.push(p);
    }
    ctx.db.write(move |t| {
        t.execute_batch(&sql)?;
        Ok(())
    })
    .unwrap();
    let router = bc_meta::router(ctx.clone());
    App { dir, ctx, router, paths }
}

impl App {
    async fn call(&self, m: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(m).uri(uri);
        let b = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let resp = self.router.clone().oneshot(req.body(b).unwrap()).await.unwrap();
        let st = resp.status();
        let bytes = to_bytes(resp.into_body(), 16 << 20).await.unwrap();
        (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Wait for the tracked job to settle.
    fn drain(&self, job_id: &str) -> Value {
        let t = Instant::now();
        loop {
            let info = self.ctx.jobs.get(job_id).unwrap();
            if info.state != "running" {
                return info.result.unwrap_or(Value::Null);
            }
            assert!(t.elapsed() < Duration::from_secs(30), "job did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn mtimes(&self) -> Vec<u128> {
        self.paths.iter().map(|p| std::fs::metadata(p).unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()).collect()
    }

    async fn write_all(&self) -> String {
        let (s, v) = self.call(Method::POST, "/metadata/write", Some(json!({"dry_run": false, "confirm": true}))).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{v}");
        let id = v["job_id"].as_str().unwrap().to_string();
        self.drain(&id);
        id
    }
}

#[tokio::test]
async fn status_reports_what_can_be_written() {
    let a = app();
    let (_, body) = a.call(Method::GET, "/metadata/status", None).await;
    assert_eq!((body["total_tracks"].as_i64(), body["analysed"].as_i64()), (Some(3), Some(3)));
    assert!(body["writable_extensions"].as_array().unwrap().iter().any(|e| e == ".mp3"));
    assert!(!body["default_groups"].as_array().unwrap().iter().any(|g| g == "energy"));
}

#[tokio::test]
async fn preview_changes_nothing_on_disk() {
    let a = app();
    let before = a.mtimes();
    let (_, body) = a.call(Method::POST, "/metadata/preview", Some(json!({"scope": "analysed"}))).await;
    assert_eq!(body["summary"]["tracks"], 3);
    assert_eq!(body["summary"]["would_write"], 3);
    assert_eq!(body["summary"]["per_field"]["camelot"], 3);
    assert!(body["items"][0]["fields"].as_array().unwrap().iter().any(|f| f["status"] == "gap"));
    assert_eq!(a.mtimes(), before);
}

#[tokio::test]
async fn a_real_write_needs_confirmation() {
    let a = app();
    let (s, v) = a.call(Method::POST, "/metadata/write", Some(json!({"dry_run": false}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(v["detail"].as_str().unwrap().contains("confirm"));
}

#[tokio::test]
async fn a_dry_run_job_touches_no_file() {
    let a = app();
    let before = a.mtimes();
    let (_, q) = a.call(Method::POST, "/metadata/write", Some(json!({"dry_run": true}))).await;
    assert_eq!(q["queued"], 3);
    let res = a.drain(q["job_id"].as_str().unwrap());
    assert_eq!(res["written"], 3, "would-write count");
    assert_eq!(a.mtimes(), before);
    assert!(!bc_meta::journal::path(&a.ctx.config.backups_dir(), q["job_id"].as_str().unwrap()).exists(), "no journal for a dry run");
}

#[tokio::test]
async fn a_write_is_not_seen_as_an_external_edit() {
    let a = app();
    a.write_all().await;
    for (p, title) in a.paths.iter().zip(TITLES) {
        let t = read_tags(p);
        assert_eq!(t.camelot.as_deref(), Some("8A"));
        assert_eq!(t.initial_key.as_deref(), Some("Am"));
        assert_eq!(t.title.as_deref(), Some(title), "nothing else disturbed");
    }
    // the re-stamp: stat and tag_hash in the DB describe the file as we left it
    for (i, p) in a.paths.iter().enumerate() {
        let id = i as i64 + 1;
        let m = std::fs::metadata(p).unwrap();
        use std::os::unix::fs::MetadataExt;
        let (size, mtime, hash): (i64, i64, String) = a
            .ctx
            .read(|c| Ok(c.query_row("SELECT size_bytes, mtime_ns, tag_hash FROM files WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?))
            .unwrap();
        assert_eq!(size, m.len() as i64);
        assert_eq!(mtime, m.mtime() * 1_000_000_000 + m.mtime_nsec());
        assert_eq!(hash, read_tags(p).tag_hash());
    }
}

#[tokio::test]
async fn a_second_pass_has_nothing_left_to_fill() {
    let a = app();
    a.write_all().await;
    let (_, body) = a.call(Method::POST, "/metadata/preview", Some(json!({"scope": "analysed"}))).await;
    assert_eq!((body["summary"]["would_write"].as_i64(), body["summary"]["no_gaps"].as_i64()), (Some(0), Some(3)));
}

#[tokio::test]
async fn undo_takes_back_exactly_what_was_written() {
    let a = app();
    let job = a.write_all().await;
    let p = a.call(Method::GET, &format!("/metadata/jobs/{job}/snapshot"), None).await;
    assert_eq!(p.0, StatusCode::OK);
    let (_, body) = a.call(Method::POST, &format!("/metadata/jobs/{job}/undo"), None).await;
    assert_eq!((body["entries"].as_i64(), body["restored"].as_i64(), body["skipped_changed"].as_i64()), (Some(3), Some(3), Some(0)));
    for (p, title) in a.paths.iter().zip(TITLES) {
        let t = read_tags(p);
        assert!(t.camelot.is_none() && t.bpm.is_none());
        assert_eq!(t.title.as_deref(), Some(title), "the rest survived the undo");
    }
    // and the stat re-stamp after undo
    let hash: String = a.ctx.read(|c| Ok(c.query_row("SELECT tag_hash FROM files WHERE id = 1", [], |r| r.get(0))?)).unwrap();
    assert_eq!(hash, read_tags(&a.paths[0]).tag_hash());
}

#[tokio::test]
async fn undo_leaves_a_field_edited_since_the_write() {
    let a = app();
    let job = a.write_all().await;
    // the user corrects the camelot value by hand after our write
    let edited = &a.paths[0];
    bc_media::write::remove_dj_fields(edited, &["camelot".to_string()]).unwrap();
    let vals = bc_media::write::DjFields { camelot: Some("11B".into()), ..Default::default() };
    bc_media::write::fill_dj_fields(edited, &vals, &[bc_media::write::FieldGroup::Camelot]).unwrap();
    let (_, body) = a.call(Method::POST, &format!("/metadata/jobs/{job}/undo"), None).await;
    assert_eq!(body["failed"], 0);
    assert_eq!(read_tags(edited).camelot.as_deref(), Some("11B"), "the edited field survives");
    assert!(read_tags(edited).bpm.is_none(), "untouched fields are still taken back");
}

#[tokio::test]
async fn write_one_track_now() {
    let a = app();
    let (s, body) = a.call(Method::POST, "/metadata/tracks/1?dry_run=false", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["status"], "written");
    assert!(body["would_write"].as_array().unwrap().iter().any(|f| f == "camelot"));
    assert_eq!(a.call(Method::POST, "/metadata/tracks/999", None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unanalysed_track_is_skipped_not_failed() {
    let a = app();
    a.ctx.db.write(|t| {
        t.execute("DELETE FROM analysis", [])?;
        Ok(())
    })
    .unwrap();
    let (_, q) = a.call(Method::POST, "/metadata/write", Some(json!({"scope": "all", "dry_run": false, "confirm": true}))).await;
    let res = a.drain(q["job_id"].as_str().unwrap());
    assert_eq!((res["written"].as_i64(), res["skipped"].as_i64(), res["failed"].as_i64()), (Some(0), Some(3), Some(0)));
}

#[tokio::test]
async fn the_journal_records_every_written_field_and_conflicts_are_reported_not_written() {
    let a = app();
    // pre-existing, different key in file 1: a conflict, never overwritten
    let vals = bc_media::write::DjFields { camelot: Some("1A".into()), ..Default::default() };
    bc_media::write::fill_dj_fields(&a.paths[0], &vals, &[bc_media::write::FieldGroup::Camelot]).unwrap();
    let (_, pv) = a.call(Method::POST, "/metadata/preview", Some(json!({"scope": "analysed"}))).await;
    assert_eq!(pv["summary"]["conflicts"], 1);
    let job = a.write_all().await;
    assert_eq!(read_tags(&a.paths[0]).camelot.as_deref(), Some("1A"), "a value already in the file wins");
    let entries = bc_meta::journal::read_entries(&bc_meta::journal::path(&a.ctx.config.backups_dir(), &job));
    assert_eq!(entries.len(), 3);
    let written = entries[1]["written"].as_object().unwrap();
    for k in ["bpm", "initial_key", "camelot"] {
        assert!(written.contains_key(k), "{k}");
    }
    assert!(!entries[0]["written"].as_object().unwrap().contains_key("camelot"));
    let _ = &a.dir;
}
