//! Download engine tests, driven by the fake bandcamp-dl (`fake_bandcamp_dl`).
//!
//! Ports the adapter/classifier-level cases of `test_download_engine.py`. Every
//! scenario corresponds to a defect verified by reading the installed 0.0.17
//! source; they are regression tests for behaviour the gen-2 prototype got
//! wrong, not hypotheticals.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use bc_bandcamp::download::bcdl::{
    BandcampDl, BcdlError, FLAT_TEMPLATE, RawRun, RunOptions, SweepConfig, is_downloadable, progress_regex,
    purge_tmp_files, snapshot_tree, sweep_before_attempt,
};
use bc_bandcamp::download::verify::classify;
use bc_bandcamp::download::{DownloadSpec, Downloader, OutcomeKind, Progress};
use tokio_util::sync::CancellationToken;

const FAKE: &str = env!("CARGO_BIN_EXE_fake_bandcamp_dl");
const URL: &str = "https://x.bandcamp.com/album/y";

fn dl(mode: &str) -> BandcampDl {
    BandcampDl::new(FAKE)
        .with_env("FAKE_BCDL_MODE", mode)
        .with_sweep(SweepConfig { probe: Duration::from_millis(50), ..Default::default() })
}

fn opts() -> RunOptions {
    RunOptions { timeout: Duration::from_secs(60), ..Default::default() }
}

async fn run_plain(d: &BandcampDl, dir: &Path) -> RawRun {
    d.run(URL, dir, &opts(), None, &CancellationToken::new()).await.expect("run")
}

fn find(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn rec(dir: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                rec(&p, suffix, out);
            } else if p.to_string_lossy().ends_with(suffix) {
                out.push(p);
            }
        }
    }
    rec(dir, suffix, &mut out);
    out.sort();
    out
}

fn age(path: &Path, seconds: u64) {
    let stamp = SystemTime::now() - Duration::from_secs(seconds);
    let f = std::fs::File::options().write(true).open(path).expect("open");
    f.set_modified(stamp).expect("set mtime");
}

/// A throwaway executable shell script standing in for bandcamp-dl.
fn script(dir: &Path, body: &str) -> String {
    let p = dir.join("fake-bcdl.sh");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    p.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// Progress parsing (defect 4: no newlines, \r-separated, width-padded)
// ---------------------------------------------------------------------------

#[test]
fn progress_regex_matches_the_real_format() {
    let line = format!("(3/12) [{}{}] :: Downloading: some-track", "=".repeat(20), " ".repeat(30));
    let m = progress_regex().captures(&line).expect("match");
    assert_eq!(&m["n"], "3");
    assert_eq!(&m["total"], "12");
    assert_eq!(&m["phase"], "Downloading");
    assert_eq!(&m["name"], "some-track");
}

/// The real tool never emits a newline, so a readline()-based reader would see
/// nothing until exit. This asserts we get events *while* it runs.
#[tokio::test]
async fn progress_is_reported_during_a_run() {
    let tmp = tempfile::tempdir().unwrap();
    let d = dl("success");
    let mut seen: Vec<Progress> = Vec::new();
    let mut cb = |p: Progress| seen.push(p);
    let run = d.run(URL, tmp.path(), &opts(), Some(&mut cb), &CancellationToken::new()).await.unwrap();

    assert_eq!(run.exit_code, 0);
    assert!(!seen.is_empty(), "no progress events were parsed");
    assert_eq!(seen.last().unwrap().track_total, 4);
    assert!(seen.iter().all(|p| (0.0..=1.0).contains(&p.fraction)));
    assert_eq!(run.finished_count, 4);
    assert_eq!(run.track_total, Some(4));
}

/// Events arrive while the process is still running, not in one burst at exit.
#[tokio::test]
async fn progress_streams_while_the_process_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let d = dl("slow").with_env("FAKE_BCDL_STEP_MS", "60");
    let mut stamps: Vec<Instant> = Vec::new();
    let mut cb = |_p: Progress| stamps.push(Instant::now());
    let started = Instant::now();
    let run = d.run(URL, tmp.path(), &opts(), Some(&mut cb), &CancellationToken::new()).await.unwrap();
    let finished = Instant::now();

    assert_eq!(run.finished_count, 4);
    let first = stamps.first().expect("events");
    assert!(first.duration_since(started) < finished.duration_since(started) / 2, "first event must come early");
    assert!(stamps.last().unwrap().duration_since(*first) >= Duration::from_millis(200), "events are spread out");
}

/// A 50-track album would otherwise emit thousands of events.
#[tokio::test]
async fn progress_is_coalesced_but_finished_always_gets_through() {
    let tmp = tempfile::tempdir().unwrap();
    let d = dl("success"); // 4 tracks x (Downloading, Encoding, Finished) in a few ms
    let mut seen: Vec<Progress> = Vec::new();
    let mut cb = |p: Progress| seen.push(p);
    d.run(URL, tmp.path(), &opts(), Some(&mut cb), &CancellationToken::new()).await.unwrap();

    assert!(seen.len() < 12, "{} events for 12 records", seen.len());
    assert_eq!(seen.iter().filter(|p| p.phase == "Finished").count(), 4);
    assert_eq!(seen.last().unwrap().fraction, 1.0);
}

