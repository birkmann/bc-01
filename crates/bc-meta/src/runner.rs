use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bc_db::rusqlite::{Connection, params};
use bc_libcore::{ApiError, ApiResult, Ctx, JobHandle};
use bc_media::tags::read_tags;
use bc_media::write::{self as w, AnalysisInput, DjFields, FieldGroup, TagWritePlan, WriteMode, WriteStatus};
use bc_types::library::{MetadataProgress, TOPIC_METADATA_PROGRESS};
use serde::{Deserialize, Serialize};

use crate::journal;

pub const COMMIT_BATCH: usize = 50;
pub const KIND: &str = "metadata_bulk";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobParams {
    pub groups: Vec<String>,
    pub dry_run: bool,
    pub mode: String,
}

impl JobParams {
    pub fn groups_enum(&self) -> Vec<FieldGroup> {
        let g: Vec<FieldGroup> = self.groups.iter().filter_map(|s| FieldGroup::parse(s)).collect();
        if g.is_empty() { w::DEFAULT_GROUPS.to_vec() } else { g }
    }
}

pub fn parse_groups(raw: &[String]) -> Vec<FieldGroup> {
    let g: Vec<FieldGroup> = raw.iter().filter_map(|s| FieldGroup::parse(s)).collect();
    if g.is_empty() { w::DEFAULT_GROUPS.to_vec() } else { g }
}

pub fn group_names(g: &[FieldGroup]) -> Vec<String> {
    g.iter()
        .map(|g| match g {
            FieldGroup::Bpm => "bpm",
            FieldGroup::Key => "key",
            FieldGroup::Camelot => "camelot",
            FieldGroup::Energy => "energy",
            FieldGroup::ReplayGain => "replaygain",
        })
        .map(String::from)
        .collect()
}

pub struct TargetPlan {
    pub track_id: i64,
    pub file_id: i64,
    pub path: PathBuf,
    pub plan: TagWritePlan,
}

#[derive(Debug, Clone, Default)]
pub struct TrackReport {
    pub track_id: i64,
    /// written | skipped | unsupported | failed | not_analysed
    pub status: String,
    pub written: Vec<String>,
    pub conflicts: Vec<String>,
    pub fields: Vec<bc_types::library::FieldPlanOut>,
    pub error: Option<String>,
}

