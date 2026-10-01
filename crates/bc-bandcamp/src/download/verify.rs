//! Decide whether a bandcamp-dl run succeeded.
//!
//! The gen-2 prototype used `if returncode != 0`. That is wrong in both
//! directions on the installed 0.0.17:
//!
//! * Failures exit **0**, because `main()` discards `download_album()`'s `False`
//!   return -- so real failures were reported as successes.
//! * An album with **no art** raises `AttributeError` on the closing
//!   `os.remove(self.album_art)` *after* every track downloaded correctly,
//!   exiting non-zero -- so real successes were reported as failures. With `-r`
//!   (embed art) set, this happens on every art-less release.
//!
//! Success is therefore decided by looking at the filesystem.

use std::path::Path;

use super::bcdl::{RawRun, SnapshotMap, is_audio_name};
use super::{Outcome, OutcomeKind};

/// Compare the directory before and after, and read the run's own signals.
pub fn classify(before: &SnapshotMap, after: &SnapshotMap, run: &RawRun, target_dir: &Path) -> Outcome {
    let mut new_audio: Vec<_> = after
        .iter()
        .filter(|(rel, meta)| before.get(*rel) != Some(*meta))
        .filter(|(rel, _)| is_audio_name(rel))
        .map(|(rel, _)| target_dir.join(rel))
        .collect();
    new_audio.sort();
    let stale_tmp: Vec<&String> = after.keys().filter(|rel| rel.ends_with(".tmp")).collect();

    let expected = run.track_total;
    let finished = run.finished_count;

    // bandcamp-dl exits 2 from bandcamp.py::parse() for a 404 / missing album.
    // Nothing about that improves on a retry.
    if run.exit_code == 2 && new_audio.is_empty() {
        return Outcome {
            kind: OutcomeKind::NotFound,
            new_files: Vec::new(),
            tracks_expected: expected,
            availability: None,
            tracks_finished: finished,
            retryable: false,
            detail: "Album or track not found on Bandcamp (HTTP 404).".into(),
        };
    }

    if run.timed_out {
        return Outcome {
            kind: OutcomeKind::Timeout,
            new_files: new_audio,
            tracks_expected: expected,
            availability: None,
            tracks_finished: finished,
            retryable: true,
            detail: format!("Timed out after {:.0}s.", run.duration_s),
        };
    }

    if new_audio.is_empty() {
        if run.has_network_error() {
            let stderr = run.stderr_text();
            return Outcome {
                kind: OutcomeKind::Network,
                new_files: Vec::new(),
                tracks_expected: expected,
                availability: None,
                tracks_finished: finished,
                retryable: true,
                detail: if stderr.is_empty() { "Network error.".into() } else { truncate(&stderr, 500) },
            };
        }

        // bandcamp-dl never sets --overwrite, so re-running against an album you
        // already have skips every file and produces nothing new. That is
        // success, not failure.
        //
        // The signal is bandcamp-dl's own "already exists and is complete"
        // notices, not a scan of `after`: with every item downloading into one
        // shared root, counting audio there answers for the whole library, not
        // for this release (it once reported "Already downloaded: 8981 file(s)"
        // for an album it had never fetched, dressing a silent failure up as a
        // success).
        if run.skipped_existing > 0 {
            // The skip notices are the only count this run produces: no track
            // is fetched, so no "(3/12)" progress line is ever printed and
            // `track_total` stays None. bandcamp-dl walked every track on the
            // page to skip it, so one notice per track *is* the record's
            // length -- which is what a fill of an album already on disk exists
            // to learn.
            return Outcome {
                kind: OutcomeKind::AlreadyHave,
                new_files: Vec::new(),
                tracks_expected: Some(expected.filter(|e| *e > 0).unwrap_or(run.skipped_existing)),
                availability: None,
                tracks_finished: if finished != 0 { finished } else { run.skipped_existing },
                retryable: false,
                detail: format!(
                    "Already downloaded: bandcamp-dl skipped {} file(s) that were already present and complete.",
                    run.skipped_existing
                ),
            };
        }

        let stderr = run.stderr_text();
        return Outcome {
            kind: OutcomeKind::NoOutput,
            new_files: Vec::new(),
            tracks_expected: expected,
            availability: None,
            tracks_finished: finished,
            retryable: true,
            detail: if stderr.is_empty() { "No audio files were produced.".into() } else { truncate(&stderr, 500) },
        };
    }

    if !stale_tmp.is_empty() || expected.is_some_and(|e| finished < e) {
        // Some tracks landed. Retryable: bandcamp-dl skips files that already
        // exist (overwrite is off), and the purge before the next attempt
        // prevents the truncation bug, so a retry fills only the gaps.
        return Outcome {
            kind: OutcomeKind::Partial,
            new_files: new_audio,
            tracks_expected: expected,
            availability: None,
            tracks_finished: finished,
            retryable: true,
            detail: match expected {
                Some(e) if e > 0 => format!("Downloaded {finished}/{e} tracks."),
                _ => format!("Incomplete: {} partial file(s) left behind.", stale_tmp.len()),
            },
        };
    }

    // Deliberately reached even when exit_code != 0 -- this is the art-less
    // album crash, which the old app logged as a failure despite a complete
    // download.
    let mut detail = format!("Downloaded {} file(s).", new_audio.len());
    if run.exit_code != 0 {
        detail.push_str(&format!(
            " (bandcamp-dl exited {}; all expected tracks are present, which is the known art-less-album crash.)",
            run.exit_code
        ));
    }
    let n = new_audio.len() as u32;
    Outcome {
        kind: OutcomeKind::Ok,
        new_files: new_audio,
        tracks_expected: expected,
        availability: None,
        tracks_finished: if finished != 0 { finished } else { n },
        retryable: false,
        detail,
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}