// ---------------------------------------------------------------------------
// Per-run template override (flat single-folder jobs)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn template_override_replaces_the_configured_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let d = dl("success");
    let default_cmd = d.build_command(URL, tmp.path(), true, None).await.unwrap();
    let flat_cmd = d.build_command(URL, tmp.path(), true, Some(FLAT_TEMPLATE)).await.unwrap();

    let at = |c: &[String]| c[c.iter().position(|a| a == "--template").unwrap() + 1].clone();
    assert_eq!(at(&default_cmd), d.template);
    assert_eq!(at(&flat_cmd), FLAT_TEMPLATE);
    assert!(!FLAT_TEMPLATE.contains('/'), "a flat template must not create directories");
}

// ---------------------------------------------------------------------------
// Defect 1: the exit code is not a success signal
// ---------------------------------------------------------------------------

/// Exit 1 *after* every track downloaded. The old app called this a failure.
#[tokio::test]
async fn artless_album_crash_is_classified_as_success() {
    let tmp = tempfile::tempdir().unwrap();
    let before = snapshot_tree(tmp.path());
    let run = run_plain(&dl("no_art_crash"), tmp.path()).await;
    let after = snapshot_tree(tmp.path());

    assert_eq!(run.exit_code, 1, "the fake must reproduce the non-zero exit");
    let outcome = classify(&before, &after, &run, tmp.path());
    assert_eq!(outcome.kind, OutcomeKind::Ok);
    assert_eq!(outcome.new_files.len(), 4);
    assert!(outcome.detail.contains("art-less"), "{}", outcome.detail);
}

/// Exit 0 with no output. The old app called this a success.
#[tokio::test]
async fn silent_failure_with_exit_zero_is_classified_as_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let before = snapshot_tree(tmp.path());
    let run = run_plain(&dl("silent_fail"), tmp.path()).await;
    assert_eq!(run.exit_code, 0, "the fake must reproduce the zero exit");
    let outcome = classify(&before, &snapshot_tree(tmp.path()), &run, tmp.path());

    assert_eq!(outcome.kind, OutcomeKind::NoOutput);
    assert!(outcome.retryable);
    assert!(outcome.new_files.is_empty());
}

#[tokio::test]
async fn not_found_is_terminal_not_retried() {
    let tmp = tempfile::tempdir().unwrap();
    let before = snapshot_tree(tmp.path());
    let run = run_plain(&dl("not_found"), tmp.path()).await;
    let outcome = classify(&before, &snapshot_tree(tmp.path()), &run, tmp.path());

    assert_eq!(run.exit_code, 2);
    assert_eq!(outcome.kind, OutcomeKind::NotFound);
    assert!(!outcome.retryable, "a 404 never improves on retry");
}

#[tokio::test]
async fn network_error_is_retryable() {
    let tmp = tempfile::tempdir().unwrap();
    let before = snapshot_tree(tmp.path());
    let run = run_plain(&dl("network"), tmp.path()).await;
    let outcome = classify(&before, &snapshot_tree(tmp.path()), &run, tmp.path());

    assert_eq!(outcome.kind, OutcomeKind::Network);
    assert!(outcome.retryable);
    assert!(outcome.detail.contains("Max retries"));
}

/// The skip notices have to be tallied during the run, not read off
/// `stdout_tail` -- that buffer is capped, so on a long album the early ones are
/// long gone by the time the process exits.
#[tokio::test]
async fn skip_notices_are_counted_as_they_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let before = snapshot_tree(tmp.path());
    let run = run_plain(&dl("already_have"), tmp.path()).await;
    let outcome = classify(&before, &snapshot_tree(tmp.path()), &run, tmp.path());

    assert_eq!(run.skipped_existing, 4);
    assert_eq!(outcome.kind, OutcomeKind::AlreadyHave);
    assert!(outcome.ok());
    assert!(!outcome.retryable);
}

#[tokio::test]
async fn skip_notices_and_tail_survive_a_long_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let s = script(
        tmp.path(),
        r#"i=0; while [ $i -lt 300 ]; do echo "File: $i already exists and is complete, skipping.."; i=$((i+1)); done"#,
    );
    let run = run_plain(&BandcampDl::new(s), &tmp.path().join("dl")).await;
    assert_eq!(run.skipped_existing, 300, "counted per notice as they stream");
    assert_eq!(run.stdout_tail.len(), 200, "the tail is bounded");
    assert!(run.stdout_tail.last().unwrap().contains("299"));
}

#[tokio::test]
async fn partial_download_is_retryable() {
    let tmp = tempfile::tempdir().unwrap();
    let before = snapshot_tree(tmp.path());
    let run = run_plain(&dl("partial"), tmp.path()).await;
    let outcome = classify(&before, &snapshot_tree(tmp.path()), &run, tmp.path());

    assert_eq!(outcome.kind, OutcomeKind::Partial);
    assert!(outcome.retryable);
    assert_eq!(outcome.new_files.len(), 2);
}

