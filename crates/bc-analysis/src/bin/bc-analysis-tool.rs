//! Dev tool for the analysis accuracy gate and calibration.
//!
//!   bc-analysis-tool bench   --db <legacy.db> [--n 2000] [--skip 0] [--seed 1] [--threads N]
//!                            [--profiles profiles.json] [--out rows.jsonl] [--store <new library.db>]
//!   bc-analysis-tool fit-key-profiles --train rows.jsonl [--test rows2.jsonl] [--out profiles.json] [--emit-rust]
//!   bc-analysis-tool calibrate-energy --rows rows.jsonl [--out cal.json] [--emit-rust]
//!
//! The legacy DB is opened read-only.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use bc_analysis::bench::{self, Row};
use bc_analysis::energy::{EnergyCalibration, EnergyRaw};
use bc_analysis::key::{self, Example, KeyProfiles};
use bc_analysis::pipeline::AnalyzeOptions;

fn args() -> (String, HashMap<String, String>) {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_default();
    let mut m = HashMap::new();
    let rest: Vec<String> = it.collect();
    let mut i = 0;
    while i < rest.len() {
        if let Some(k) = rest[i].strip_prefix("--") {
            if i + 1 < rest.len() && !rest[i + 1].starts_with("--") {
                m.insert(k.to_string(), rest[i + 1].clone());
                i += 2;
                continue;
            }
            m.insert(k.to_string(), "1".into());
        }
        i += 1;
    }
    (cmd, m)
}

fn read_rows(path: &str) -> Vec<Row> {
    let f = std::fs::File::open(path).expect("open rows");
    std::io::BufReader::new(f).lines().map_while(Result::ok).filter_map(|l| serde_json::from_str(&l).ok()).collect()
}

