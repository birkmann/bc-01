//! Port of the API half of `tests/integration/test_analysis.py` plus the runner end to end.

mod common;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bc_analysis::AnalysisService;
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_jobs::JobsService;
use common::*;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

struct Env {
    db: Db,
    bus: Arc<EventBus>,
    svc: AnalysisService,
    jobs: JobsService,
    track_id: i64,
    _dir: tempfile::TempDir,
}

fn env() -> Env {
    let dir = tempfile::Builder::new().prefix("bc-an-").tempdir_in(std::env::var_os("BC_TMP").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir)).unwrap();
    let db = Db::open(dir.path().join("library.db")).unwrap();
    let wav = dir.path().join("01 - track.wav");
    write_wav(&wav, &click_samples(128.0, 10.0), SR);
    let wav_s = wav.to_string_lossy().to_string();
    let track_id = db
        .write(move |t| {
            t.execute("INSERT INTO library_roots (id, path, kind, watch, enabled) VALUES (1, '/m', 'library', 0, 1)", [])?;
            t.execute("INSERT INTO artists (id, name, name_key, created_at) VALUES (1, 'Somatic', 'somatic', CURRENT_TIMESTAMP)", [])?;
            t.execute("INSERT INTO releases (id, title, title_key, artist_id, kind, added_at) VALUES (1, 'Grid Failure', 'grid failure', 1, 'album', CURRENT_TIMESTAMP)", [])?;
            t.execute("INSERT INTO tracks (id, release_id, artist_id, title, title_key, loved, play_count, skip_count, added_at) VALUES (1, 1, 1, 'Track', 'track', 0, 0, 0, CURRENT_TIMESTAMP)", [])?;
            t.execute(
                "INSERT INTO files (track_id, root_id, path, rel_path, ext, size_bytes, mtime_ns, first_seen_at, last_seen_at) VALUES (1, 1, ?1, 'x', '.wav', 1, 1, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
                [&wav_s],
            )?;
            Ok(1i64)
        })
        .unwrap();
    let bus = Arc::new(EventBus::new());
    let mut cfg = Config::from_env();
    cfg.data_dir = dir.path().join("data");
    cfg.analysis_workers = 2;
    let cfg = Arc::new(cfg);
    let jobs = JobsService::new(db.clone(), bus.clone());
    let svc = AnalysisService::new(db.clone(), bus.clone(), cfg, &jobs);
    Env { db, bus, svc, jobs, track_id, _dir: dir }
}

async fn call(e: &Env, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let mut req = Request::builder().method(method).uri(uri);
    let b = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = e.svc.router().oneshot(req.body(b).unwrap()).await.unwrap();
    let (parts, body) = resp.into_parts();
    (parts.status, body.collect().await.unwrap().to_bytes().to_vec(), parts.headers)
}