// ---------------------------------------------------------------------------
// -f: a release only part of which streams
// ---------------------------------------------------------------------------

/// The skip notice is printed with a newline and no trailing `\r`, so it only
/// reaches the reader in the tail flush at EOF. Losing it there is how a release
/// nothing would ever download from looked like a plain empty run.
#[tokio::test]
async fn full_album_flag_skip_is_reported_not_swallowed() {
    let tmp = tempfile::tempdir().unwrap();
    let before = snapshot_tree(tmp.path());
    let run = run_plain(&dl("part_stream"), tmp.path()).await;
    let outcome = classify(&before, &snapshot_tree(tmp.path()), &run, tmp.path());

    assert_eq!(run.exit_code, 0, "the fake must reproduce the zero exit");
    assert!(run.full_album_skipped);
    assert_eq!(outcome.kind, OutcomeKind::NoOutput);
}

#[tokio::test]
async fn dropping_the_flag_fetches_what_does_stream() {
    let tmp = tempfile::tempdir().unwrap();
    let before = snapshot_tree(tmp.path());
    let o = RunOptions { full_album: false, ..opts() };
    let run = dl("part_stream").run(URL, tmp.path(), &o, None, &CancellationToken::new()).await.unwrap();
    let outcome = classify(&before, &snapshot_tree(tmp.path()), &run, tmp.path());

    assert!(!run.full_album_skipped);
    assert_eq!(outcome.kind, OutcomeKind::Ok);
    assert_eq!(outcome.new_files.len(), 1);
}

#[tokio::test]
async fn full_album_is_the_default_and_droppable() {
    let tmp = tempfile::tempdir().unwrap();
    let d = dl("success");
    assert!(d.build_command(URL, tmp.path(), true, None).await.unwrap().contains(&"-f".to_string()));
    let no_f = d.build_command(URL, tmp.path(), false, None).await.unwrap();
    assert!(!no_f.contains(&"-f".to_string()));
    // -r is not part of the bargain: art still embeds either way.
    assert!(no_f.contains(&"-r".to_string()));
    assert!(RunOptions::default().full_album);
}

/// The adapter's own `Downloader::download` retries once without `-f` when the
/// run reports "Full album not available", and classifies the second run.
#[tokio::test]
async fn downloader_retries_without_the_flag_when_the_album_is_part_streamable() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("argv.log");
    let base = tmp.path().join("dl");
    let d = dl("part_stream").with_env("FAKE_BCDL_ARGV_LOG", log.to_string_lossy());
    let spec = DownloadSpec::new(URL, &base);
    let mut cb = |_p: Progress| {};
    let outcome = d.download(&spec, &mut cb, &CancellationToken::new()).await;

    assert_eq!(outcome.kind, OutcomeKind::Ok, "{}", outcome.detail);
    assert_eq!(outcome.new_files.len(), 1);
    let lines: Vec<String> = std::fs::read_to_string(&log).unwrap().lines().map(String::from).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].contains(" -f "), "first attempt uses -f: {}", lines[0]);
    assert!(!lines[1].contains(" -f "), "second attempt drops it: {}", lines[1]);
}

#[tokio::test]
async fn downloader_does_not_retry_when_the_flag_was_already_off() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("argv.log");
    let d = dl("silent_fail").with_env("FAKE_BCDL_ARGV_LOG", log.to_string_lossy());
    let mut spec = DownloadSpec::new(URL, tmp.path().join("dl"));
    spec.full_album = false;
    let mut cb = |_p: Progress| {};
    let outcome = d.download(&spec, &mut cb, &CancellationToken::new()).await;
    assert_eq!(outcome.kind, OutcomeKind::NoOutput);
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
}

#[tokio::test]
async fn downloader_classifies_by_filesystem_for_every_mode() {
    let cases = [
        ("success", OutcomeKind::Ok),
        ("no_art_crash", OutcomeKind::Ok),
        ("with_art", OutcomeKind::Ok),
        ("silent_fail", OutcomeKind::NoOutput),
        ("not_found", OutcomeKind::NotFound),
        ("network", OutcomeKind::Network),
        ("already_have", OutcomeKind::AlreadyHave),
        ("partial", OutcomeKind::Partial),
        ("part_stream", OutcomeKind::Ok),
    ];
    for (mode, want) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let spec = DownloadSpec::new(URL, tmp.path());
        let mut cb = |_p: Progress| {};
        let outcome = dl(mode).download(&spec, &mut cb, &CancellationToken::new()).await;
        assert_eq!(outcome.kind, want, "mode {mode}: {}", outcome.detail);
    }
}

#[tokio::test]
async fn downloader_times_out_with_a_retryable_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let mut spec = DownloadSpec::new(URL, tmp.path());
    spec.timeout = Duration::from_secs(1);
    let mut cb = |_p: Progress| {};
    let outcome = dl("hang").download(&spec, &mut cb, &CancellationToken::new()).await;
    assert_eq!(outcome.kind, OutcomeKind::Timeout);
    assert!(outcome.retryable);
}

