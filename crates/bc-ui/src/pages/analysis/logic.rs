//! Pure analysis-page logic: scan scopes, in-place patching of the cached status /
//! queue from WS events (no refetch on progress ticks), analyzer breakdown and ETA.
use std::collections::BTreeMap;

use bc_types::analysis::*;

pub const QUEUE_URL: &str = "/analysis/queue?limit=200&jobs_limit=20";
pub const STATUS_URL: &str = "/analysis/status";

/// (scope, label, what it does)
pub const SCOPES: [(ScanScope, &str, &str); 5] = [
    (ScanScope::Missing, "Missing", "Tracks never analysed"),
    (ScanScope::Stale, "Stale", "Analysed by an older analyzer version"),
    (ScanScope::Failed, "Failed", "Tracks whose last analysis failed"),
    (ScanScope::Upgrade, "Upgrade", "Imported results that lack a waveform and beat grid"),
    (ScanScope::All, "Everything", "Re-analyse the whole library"),
];

pub fn is_active_job(status: &str) -> bool {
    matches!(status, "queued" | "running")
}

/// Totals over the unfinished jobs: `(settled, total)`.
pub fn aggregate(jobs: &[AnalysisJobOut]) -> (i64, i64) {
    jobs.iter().filter(|j| is_active_job(&j.status)).fold((0, 0), |(d, t), j| (d + j.completed + j.failed + j.skipped, t + j.total))
}

pub fn fraction(done: i64, total: i64) -> Option<f64> {
    (total > 0).then(|| (done as f64 / total as f64).clamp(0.0, 1.0))
}

/// `analysis.progress`: authoritative job counters. Returns true when the job just settled
/// (the caller refetches the status once, since coverage changed).
pub fn patch_progress(q: &mut AnalysisQueue, ev: &AnalysisProgressEvent) -> bool {
    match q.jobs.iter_mut().find(|j| j.id == ev.job_id) {
        Some(j) => {
            let was_active = is_active_job(&j.status);
            j.status = ev.status.clone();
            j.total = ev.total;
            j.completed = ev.completed;
            j.failed = ev.failed;
            let settled = j.completed + j.failed + j.skipped;
            j.progress = if j.total > 0 { settled as f64 / j.total as f64 } else { 0.0 };
            was_active && !is_active_job(&j.status)
        }
        // A job we have never seen (started elsewhere): the caller refetches.
        None => true,
    }
}

/// `analysis.batch`: these tracks were claimed by workers.
pub fn patch_batch(q: &mut AnalysisQueue, ev: &AnalysisBatchEvent) {
    for it in q.items.iter_mut() {
        if it.track_id.map(|t| ev.tracks.contains(&t)).unwrap_or(false) && it.status == "pending" {
            it.status = "running".into();
        }
    }
    for t in &ev.tracks {
        if !q.active_track_ids.contains(t) {
            q.active_track_ids.push(*t);
        }
    }
    q.queued_track_ids.retain(|t| !ev.tracks.contains(t));
}

pub fn item_message(ev: &AnalysisItemEvent) -> String {
    let mut parts = vec![];
    if let Some(b) = ev.bpm {
        parts.push(format!("{b:.0} BPM"));
    }
    if let Some(k) = ev.camelot.as_ref().or(ev.key.as_ref()) {
        parts.push(k.clone());
    }
    parts.join(" ")
}

/// `analysis.item`: one track finished.
pub fn patch_item(q: &mut AnalysisQueue, ev: &AnalysisItemEvent) {
    let failed = ev.status == "failed";
    if let Some(it) = q.items.iter_mut().find(|i| i.track_id == Some(ev.track_id)) {
        it.status = if failed { "failed".into() } else { "done".into() };
        it.message = Some(item_message(ev));
        it.last_error = ev.error.clone();
    }
    q.active_track_ids.retain(|t| *t != ev.track_id);
    q.queued_track_ids.retain(|t| *t != ev.track_id);
    if failed && !q.failed_track_ids.contains(&ev.track_id) {
        q.failed_track_ids.push(ev.track_id);
    }
}