async fn json(e: &Env, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (s, b, _) = call(e, method, uri, body).await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

#[tokio::test]
async fn status_reports_coverage() {
    let e = env();
    let (s, b) = json(&e, "GET", "/analysis/status", None).await;
    assert_eq!(s, 200);
    assert!(b["total_tracks"].as_i64().unwrap() >= 1);
    assert_eq!(b["coverage"], 0.0);
    assert_eq!(b["analyzer_version"], 2);
    assert!(b["backends"]["bc-rs-1"].as_bool().unwrap());
}

#[tokio::test]
async fn analyse_now_then_read_back_and_serve_waveforms() {
    let e = env();
    let id = e.track_id;
    assert_eq!(json(&e, "GET", &format!("/tracks/{id}/peaks"), None).await.0, 404);
    let (s, a) = json(&e, "POST", &format!("/analysis/tracks/{id}?wait=true"), None).await;
    assert_eq!(s, 200, "{a}");
    assert!(a["status"] == "ok" || a["status"] == "partial", "{a}");
    assert!(a["bpm"].as_f64().is_some());
    let expect = if bc_analysis::sidecar::available() { "essentia-sidecar" } else { "bc-rs-1" };
    assert_eq!(a["analyzer"], expect, "{a}");
    let (_, stored) = json(&e, "GET", &format!("/analysis/tracks/{id}"), None).await;
    assert_eq!(stored["bpm"], a["bpm"]);
    assert!(stored["loudness_lufs"].as_f64().is_some());
    assert!(stored["energy_v2"].as_f64().unwrap() >= 1.0);

    // peaks (legacy JSON)
    let (s, p) = json(&e, "GET", &format!("/tracks/{id}/peaks?points=100"), None).await;
    assert_eq!(s, 200);
    let peaks = p["peaks"].as_array().unwrap();
    assert_eq!(peaks.len(), 100);
    assert!(peaks.iter().all(|x| x.as_array().unwrap().len() == 2));

    // binary waveform with ETag + 304
    let (s, bytes, h) = call(&e, "GET", &format!("/tracks/{id}/waveform?level=detail"), None).await;
    assert_eq!(s, 200);
    assert_eq!(&bytes[..4], b"BCW2");
    let etag = h.get("etag").unwrap().to_str().unwrap().to_string();
    let req = Request::builder().uri(format!("/tracks/{id}/waveform?level=detail")).header("if-none-match", &etag).body(Body::empty()).unwrap();
    assert_eq!(e.svc.router().oneshot(req).await.unwrap().status(), 304);
    let (s, ov, _) = call(&e, "GET", &format!("/tracks/{id}/waveform?level=overview"), None).await;
    assert_eq!(s, 200);
    assert_eq!(ov.len(), 64 + 6 * 2048, "wire overview is header + 2048 raw points");
    assert_eq!(call(&e, "GET", &format!("/tracks/{id}/waveform?level=bogus"), None).await.0, 400);

    // grid + cues + mix points in one fetch
    let (s, m) = json(&e, "GET", &format!("/tracks/{id}/music"), None).await;
    assert_eq!(s, 200);
    assert!(!m["grid"]["segments"].as_array().unwrap().is_empty());
    // user cues round trip, auto cues untouched
    let cue = serde_json::json!([{"id":null,"track_id":id,"kind":"hot","pos_ms":1234.0,"end_ms":null,"label":"A","color":"#ff0000","slot":0,"auto":false}]);
    let (s, cues) = json(&e, "PUT", &format!("/tracks/{id}/cues"), Some(cue)).await;
    assert_eq!(s, 200);
    assert!(cues.as_array().unwrap().iter().any(|c| c["kind"] == "hot" && c["slot"] == 0));
}

#[tokio::test]
async fn scan_queues_a_job_and_nothing_to_do_is_not_an_error() {
    let e = env();
    let (s, b) = json(&e, "POST", "/analysis/scan", Some(serde_json::json!({"scope":"missing"}))).await;
    assert_eq!(s, 202);
    assert!(b["queued"].as_i64().unwrap() >= 1);
    let job = b["job_id"].as_str().unwrap().to_string();
    let n: i64 = e.db.read(|c| Ok(c.query_row("SELECT count(*) FROM jobs WHERE kind='analyze'", [], |r| r.get(0))?)).unwrap();
    assert_eq!(n, 1);

    let (s, b) = json(&e, "POST", "/analysis/scan", Some(serde_json::json!({"scope":"failed"}))).await;
    assert_eq!(s, 200);
    assert_eq!(b["queued"], 0);
    assert!(b["job_id"].is_null());

    // status counts the queue it just created (runner not started => nothing moves)
    let (_, st) = json(&e, "GET", "/analysis/status", None).await;
    assert_eq!(st["queued_tracks"], 1);
    assert_eq!(st["batch_total"], 1);
    assert_eq!(st["batch_done"], 0);
    assert_eq!(st["running_jobs"], 1);

    let (_, q) = json(&e, "GET", "/analysis/queue", None).await;
    assert_eq!(q["jobs"][0]["status"], "queued");
    assert_eq!(q["jobs"][0]["progress"], 0.0);
    assert_eq!(q["items"][0]["status"], "pending");
    assert_eq!(q["items"][0]["title"], "Track");
    assert_eq!(q["queued_track_ids"], serde_json::json!([e.track_id]));
    assert_eq!(q["active_track_ids"], serde_json::json!([]));
    let _ = job;
}

#[tokio::test]
async fn queue_is_empty_before_anything_is_asked_for() {
    let e = env();
    let (_, q) = json(&e, "GET", "/analysis/queue", None).await;
    assert_eq!(
        q,
        serde_json::json!({"jobs":[],"items":[],"active_track_ids":[],"queued_track_ids":[],"failed_track_ids":[]})
    );
}

#[tokio::test]
async fn scan_by_ids_dedups_and_only_missing_skips_analysed() {
    let e = env();
    let id = e.track_id;
    let (s, b) = json(&e, "POST", "/analysis/scan", Some(serde_json::json!({"scope":"ids","track_ids":[id,id]}))).await;
    assert_eq!(s, 202);
    assert_eq!(b["queued"], 1);
    assert_eq!(b["requested"], 1);
    json(&e, "POST", &format!("/analysis/tracks/{id}?wait=true"), None).await;
    let (_, b) = json(&e, "POST", "/analysis/scan", Some(serde_json::json!({"scope":"ids","track_ids":[id],"only_missing":true}))).await;
    assert_eq!(b["queued"], 0);
    assert_eq!(b["requested"], 1);
    assert!(b["job_id"].is_null());
    assert_eq!(b["detail"], "already analysed");
    let (_, forced) = json(&e, "POST", "/analysis/scan", Some(serde_json::json!({"scope":"ids","track_ids":[id]}))).await;
    assert_eq!(forced["queued"], 1);
}

#[tokio::test]
async fn compatible_crate_requires_an_analysed_seed() {
    let e = env();
    let id = e.track_id;
    assert_eq!(json(&e, "GET", &format!("/analysis/compatible/{id}"), None).await.0, 400);
    json(&e, "POST", &format!("/analysis/tracks/{id}?wait=true"), None).await;
    assert_eq!(json(&e, "GET", &format!("/analysis/compatible/{id}"), None).await.0, 200);
    assert_eq!(json(&e, "GET", &format!("/analysis/compatibility/{id}/{id}"), None).await.0, 200);
}

#[tokio::test]
async fn the_runner_drains_the_queue_and_publishes_events() {
    let e = env();
    let mut rx = e.bus.subscribe();
    e.jobs.start().await;
    e.svc.start().await;
    let (s, b) = json(&e, "POST", "/analysis/scan", Some(serde_json::json!({"scope":"missing"}))).await;
    assert_eq!(s, 202);
    let job_id = b["job_id"].as_str().unwrap().to_string();
    let mut saw_item = None;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let st: String = e.db.read(|c| Ok(c.query_row("SELECT status FROM jobs WHERE id=?1", [&job_id], |r| r.get(0))?)).unwrap();
        while let Ok(ev) = rx.try_recv() {
            if ev.topic == "analysis.item" {
                saw_item = Some(ev.payload);
            }
        }
        if st == "completed" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "job stuck in {st}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(ev) = rx.try_recv() {
        if ev.topic == "analysis.item" {
            saw_item = Some(ev.payload);
        }
    }
    let ev = saw_item.expect("analysis.item event");
    for k in ["track_id", "status", "bpm", "camelot", "key", "energy", "done", "batch"] {
        assert!(ev.get(k).is_some(), "event lacks {k}: {ev}");
    }
    let (grid, wf, analyzer): (i64, i64, String) = e
        .db
        .read(|c| {
            Ok((
                c.query_row("SELECT count(*) FROM beat_grids", [], |r| r.get(0))?,
                c.query_row("SELECT count(*) FROM waveform_meta", [], |r| r.get(0))?,
                c.query_row("SELECT analyzer FROM analysis WHERE track_id=1", [], |r| r.get(0))?,
            ))
        })
        .unwrap();
    let expect = if bc_analysis::sidecar::available() { "essentia-sidecar" } else { "bc-rs-1" };
    assert_eq!((grid, wf, analyzer.as_str()), (1, 1, expect));
    e.svc.stop();
}