#[tokio::test]
async fn downloader_reports_a_missing_binary_as_a_crash_not_a_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let d = BandcampDl::new("/nonexistent/bandcamp-dl");
    let mut cb = |_p: Progress| {};
    let outcome = d.download(&DownloadSpec::new(URL, tmp.path()), &mut cb, &CancellationToken::new()).await;
    assert_eq!(outcome.kind, OutcomeKind::Crash);
    assert!(!outcome.retryable);
}

// ---------------------------------------------------------------------------
// Defect 2: a stale .tmp silently truncates the track
// ---------------------------------------------------------------------------

#[test]
fn purge_removes_tmp_files_recursively() {
    let tmp = tempfile::tempdir().unwrap();
    let nested = tmp.path().join("Artist").join("Album");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("01 - a.mp3.tmp"), b"partial").unwrap();
    std::fs::write(nested.join("02 - b.mp3"), b"complete").unwrap();

    assert_eq!(purge_tmp_files(tmp.path(), None), 1);
    assert!(!nested.join("01 - a.mp3.tmp").exists());
    assert!(nested.join("02 - b.mp3").exists(), "complete files must be untouched");
}

/// The startup sweep must not sabotage a download running elsewhere.
#[test]
fn purge_respects_an_age_guard() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("fresh.mp3.tmp"), b"in flight").unwrap();
    assert_eq!(purge_tmp_files(tmp.path(), Some(Duration::from_secs(300))), 0);
    assert_eq!(purge_tmp_files(tmp.path(), None), 1);
}

/// The critical fix.
///
/// bandcamp-dl skips downloading when `<name>.mp3.tmp` exists and renames the
/// partial straight to `.mp3`. Without the purge, every retry after a crash
/// produces a corrupt track that looks complete.
#[tokio::test]
async fn run_purges_stale_tmp_before_starting() {
    let tmp = tempfile::tempdir().unwrap();
    run_plain(&dl("leave_tmp"), tmp.path()).await;
    assert!(!find(tmp.path(), ".tmp").is_empty(), "the fake should have left a .tmp behind");

    run_plain(&dl("success"), tmp.path()).await;

    assert!(find(tmp.path(), ".tmp").is_empty(), "stale .tmp must be purged before the retry");
    let mp3 = find(tmp.path(), ".mp3");
    assert_eq!(mp3.len(), 4);
    for p in mp3 {
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 2052, "{} was truncated", p.display());
    }
}

/// Proof the fake reproduces the real bug, i.e. that the test above is
/// meaningful: run it directly (no purge) after a kill and the first track is a
/// 7-byte "complete" file.
#[test]
fn without_the_purge_a_stale_tmp_becomes_a_truncated_track() {
    let tmp = tempfile::tempdir().unwrap();
    let run = |mode: &str| {
        std::process::Command::new(FAKE)
            .args(["--base-dir", tmp.path().to_str().unwrap(), "-r", "-f", URL])
            .env("FAKE_BCDL_MODE", mode)
            .output()
            .unwrap()
    };
    run("leave_tmp");
    run("success");
    let first = tmp.path().join("Somatic/Grid Failure/01 - opening-drift.mp3");
    assert_eq!(std::fs::metadata(first).unwrap().len(), 7, "the stale tmp was renamed without fetching");
    let second = tmp.path().join("Somatic/Grid Failure/02 - grid-failure.mp3");
    assert_eq!(std::fs::metadata(second).unwrap().len(), 2052);
}