impl TrackReport {
    pub fn message(&self) -> String {
        match self.status.as_str() {
            "written" => format!("filled {}", self.written.join(", ")),
            "failed" => self.error.clone().unwrap_or_else(|| "write failed".into()),
            "not_analysed" => "not analysed".into(),
            "unsupported" => "container cannot be written".into(),
            _ if !self.conflicts.is_empty() => format!("no gaps; conflicts on {}", self.conflicts.join(", ")),
            _ => "no gaps".into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BatchReport {
    pub written: i64,
    pub skipped: i64,
    pub failed: i64,
    pub conflicts: i64,
    pub fields: BTreeMap<String, i64>,
}

/// Tracks worth queueing. `analysed` is the default so the queue length means something.
pub fn pending_track_ids(c: &Connection, scope_all: bool) -> ApiResult<Vec<i64>> {
    let sql = if scope_all {
        "SELECT t.id FROM tracks t WHERE EXISTS (SELECT 1 FROM files f WHERE f.track_id = t.id AND f.missing_since IS NULL) ORDER BY t.added_at DESC, t.id"
    } else {
        "SELECT t.id FROM tracks t JOIN analysis a ON a.track_id = t.id WHERE a.status != 'failed'
            AND EXISTS (SELECT 1 FROM files f WHERE f.track_id = t.id AND f.missing_since IS NULL) ORDER BY t.added_at DESC, t.id"
    };
    let mut st = c.prepare(sql)?;
    Ok(st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
}

fn analysis_input(c: &Connection, track_id: i64) -> ApiResult<Option<AnalysisInput>> {
    use bc_db::rusqlite::OptionalExtension;
    Ok(c.query_row(
        "SELECT status, bpm, bpm_confidence, key_root, key_mode, camelot, key_confidence, energy, replaygain_gain, true_peak_db FROM analysis WHERE track_id = ?1",
        [track_id],
        |r| {
            Ok(AnalysisInput {
                failed: r.get::<_, String>(0)? == "failed",
                bpm: r.get(1)?,
                bpm_confidence: r.get(2)?,
                key_root: r.get(3)?,
                key_mode: r.get(4)?,
                camelot: r.get(5)?,
                key_confidence: r.get(6)?,
                energy: r.get(7)?,
                replaygain_gain: r.get(8)?,
                true_peak_db: r.get(9)?,
            })
        },
    )
    .optional()?)
}

/// Work out what would be written to a track's files, touching nothing. Every present file of the track
/// is planned (a track held as FLAC and MP3 must not end up with tags in one file only).
pub fn plan_track(c: &Connection, track_id: i64, groups: &[FieldGroup]) -> ApiResult<(TrackReport, Vec<TargetPlan>)> {
    let values = match analysis_input(c, track_id)? {
        None => DjFields::default(),
        Some(a) => w::from_analysis(&a, groups, 0.0, 0.0),
    };
    let mut rep = TrackReport { track_id, ..Default::default() };
    if values.is_empty() {
        rep.status = "not_analysed".into();
        return Ok((rep, vec![]));
    }
    let files: Vec<(i64, String)> = {
        let mut st = c.prepare("SELECT id, path FROM files WHERE track_id = ?1 AND missing_since IS NULL ORDER BY id")?;
        st.query_map([track_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
    };
    let mut plans = Vec::new();
    for (file_id, p) in files {
        let path = PathBuf::from(p);
        let plan = w::plan_gaps(&path, &read_tags(&path), &values);
        plans.push(TargetPlan { track_id, file_id, path, plan });
    }
    if plans.is_empty() {
        rep.status = "skipped".into();
        return Ok((rep, plans));
    }
    let mut conflicts: Vec<String> = vec![];
    let mut gaps: Vec<String> = vec![];
    for p in &plans {
        for f in p.plan.conflicts() {
            if !conflicts.iter().any(|c| c == f) {
                conflicts.push(f.to_string());
            }
        }
        for f in p.plan.gaps() {
            if !gaps.iter().any(|c| c == f) {
                gaps.push(f.to_string());
            }
        }
    }
    rep.conflicts = conflicts;
    rep.fields = plans[0]
        .plan
        .fields
        .iter()
        .map(|f| bc_types::library::FieldPlanOut { field: f.field.into(), status: f.status.as_str().into(), existing: f.existing.clone(), proposed: f.proposed.clone() })
        .collect();
    if gaps.is_empty() {
        rep.status = if plans.iter().all(|p| !p.plan.supported) { "unsupported" } else { "skipped" }.into();
    } else {
        rep.status = "written".into(); // a proposal until write_track says otherwise
        rep.written = gaps;
    }
    Ok((rep, plans))
}

struct Stamp {
    file_id: i64,
    path: PathBuf,
    size: i64,
    mtime_ns: i64,
    inode: Option<i64>,
    tag_hash: String,
}

fn stamp_of(file_id: i64, path: &PathBuf) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some(Stamp {
        file_id,
        path: path.clone(),
        size: m.len() as i64,
        mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
        inode: Some(m.ino() as i64),
        tag_hash: read_tags(path).tag_hash(),
    })
}

fn commit_stamps(ctx: &Ctx, stamps: Vec<Stamp>) -> ApiResult<()> {
    if stamps.is_empty() {
        return Ok(());
    }
    ctx.write(move |t| {
        let now = bc_db::util::now_db();
        let mut st = t.prepare_cached("UPDATE files SET size_bytes = ?2, mtime_ns = ?3, inode = ?4, tag_hash = ?5, last_seen_at = ?6 WHERE id = ?1")?;
        for s in &stamps {
            st.execute(params![s.file_id, s.size, s.mtime_ns, s.inode, s.tag_hash, now])?;
            let _ = &s.path;
        }
        Ok(())
    })
}

/// Fill one track's empty DJ tags. With `dry_run` nothing is touched. The new stat and `tag_hash` of every
/// written file are committed in one transaction (`defer_stamps` lets a batch runner collect them instead).
pub fn write_track(
    ctx: &Ctx,
    track_id: i64,
    groups: &[FieldGroup],
    dry_run: bool,
    journal: Option<&mut journal::Writer>,
    defer_stamps: Option<&mut Vec<(i64, PathBuf)>>,
) -> ApiResult<TrackReport> {
    let (mut report, plans) = ctx.read(|c| plan_track(c, track_id, groups))?;
    if report.status != "written" || dry_run {
        return Ok(report);
    }
    let mut written: Vec<String> = vec![];
    let mut errors: Vec<String> = vec![];
    let mut stamped: Vec<(i64, PathBuf)> = vec![];
    let mut journal = journal;
    for target in &plans {
        if target.plan.is_noop() {
            continue;
        }
        if let Some(j) = journal.as_deref_mut() {
            let wmap: BTreeMap<String, String> =
                target.plan.fields.iter().filter(|f| f.status == w::FieldStatus::Gap).filter_map(|f| f.proposed.clone().map(|p| (f.field.to_string(), p))).collect();
            let entry = serde_json::json!({
                "track_id": target.track_id, "file_id": target.file_id, "path": target.path.to_string_lossy(),
                "fields": target.plan.gaps(), "written": wmap, "ts": bc_db::util::iso_now(),
            });
            j.entry(entry).and_then(|_| j.flush()).map_err(|e| ApiError::internal(format!("undo journal: {e}")))?;
        }
        match w::write_dj_fields(&target.path, &target.plan, WriteMode::Auto) {
            Ok(o) if o.status == WriteStatus::Written => {
                for f in o.fields {
                    if !written.contains(&f) {
                        written.push(f);
                    }
                }
                stamped.push((target.file_id, target.path.clone()));
            }
            Ok(_) => {}
            Err(e) => errors.push(format!("{}: {e}", target.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())),
        }
    }
    match defer_stamps {
        Some(d) => d.extend(stamped),
        None => {
            let st: Vec<Stamp> = stamped.iter().filter_map(|(id, p)| stamp_of(*id, p)).collect();
            commit_stamps(ctx, st)?;
        }
    }
    if !errors.is_empty() && written.is_empty() {
        report.status = "failed".into();
        report.written.clear();
        report.error = Some(errors.join("; "));
    } else if written.is_empty() {
        report.status = "skipped".into();
        report.written.clear();
    } else {
        written.sort();
        report.written = written;
        report.error = if errors.is_empty() { None } else { Some(errors.join("; ")) };
    }
    Ok(report)
}

/// The bulk job body (blocking): journal, batches of 50, stat re-stamp per batch, progress events.
pub fn run_job(ctx: &Ctx, handle: &JobHandle, ids: Vec<i64>, params: &JobParams) -> ApiResult<BatchReport> {
    let groups = params.groups_enum();
    let mut report = BatchReport::default();
    let mut journal = if params.dry_run {
        None
    } else {
        let path = journal::path(&ctx.config.backups_dir(), &handle.id);
        let mut j = journal::Writer::open(&path).map_err(|e| ApiError::internal(format!("journal: {e}")))?;
        j.header(&handle.id, serde_json::to_value(params).unwrap_or_default()).map_err(|e| ApiError::internal(e.to_string()))?;
        Some(j)
    };
    let total = ids.len() as i64;
    let mut last_pub = Instant::now() - Duration::from_secs(1);
    let mut pending: Vec<(i64, PathBuf)> = vec![];
    for (n, id) in ids.iter().enumerate() {
        if handle.cancelled() {
            break;
        }
        match write_track(ctx, *id, &groups, params.dry_run, journal.as_mut(), Some(&mut pending)) {
            Ok(r) => {
                if !r.conflicts.is_empty() {
                    report.conflicts += 1;
                }
                match r.status.as_str() {
                    "written" => {
                        report.written += 1;
                        if !params.dry_run {
                            for f in &r.written {
                                *report.fields.entry(f.clone()).or_default() += 1;
                            }
                        }
                    }
                    "failed" => report.failed += 1,
                    _ => report.skipped += 1,
                }
            }
            Err(e) => {
                tracing::warn!(track = id, error = %e, "tag write failed");
                report.failed += 1;
            }
        }
        if pending.len() >= COMMIT_BATCH || n + 1 == ids.len() {
            let st: Vec<Stamp> = pending.drain(..).filter_map(|(i, p)| stamp_of(i, &p)).collect();
            commit_stamps(ctx, st)?;
        }
        let done = (n + 1) as i64;
        if last_pub.elapsed() >= Duration::from_millis(250) || done == total {
            last_pub = Instant::now();
            handle.progress(done, Some(total), None);
            ctx.bus.publish(
                TOPIC_METADATA_PROGRESS,
                &MetadataProgress { job_id: handle.id.clone(), done, total, written: report.written, failed: report.failed, finished: done == total },
            );
        }
    }
    if let Some(mut j) = journal {
        let _ = j.flush();
    }
    Ok(report)
}

/// Remove what a job wrote, leaving any field edited since alone.
pub fn undo_job(ctx: &Ctx, job_id: &str) -> ApiResult<bc_types::library::UndoOut> {
    let path = journal::path(&ctx.config.backups_dir(), job_id);
    if !path.is_file() {
        return Err(ApiError::not_found(format!("no undo journal for job {job_id}")));
    }
    let mut out = bc_types::library::UndoOut { job_id: job_id.to_string(), ..Default::default() };
    let mut stamps: Vec<Stamp> = vec![];
    for e in journal::read_entries(&path) {
        out.entries += 1;
        let file_path = PathBuf::from(e.get("path").and_then(|p| p.as_str()).unwrap_or_default());
        if !file_path.is_file() {
            out.missing += 1;
            continue;
        }
        let fields: Vec<String> = e.get("fields").and_then(|f| f.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        let written: BTreeMap<String, String> =
            e.get("written").and_then(|f| f.as_object()).map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect()).unwrap_or_default();
        match w::undo_fields(&file_path, &fields, &written) {
            Err(err) => {
                tracing::warn!(path = %file_path.display(), error = %err, "undo failed");
                out.failed += 1;
            }
            Ok(r) if r.removed.is_empty() => out.skipped_changed += 1,
            Ok(_) => {
                out.restored += 1;
                if let Some(id) = e.get("file_id").and_then(|i| i.as_i64())
                    && let Some(s) = stamp_of(id, &file_path)
                {
                    stamps.push(s);
                }
            }
        }
    }
    commit_stamps(ctx, stamps)?;
    Ok(out)
}

pub fn writable_extensions() -> Vec<String> {
    let mut v: Vec<String> = bc_media::tags::AUDIO_EXTENSIONS
        .iter()
        .map(|e| format!(".{e}"))
        .filter(|e| w::is_writable(std::path::Path::new(&format!("x{e}"))))
        .collect();
    v.sort();
    v
}

pub fn spawn_job(ctx: &Ctx, ids: Vec<i64>, params: JobParams) -> String {
    let verb = if params.dry_run { "preview" } else { "write" };
    let handle = ctx.jobs.begin(KIND, &format!("{verb} tags for {} track(s)", ids.len()));
    let id = handle.id.clone();
    let ctx2 = ctx.clone();
    let p: Arc<JobParams> = Arc::new(params);
    std::thread::spawn(move || match run_job(&ctx2, &handle, ids, &p) {
        Ok(r) => {
            handle.finish_ok(serde_json::json!({"written": r.written, "skipped": r.skipped, "failed": r.failed, "conflicts": r.conflicts, "fields": r.fields, "dry_run": p.dry_run}));
            if !p.dry_run && r.written > 0 {
                ctx2.bus.invalidate("track", vec![]);
            }
        }
        Err(e) => handle.finish_err(e),
    });
    id
}
