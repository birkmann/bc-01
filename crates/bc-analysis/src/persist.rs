//! Writes analysis results. Parent-process only (single writer): one transaction per ~200 rows.

use bc_db::rusqlite::{OptionalExtension, Transaction, params};
use bc_db::{Db, Result};
use bc_music::beatgrid::GridQuery;
use bc_types::analysis::{BeatGrid, CueKind, GridKind, WaveformMeta};

use crate::pipeline::TrackAnalysis;

pub const ANALYZER_NATIVE: &str = "bc-rs-1";
pub const ANALYZER_IMPORT: &str = "essentia-import";
pub const ANALYZER_SIDECAR: &str = "essentia-sidecar";
/// `analysis.analyzer_version` written by this pipeline (legacy essentia rows are 1).
pub const ANALYZER_VERSION: i64 = 2;
pub const SETTING_NATIVE_BPM_KEY: &str = "analysis.native_bpm_key";

/// What the worker hands back for one track (waveform already written to disk).
#[derive(Debug, Clone)]
pub struct WorkerOutput {
    pub analysis: TrackAnalysis,
    /// essentia result for tracks that had no BPM/key (new downloads), when the sidecar exists.
    pub sidecar: Option<crate::sidecar::SidecarResult>,
    pub wf_meta: Option<WaveformMeta>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Policy {
    /// True once the PLAN 7.2 library gate passed: native BPM/key overwrite imported essentia
    /// values. False (default) keeps imported values authoritative.
    pub native_bpm_key: bool,
}

pub fn read_policy(db: &Db) -> Policy {
    let v = db.read(|c| bc_db::settings::get(c, SETTING_NATIVE_BPM_KEY)).ok().flatten();
    Policy { native_bpm_key: matches!(v.as_deref(), Some("1") | Some("true") | Some("\"1\"")) }
}

fn json_f(v: &[f64]) -> String {
    serde_json::to_string(&v.iter().map(|x| (x * 100.0).round() / 100.0).collect::<Vec<_>>()).unwrap_or_else(|_| "[]".into())
}

/// Rescale a grid to the octave of `target_bpm` (when the analyser landed on half/double time
/// of the authoritative imported tempo).
pub fn octave_match(grid: &mut BeatGrid, target_bpm: f64) {
    let g = grid.bpm();
    if g <= 0.0 || target_bpm <= 0.0 {
        return;
    }
    let ratio = g / target_bpm;
    let factor = if (ratio - 2.0).abs() < 0.1 {
        0.5
    } else if (ratio - 0.5).abs() < 0.05 {
        2.0
    } else {
        return;
    };
    for s in &mut grid.segments {
        s.bpm *= factor;
        s.beats = s.beats.map(|b| ((b as f64) * factor).round() as u32);
    }
    grid.downbeat_phase = None;
    grid.phrase_starts_ms.clear();
    grid.confidence *= 0.8;
}

pub fn persist_one(
    tx: &Transaction<'_>,
    track_id: i64,
    out: &Result<WorkerOutput, String>,
    policy: Policy,
) -> Result<()> {
    let now = crate::time_now();
    let existing: Option<(Option<String>, String, Option<f64>)> = tx
        .query_row("SELECT analyzer, backend, bpm FROM analysis WHERE track_id = ?1", [track_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    let imported = existing
        .as_ref()
        .map(|(an, be, _)| an.as_deref() == Some(ANALYZER_IMPORT) || (an.is_none() && be == "essentia"))
        .unwrap_or(false);

    let out = match out {
        Err(msg) => {
            // never clobber a good row with a failure; only record failures for new rows
            if existing.is_none() {
                tx.execute(
                    "INSERT INTO analysis (track_id, analyzer_version, backend, status, error, analyzed_at, analyzer) \
                     VALUES (?1, ?2, ?3, 'failed', ?4, ?5, ?3)",
                    params![track_id, ANALYZER_VERSION, ANALYZER_NATIVE, msg.chars().take(500).collect::<String>(), now],
                )?;
            }
            return Ok(());
        }
        Ok(o) => o,
    };
    let a = &out.analysis;
    let t = a.tempo.as_ref();
    let k = a.key.as_ref();
    let l = a.loudness.as_ref();
    // Policy: imported essentia values are never replaced (unless native_bpm_key); a track without
    // values gets the essentia sidecar's BPM/key when available, else the native ones. Native
    // grids, loudness, energy and waveforms are always written.
    let sc = out.sidecar.as_ref().filter(|s| !imported && (s.bpm.is_some() || s.camelot().is_some()));
    let bpm_v = sc.and_then(|s| s.bpm).or(t.map(|t| t.bpm));
    let bpm_conf_v = if sc.is_some_and(|s| s.bpm.is_some()) { sc.and_then(|s| s.bpm_confidence) } else { t.map(|t| t.confidence) };
    let sc_key = sc.filter(|s| s.camelot().is_some());
    let key_root_v = sc_key.and_then(|s| s.key_root).or(k.map(|k| k.result.pitch_class as i64));
    let key_mode_v = sc_key.and_then(|s| s.key_mode.clone()).or(k.map(|k| if k.result.minor { "minor".to_string() } else { "major".to_string() }));
    let camelot_v = sc_key.and_then(|s| s.camelot()).or(k.map(|k| k.result.camelot));
    let key_conf_v = if sc_key.is_some() { sc_key.and_then(|s| s.key_confidence) } else { k.map(|k| k.result.strength) };
    let analyzer_name = if sc.is_some() { ANALYZER_SIDECAR } else { ANALYZER_NATIVE };
    let backend_name = if sc.is_some() { "essentia" } else { ANALYZER_NATIVE };
    let status = if bpm_v.is_some() && camelot_v.is_some() { "ok" } else { "partial" };
    let first_beat = t.and_then(|t| t.grid.segments.first().map(|s| s.origin_ms));
    let downbeat_ms = t.and_then(|t| {
        let g = &t.grid;
        let p = g.downbeat_phase? as f64;
        Some(g.beat_time_ms(p))
    });
    let grid_kind = t.map(|t| if t.grid.kind == GridKind::Constant { "constant" } else { "variable" });
    let cands = t.map(|t| json_f(&t.candidates));
    let overwrite_bpm_key = !imported || policy.native_bpm_key;

    if overwrite_bpm_key {
        tx.execute(
            "INSERT INTO analysis (track_id, analyzer_version, backend, status, error, analyzed_at, bpm, bpm_confidence, \
               beat_offset_ms, key_root, key_mode, camelot, key_confidence, loudness_lufs, true_peak_db, replaygain_gain, \
               energy, bpm_candidates, grid_kind, downbeat_offset_ms, lra, true_peak_dbtp, energy_v2, analyzer) \
             VALUES (?1,?2,?23,?4,NULL,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?3) \
             ON CONFLICT(track_id) DO UPDATE SET analyzer_version=excluded.analyzer_version, backend=excluded.backend, \
               status=excluded.status, error=NULL, analyzed_at=excluded.analyzed_at, \
               bpm=COALESCE(excluded.bpm, bpm), bpm_confidence=COALESCE(excluded.bpm_confidence, bpm_confidence), \
               beat_offset_ms=COALESCE(excluded.beat_offset_ms, beat_offset_ms), \
               key_root=COALESCE(excluded.key_root, key_root), key_mode=COALESCE(excluded.key_mode, key_mode), \
               camelot=COALESCE(excluded.camelot, camelot), key_confidence=COALESCE(excluded.key_confidence, key_confidence), \
               loudness_lufs=COALESCE(excluded.loudness_lufs, loudness_lufs), true_peak_db=COALESCE(excluded.true_peak_db, true_peak_db), \
               replaygain_gain=COALESCE(excluded.replaygain_gain, replaygain_gain), energy=excluded.energy, \
               bpm_candidates=COALESCE(excluded.bpm_candidates, bpm_candidates), grid_kind=COALESCE(excluded.grid_kind, grid_kind), \
               downbeat_offset_ms=COALESCE(excluded.downbeat_offset_ms, downbeat_offset_ms), lra=COALESCE(excluded.lra, lra), \
               true_peak_dbtp=COALESCE(excluded.true_peak_dbtp, true_peak_dbtp), energy_v2=excluded.energy_v2, analyzer=excluded.analyzer",
            params![
                track_id,
                ANALYZER_VERSION,
                analyzer_name,
                status,
                now,
                bpm_v.map(|b| (b * 100.0).round() / 100.0),
                bpm_conf_v.map(|c| (c * 1000.0).round() / 1000.0),
                first_beat,
                key_root_v,
                key_mode_v,
                camelot_v,
                key_conf_v.map(|c| (c * 1000.0).round() / 1000.0),
                l.map(|l| (l.lufs * 100.0).round() / 100.0),
                l.map(|l| (l.true_peak_dbtp * 100.0).round() / 100.0),
                l.map(|l| (l.replaygain_gain * 100.0).round() / 100.0),
                (a.energy_legacy * 1000.0).round() / 1000.0,
                cands,
                grid_kind,
                downbeat_ms,
                l.map(|l| (l.lra * 100.0).round() / 100.0),
                l.map(|l| (l.true_peak_dbtp * 100.0).round() / 100.0),
                (a.energy_v2 * 100.0).round() / 100.0,
                backend_name,
            ],
        )?;
    } else {
        // imported essentia BPM/key stay authoritative: add everything else
        tx.execute(
            "UPDATE analysis SET analyzer_version=?2, analyzed_at=?3, \
               loudness_lufs=COALESCE(?4, loudness_lufs), true_peak_db=COALESCE(?5, true_peak_db), \
               replaygain_gain=COALESCE(?6, replaygain_gain), lra=?7, true_peak_dbtp=?5, energy_v2=?8, \
               grid_kind=?9, downbeat_offset_ms=?10 WHERE track_id=?1",
            params![
                track_id,
                ANALYZER_VERSION,
                now,
                l.map(|l| (l.lufs * 100.0).round() / 100.0),
                l.map(|l| (l.true_peak_dbtp * 100.0).round() / 100.0),
                l.map(|l| (l.replaygain_gain * 100.0).round() / 100.0),
                l.map(|l| (l.lra * 100.0).round() / 100.0),
                (a.energy_v2 * 100.0).round() / 100.0,
                grid_kind,
                downbeat_ms,
            ],
        )?;
    }

    tx.execute(
        "UPDATE tracks SET duration_ms = ?2 WHERE id = ?1 AND (duration_ms IS NULL OR duration_ms = 0)",
        params![track_id, a.duration_ms],
    )?;

    // beat grid
    if let Some(t) = t {
        let mut grid = t.grid.clone();
        if !overwrite_bpm_key {
            if let Some((_, _, Some(b))) = &existing {
                octave_match(&mut grid, *b);
            }
        } else if let Some(b) = sc.and_then(|s| s.bpm) {
            octave_match(&mut grid, b);
        }
        tx.execute(
            "INSERT INTO beat_grids (track_id, kind, grid, downbeat_phase, confidence, source, updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(track_id) DO UPDATE SET kind=excluded.kind, grid=excluded.grid, \
               downbeat_phase=excluded.downbeat_phase, confidence=excluded.confidence, source=excluded.source, updated_at=excluded.updated_at",
            params![
                track_id,
                if grid.kind == GridKind::Constant { "constant" } else { "variable" },
                serde_json::to_string(&grid).unwrap_or_else(|_| "{}".into()),
                grid.downbeat_phase.map(|p| p as i64),
                grid.confidence,
                grid.source,
                now
            ],
        )?;
    }

    // auto cues
    tx.execute("DELETE FROM cue_points WHERE track_id = ?1 AND auto = 1", [track_id])?;
    if let Some(mix) = &a.mix {
        let ins = |kind: CueKind, pos: i64, label: &str| -> Result<()> {
            tx.execute(
                "INSERT INTO cue_points (track_id, kind, pos_ms, label, auto, created_at) VALUES (?1,?2,?3,?4,1,?5)",
                params![track_id, cue_kind_str(kind), pos as f64, label, now],
            )?;
            Ok(())
        };
        if mix.points.cue_in_ms > 0 {
            ins(CueKind::MixIn, mix.points.cue_in_ms, "Mix in")?;
        }
        if let Some(out) = mix.points.cue_out_ms {
            ins(CueKind::MixOut, out, "Mix out")?;
        }
        if let Some(d) = mix.drop_ms {
            ins(CueKind::Drop, d, "Drop")?;
        }
    }

    if let Some(m) = &out.wf_meta {
        tx.execute(
            "INSERT INTO waveform_meta (track_id, format_version, source_hash, sample_rate, duration_ms, overview_points, \
               detail_points, bytes, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) \
             ON CONFLICT(track_id) DO UPDATE SET format_version=excluded.format_version, source_hash=excluded.source_hash, \
               sample_rate=excluded.sample_rate, duration_ms=excluded.duration_ms, overview_points=excluded.overview_points, \
               detail_points=excluded.detail_points, bytes=excluded.bytes, updated_at=excluded.updated_at",
            params![
                track_id,
                m.format_version,
                m.source_hash,
                m.sample_rate,
                m.duration_ms,
                m.overview_points,
                m.detail_points,
                m.bytes,
                now
            ],
        )?;
    }
    Ok(())
}

pub fn cue_kind_str(k: CueKind) -> &'static str {
    match k {
        CueKind::Hot => "hot",
        CueKind::Memory => "memory",
        CueKind::MixIn => "mix_in",
        CueKind::MixOut => "mix_out",
        CueKind::Drop => "drop",
        CueKind::Intro => "intro",
        CueKind::Outro => "outro",
        CueKind::Loop => "loop",
    }
}

pub fn cue_kind_parse(s: &str) -> Option<CueKind> {
    Some(match s {
        "hot" => CueKind::Hot,
        "memory" => CueKind::Memory,
        "mix_in" => CueKind::MixIn,
        "mix_out" => CueKind::MixOut,
        "drop" => CueKind::Drop,
        "intro" => CueKind::Intro,
        "outro" => CueKind::Outro,
        "loop" => CueKind::Loop,
        _ => return None,
    })
}

/// Write a whole batch in one transaction.
pub fn persist_batch(db: &Db, batch: Vec<(i64, Result<WorkerOutput, String>)>, policy: Policy) -> Result<()> {
    db.write(move |tx| {
        for (track_id, out) in &batch {
            persist_one(tx, *track_id, out, policy)?;
        }
        Ok(())
    })
}