/// Every item downloads into the same root, so the pre-attempt sweep sees its
/// siblings' partial files. Deleting one leaves bandcamp-dl writing to an
/// unlinked inode until its closing rename raises.
#[test]
fn sweep_spares_a_tmp_that_is_still_growing() {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let tmp = tempfile::tempdir().unwrap();
    let live = tmp.path().join("Live Artist/Album/01 - track.mp3.tmp");
    let abandoned = tmp.path().join("Dead Artist/Album/01 - track.mp3.tmp");
    for p in [&live, &abandoned] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"partial").unwrap();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let (stop2, live2) = (stop.clone(), live.clone());
    let writer = std::thread::spawn(move || {
        let mut f = std::fs::OpenOptions::new().append(true).open(live2).unwrap();
        while !stop2.load(Ordering::Relaxed) {
            f.write_all(b"more").unwrap();
            f.flush().unwrap();
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let report = sweep_before_attempt(tmp.path(), SweepConfig { probe: Duration::from_millis(300), ..Default::default() });
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();

    assert_eq!(report.tmp_removed, 1);
    assert!(live.exists(), "a partial another download is writing must survive");
    assert!(!abandoned.exists(), "an abandoned partial must still be purged");
}

// ---------------------------------------------------------------------------
// Defect 3: a cover.jpg already in the folder kills the whole album
// ---------------------------------------------------------------------------

/// With -r, bandcamp-dl only assigns self.album_art when cover.jpg is *absent*,
/// then reads it for every track -- so a leftover cover crashes the album on
/// track 1, forever.
#[test]
fn sweep_clears_a_cover_left_by_a_failed_download() {
    let tmp = tempfile::tempdir().unwrap();
    let orphan = tmp.path().join("Artist/Never Finished/cover.jpg");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"\xff\xd8\xff").unwrap();
    age(&orphan, 3600);

    let report = sweep_before_attempt(tmp.path(), SweepConfig::default());
    assert_eq!(report.covers_removed, 1);
    assert!(!orphan.exists());
}

/// The whole library sits under this root. Art beside audio is the user's.
#[test]
fn sweep_keeps_cover_art_that_belongs_to_real_music() {
    let tmp = tempfile::tempdir().unwrap();
    let album = tmp.path().join("Artist/Real Album");
    std::fs::create_dir_all(&album).unwrap();
    let cover = album.join("cover.jpg");
    std::fs::write(&cover, b"\xff\xd8\xff").unwrap();
    std::fs::write(album.join("01 - track.mp3"), [0xff, 0xfb, 0, 0]).unwrap();
    age(&cover, 3600);

    let report = sweep_before_attempt(tmp.path(), SweepConfig::default());
    assert_eq!(report.covers_removed, 0);
    assert!(cover.exists());
}

/// A concurrent download writes its cover before its first track, so for a
/// moment its folder looks exactly like an abandoned one.
#[test]
fn sweep_leaves_a_cover_a_sibling_run_just_wrote() {
    let tmp = tempfile::tempdir().unwrap();
    let fresh = tmp.path().join("Artist/In Flight/cover.jpg");
    std::fs::create_dir_all(fresh.parent().unwrap()).unwrap();
    std::fs::write(&fresh, b"\xff\xd8\xff").unwrap();

    let report = sweep_before_attempt(tmp.path(), SweepConfig { cover_min_age: Duration::from_secs(300), ..Default::default() });
    assert_eq!(report.covers_removed, 0);
    assert!(fresh.exists());
}

/// End to end: a failed run that left a cover behind crashes the retry on track
/// 1 -- unless the adapter sweeps first.
#[tokio::test]
async fn an_orphaned_cover_no_longer_kills_the_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let args = |dir: &Path| {
        std::process::Command::new(FAKE)
            .args(["--base-dir", dir.to_str().unwrap(), "-r", "-f", URL])
            .env("FAKE_BCDL_MODE", "cover_crash")
            .output()
            .unwrap()
    };
    // The prototype's leftover: a cover and no music.
    let folder = tmp.path().join("Somatic/Grid Failure");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("cover.jpg"), b"\xff\xd8\xff").unwrap();
    age(&folder.join("cover.jpg"), 3600);

    // Unswept: the real defect, exit 1 and nothing produced.
    let crashed = args(tmp.path());
    assert_eq!(crashed.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&crashed.stderr).contains("album_art"));
    assert!(find(tmp.path(), ".mp3").is_empty());
    assert!(!find(tmp.path(), ".tmp").is_empty(), "the crash leaves track 1's .tmp behind");

    // Through the adapter: tmp purged, orphan cover swept, album downloads, and
    // the cover it fetched is removed again (embedded).
    let d = dl("cover_crash");
    age(&find(tmp.path(), ".tmp")[0], 3600);
    let before = snapshot_tree(tmp.path());
    let run = run_plain(&d, tmp.path()).await;
    let outcome = classify(&before, &snapshot_tree(tmp.path()), &run, tmp.path());
    assert_eq!(outcome.kind, OutcomeKind::Ok, "{}", outcome.detail);
    assert_eq!(find(tmp.path(), ".mp3").len(), 4);
    assert!(find(tmp.path(), "cover.jpg").is_empty());
}

// ---------------------------------------------------------------------------
// Timeout / cancel / process-group kill
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_kills_the_process_group() {
    let tmp = tempfile::tempdir().unwrap();
    let o = RunOptions { timeout: Duration::from_secs(2), ..Default::default() };
    let started = Instant::now();
    let run = dl("hang").run(URL, tmp.path(), &o, None, &CancellationToken::new()).await.unwrap();

    assert!(run.timed_out);
    assert!(started.elapsed() < Duration::from_secs(10));
    let outcome = classify(&Default::default(), &snapshot_tree(tmp.path()), &run, tmp.path());
    assert_eq!(outcome.kind, OutcomeKind::Timeout);
    assert!(outcome.retryable);
}

fn pid_alive(pid: i32) -> bool {
    // A zombie still answers kill(0); read the state instead.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z'),
        Err(_) => false,
    }
}

/// The *group* dies, not just the leader: a child holding no pipe and ignoring
/// the leader's death would otherwise be orphaned.
#[tokio::test]
async fn timeout_kills_children_in_the_group_too() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("child.pid");
    let d = dl("hang_child").with_env("FAKE_BCDL_PIDFILE", pidfile.to_string_lossy());
    let o = RunOptions { timeout: Duration::from_secs(2), ..Default::default() };
    let run = d.run(URL, tmp.path(), &o, None, &CancellationToken::new()).await.unwrap();
    assert!(run.timed_out);

    let pid: i32 = std::fs::read_to_string(&pidfile).expect("child pid written").trim().parse().unwrap();
    // Give the kernel a moment to deliver and reap.
    let deadline = Instant::now() + Duration::from_secs(3);
    while pid_alive(pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!pid_alive(pid), "the grandchild survived the group kill");
}

