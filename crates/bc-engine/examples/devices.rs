//! List output devices and try to open each one (diagnostics for the cpal host).
//!   cargo run -p bc-engine --example devices
//!   cargo run -p bc-engine --example devices -- --cue   # each device as the cue (headphone) output
//!                                                       # next to the default main output

use bc_dsp::stretch::StretchQuality;
use bc_engine::host::{Engine, OutputKind, list_output_devices};
use bc_types::player::OutputTarget;

fn main() {
    let as_cue = std::env::args().any(|a| a == "--cue");
    let devs = list_output_devices();
    println!("{} output device(s){}", devs.len(), if as_cue { ", each tried as the cue device" } else { "" });
    for d in &devs {
        let t = if as_cue {
            OutputTarget { device: None, cue_device: Some(d.name.clone()), buffer_frames: None }
        } else {
            OutputTarget { device: Some(d.name.clone()), cue_device: None, buffer_frames: None }
        };
        let res = Engine::open(OutputKind::Cpal(t), StretchQuality::Fast);
        println!(
            "{} {:<40} label={:?} {} Hz {} ch -> {}",
            if d.is_default { "*" } else { " " },
            d.name,
            d.label,
            d.default_sample_rate,
            d.max_channels,
            match res {
                Ok(e) if as_cue => match &e.info.cue_device {
                    Some(c) => format!("cue opens: {c} @ {} Hz (main {} @ {} Hz)", e.info.cue_sample_rate, e.info.device, e.info.sample_rate),
                    None => format!("cue FAILS, previews fall back to the main output ({} @ {} Hz)", e.info.device, e.info.sample_rate),
                },
                Ok(e) => format!("opens: {} @ {} Hz, buffer {} frames (~{:.1} ms)", e.info.backend, e.info.sample_rate, e.info.buffer_frames, e.info.latency_ms),
                Err(e) => format!("FAILS: {e}"),
            }
        );
    }
}