/// Keep the coverage numbers moving between refetches.
pub fn patch_status_item(s: &mut AnalysisStatus, ev: &AnalysisItemEvent) {
    if ev.status == "failed" {
        s.failed += 1;
        s.missing = (s.missing - 1).max(0);
    } else {
        s.analysed += 1;
        s.missing = (s.missing - 1).max(0);
    }
    s.running_tracks = (s.running_tracks - 1).max(0);
    s.batch_done += 1;
    if s.total_tracks > 0 {
        s.coverage = s.analysed as f64 / s.total_tracks as f64;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzerRow {
    pub id: String,
    pub label: String,
    pub detail: &'static str,
    pub count: i64,
    pub share: f64,
}

pub fn analyzer_label(id: &str) -> (&'static str, &'static str) {
    match id {
        "essentia-import" => ("Imported from the old app", "Essentia results copied over; no waveform or beat grid yet"),
        "bc-rs-1" => ("Native analyzer", "bc-rs-1: waveform, beat grid, key, loudness"),
        "essentia-sidecar" => ("Essentia sidecar", "Optional Python analyzer"),
        _ => ("Other analyzer", ""),
    }
}

/// Rows per analyzer, largest first, with their share of all analysed tracks.
pub fn analyzer_rows(by: &BTreeMap<String, i64>) -> Vec<AnalyzerRow> {
    let total: i64 = by.values().sum();
    let mut rows: Vec<AnalyzerRow> = by
        .iter()
        .map(|(id, count)| {
            let (label, detail) = analyzer_label(id);
            AnalyzerRow { id: id.clone(), label: label.to_string(), detail, count: *count, share: if total > 0 { *count as f64 / total as f64 } else { 0.0 } }
        })
        .collect();
    rows.sort_by(|a, b| b.count.cmp(&a.count).then(a.id.cmp(&b.id)));
    rows
}

/// Seconds remaining from `(time_ms, settled)` samples (oldest first), or `None` when the rate is unknown.
pub fn eta_seconds(samples: &[(f64, i64)], remaining: i64) -> Option<f64> {
    if remaining <= 0 {
        return Some(0.0);
    }
    let (t0, d0) = *samples.first()?;
    let (t1, d1) = *samples.last()?;
    let dt = (t1 - t0) / 1000.0;
    let dd = (d1 - d0) as f64;
    (dt >= 3.0 && dd > 0.0).then(|| remaining as f64 / (dd / dt))
}

pub fn format_eta(secs: f64) -> String {
    let s = secs.round() as i64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, status: &str, total: i64, done: i64) -> AnalysisJobOut {
        AnalysisJobOut { id: id.into(), status: status.into(), label: None, total, completed: done, failed: 0, skipped: 0, progress: 0.0, error: None, created_at: None, started_at: None, finished_at: None }
    }
    fn item(id: i64, track: i64, status: &str) -> AnalysisItemOut {
        AnalysisItemOut { id, job_id: "j".into(), seq: id, status: status.into(), track_id: Some(track), title: None, artist: None, message: None, last_error: None, started_at: None, finished_at: None }
    }
    fn ev(track: i64, status: &str) -> AnalysisItemEvent {
        AnalysisItemEvent { track_id: track, status: status.into(), bpm: Some(127.6), camelot: Some("8A".into()), key: None, energy: None, error: None, done: 1, batch: 1 }
    }

    #[test]
    fn aggregate_only_active() {
        let jobs = vec![job("a", "running", 100, 40), job("b", "completed", 50, 50), job("c", "queued", 10, 0)];
        assert_eq!(aggregate(&jobs), (40, 110));
        assert_eq!(fraction(40, 110).map(|f| (f * 100.0).round()), Some(36.0));
        assert_eq!(fraction(0, 0), None);
    }

    #[test]
    fn progress_patch_reports_settling() {
        let mut q = AnalysisQueue { jobs: vec![job("a", "running", 10, 2)], ..Default::default() };
        let mid = AnalysisProgressEvent { job_id: "a".into(), status: "running".into(), total: 10, completed: 5, failed: 1 };
        assert!(!patch_progress(&mut q, &mid));
        assert!((q.jobs[0].progress - 0.6).abs() < 1e-9);
        let end = AnalysisProgressEvent { job_id: "a".into(), status: "completed".into(), total: 10, completed: 9, failed: 1 };
        assert!(patch_progress(&mut q, &end));
        let unknown = AnalysisProgressEvent { job_id: "zz".into(), status: "running".into(), total: 1, completed: 0, failed: 0 };
        assert!(patch_progress(&mut q, &unknown));
    }

    #[test]
    fn batch_and_item_patch() {
        let mut q = AnalysisQueue { items: vec![item(1, 10, "pending"), item(2, 11, "pending")], queued_track_ids: vec![10, 11], ..Default::default() };
        patch_batch(&mut q, &AnalysisBatchEvent { tracks: vec![10], status: "running".into() });
        assert_eq!(q.items[0].status, "running");
        assert_eq!(q.items[1].status, "pending");
        assert_eq!(q.active_track_ids, vec![10]);
        assert_eq!(q.queued_track_ids, vec![11]);
        patch_item(&mut q, &ev(10, "ok"));
        assert_eq!(q.items[0].status, "done");
        assert_eq!(q.items[0].message.as_deref(), Some("128 BPM 8A"));
        assert!(q.active_track_ids.is_empty());
        let mut bad = ev(11, "failed");
        bad.error = Some("decode".into());
        patch_item(&mut q, &bad);
        assert_eq!(q.items[1].status, "failed");
        assert_eq!(q.failed_track_ids, vec![11]);
    }

    #[test]
    fn status_patch() {
        let mut s = AnalysisStatus { total_tracks: 100, analysed: 50, missing: 50, running_tracks: 2, ..Default::default() };
        patch_status_item(&mut s, &ev(1, "ok"));
        assert_eq!((s.analysed, s.missing, s.running_tracks), (51, 49, 1));
        assert!((s.coverage - 0.51).abs() < 1e-9);
        patch_status_item(&mut s, &ev(2, "failed"));
        assert_eq!((s.failed, s.missing), (1, 48));
    }

    #[test]
    fn analyzers_sorted() {
        let mut m = BTreeMap::new();
        m.insert("bc-rs-1".to_string(), 25);
        m.insert("essentia-import".to_string(), 75);
        let rows = analyzer_rows(&m);
        assert_eq!(rows[0].id, "essentia-import");
        assert!((rows[0].share - 0.75).abs() < 1e-9);
        assert_eq!(rows[1].label, "Native analyzer");
    }

    #[test]
    fn eta() {
        let s = [(0.0, 0), (10_000.0, 20)];
        assert_eq!(eta_seconds(&s, 100), Some(50.0));
        assert_eq!(eta_seconds(&[(0.0, 0), (1000.0, 5)], 100), None);
        assert_eq!(eta_seconds(&s, 0), Some(0.0));
        assert_eq!(format_eta(50.0), "50s");
        assert_eq!(format_eta(125.0), "2m 05s");
        assert_eq!(format_eta(7500.0), "2h 05m");
    }
}
