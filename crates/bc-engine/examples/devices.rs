//! List output devices and try to open each one (diagnostics for the cpal host).
//!   cargo run -p bc-engine --example devices

use bc_dsp::stretch::StretchQuality;
use bc_engine::host::{Engine, OutputKind, list_output_devices};
use bc_types::player::OutputTarget;

fn main() {
    let devs = list_output_devices();
    println!("{} output device(s)", devs.len());
    for d in &devs {
        let t = OutputTarget { device: Some(d.name.clone()), cue_device: None, buffer_frames: None };
        let res = Engine::open(OutputKind::Cpal(t), StretchQuality::Fast);
        println!(
            "{} {:<40} label={:?} {} Hz {} ch -> {}",
            if d.is_default { "*" } else { " " },
            d.name,
            d.label,
            d.default_sample_rate,
            d.max_channels,
            match res {
                Ok(e) => format!("opens: {} @ {} Hz, buffer {} frames (~{:.1} ms)", e.info.backend, e.info.sample_rate, e.info.buffer_frames, e.info.latency_ms),
                Err(e) => format!("FAILS: {e}"),
            }
        );
    }
}
