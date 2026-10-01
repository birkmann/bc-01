//! Manual smoke player: plays real audio through the whole stack (decode ->
//! rings -> mixer -> cpal) as a `PlayerService`, with MPRIS (`playerctl`) and the
//! event bus, printing `player.*` events.
//!
//!   cargo run -p bc-engine --example play -- a.mp3 [b.flac ...] [options]
//!
//! Options: `--null` (no sound card: software-paced output), `--mix` (DJ mix on),
//! `--bpm 128` (assume this tempo + a grid at the first beat), `--seek S`,
//! `--secs N` (play for N seconds, default 20), `--shuffle`, `--device NAME`,
//! `--buffer FRAMES`, `--volume V` (default 0.25),
//! `--db <library.db> --track ID` (play a library track, DB opened strictly read-only).
//! While it runs: `BC_MPRIS_NAME=bc_rust_play` is advisable next to a running server; `playerctl -p bc_rust_play status|metadata|next|pause`.

use bc_core::EventBus;
use bc_engine::PlayerService;
use bc_engine::host::OutputKind;
use bc_engine::ports::FilePorts;
use bc_engine::session::SessionConfig;
use bc_types::player::*;
use serde_json::json;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `println!` that flushes, so the log is live even when stdout is a file.
macro_rules! say {
    ($($a:tt)*) => {{
        println!($($a)*);
        let _ = std::io::stdout().flush();
    }};
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let mut it = std::env::args().skip(1);
    let mut files: Vec<PathBuf> = vec![];
    let (mut null, mut mix, mut shuffle) = (false, false, false);
    let (mut bpm, mut seek, mut secs, mut device, mut buffer): (Option<f64>, Option<f64>, f64, Option<String>, Option<u32>) =
        (None, None, 20.0, None, None);
    let mut volume = 0.25f64;
    let (mut db, mut track): (Option<PathBuf>, Option<i64>) = (None, None);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--null" => null = true,
            "--mix" => mix = true,
            "--shuffle" => shuffle = true,
            "--bpm" => bpm = it.next().and_then(|v| v.parse().ok()),
            "--seek" => seek = it.next().and_then(|v| v.parse().ok()),
            "--volume" => volume = it.next().and_then(|v| v.parse().ok()).unwrap_or(0.25),
            "--secs" => secs = it.next().and_then(|v| v.parse().ok()).unwrap_or(20.0),
            "--device" => device = it.next(),
            "--buffer" => buffer = it.next().and_then(|v| v.parse().ok()),
            "--db" => db = it.next().map(PathBuf::from),
            "--track" => track = it.next().and_then(|v| v.parse().ok()),
            f => files.push(PathBuf::from(f)),
        }
    }
    // `--db` reads file paths (and tempo) from the library DB strictly read-only; nothing is ever written.
    if let (Some(db), Some(id)) = (db, track) {
        let c = bc_db::rusqlite::Connection::open_with_flags(
            &db,
            bc_db::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | bc_db::rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .expect("open db read-only");
        let (path, b): (String, Option<f64>) = c
            .query_row(
                "SELECT f.path, a.bpm FROM files f LEFT JOIN analysis a ON a.track_id = f.track_id WHERE f.track_id = ?1 AND f.missing_since IS NULL LIMIT 1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("track has no file");
        say!("playing {path} (bpm {b:?}) from the library DB (read-only)");
        files.push(PathBuf::from(path));
        bpm = bpm.or(b);
    }
    assert!(!files.is_empty(), "give at least one audio file");
    let mut fp = FilePorts::new(files.clone());
    fp.bpm = bpm;
    let queue: Vec<QueueItem> = (1..=files.len() as i64).filter_map(|i| fp.item(i)).collect();
    let output = if null {
        OutputKind::Null { sample_rate: 48_000, block: 512, speed: 1.0, capture: None }
    } else {
        OutputKind::Cpal(OutputTarget { device, cue_device: None, buffer_frames: buffer })
    };
    let bus = Arc::new(EventBus::new());
    let svc = PlayerService::with_ports(fp.into_ports(), bus.clone(), SessionConfig { output, ..Default::default() });
    svc.start().await;

    // print what the WebSocket would carry
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        let mut last_clock = Instant::now() - Duration::from_secs(5);
        while let Ok(ev) = rx.recv().await {
            match ev.topic.as_str() {
                TOPIC_PLAYER_STATE => {
                    let s: PlayerState = serde_json::from_value(ev.payload).unwrap();
                    if let Some(c) = &s.current {
                        say!("[state] {:?} #{} {}  mix={} shuffle={} out={} {}Hz buf={} (~{:.1} ms) xruns={}", s.status, s.queue_index, c.title, s.mix, s.shuffle, s.devices.backend, s.devices.sample_rate, s.devices.buffer_frames, s.devices.latency_ms, s.devices.xruns);
                    }
                    if let Some(e) = &s.error {
                        say!("[error] {e}");
                    }
                }
                TOPIC_PLAYER_CLOCK if last_clock.elapsed() >= Duration::from_secs(1) => {
                    last_clock = Instant::now();
                    let c: Clock = serde_json::from_value(ev.payload).unwrap();
                    say!(
                        "[clock] {:6.2}/{:6.2}s rate {:.4} buffered {:.1}s xruns {} frames {} ts_lead {:.1} ms",
                        c.position_s, c.duration_s, c.rate, c.buffered_s, c.xruns, c.frames_played,
                        (c.output_timestamp_ns as f64 - c.server_time_ns as f64) / 1e6
                    );
                }
                TOPIC_PLAYER_TRANSITION => {
                    say!("[transition] {}", ev.payload);
                }
                _ => {}
            }
        }
    });

    let cmd = |v: serde_json::Value| {
        let svc = &svc;
        async move {
            if let Err(e) = svc.handle_command(v).await {
                say!("[command failed] {e}");
            }
        }
    };
    cmd(json!({"cmd": "set_volume", "volume": volume})).await;
    cmd(json!({"cmd": "set_mix", "on": mix})).await;
    cmd(json!({"cmd": "set_shuffle", "on": shuffle})).await;
    cmd(json!({"cmd": "play_queue", "items": queue, "start_index": 0})).await;
    if let Some(s) = seek {
        tokio::time::sleep(Duration::from_millis(800)).await;
        cmd(json!({"cmd": "seek", "seconds": s})).await;
    }
    tokio::time::sleep(Duration::from_secs_f64(secs)).await;
    svc.shutdown();
}
