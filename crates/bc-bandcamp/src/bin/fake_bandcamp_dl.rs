//! TEST FIXTURE ONLY -- a stand-in for the `bandcamp-dl` CLI that reproduces its
//! real quirks. Never shipped; integration tests locate it through
//! `env!("CARGO_BIN_EXE_fake_bandcamp_dl")`.
//!
//! Ported from `web/backend/tests/fixtures/fake_bandcamp_dl.py`. Behaviour is
//! chosen by `FAKE_BCDL_MODE`:
//!
//! * `success`        full download, exit 0
//! * `no_art_crash`   full download, then an AttributeError traceback and exit 1.
//!   The real defect: `self.album_art` is never initialised, so an art-less album
//!   crashes on the closing `os.remove(self.album_art)` *after* every track
//!   downloaded fine. An exit-code check reports this success as a failure.
//! * `silent_fail`    no files produced, but **exit 0** (`main()` discards
//!   `download_album()`'s `False`). An exit-code check reports this failure as a
//!   success.
//! * `not_found`      exit 2, the genuine 404 path.
//! * `network`        a connection error on stderr, exit 0, nothing written.
//! * `already_have`   one "already exists and is complete, skipping.." per track,
//!   nothing written -- what a re-run against an album you already have looks like.
//! * `partial`        some tracks plus a stale `.tmp`, exit 0.
//! * `part_stream`    a release where only one track has a public stream. With
//!   `-f` the album is dropped before anything is fetched ("Full album not
//!   available. Skipping", exit 0); without `-f` the one streamable track
//!   downloads and the counter reads 1/1.
//! * `leave_tmp`      writes only a *truncated* `<track>.mp3.tmp` and exits 1,
//!   simulating a kill.
//! * `with_art`       like `success` but downloads `cover.jpg` first and removes
//!   it at the end (what `-r` does on a release that has art).
//! * `cover_crash`    the real defect 3: `cover.jpg` is only fetched (and
//!   `album_art` only set) when the folder has none; the first tag write then
//!   raises AttributeError, leaving the track's `.tmp` behind. exit 1.
//! * `hang`           sleeps forever (timeout / process-group kill).
//! * `hang_child`     like `hang`, plus a child process in the same group whose
//!   pid is written to `$FAKE_BCDL_PIDFILE` (proves the *group* is killed).
//! * `hang_ignore_term` ignores SIGTERM and hangs (proves the SIGKILL escalation).
//! * `slow`           `success` with `$FAKE_BCDL_STEP_MS` (default 400) between
//!   progress records, so progress arrives while the process runs.
//! * `big_progress`   a 100-track album, so far more records than the 200-line tail.
//!
//! The write path imitates the real one, **including the stale-tmp bug**: a track
//! is written to `<name>.mp3.tmp` then renamed, but if that `.tmp` already exists
//! it is renamed *without fetching* -- so a stale partial becomes a corrupt track.
//!
//! Progress is written exactly as the real tool does: `\r`-prefixed, padded to
//! `$COLUMNS`, with **no newlines**.
//!
//! Other env vars: `FAKE_BCDL_HIDE_FLAGS` (comma list of flags to leave out of
//! `--help`), `FAKE_BCDL_ARGV_LOG` (append argv + selected env, one line per run).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

const TRACKS: [&str; 4] = ["opening-drift", "grid-failure", "iron-lung", "static-bloom"];
const ARTIST: &str = "Somatic";
const ALBUM: &str = "Grid Failure";
/// Bytes of a complete fake track.
const BODY: usize = 2052;

