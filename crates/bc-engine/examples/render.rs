//! Offline set render through the live graph, faster than real time.
//!
//!   cargo run -p bc-engine --example render -- out.wav a.flac b.mp3 [c.ogg ...] [--overlap-ms 8000]
//!       [--cue-in-ms 0] [--cue-out-ms 60000] [--transition blend|bass_swap|filter|echo_out|cut] [--tempo-pct 0,1.5,...] [--key-lock]
//!
//! `out.mp3` is encoded through `ffmpeg`.

use bc_engine::render::{RenderOptions, RenderSlot, render_to_file};
use bc_types::player::TransitionKind;
use std::path::PathBuf;

fn main() {
    let mut it = std::env::args().skip(1);
    let mut files: Vec<PathBuf> = vec![];
    let (mut overlap, mut cue_in, mut cue_out, mut key_lock) = (8000i64, 0i64, 60_000i64, false);
    let mut kind = TransitionKind::Blend;
    let mut tempos: Vec<f64> = vec![];
    while let Some(a) = it.next() {
        match a.as_str() {
            "--overlap-ms" => overlap = it.next().and_then(|v| v.parse().ok()).unwrap_or(8000),
            "--cue-in-ms" => cue_in = it.next().and_then(|v| v.parse().ok()).unwrap_or(0),
            "--cue-out-ms" => cue_out = it.next().and_then(|v| v.parse().ok()).unwrap_or(60_000),
            "--key-lock" => key_lock = true,
            "--tempo-pct" => tempos = it.next().unwrap_or_default().split(',').filter_map(|t| t.parse().ok()).collect(),
            "--transition" => {
                kind = serde_json::from_value(serde_json::Value::String(it.next().unwrap_or_default())).unwrap_or_default();
            }
            f => files.push(PathBuf::from(f)),
        }
    }
    assert!(files.len() >= 2, "usage: render <out.wav|out.mp3> <in1> [in2 ...]");
    let out = files.remove(0);
    let n = files.len();
    let slots: Vec<RenderSlot> = files
        .iter()
        .enumerate()
        .map(|(k, f)| {
            let mut s = RenderSlot::new(f, cue_in, cue_out);
            s.overlap_out_ms = if k + 1 < n { overlap } else { 0 };
            s.transition = kind;
            s.key_lock = key_lock;
            s.tempo_adjust_pct = tempos.get(k).copied().unwrap_or(0.0);
            s
        })
        .collect();
    let rep = render_to_file(&slots, &RenderOptions::default(), &out).expect("render");
    println!("rendered {} slots: {:.1} s of audio ({} frames) at {:.0}x real time -> {}", rep.slots, rep.seconds, rep.frames, rep.realtime_factor, out.display());
}