#[tokio::test]
async fn imported_essentia_values_stay_authoritative_by_default() {
    let e = env();
    e.db
        .write(|t| {
            t.execute(
                "INSERT INTO analysis (track_id, analyzer_version, backend, status, analyzed_at, bpm, camelot, key_root, key_mode, analyzer) \
                 VALUES (1, 1, 'essentia', 'ok', CURRENT_TIMESTAMP, 140.0, '5A', 0, 'minor', 'essentia-import')",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    json(&e, "POST", "/analysis/tracks/1?wait=true", None).await;
    let (_, a) = json(&e, "GET", "/analysis/tracks/1", None).await;
    assert_eq!(a["bpm"], 140.0, "{a}");
    assert_eq!(a["camelot"], "5A");
    assert_eq!(a["analyzer"], "essentia-import");
    assert!(a["loudness_lufs"].as_f64().is_some(), "real loudness is still written");
    assert!(a["energy_v2"].as_f64().is_some());
    // flip the policy: native values overwrite
    e.db.write(|t| bc_db::settings::set(t, "analysis.native_bpm_key", "1")).unwrap();
    json(&e, "POST", "/analysis/tracks/1?wait=true", None).await;
    let (_, a) = json(&e, "GET", "/analysis/tracks/1", None).await;
    assert!((a["bpm"].as_f64().unwrap() - 128.0).abs() < 0.2, "{a}");
    assert_eq!(a["analyzer"], "bc-rs-1");
}

#[tokio::test]
async fn accuracy_report_and_waveform_cache_routes() {
    let e = env();
    let (s, b) = json(&e, "GET", "/analysis/accuracy", None).await;
    assert_eq!(s, 200);
    assert!(b["report"].is_null());
    assert_eq!(b["native_bpm_key"], false);
    let rep = serde_json::json!({"generated_at":"2026-10-01T10:00:00Z","n":2000,"failed":0,"bpm_within_0_5_pct_octave":0.777,
        "bpm_within_0_5_pct_strict":0.725,"bpm_within_3_pct_octave":0.89,"key_exact":0.38,"key_exact_or_compatible":0.53,
        "tracks_per_s":4.6,"realtime_x":1622.0,"threads":24,"passed":false});
    let j = rep.to_string();
    e.db.write(move |t| bc_db::settings::set(t, "analysis.accuracy_report", &j)).unwrap();
    let (_, b) = json(&e, "GET", "/analysis/accuracy", None).await;
    assert_eq!(b["report"]["n"], 2000);
    assert_eq!(b["report"]["passed"], false);

    let (s, c) = json(&e, "GET", "/analysis/waveform-cache", None).await;
    assert_eq!(s, 200);
    assert_eq!(c["files"], 0);
    json(&e, "POST", &format!("/analysis/tracks/{}?wait=true", e.track_id), None).await;
    let (_, c) = json(&e, "GET", "/analysis/waveform-cache", None).await;
    assert_eq!(c["files"], 1);
    assert_eq!(c["detail_files"], 1);
    assert!(c["bytes"].as_u64().unwrap() > 1000);
    assert!(c["cap_bytes"].as_u64().unwrap() > 0);
}