/// SIGTERM first; a process that ignores it is SIGKILLed after 5 s.
#[tokio::test]
async fn a_process_ignoring_sigterm_is_killed_after_the_grace_period() {
    let tmp = tempfile::tempdir().unwrap();
    let o = RunOptions { timeout: Duration::from_secs(1), ..Default::default() };
    let started = Instant::now();
    let run = dl("hang_ignore_term").run(URL, tmp.path(), &o, None, &CancellationToken::new()).await.unwrap();
    let took = started.elapsed();

    assert!(run.timed_out);
    assert!(took >= Duration::from_secs(5), "escalated too early: {took:?}");
    assert!(took < Duration::from_secs(12), "escalated too late: {took:?}");
    assert_eq!(run.exit_code, -9, "killed by SIGKILL");
}

#[tokio::test]
async fn cancel_kills_the_run_promptly() {
    let tmp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        c2.cancel();
    });
    let started = Instant::now();
    let run = dl("hang").run(URL, tmp.path(), &opts(), None, &cancel).await.unwrap();

    assert!(run.cancelled);
    assert!(!run.timed_out);
    assert!(started.elapsed() < Duration::from_secs(8));

    // Through the trait the cancel is an outcome, never a panic.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut cb = |_p: Progress| {};
    let outcome = dl("hang").download(&DownloadSpec::new(URL, tmp.path()), &mut cb, &cancel).await;
    assert_eq!(outcome.kind, OutcomeKind::Crash);
    assert_eq!(outcome.detail, "Cancelled.");
}

// ---------------------------------------------------------------------------
// Command construction, flag probing, environment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn command_uses_hierarchical_template_and_probed_flags() {
    let tmp = tempfile::tempdir().unwrap();
    let cmd = dl("success").build_command(URL, tmp.path(), true, None).await.unwrap();

    let i = cmd.iter().position(|a| a == "--template").unwrap();
    // A flat template has no album component, so two albums sharing a track title
    // collide -- and with overwrite off the second is silently skipped.
    assert!(cmd[i + 1].contains("%{album}"));
    assert_eq!(cmd[cmd.iter().position(|a| a == "--base-dir").unwrap() + 1], tmp.path().to_string_lossy());
    assert!(cmd.contains(&"-f".to_string()) && cmd.contains(&"-r".to_string()));
    assert!(cmd.contains(&"--no-confirm".to_string()) && cmd.contains(&"--embed-genres".to_string()));
    assert_eq!(cmd.last().unwrap(), URL);
}

/// Hardcoding --embed-genres risks passing a flag the binary rejects.
#[tokio::test]
async fn flags_are_probed_from_the_installed_binary() {
    let d = dl("success");
    let flags = d.supported_flags().await;
    assert!(flags.contains("--base-dir"));
    assert!(flags.contains("--template"));
    assert_eq!(d.version().await.as_deref(), Some("bandcamp-dl 0.0.17-fake"));
}

#[tokio::test]
async fn unsupported_flags_are_not_passed() {
    let tmp = tempfile::tempdir().unwrap();
    let d = dl("success").with_env("FAKE_BCDL_HIDE_FLAGS", "--no-confirm,--embed-genres");
    let cmd = d.build_command(URL, tmp.path(), true, None).await.unwrap();
    assert!(!cmd.contains(&"--no-confirm".to_string()));
    assert!(!cmd.contains(&"--embed-genres".to_string()));
    assert!(cmd.contains(&"-r".to_string()));
}

#[tokio::test]
async fn an_unresolvable_binary_probes_to_no_flags() {
    let d = BandcampDl::new("definitely-not-a-binary-xyz");
    assert!(d.supported_flags().await.is_empty());
    assert_eq!(d.version().await, None);
}

#[tokio::test]
async fn the_process_gets_the_pinned_environment_and_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("argv.log");
    let base = tmp.path().join("base");
    let d = dl("success").with_env("FAKE_BCDL_ARGV_LOG", log.to_string_lossy());
    run_plain_in(&d, &base).await;
    let line = std::fs::read_to_string(&log).unwrap();
    assert!(line.contains("PYTHONUNBUFFERED=1"), "{line}");
    assert!(line.contains("COLUMNS=200"), "{line}");
    assert!(line.contains("LC_ALL=C.UTF-8"), "{line}");
    assert!(line.contains(&format!("CWD={}", base.canonicalize().unwrap().display())), "{line}");
    assert!(line.contains("-r -f"), "{line}");
}

async fn run_plain_in(d: &BandcampDl, dir: &Path) -> RawRun {
    run_plain(d, dir).await
}

