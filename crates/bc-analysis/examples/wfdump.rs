//! Dev helper: build a waveform from an audio file and write it as a `.bcw2` wire file (raw blocks,
//! overview + detail) for the browser demo. Usage: wfdump <audio> <out.bcw2>
use bc_analysis::pipeline::{AnalyzeOptions, analyze_file};
use bc_waveform::EncodeOpts;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let opts = AnalyzeOptions { loudness: false, tempo: false, key: false, waveform: true, ..Default::default() };
    let mut res = analyze_file(std::path::Path::new(&a[1]), &opts).expect("analyze");
    let wf = res.waveform.take().expect("waveform");
    let ov_only = a.get(3).is_some_and(|s| s == "overview");
    let bytes = wf.to_bytes(&if ov_only { EncodeOpts::wire_overview() } else { EncodeOpts::wire() });
    std::fs::write(&a[2], bytes).expect("write");
    eprintln!("{} s, {} detail points", wf.duration_s(), wf.detail.as_ref().map_or(0, |d| d.n));
}