fn main() {
    let (cmd, a) = args();
    match cmd.as_str() {
        "bench" => {
            let db = PathBuf::from(a.get("db").cloned().unwrap_or_else(|| std::env::var("BC_LEGACY_DB").unwrap_or_default()));
            let n: usize = a.get("n").and_then(|v| v.parse().ok()).unwrap_or(2000);
            let skip: usize = a.get("skip").and_then(|v| v.parse().ok()).unwrap_or(0);
            let seed: i64 = a.get("seed").and_then(|v| v.parse().ok()).unwrap_or(1);
            let threads: usize = a
                .get("threads")
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get().saturating_sub(1).max(1)).unwrap_or(4));
            let mut opts = AnalyzeOptions::default();
            let env_dir = a.get("env-dir").map(PathBuf::from);
            if let Some(d) = &env_dir {
                std::fs::create_dir_all(d).expect("env dir");
                opts.debug = true;
            }
            if let Some(p) = a.get("profiles") {
                let prof: KeyProfiles = serde_json::from_str(&std::fs::read_to_string(p).expect("profiles")).expect("profiles json");
                opts.profiles = Arc::new(prof);
            }
            eprintln!("sampling {n} tracks (skip {skip}, seed {seed}) from {}", db.display());
            let samples = bench::sample(&db, n, skip, seed).expect("sample");
            eprintln!("analysing {} tracks on {threads} threads (nice 10)", samples.len());
            let (rows, wall) = bench::run(&samples, &opts, threads, true, env_dir.as_deref());
            if let Some(out) = a.get("out") {
                let mut f = std::fs::File::create(out).expect("out");
                for r in &rows {
                    writeln!(f, "{}", serde_json::to_string(r).unwrap()).unwrap();
                }
            }
            let rep = bench::report(&rows, wall, threads);
            println!("{}", serde_json::to_string_pretty(&rep).unwrap());
            if let Some(p) = a.get("store") {
                // persist the gate report into the NEW library DB (never the legacy one)
                let new_db = bc_db::Db::open(p).expect("open --store db");
                bench::store_report(&new_db, &rep).expect("store report");
                eprintln!("gate report stored in {p}");
            }
            let errs: Vec<_> = rows.iter().filter_map(|r| r.error.as_ref()).take(5).collect();
            if !errs.is_empty() {
                eprintln!("sample errors: {errs:?}");
            }
        }
        "tempo-eval" => {
            // re-run tempo analysis over dumped envelopes (no decoding): fast iteration
            let rows = read_rows(a.get("rows").expect("--rows"));
            let dir = PathBuf::from(a.get("env-dir").expect("--env-dir"));
            let excerpt = a.contains_key("excerpt");
            let mut ok_oct = 0usize;
            let mut ok_strict = 0usize;
            let mut n = 0usize;
            let out: Vec<(f64, f64, f64)> = {
                use rayon::prelude::*;
                rows.par_iter()
                    .filter_map(|r| {
                        let (mut env, fps) = bench::load_env(&dir.join(format!("{}.env", r.track_id)))?;
                        if excerpt {
                            let dur = env.len() as f64 / fps as f64;
                            let st = bc_music::beatgrid::legacy_excerpt_start_s(dur);
                            let a = (st * fps as f64) as usize;
                            let b = (((st + 120.0) * fps as f64) as usize).min(env.len());
                            env = env[a.min(b)..b].to_vec();
                        }
                        let ft = |f: f64| f * 1000.0 / fps as f64;
                        let t = bc_analysis::tempo::analyze(&env, fps, ft)?;
                        Some((t.bpm, r.ref_bpm?, t.confidence))
                    })
                    .collect()
            };
            let mut worst = Vec::new();
            for (b, rb, c) in &out {
                n += 1;
                if bench::octave_err(*b, *rb) < 0.005 {
                    ok_oct += 1;
                } else if worst.len() < 25 {
                    worst.push((*b, *rb, *c));
                }
                if (b / rb - 1.0).abs() < 0.005 {
                    ok_strict += 1;
                }
            }
            println!("n={n} octave-equiv within 0.5%: {:.4}  strict: {:.4}", ok_oct as f64 / n as f64, ok_strict as f64 / n as f64);
            if a.contains_key("show") {
                for w in worst {
                    println!("  ours {:.2} ref {:.2} conf {:.2} ratio {:.3}", w.0, w.1, w.2, w.0 / w.1);
                }
            }
        }
        "fit-key-profiles" => {
            let train = read_rows(a.get("train").expect("--train rows.jsonl"));
            let ex = |rows: &[Row]| -> Vec<Example> {
                rows.iter()
                    .filter_map(|r| Some(Example { feat: bench::feat_of(r)?, label: bench::label_of(r.ref_camelot.as_deref()?)? }))
                    .collect()
            };
            let tr = ex(&train);
            eprintln!("fitting on {} examples", tr.len());
            let mut best: Option<(KeyProfiles, f64)> = None;
            let test = a.get("test").map(|p| ex(&read_rows(p)));
            for l2 in [1e-4f32, 1e-3, 1e-2] {
                let (p, acc) = key::fit_profiles(&tr, l2, 400, &KeyProfiles::template());
                let te = test.as_ref().map(|t| {
                    let ok = t.iter().filter(|e| {
                        let r = key::classify(&e.feat, &p);
                        (r.minor as usize) * 12 + r.pitch_class as usize == e.label
                    });
                    ok.count() as f64 / t.len().max(1) as f64
                });
                eprintln!("l2={l2}: train acc {acc:.4} test acc {te:?}");
                let score = te.unwrap_or(acc);
                if best.as_ref().is_none_or(|b| score > b.1) {
                    best = Some((p, score));
                }
            }
            let (p, score) = best.expect("fit");
            eprintln!("best score {score:.4}");
            if let Some(out) = a.get("out") {
                std::fs::write(out, serde_json::to_string_pretty(&p).unwrap()).unwrap();
            }
            if a.contains_key("emit-rust") {
                println!("{}", emit_profiles_rust(&p));
            }
        }
        "calibrate-energy" => {
            let rows = read_rows(a.get("rows").expect("--rows rows.jsonl"));
            let raws: Vec<EnergyRaw> = rows
                .iter()
                .filter_map(|r| r.raw_energy)
                .map(|e| EnergyRaw { loud_p75: e[0], onset_density: e[1], centroid_hz: e[2] })
                .collect();
            let cal = EnergyCalibration::fit(&raws);
            if let Some(out) = a.get("out") {
                std::fs::write(out, serde_json::to_string_pretty(&cal).unwrap()).unwrap();
            }
            if a.contains_key("emit-rust") {
                println!("{}", emit_energy_rust(&cal));
            }
            eprintln!("fitted on {} tracks", raws.len());
        }
        _ => {
            eprintln!("usage: bc-analysis-tool bench|fit-key-profiles|calibrate-energy (see source header)");
            std::process::exit(2);
        }
    }
}

fn arr12(v: &[f32; 12]) -> String {
    format!("[{}]", v.iter().map(|x| format!("{x:.5}")).collect::<Vec<_>>().join(", "))
}

fn mode_w(w: &[[f32; 12]; bc_analysis::key::K]) -> String {
    format!("[{}]", w.iter().map(arr12).collect::<Vec<_>>().join(", "))
}

fn emit_profiles_rust(p: &KeyProfiles) -> String {
    format!(
        "use crate::key::KeyProfiles;

pub fn default_profiles() -> KeyProfiles {{
    KeyProfiles {{
        w: [{}, {}],
        bias: [{:.5}, {:.5}],
        name: \"fitted-default\".into(),
    }}
}}
",
        mode_w(&p.w[0]), mode_w(&p.w[1]), p.bias[0], p.bias[1]
    )
}

fn emit_energy_rust(c: &EnergyCalibration) -> String {
    format!(
        "use crate::energy::EnergyCalibration;\n\npub fn default_calibration() -> EnergyCalibration {{\n    EnergyCalibration {{\n        mean: {:?},\n        sd: {:?},\n        weights: {:?},\n        quantiles: vec!{:?},\n    }}\n}}\n",
        c.mean, c.sd, c.weights, c.quantiles
    )
}