/// Defect 5: a URL bandcamp-dl cannot act on is skipped in silence, so it must
/// never become argv.
#[tokio::test]
async fn a_band_page_never_becomes_argv() {
    let tmp = tempfile::tempdir().unwrap();
    let d = dl("success");
    for rejected in [
        "https://lemos.bandcamp.com/music",
        "https://lemos.bandcamp.com",
        "https://lemos.bandcamp.com/artists",
        "https://bandcamp.com/someone",
    ] {
        let err = d.build_command(rejected, tmp.path(), true, None).await.unwrap_err();
        assert!(matches!(err, BcdlError::NotDownloadable(_)));
        assert!(err.to_string().contains("not an album or track page"), "{err}");
        assert!(!is_downloadable(rejected));
        // And the run itself refuses before touching the disk or spawning.
        assert!(d.run(rejected, &tmp.path().join("never"), &opts(), None, &CancellationToken::new()).await.is_err());
        assert!(!tmp.path().join("never").exists());
    }
    for accepted in [
        "https://lemos.bandcamp.com/album/kontrol-lemos-06",
        "https://lemos.bandcamp.com/track/one",
        "https://music.example.com/album/on-a-custom-domain",
    ] {
        let cmd = d.build_command(accepted, tmp.path(), true, None).await.unwrap();
        assert_eq!(cmd.last().unwrap(), accepted, "the URL is the last argument");
        assert!(is_downloadable(accepted));
    }
}

#[tokio::test]
async fn downloader_refuses_a_band_page_without_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cb = |_p: Progress| {};
    let outcome = dl("success")
        .download(&DownloadSpec::new("https://lemos.bandcamp.com/music", tmp.path()), &mut cb, &CancellationToken::new())
        .await;
    assert_eq!(outcome.kind, OutcomeKind::NoOutput);
    assert!(!outcome.retryable);
}

// ---------------------------------------------------------------------------
// Stream reading: chunking and the last record at EOF
// ---------------------------------------------------------------------------

/// Progress lines are `\r`-prefixed, not `\r`-terminated, so the final record has
/// no separator after it. Dropping it loses the last "Finished:" line, which makes
/// a complete album look one track short.
#[tokio::test]
async fn the_last_record_without_a_terminator_is_parsed_at_eof() {
    let tmp = tempfile::tempdir().unwrap();
    let s = script(
        tmp.path(),
        r#"printf '\r(1/2) [====                ] :: Downloading: a\r(1/2) [=====] :: Finished: a\r(2/2) [=====] :: Finished: b'"#,
    );
    let run = run_plain(&BandcampDl::new(s), &tmp.path().join("dl")).await;
    assert_eq!(run.track_total, Some(2));
    assert_eq!(run.finished_count, 2, "the unterminated last record counts");
}

/// A record longer than the 4 KiB read size straddles chunks and is still one record.
#[tokio::test]
async fn a_record_spanning_read_chunks_is_reassembled() {
    let tmp = tempfile::tempdir().unwrap();
    let s = script(
        tmp.path(),
        r#"printf '\r(1/3) [=====] :: Downloading: a'; head -c 9000 /dev/zero | tr '\0' ' '; printf '\r(3/3) [=====] :: Finished: z'; head -c 5000 /dev/zero | tr '\0' ' '"#,
    );
    let run = run_plain(&BandcampDl::new(s), &tmp.path().join("dl")).await;
    assert_eq!(run.track_total, Some(3));
    assert_eq!(run.finished_count, 3);
}

#[tokio::test]
async fn multibyte_names_split_across_chunks_are_not_mangled() {
    let tmp = tempfile::tempdir().unwrap();
    // 4095 bytes of padding then a 2-byte char: the char straddles the 4096 boundary.
    let s = script(
        tmp.path(),
        r#"printf '\r(1/1) [=====] :: Downloading: '; head -c 4060 /dev/zero | tr '\0' 'a'; printf 'ü\r(1/1) [=====] :: Finished: ü'"#,
    );
    let mut seen: Vec<Progress> = Vec::new();
    let mut cb = |p: Progress| seen.push(p);
    BandcampDl::new(s)
        .run(URL, &tmp.path().join("dl"), &opts(), Some(&mut cb), &CancellationToken::new())
        .await
        .unwrap();
    assert!(seen.iter().all(|p| !p.track_name.contains('\u{fffd}')), "{seen:?}");
    assert_eq!(seen.last().unwrap().track_name, "ü");
}

#[tokio::test]
async fn stdout_tail_is_bounded_at_200_records() {
    let tmp = tempfile::tempdir().unwrap();
    let run = run_plain(&dl("big_progress"), tmp.path()).await;
    assert_eq!(run.stdout_tail.len(), 200);
    assert_eq!(run.finished_count, 100);
    assert!(run.stdout_tail.last().unwrap().contains("Finished: track-100"));
}

// ---------------------------------------------------------------------------
// Classification straight from RawRun (no process)
// ---------------------------------------------------------------------------

fn make_files(dir: &Path, n: usize) {
    let album = dir.join("Artist/Album");
    std::fs::create_dir_all(&album).unwrap();
    for i in 1..=n {
        std::fs::write(album.join(format!("0{i} - track.mp3")), [0xff, 0xfb, 0, 0]).unwrap();
    }
}