fn step_delay() -> Duration {
    let default = if std::env::var("FAKE_BCDL_MODE").as_deref() == Ok("slow") { 400 } else { 0 };
    let ms = std::env::var("FAKE_BCDL_STEP_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    Duration::from_millis(ms)
}

fn emit(n: usize, total: usize, phase: &str, name: &str) {
    let filled = if phase == "Downloading" { 50 * n / total.max(1) } else { 50 };
    let line = format!("({n}/{total}) [{}{}] :: {phase}: {name}", "=".repeat(filled), " ".repeat(50 - filled));
    let width: usize = std::env::var("COLUMNS").ok().and_then(|v| v.parse().ok()).unwrap_or(80);
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\r{line:<width$}");
    let _ = out.flush();
    let d = step_delay();
    if !d.is_zero() {
        std::thread::sleep(d);
    }
}

fn folder(base: &Path) -> PathBuf {
    base.join(ARTIST).join(ALBUM)
}

fn track_path(base: &Path, n: usize, name: &str) -> PathBuf {
    folder(base).join(format!("{n:02} - {name}.mp3"))
}

fn with_tmp(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

fn full_body() -> Vec<u8> {
    let mut b = vec![0xff, 0xfb, 0x90, 0x00];
    b.resize(BODY, 0);
    b
}

/// The real write path: fetch into `.tmp`, then rename. A pre-existing `.tmp` is
/// renamed as is (the truncation bug).
fn write_track(base: &Path, n: usize, name: &str) {
    let path = track_path(base, n, name);
    let tmp = with_tmp(&path);
    let _ = std::fs::create_dir_all(folder(base));
    if !tmp.exists() {
        let _ = std::fs::write(&tmp, full_body());
    }
    let _ = std::fs::rename(&tmp, &path);
}

fn write_tmp_only(base: &Path, n: usize, name: &str, body: &[u8]) {
    let _ = std::fs::create_dir_all(folder(base));
    let _ = std::fs::write(with_tmp(&track_path(base, n, name)), body);
}

fn eprint_traceback_no_art() {
    eprint!(
        "Traceback (most recent call last):\n  File \"bandcampdownloader.py\", line 232, in download_album\n    \
         os.remove(self.album_art)\nAttributeError: 'BandcampDownloader' object has no attribute 'album_art'\n"
    );
}

const HELP_FLAGS: &[(&str, &str)] = &[
    ("-h, --help", "show this help message and exit"),
    ("--template TEMPLATE", "Output filename template"),
    ("--base-dir BASE_DIR", "Base location of which all files are downloaded"),
    ("-f, --full-album", "Download only if all tracks are available"),
    ("-r, --embed-art", "Embed album art (if available)"),
    ("--no-confirm", "Do not ask for confirmation of incomplete albums"),
    ("--embed-genres", "Embed Bandcamp tags as genre"),
    ("-v, --version", "Show version"),
];

fn print_help() {
    let hidden: Vec<String> = std::env::var("FAKE_BCDL_HIDE_FLAGS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    println!("usage: bandcamp-dl [options] [URL ...]\n\noptions:");
    for (flags, help) in HELP_FLAGS {
        if hidden.iter().any(|h| flags.contains(h.as_str())) {
            continue;
        }
        println!("  {flags:<28}{help}");
    }
}

struct Args {
    base_dir: PathBuf,
    full_album: bool,
    embed_art: bool,
    urls: Vec<String>,
    version: bool,
    help: bool,
}

fn parse_args() -> Args {
    let mut a = Args { base_dir: PathBuf::from("."), full_album: false, embed_art: false, urls: vec![], version: false, help: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--template" => {
                let _ = it.next();
            }
            "--base-dir" => a.base_dir = PathBuf::from(it.next().unwrap_or_default()),
            "-f" | "--full-album" => a.full_album = true,
            "-r" | "--embed-art" => a.embed_art = true,
            "-v" | "--version" => a.version = true,
            "-h" | "--help" => a.help = true,
            "--no-confirm" | "--embed-genres" => {}
            other if other.starts_with("--template=") => {}
            other if other.starts_with("--base-dir=") => a.base_dir = PathBuf::from(&other["--base-dir=".len()..]),
            other => a.urls.push(other.to_string()),
        }
    }
    a
}

fn log_argv() {
    let Ok(path) = std::env::var("FAKE_BCDL_ARGV_LOG") else { return };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let line = format!(
        "ARGV {}\tPYTHONUNBUFFERED={}\tCOLUMNS={}\tLC_ALL={}\tCWD={}\n",
        argv.join(" "),
        env("PYTHONUNBUFFERED"),
        env("COLUMNS"),
        env("LC_ALL"),
        std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default()
    );
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn hang_forever() -> ! {
    loop {
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn run_album(base: &Path, total_tracks: usize, name_of: &dyn Fn(usize) -> String, encode: bool) {
    for n in 1..=total_tracks {
        let name = name_of(n);
        emit(n, total_tracks, "Downloading", &name);
        write_track(base, n, &name);
        if encode {
            emit(n, total_tracks, "Encoding", &name);
        }
        emit(n, total_tracks, "Finished", &name);
    }
}

fn main() {
    let args = parse_args();
    if args.help {
        print_help();
        std::process::exit(0);
    }
    if args.version {
        println!("bandcamp-dl 0.0.17-fake");
        std::process::exit(0);
    }
    log_argv();

    let mode = std::env::var("FAKE_BCDL_MODE").unwrap_or_else(|_| "success".into());
    let base = args.base_dir.clone();
    let track_name = |n: usize| TRACKS[n - 1].to_string();

    match mode.as_str() {
        "not_found" => {
            eprintln!("The url {:?} is not a valid bandcamp page.", args.urls);
            std::process::exit(2);
        }
        "hang" => hang_forever(),
        "hang_child" => {
            // A child in our process group that outlives a plain kill of the leader.
            if std::env::var("FAKE_BCDL_CHILD").is_ok() {
                hang_forever();
            }
            if let Ok(exe) = std::env::current_exe()
                && let Ok(child) = std::process::Command::new(exe).env("FAKE_BCDL_CHILD", "1").spawn()
                && let Ok(pidfile) = std::env::var("FAKE_BCDL_PIDFILE")
            {
                let _ = std::fs::write(pidfile, child.id().to_string());
            }
            hang_forever();
        }
        "hang_ignore_term" => {
            // SAFETY: SIG_IGN installation, no handler code runs.
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            hang_forever();
        }
        "silent_fail" => {
            // Produces nothing, yet exits 0 -- the real tool's discarded return value.
            eprintln!("Maximum retries reached.. skipping.");
        }
        "network" => {
            eprintln!("HTTPSConnectionPool: Max retries exceeded (connection refused)");
        }
        "already_have" => {
            // The real message, verbatim, and printed the same way: newline-terminated,
            // unlike the \r-separated progress lines.
            for (i, name) in TRACKS.iter().enumerate() {
                println!("File: {:02} - {name}.mp3 already exists and is complete, skipping..", i + 1);
            }
        }
        "leave_tmp" => {
            // A kill mid-download: a *partial* body under the .tmp name.
            write_tmp_only(&base, 1, TRACKS[0], b"partial");
            std::process::exit(1);
        }
        "part_stream" => {
            if args.full_album {
                // Verbatim from __main__.py: newline terminated, no \r anywhere near it.
                println!("Full album not available. Skipping  Grid Failure  ...");
            } else {
                emit(1, 1, "Downloading", TRACKS[2]);
                write_track(&base, 3, TRACKS[2]);
                emit(1, 1, "Finished", TRACKS[2]);
            }
        }
        "partial" => {
            let total = TRACKS.len();
            for n in 1..=2 {
                emit(n, total, "Downloading", TRACKS[n - 1]);
                write_track(&base, n, TRACKS[n - 1]);
                emit(n, total, "Finished", TRACKS[n - 1]);
            }
            // A partial file left behind is what silently truncates on retry.
            write_tmp_only(&base, 3, TRACKS[2], &full_body());
        }
        "cover_crash" => {
            // Defect 3: album_art is only assigned when this run writes the cover.
            let dir = folder(&base);
            let _ = std::fs::create_dir_all(&dir);
            let cover = dir.join("cover.jpg");
            let mut album_art_set = false;
            if !cover.exists() && std::fs::write(&cover, b"\xff\xd8\xff").is_ok() {
                album_art_set = true;
            }
            emit(1, TRACKS.len(), "Downloading", TRACKS[0]);
            // The track body lands in .tmp, then write_id3_tags opens album_art.
            write_tmp_only(&base, 1, TRACKS[0], &full_body());
            if args.embed_art && !album_art_set {
                eprint!(
                    "Traceback (most recent call last):\n  File \"bandcampdownloader.py\", line 181, in write_id3_tags\n    \
                     with open(self.album_art, 'rb') as albumart:\nAttributeError: 'BandcampDownloader' object has no attribute 'album_art'\n"
                );
                std::process::exit(1);
            }
            // Fell through: behave like a normal run from here.
            let _ = std::fs::rename(with_tmp(&track_path(&base, 1, TRACKS[0])), track_path(&base, 1, TRACKS[0]));
            emit(1, TRACKS.len(), "Finished", TRACKS[0]);
            for n in 2..=TRACKS.len() {
                emit(n, TRACKS.len(), "Downloading", TRACKS[n - 1]);
                write_track(&base, n, TRACKS[n - 1]);
                emit(n, TRACKS.len(), "Finished", TRACKS[n - 1]);
            }
            if args.embed_art {
                let _ = std::fs::remove_file(&cover);
            }
        }
        "with_art" => {
            let dir = folder(&base);
            let _ = std::fs::create_dir_all(&dir);
            let cover = dir.join("cover.jpg");
            if !cover.exists() {
                let _ = std::fs::write(&cover, b"\xff\xd8\xff");
            }
            run_album(&base, TRACKS.len(), &track_name, true);
            if args.embed_art {
                let _ = std::fs::remove_file(&cover);
            }
        }
        "big_progress" => {
            run_album(&base, 100, &|n| format!("track-{n:03}"), true);
        }
        "slow" => {
            run_album(&base, TRACKS.len(), &track_name, true);
        }
        // success / no_art_crash / anything else
        _ => {
            run_album(&base, TRACKS.len(), &track_name, true);
            if mode == "no_art_crash" {
                eprint_traceback_no_art();
                std::process::exit(1);
            }
        }
    }
}
