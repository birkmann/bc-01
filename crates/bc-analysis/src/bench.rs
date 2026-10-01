//! Accuracy benchmark against the imported essentia values (PLAN 7.2) plus throughput.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use bc_db::rusqlite::{Connection, OpenFlags};
use bc_music::camelot::{key_compatibility, parse_key};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::key::Feat;
use crate::pipeline::{AnalyzeOptions, analyze_file};

/// One sampled library track with its essentia reference.
#[derive(Debug, Clone)]
pub struct Sample {
    pub track_id: i64,
    pub path: PathBuf,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub duration_ms: Option<i64>,
}

/// Sample `n` tracks with essentia values and an existing file, in a seeded hash order,
/// skipping the first `skip` of that order (so train and test sets are disjoint).
pub fn sample(db: &Path, n: usize, skip: usize, seed: i64) -> Result<Vec<Sample>, String> {
    let c = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    // one file per track; order by the seeded hash
    let sql = format!(
        "SELECT t.id, (SELECT f.path FROM files f WHERE f.track_id = t.id AND f.missing_since IS NULL ORDER BY f.id LIMIT 1), \
                a.bpm, a.camelot, t.duration_ms \
         FROM tracks t JOIN analysis a ON a.track_id = t.id \
         WHERE a.bpm IS NOT NULL AND a.camelot IS NOT NULL AND a.status = 'ok' \
         ORDER BY (t.id * 2654435761 + {seed}) % 4294967296 LIMIT {} OFFSET {skip}",
        n * 2 // oversample: some paths are missing
    );
    let mut st = c.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| {
            Ok(Sample {
                track_id: r.get(0)?,
                path: PathBuf::from(r.get::<_, Option<String>>(1)?.unwrap_or_default()),
                bpm: r.get(2)?,
                camelot: r.get(3)?,
                duration_ms: r.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for r in rows {
        let s = r.map_err(|e| e.to_string())?;
        if !s.path.as_os_str().is_empty() && s.path.exists() {
            out.push(s);
            if out.len() >= n {
                break;
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub track_id: i64,
    pub ref_bpm: Option<f64>,
    pub ref_camelot: Option<String>,
    pub bpm: Option<f64>,
    pub bpm_conf: Option<f64>,
    pub camelot: Option<String>,
    pub key_strength: Option<f64>,
    pub grid_kind: Option<String>,
    pub downbeat_conf: Option<f64>,
    pub energy_v2: Option<f64>,
    pub lufs: Option<f64>,
    pub duration_s: f64,
    pub secs: f64,
    pub decode_s: f64,
    pub post_s: f64,
    pub error: Option<String>,
    pub raw_energy: Option<[f32; 3]>,
    #[serde(default)]
    pub chroma36: Option<Vec<Vec<f64>>>,
}

pub fn run(samples: &[Sample], opts: &AnalyzeOptions, threads: usize, progress: bool, env_dir: Option<&Path>) -> (Vec<Row>, f64) {
    let pool = crate::pool::build_pool(threads);
    let done = Arc::new(AtomicUsize::new(0));
    let total = samples.len();
    let t0 = Instant::now();
    let rows: Vec<Row> = pool.install(|| {
        samples
            .par_iter()
            .map(|s| {
                let t = Instant::now();
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| analyze_file(&s.path, opts)));
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if progress && n % 50 == 0 {
                    eprintln!("  {n}/{total}  {:.1}s elapsed", t0.elapsed().as_secs_f64());
                }
                let mut row = Row {
                    track_id: s.track_id,
                    ref_bpm: s.bpm,
                    ref_camelot: s.camelot.clone(),
                    bpm: None,
                    bpm_conf: None,
                    camelot: None,
                    key_strength: None,
                    grid_kind: None,
                    downbeat_conf: None,
                    energy_v2: None,
                    lufs: None,
                    duration_s: 0.0,
                    secs: t.elapsed().as_secs_f64(),
                    decode_s: 0.0,
                    post_s: 0.0,
                    error: None,
                    raw_energy: None,
                    chroma36: None,
                };
                match res {
                    Ok(Ok(a)) => {
                        row.duration_s = a.duration_ms as f64 / 1000.0;
                        row.bpm = a.tempo.as_ref().map(|t| t.bpm);
                        row.bpm_conf = a.tempo.as_ref().map(|t| t.confidence);
                        row.grid_kind = a.tempo.as_ref().map(|t| format!("{:?}", t.grid.kind));
                        row.downbeat_conf = a.tempo.as_ref().and_then(|t| t.downbeat_confidence);
                        row.camelot = a.key.as_ref().map(|k| k.result.camelot.to_string());
                        row.key_strength = a.key.as_ref().map(|k| k.result.strength);
                        row.energy_v2 = Some(a.energy_v2);
                        row.lufs = a.loudness.as_ref().map(|l| l.lufs);
                        row.decode_s = a.timings.decode_s;
                        row.post_s = a.timings.post_s;
                        row.chroma36 = a.debug_chroma.clone();
                        if let Some(dir) = env_dir {
                            let mut bytes = Vec::with_capacity(a.debug_onset.len() * 4 + 4);
                            bytes.extend_from_slice(&a.debug_fps.to_le_bytes());
                            for v in &a.debug_onset {
                                bytes.extend_from_slice(&v.to_le_bytes());
                            }
                            let _ = std::fs::write(dir.join(format!("{}.env", s.track_id)), bytes);
                        }
                        row.raw_energy = Some([a.energy_raw.loud_p75, a.energy_raw.onset_density, a.energy_raw.centroid_hz]);
                    }
                    Ok(Err(e)) => row.error = Some(e.to_string()),
                    Err(_) => row.error = Some("panic".into()),
                }
                row
            })
            .collect()
    });
    (rows, t0.elapsed().as_secs_f64())
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub n: usize,
    pub failed: usize,
    pub bpm_within_0_5_pct_octave: f64,
    pub bpm_within_0_5_pct_strict: f64,
    pub bpm_within_3_pct_octave: f64,
    pub key_exact: f64,
    pub key_exact_or_compatible: f64,
    pub key_relative: f64,
    pub key_fifth: f64,
    pub wall_s: f64,
    pub tracks_per_s: f64,
    pub audio_hours: f64,
    pub realtime_x: f64,
    pub mean_secs_per_track: f64,
    pub threads: usize,
}

pub fn octave_err(ours: f64, theirs: f64) -> f64 {
    [0.5, 1.0, 2.0, 1.0 / 3.0, 3.0].iter().take(3).map(|m| ((ours * m) / theirs - 1.0).abs()).fold(f64::INFINITY, f64::min)
}

pub fn report(rows: &[Row], wall_s: f64, threads: usize) -> Report {
    let ok: Vec<&Row> = rows.iter().filter(|r| r.error.is_none()).collect();
    let n = rows.len();
    let frac = |count: usize| count as f64 / n.max(1) as f64;
    let mut oct = 0;
    let mut strict = 0;
    let mut loose = 0;
    let mut kexact = 0;
    let mut kcompat = 0;
    let mut krel = 0;
    let mut kfifth = 0;
    for r in &ok {
        if let (Some(b), Some(rb)) = (r.bpm, r.ref_bpm) {
            if octave_err(b, rb) < 0.005 {
                oct += 1;
            }
            if (b / rb - 1.0).abs() < 0.005 {
                strict += 1;
            }
            if octave_err(b, rb) < 0.03 {
                loose += 1;
            }
        }
        if let (Some(k), Some(rk)) = (&r.camelot, &r.ref_camelot) {
            if k == rk {
                kexact += 1;
                kcompat += 1;
            } else {
                let c = key_compatibility(Some(rk), Some(k));
                if c.ok() {
                    kcompat += 1;
                }
                if c.reason.contains("relative") {
                    krel += 1;
                }
                if c.reason.contains("one step") {
                    kfifth += 1;
                }
            }
        }
    }
    let audio_s: f64 = rows.iter().map(|r| r.duration_s).sum();
    Report {
        n,
        failed: n - ok.len(),
        bpm_within_0_5_pct_octave: frac(oct),
        bpm_within_0_5_pct_strict: frac(strict),
        bpm_within_3_pct_octave: frac(loose),
        key_exact: frac(kexact),
        key_exact_or_compatible: frac(kcompat),
        key_relative: frac(krel),
        key_fifth: frac(kfifth),
        wall_s,
        tracks_per_s: n as f64 / wall_s.max(1e-9),
        audio_hours: audio_s / 3600.0,
        realtime_x: audio_s / wall_s.max(1e-9),
        mean_secs_per_track: rows.iter().map(|r| r.secs).sum::<f64>() / n.max(1) as f64,
        threads,
    }
}

pub fn feat_of(row: &Row) -> Option<Feat> {
    crate::key::features_from_vecs(row.chroma36.as_ref()?)
}

pub fn label_of(camelot: &str) -> Option<usize> {
    let (pc, mode) = parse_key(camelot)?;
    Some(if mode == bc_music::camelot::Mode::Minor { 12 + pc as usize } else { pc as usize })
}

/// Load an envelope dumped by `run(.., env_dir)`.
pub fn load_env(path: &Path) -> Option<(Vec<f32>, f32)> {
    let b = std::fs::read(path).ok()?;
    if b.len() < 8 {
        return None;
    }
    let fps = f32::from_le_bytes(b[..4].try_into().ok()?);
    let v = b[4..].as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
    Some((v, fps))
}

/// Convert a bench report into the stored DTO (gate: BPM >= 95 %, key exact >= 80 %, key
/// exact-or-compatible >= 92 %).
pub fn to_accuracy(r: &Report) -> bc_types::analysis::AccuracyReport {
    bc_types::analysis::AccuracyReport {
        generated_at: crate::time_now().replace(' ', "T") + "Z",
        n: r.n as u64,
        failed: r.failed as u64,
        bpm_within_0_5_pct_octave: r.bpm_within_0_5_pct_octave,
        bpm_within_0_5_pct_strict: r.bpm_within_0_5_pct_strict,
        bpm_within_3_pct_octave: r.bpm_within_3_pct_octave,
        key_exact: r.key_exact,
        key_exact_or_compatible: r.key_exact_or_compatible,
        tracks_per_s: r.tracks_per_s,
        realtime_x: r.realtime_x,
        threads: r.threads as u64,
        passed: r.bpm_within_0_5_pct_octave >= 0.95 && r.key_exact >= 0.80 && r.key_exact_or_compatible >= 0.92,
    }
}

/// Persist the gate report into `settings` of the NEW library DB (`bc analyze --gate` calls this).
pub fn store_report(db: &bc_db::Db, r: &Report) -> bc_db::Result<()> {
    let json = serde_json::to_string(&to_accuracy(r)).unwrap_or_default();
    db.write(move |t| bc_db::settings::set(t, bc_types::analysis::SETTING_ACCURACY_REPORT, &json))
}