/// bandcamp-dl never passes --overwrite, so a second run against an album you
/// already have skips every file and produces nothing new. Reporting that as
/// `no_output` made re-queueing a downloaded album look broken.
#[test]
fn already_downloaded_album_is_a_success_not_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    make_files(tmp.path(), 5);
    let tree = snapshot_tree(tmp.path());
    let run = RawRun { skipped_existing: 5, ..RawRun::new(0, false, Some(5), 0) };

    // `before` equals `after`: nothing changed on disk.
    let outcome = classify(&tree, &tree, &run, tmp.path());
    assert_eq!(outcome.kind, OutcomeKind::AlreadyHave);
    assert!(outcome.ok(), "must not be reported as a failure");
    assert!(!outcome.retryable, "retrying would skip the files again");
    assert!(outcome.new_files.is_empty(), "nothing to ingest");
    assert!(outcome.detail.to_lowercase().contains("already"));
}

/// Audio sitting in the target is not evidence about *this* album: every item
/// downloads into the shared root, so counting audio there once reported
/// "Already downloaded: 8981 file(s)" for an album that had never been touched.
#[test]
fn already_have_is_read_from_the_downloader_not_the_directory() {
    let tmp = tempfile::tempdir().unwrap();
    make_files(tmp.path(), 8);
    let tree = snapshot_tree(tmp.path());
    let run = RawRun::new(0, false, Some(4), 0);

    let outcome = classify(&tree, &tree, &run, tmp.path());
    assert_eq!(outcome.kind, OutcomeKind::NoOutput);
    assert!(!outcome.ok());
    assert!(outcome.retryable, "a silent failure has to be retried");
}

/// The already-have path must not mask a real no-output failure.
#[test]
fn genuinely_empty_target_is_still_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let empty = snapshot_tree(tmp.path());
    let outcome = classify(&empty, &empty, &RawRun::new(0, false, None, 0), tmp.path());
    assert_eq!(outcome.kind, OutcomeKind::NoOutput);
    assert!(!outcome.ok());
    assert!(outcome.retryable);
}

/// An error.log or stray cover art is not a downloaded album.
#[test]
fn non_audio_leftovers_do_not_count_as_already_have() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("error.log"), "previous failure").unwrap();
    std::fs::write(tmp.path().join("cover.jpg"), b"\xff\xd8\xff").unwrap();
    let tree = snapshot_tree(tmp.path());
    let outcome = classify(&tree, &tree, &RawRun::new(0, false, None, 0), tmp.path());
    assert_eq!(outcome.kind, OutcomeKind::NoOutput);
}

/// A run that downloads nothing still knows how long the record is: bandcamp-dl
/// prints one "already exists" notice per track, so the notice count is the
/// record's length (what the fill button needs to learn).
#[test]
fn a_fully_skipped_album_reports_its_length_from_the_skip_notices() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = snapshot_tree(tmp.path());
    let run = RawRun { skipped_existing: 8, ..RawRun::new(0, false, None, 0) };
    let outcome = classify(&tree, &tree, &run, tmp.path());

    assert_eq!(outcome.kind, OutcomeKind::AlreadyHave);
    assert_eq!(outcome.tracks_expected, Some(8), "the skip notices are the only count this run produces");
}

/// Bandcamp's own "(n/12)" wins when the run managed to print one.
#[test]
fn a_parsed_track_total_still_outranks_the_skip_count() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = snapshot_tree(tmp.path());
    let run = RawRun { skipped_existing: 5, ..RawRun::new(0, false, Some(12), 0) };
    let outcome = classify(&tree, &tree, &run, tmp.path());

    assert_eq!(outcome.kind, OutcomeKind::AlreadyHave);
    assert_eq!(outcome.tracks_expected, Some(12));
}

#[test]
fn exit_code_one_with_a_complete_track_set_is_ok_and_exit_zero_with_no_output_is_not() {
    let tmp = tempfile::tempdir().unwrap();
    let before = snapshot_tree(tmp.path());
    make_files(tmp.path(), 3);
    let after = snapshot_tree(tmp.path());

    let crashed = RawRun::new(1, false, Some(3), 3);
    assert_eq!(classify(&before, &after, &crashed, tmp.path()).kind, OutcomeKind::Ok);
    let silent = RawRun::new(0, false, Some(3), 0);
    assert_eq!(classify(&before, &before, &silent, tmp.path()).kind, OutcomeKind::NoOutput);
    // Exit 2 with new audio is not "not found" (the files landed).
    let odd = RawRun::new(2, false, Some(3), 3);
    assert_eq!(classify(&before, &after, &odd, tmp.path()).kind, OutcomeKind::Ok);
    // Fewer finished than expected: partial.
    let short = RawRun::new(0, false, Some(4), 3);
    assert_eq!(classify(&before, &after, &short, tmp.path()).kind, OutcomeKind::Partial);
}

#[test]
fn a_stale_tmp_beside_new_audio_is_partial() {
    let tmp = tempfile::tempdir().unwrap();
    let before = snapshot_tree(tmp.path());
    make_files(tmp.path(), 2);
    std::fs::write(tmp.path().join("Artist/Album/03 - x.mp3.tmp"), b"p").unwrap();
    let after = snapshot_tree(tmp.path());
    let outcome = classify(&before, &after, &RawRun::new(0, false, None, 0), tmp.path());
    assert_eq!(outcome.kind, OutcomeKind::Partial);
    assert!(outcome.detail.contains("1 partial"));
}
