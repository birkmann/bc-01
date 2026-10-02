//! `GET /sets/{id}/render`: a DJ set in the library DB performed offline through
//! the live graph (cues, tempo, overlap from the incoming slot's beats), streamed.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bc_core::EventBus;
use bc_db::Db;
use bc_engine::PlayerService;
use bc_engine::host::OutputKind;
use bc_engine::render::{overlap_ms, slots_from_set};
use bc_engine::session::SessionConfig;
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;

fn tone(path: &std::path::Path, freq: f32, secs: f32) {
    let spec = hound::WavSpec { channels: 2, sample_rate: 44_100, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..(44_100.0 * secs) as usize {
        let v = ((2.0 * std::f32::consts::PI * freq * i as f32 / 44_100.0).sin() * 0.4 * 32767.0) as i16;
        w.write_sample(v).unwrap();
        w.write_sample(v).unwrap();
    }
    w.finalize().unwrap();
}

fn setup() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path().join("lib.db")).unwrap();
    let (a, b, c) = (dir.path().join("a.wav"), dir.path().join("b.wav"), dir.path().join("c.wav"));
    tone(&a, 220.0, 6.0);
    tone(&b, 330.0, 6.0);
    tone(&c, 440.0, 6.0);
    let paths = [a, b, c];
    db.write(move |t| {
        t.execute("INSERT INTO library_roots(id, path, kind, watch, enabled) VALUES (1, '/x', 'library', 0, 1)", [])?;
        for (i, p) in paths.iter().enumerate() {
            let id = i as i64 + 1;
            t.execute(
                "INSERT INTO tracks(id, title, title_key, loved, play_count, skip_count, added_at, duration_ms) VALUES (?1, ?2, ?2, 0, 0, 0, CURRENT_TIMESTAMP, 6000)",
                bc_db::rusqlite::params![id, format!("t{id}")],
            )?;
            t.execute(
                "INSERT INTO files(track_id, root_id, path, rel_path, ext, size_bytes, mtime_ns, first_seen_at, last_seen_at) VALUES (?1, 1, ?2, ?2, 'wav', 1, 1, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
                bc_db::rusqlite::params![id, p.to_string_lossy()],
            )?;
            t.execute(
                "INSERT INTO analysis(track_id, analyzer_version, backend, status, analyzed_at, bpm, loudness_lufs) VALUES (?1, 1, 'test', 'ok', CURRENT_TIMESTAMP, 120, ?2)",
                bc_db::rusqlite::params![id, -14.0 - id as f64],
            )?;
        }
        t.execute("INSERT INTO dj_sets(id, name, status, created_at, updated_at) VALUES (1, 'My set!', 'draft', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)", [])?;
        // slot 1: cues 1.0 .. 5.0; slot 2: 8 beats at 120 BPM (4 s) into it, cues 0 .. 6; slot 3: a hole (no track), slot 4 -> track 3 with 4 beats
        let item = |id: i64, pos: f64, track: Option<i64>, cin: Option<i64>, cout: Option<i64>, beats: Option<i64>, ttype: Option<&str>| {
            t.execute(
                "INSERT INTO dj_set_items(id, set_id, track_id, position, cue_in_ms, cue_out_ms, tempo_adjust_pct, key_lock, transition_type, transition_beats, snapshot) VALUES (?1, 1, ?2, ?3, ?4, ?5, 0, 1, ?7, ?6, '{}')",
                bc_db::rusqlite::params![id, track, pos, cin, cout, beats, ttype],
            )
        };
        item(1, 1.0, Some(1), Some(1000), Some(5000), None, None)?;
        item(2, 2.0, Some(2), None, None, Some(8), Some("blend"))?;
        item(3, 3.0, None, None, None, Some(8), None)?;
        item(4, 4.0, Some(3), Some(0), Some(4000), Some(4), Some("cut"))?;
        Ok(())
    })
    .unwrap();
    (db, dir)
}

#[test]
fn slots_follow_the_plan_cues_overlaps_and_holes() {
    let (db, _d) = setup();
    let (name, slots) = slots_from_set(&db, 1).unwrap();
    assert_eq!(name, "My set!");
    // the hole is dropped; its neighbours join with a cut (no overlap into a missing slot)
    assert_eq!(slots.len(), 3);
    assert_eq!((slots[0].cue_in_ms, slots[0].cue_out_ms), (1000, 5000));
    assert_eq!(slots[0].overlap_out_ms, overlap_ms(Some(8), Some(120.0)), "8 beats at 120 BPM = 4 s, from the incoming slot");
    assert_eq!(slots[0].overlap_out_ms, 4000);
    assert_eq!((slots[1].cue_in_ms, slots[1].cue_out_ms), (0, 6000), "defaults: the whole file");
    assert_eq!(slots[1].overlap_out_ms, 0, "the next slot is a hole");
    assert_eq!(slots[2].overlap_out_ms, 0, "last slot");
    assert!(slots[0].loudness_lufs.unwrap() < slots[1].loudness_lufs.unwrap() + 1.5);
    assert!(slots_from_set(&db, 99).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_streams_the_rendered_wav() {
    let (db, _d) = setup();
    // the real constructor (wires the DB for the render route), without MPRIS or an audio device
    let config = bc_core::Config::from_env();
    let svc = PlayerService::new_with(db, Arc::new(EventBus::new()), &config, SessionConfig {
        output: OutputKind::Null { sample_rate: 48_000, block: 256, speed: 0.0, capture: None, cue: None },
        mpris: false,
        ..Default::default()
    });
    let app = svc.router();

    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/sets/1/render?format=wav").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "audio/wav");
    assert!(resp.headers()["content-disposition"].to_str().unwrap().contains("My set_.wav"));
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..4], b"RIFF");
    assert_eq!(&body[8..12], b"WAVE");
    // slot0 4 s, slot1 6 s, slot2 4 s, minus the 4 s overlap: 10 s of 16-bit stereo 44.1 kHz (+ tail), after the 44-byte header
    let secs = (body.len() - 44) as f64 / (44_100.0 * 4.0);
    assert!((secs - 10.0).abs() < 0.5, "streamed {secs} s");

    let bad = app.clone().oneshot(Request::builder().uri("/sets/1/render?format=flac").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let missing = app.oneshot(Request::builder().uri("/sets/42/render").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    svc.shutdown();
}
