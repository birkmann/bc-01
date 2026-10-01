//! THE KILL -9 RESUME PROOF.
//!
//! A real download worker (`dl_harness`, a child process) is SIGKILLed in the middle of a job and
//! restarted; the job must resume and finish with every audio file complete.
//!
//! * a fake Bandcamp + CDN (axum, in this process) serves 3 albums x 3 tracks; every MP3 body has a
//!   known length and is streamed slowly in chunks, so a download of one track takes ~0.4 s and a
//!   `.part` exists for most of that time;
//! * for the **native** downloader the harness uses the production code path (settings row
//!   `downloads.downloader = native`); for the **bandcamp-dl adapter** the binary is a small bash
//!   fake that writes a half-length `<track>.mp3.tmp`, sleeps, completes it, renames, and -- like
//!   the real tool -- renames a *pre-existing* `.tmp` as is (the stale-`.tmp` truncation bug);
//! * per downloader, 3 iterations x 2 kills (6 SIGKILLs), at different moments: mid first track,
//!   right after a track landed, mid a later track, at an album boundary;
//! * after each kill the DB must still show the item `running` (it really was a crash), and with the
//!   adapter the orphaned bandcamp-dl process (own process group) must have survived the SIGKILL --
//!   and be gone, killed by the worker's startup recovery, once the restarted harness says READY;
//! * the restarted worker requeues (`error_class = crash`), resumes and finishes: no `*.part` /
//!   `*.tmp` left, 9 final files with exactly the expected length that lofty opens, no duplicates,
//!   job `completed`, items `done`, attempts sane.

use std::collections::BTreeSet;
use std::io::BufRead;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use bc_db::Db;
use bc_jobs::{JobStore, NewItem, NewJob};
use lofty::file::AudioFile;
use lofty::probe::Probe;
use serde_json::{Value, json};

const HARNESS: &str = env!("CARGO_BIN_EXE_dl_harness");
const ALBUMS: usize = 3;
const TRACKS: usize = 3;
const FRAMES: usize = 48;
/// Pause between the chunks of one CDN body (12 chunks => ~0.4 s per track).
const CHUNK_MS: u64 = 33;
/// The fake bandcamp-dl's pause between the two halves of a track (`.tmp` lives this long).
const BCDL_STEP: &str = "0.3";

fn mp3_body() -> Vec<u8> {
    // Valid MPEG-1 Layer III 128 kbps / 44.1 kHz frames (417 bytes each).
    let mut out = Vec::new();
    for _ in 0..FRAMES {
        let mut f = vec![0u8; 417];
        f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        out.extend_from_slice(&f);
    }
    out
}

// -- the fake Bandcamp + CDN ----------------------------------------------------------------------

#[derive(Clone)]
struct Site {
    cdn_requests: Arc<AtomicUsize>,
    addr: Arc<parking_lot::Mutex<String>>,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

async fn album_page(State(site): State<Site>, AxPath(name): AxPath<String>) -> impl IntoResponse {
    let addr = site.addr.lock().clone();
    let n: usize = name.trim_start_matches("album-").parse().unwrap_or(1);
    let trackinfo: Vec<Value> = (1..=TRACKS)
        .map(|t| {
            json!({"title": format!("Track {t}"), "track_num": t, "duration": 10.0,
                   "title_link": format!("/track/track-{t}"), "track_id": n * 100 + t, "artist": null,
                   "file": {"mp3-128": format!("http://{addr}/cdn/{n}/{t}.mp3")}})
        })
        .collect();
    let blob = json!({
        "for the curious": "scrapers, hello", "item_type": "album", "id": n, "artist": "Killer",
        "album_release_date": "17 Jul 2023 00:00:00 GMT",
        "current": {"title": format!("Album {n}"), "band_id": 7},
        "trackinfo": trackinfo,
    });
    Html(format!(r#"<html><body><script data-tralbum="{}"></script></body></html>"#, esc(&blob.to_string())))
}

async fn cdn(State(site): State<Site>, AxPath((_album, _track)): AxPath<(String, String)>) -> impl IntoResponse {
    site.cdn_requests.fetch_add(1, Ordering::SeqCst);
    let body = mp3_body();
    let len = body.len();
    let chunks: Vec<bytes::Bytes> = body.chunks(len / 12 + 1).map(|c| bytes::Bytes::from(c.to_vec())).collect();
    let stream = futures::stream::unfold(chunks.into_iter(), |mut it| async move {
        tokio::time::sleep(Duration::from_millis(CHUNK_MS)).await;
        it.next().map(|c| (Ok::<_, std::io::Error>(c), it))
    });
    axum::response::Response::builder()
        .header(header::CONTENT_LENGTH, len)
        .header(header::CONTENT_TYPE, "audio/mpeg")
        .body(Body::from_stream(stream))
        .expect("response")
}

async fn start_site() -> (Site, std::net::SocketAddr) {
    let site = Site { cdn_requests: Arc::new(AtomicUsize::new(0)), addr: Arc::new(parking_lot::Mutex::new(String::new())) };
    let app = Router::new().route("/album/{name}", get(album_page)).route("/cdn/{album}/{track}", get(cdn)).with_state(site.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    *site.addr.lock() = addr.to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (site, addr)
}

// -- the fake bandcamp-dl ----------------------------------------------------------------------------

fn write_fake_bcdl(dir: &Path, body: &Path) -> PathBuf {
    let script = dir.join("fake-bcdl-kill9");
    let text = format!(
        r#"#!/bin/bash
BODY="{body}"
case "$1" in
  --help) printf 'usage: bandcamp-dl [options]\n  --template TEMPLATE\n  --base-dir BASE_DIR\n  -f, --full-album\n  -r, --embed-art\n  --no-confirm\n  --embed-genres\n'; exit 0;;
  --version|-v) echo "bandcamp-dl 0.0.17-kill9"; exit 0;;
esac
BASE=.
URL=
while [ $# -gt 0 ]; do
  case "$1" in
    --base-dir) BASE="$2"; shift 2;;
    --template) shift 2;;
    -f|-r|--no-confirm|--embed-genres) shift;;
    *) URL="$1"; shift;;
  esac
done
ALBUM=$(basename "$URL")
DIR="$BASE/Killer/$ALBUM"
mkdir -p "$DIR"
SIZE=$(stat -c %s "$BODY")
HALF=$((SIZE / 2))
for n in 1 2 3; do
  F="$DIR/0$n - track-$n.mp3"
  if [ -f "$F" ]; then
    echo "File: 0$n - track-$n.mp3 already exists and is complete, skipping.."
    continue
  fi
  printf '\r(%d/3) [%-50s] :: Downloading: track-%d' "$n" "$(head -c $((n*10)) /dev/zero | tr '\0' '=')" "$n"
  # The real tool's write path, stale-.tmp bug included: an existing .tmp is renamed as is.
  if [ ! -e "$F.tmp" ]; then
    head -c "$HALF" "$BODY" > "$F.tmp"
    sleep {step}
    cat "$BODY" > "$F.tmp"
  fi
  mv "$F.tmp" "$F"
  printf '\r(%d/3) [%-50s] :: Finished: track-%d' "$n" "$(head -c 50 /dev/zero | tr '\0' '=')" "$n"
done
"#,
        body = body.display(),
        step = BCDL_STEP,
    );
    std::fs::write(&script, text).expect("write fake bcdl");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

// -- filesystem / process helpers ---------------------------------------------------------------------

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn files(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    walk(dir, &mut v);
    v.sort();
    v
}

fn is_partial(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s.ends_with(".part") || s.ends_with(".tmp")
}

/// Pids (other than ours) whose command line contains `needle`.
fn procs_with(needle: &str) -> Vec<i32> {
    let me = std::process::id() as i32;
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_string_lossy().parse::<i32>().ok() else { continue };
        if pid == me {
            continue;
        }
        let Ok(raw) = std::fs::read(e.path().join("cmdline")) else { continue };
        if String::from_utf8_lossy(&raw).replace('\0', " ").contains(needle) {
            // A zombie has an empty cmdline, so anything listed here is a live process.
            out.push(pid);
        }
    }
    out
}

/// Alive and not a zombie (an orphan reparented to init is reaped lazily).
fn alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => !s.rsplit(')').next().is_some_and(|rest| rest.trim_start().starts_with('Z')),
        Err(_) => false,
    }
}

fn db_items(db: &Db) -> Vec<(i64, String, i64, Option<String>)> {
    db.read(|c| {
        let mut st = c.prepare("SELECT id, status, attempts, error_class FROM job_items ORDER BY id")?;
        Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?)
    })
    .expect("items")
}

fn running_item(db: &Db) -> Option<i64> {
    db_items(db).into_iter().find(|i| i.1 == "running").map(|i| i.0)
}

// -- the harness child -------------------------------------------------------------------------------------

struct Harness {
    child: Child,
    lines: mpsc::Receiver<String>,
    log: Vec<String>,
}

struct Setup {
    db_path: PathBuf,
    data: PathBuf,
    addr: std::net::SocketAddr,
    downloader: &'static str,
    bcdl_bin: Option<PathBuf>,
}

impl Setup {
    fn spawn(&self, exit_when_idle: bool) -> Harness {
        let mut cmd = Command::new(HARNESS);
        cmd.arg("--db").arg(&self.db_path).arg("--data").arg(&self.data);
        cmd.arg("--bandcamp-origin").arg(format!("http://bandcamp.com:{}", self.addr.port()));
        cmd.arg("--addr").arg(self.addr.to_string());
        cmd.arg("--downloader").arg(self.downloader);
        cmd.arg("--hosts").arg("bandcamp.com,kill.bandcamp.com");
        if let Some(b) = &self.bcdl_bin {
            cmd.arg("--bcdl-bin").arg(b);
        }
        if exit_when_idle {
            cmd.arg("--exit-when-idle");
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::inherit()).stdin(Stdio::null());
        let mut child = cmd.spawn().expect("spawn dl_harness");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Harness { child, lines: rx, log: Vec::new() }
    }
}

impl Drop for Harness {
    /// A failing assertion must not leave a worker running behind the test.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Harness {
    fn wait_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left.max(Duration::from_millis(1))) {
                Ok(l) => {
                    let ready = l == "READY";
                    self.log.push(l);
                    if ready {
                        return;
                    }
                }
                Err(_) => panic!("dl_harness never printed READY; output so far: {:?}", self.log),
            }
        }
    }

    fn drain(&mut self) {
        while let Ok(l) = self.lines.try_recv() {
            self.log.push(l);
        }
    }

    /// SIGKILL (not SIGTERM): nothing in the harness gets to clean up.
    fn sigkill(&mut self) -> i32 {
        let pid = self.child.id() as i32;
        // SAFETY: plain kill(2) on our own child.
        let rc = unsafe { libc::kill(pid, libc::SIGKILL) };
        assert_eq!(rc, 0, "kill failed");
        let status = self.child.wait().expect("wait");
        assert_eq!(status.signal(), Some(libc::SIGKILL), "the harness must die of SIGKILL, got {status:?}");
        std::thread::sleep(Duration::from_millis(30));
        self.drain();
        pid
    }
}

// -- kill moments ----------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Trigger {
    /// A partial file exists, an item is `running`, and at least this many final tracks exist.
    MidTrack { min_finals: usize },
    /// At least this many final tracks exist (kill right after one landed: between tracks/albums).
    AfterFinals(usize),
}

fn wait_trigger(t: Trigger, db: &Db, dl_root: &Path, h: &mut Harness) -> String {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let fs = files(dl_root);
        let partials = fs.iter().filter(|p| is_partial(p)).count();
        let n_final = fs.iter().filter(|p| p.to_string_lossy().ends_with(".mp3")).count();
        let running = running_item(db);
        let hit = match t {
            Trigger::MidTrack { min_finals } => partials > 0 && running.is_some() && n_final >= min_finals,
            Trigger::AfterFinals(n) => n_final >= n && running.is_some(),
        };
        if hit {
            return format!("{t:?}: {n_final} final file(s), {partials} partial(s), item {running:?} running");
        }
        h.drain();
        if let Ok(Some(st)) = h.child.try_wait() {
            panic!("the harness exited ({st:?}) before the kill moment {t:?}; output: {:?}", h.log);
        }
        assert!(Instant::now() < deadline, "timed out waiting for {t:?}; files: {fs:?}; output: {:?}", h.log);
        std::thread::sleep(Duration::from_millis(8));
    }
}

// -- verification ----------------------------------------------------------------------------------------------------

/// Native files carry an ID3v2 tag in front of the untouched MPEG frames.
fn id3_len(bytes: &[u8]) -> usize {
    if bytes.len() >= 10 && &bytes[..3] == b"ID3" {
        10 + (((bytes[6] as usize) << 21) | ((bytes[7] as usize) << 14) | ((bytes[8] as usize) << 7) | bytes[9] as usize)
    } else {
        0
    }
}

fn verify_final(dl_root: &Path, downloader: &str, expect_mp3s: usize) -> Vec<String> {
    let body = mp3_body();
    let all = files(dl_root);
    let leftovers: Vec<_> = all.iter().filter(|p| is_partial(p)).collect();
    assert!(leftovers.is_empty(), "partial files left behind: {leftovers:?}");
    let staging: Vec<_> = all.iter().filter(|p| p.to_string_lossy().contains("/.staging/")).collect();
    assert!(staging.is_empty(), "staging not cleaned up: {staging:?}");
    let others: Vec<_> = all.iter().filter(|p| !p.to_string_lossy().ends_with(".mp3")).collect();
    assert!(others.is_empty(), "unexpected non-audio files (error.log? covers?): {others:?}");

    let mp3s: Vec<&PathBuf> = all.iter().collect();
    assert_eq!(mp3s.len(), expect_mp3s, "wrong number of final files: {mp3s:?}");
    let rel: BTreeSet<String> = mp3s.iter().map(|p| p.strip_prefix(dl_root).expect("under root").to_string_lossy().to_lowercase()).collect();
    assert_eq!(rel.len(), expect_mp3s, "duplicate files: {rel:?}");

    let mut report = Vec::new();
    for p in &mp3s {
        let bytes = std::fs::read(p).expect("read");
        if downloader == "bcdl" {
            assert_eq!(bytes, body, "{}: a bandcamp-dl track must equal the body byte for byte (no truncation)", p.display());
        } else {
            let tag = id3_len(&bytes);
            assert!(tag > 10, "{}: the native downloader tags every file", p.display());
            assert_eq!(bytes.len(), tag + body.len(), "{}: wrong length", p.display());
            assert_eq!(&bytes[tag..], &body[..], "{}: audio payload differs", p.display());
        }
        let tagged = Probe::open(p).and_then(|pr| pr.read()).unwrap_or_else(|e| panic!("lofty cannot open {}: {e}", p.display()));
        assert!(tagged.properties().duration() > Duration::ZERO, "{}: lofty reads no duration", p.display());
        report.push(format!("{} ({} bytes)", p.strip_prefix(dl_root).expect("rel").display(), bytes.len()));
    }
    report
}

// -- one scenario -------------------------------------------------------------------------------------------------------

struct Scenario {
    downloader: &'static str,
    kills: [Trigger; 2],
}

async fn run_scenario(sc: &Scenario, iteration: usize, site: &Site, addr: std::net::SocketAddr, fake_bcdl: &Path) {
    let started = Instant::now();
    let tmp = tempfile::tempdir().expect("tmp");
    let db_path = tmp.path().join("kill9.db");
    let data = tmp.path().join("data");
    let dl_root = data.join("downloads");
    std::fs::create_dir_all(&data).expect("data");
    let db = Db::open(&db_path).expect("open db");
    let store = JobStore::new(db.clone(), None);
    let port = addr.port();
    let items: Vec<NewItem> =
        (1..=ALBUMS).map(|n| NewItem::url(format!("http://kill.bandcamp.com:{port}/album/album-{n}"), "album")).collect();
    let job = store.create_job(NewJob::new("download", items).label("kill -9")).expect("job");
    let setup = Setup { db_path, data, addr, downloader: sc.downloader, bcdl_bin: (sc.downloader == "bcdl").then(|| fake_bcdl.to_path_buf()) };
    let tag = format!("[{} #{iteration}]", sc.downloader);
    let requests_before = site.cdn_requests.load(Ordering::SeqCst);
    let fake_name = fake_bcdl.to_string_lossy().into_owned();

    let mut crashed_items: BTreeSet<i64> = BTreeSet::new();
    let mut previous_orphans: Vec<i32> = Vec::new();
    for (k, trigger) in sc.kills.iter().enumerate() {
        let mut h = setup.spawn(false);
        h.wait_ready();
        if k > 0 {
            // Startup recovery ran before READY: the crash was requeued, and (adapter) the
            // orphaned bandcamp-dl of the previous run was killed before its partials were purged.
            let crashed = db_items(&db).iter().filter(|i| i.3.as_deref() == Some("crash")).count();
            assert!(crashed >= 1, "{tag} recovery did not mark the killed item as crash: {:?}", db_items(&db));
            for pid in &previous_orphans {
                assert!(!alive(*pid), "{tag} orphan pid {pid} survived the restart");
            }
            println!("{tag} restart {k}: recovery requeued the crash (error_class=crash); orphan(s) {previous_orphans:?} gone");
        }
        let how = wait_trigger(*trigger, &db, &dl_root, &mut h);
        let pid = h.sigkill();
        // The server really crashed: the item is still `running` in the DB, the job with it.
        let running = db_items(&db).into_iter().filter(|i| i.1 == "running").collect::<Vec<_>>();
        assert_eq!(running.len(), 1, "{tag} after SIGKILL exactly one item must still be running: {:?}", db_items(&db));
        crashed_items.insert(running[0].0);
        assert_eq!(store.get_job(&job.id).expect("job").expect("exists").status, "running");
        let fs_after = files(&dl_root);
        let partial_after = fs_after.iter().filter(|p| is_partial(p)).count();
        previous_orphans = if sc.downloader == "bcdl" { procs_with(&fake_name) } else { Vec::new() };
        println!(
            "{tag} kill {} of 2 -- SIGKILL pid {pid} at [{how}]; DB: item {} still running; {} partial file(s) left on disk; orphan bandcamp-dl pid(s) alive: {previous_orphans:?}",
            k + 1,
            running[0].0,
            partial_after
        );
        if sc.downloader == "bcdl" && matches!(trigger, Trigger::MidTrack { .. }) {
            // The process-group child outlives its parent: only the worker's recovery can stop it.
            assert!(!previous_orphans.is_empty(), "{tag} the process-group child should have survived the SIGKILL");
            assert!(partial_after > 0, "{tag} a mid-track kill of the adapter must leave a stale .tmp");
        }
    }

    // The last run finishes the job and exits by itself.
    let mut h = setup.spawn(true);
    h.wait_ready();
    for pid in &previous_orphans {
        assert!(!alive(*pid), "{tag} orphan pid {pid} survived the final restart");
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        h.drain();
        if let Some(st) = h.child.try_wait().expect("try_wait") {
            break st;
        }
        if Instant::now() > deadline {
            let _ = h.child.kill();
            panic!("{tag} the resumed harness did not finish; output: {:?}; items {:?}", h.log, db_items(&db));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    h.drain();
    assert!(status.success(), "{tag} resumed harness exit status {status:?}; output {:?}", h.log);
    assert_eq!(h.log.last().map(String::as_str), Some("DONE"), "{tag} {:?}", h.log);

    // -- invariants -------------------------------------------------------------------------------------------
    let j = store.get_job(&job.id).expect("job").expect("job exists");
    assert_eq!((j.status.as_str(), j.completed, j.failed, j.skipped, j.total), ("completed", ALBUMS as i64, 0, 0, ALBUMS as i64), "{tag}");
    let items = db_items(&db);
    assert!(items.iter().all(|i| i.1 == "done"), "{tag} items: {items:?}");
    for (id, _status, attempts, class) in &items {
        assert!((1..=3).contains(attempts), "{tag} item {id}: attempts {attempts}");
        if crashed_items.contains(id) {
            assert!(*attempts >= 2, "{tag} item {id} was killed, so it needs another attempt: {attempts}");
            assert_eq!(class.as_deref(), Some("crash"), "{tag} item {id} should carry the crash class");
        }
    }
    let report = verify_final(&dl_root, sc.downloader, ALBUMS * TRACKS);
    assert!(procs_with(&fake_name).is_empty(), "{tag} a bandcamp-dl process is still alive");
    let cdn_used = site.cdn_requests.load(Ordering::SeqCst) - requests_before;
    if sc.downloader == "native" {
        // Resumed, not restarted: complete tracks are skipped; every kill costs at most one album
        // worth of refetches (never the 9 tracks all over again).
        assert!((ALBUMS * TRACKS..=ALBUMS * TRACKS + 2 * TRACKS).contains(&cdn_used), "{tag} CDN requests {cdn_used}");
    }
    println!(
        "{tag} OK in {:.1}s: job completed, items done, attempts {:?}, killed item(s) {crashed_items:?}; {} files complete + lofty-readable, no .part/.tmp/duplicates; CDN requests {cdn_used}\n    {}",
        started.elapsed().as_secs_f64(),
        items.iter().map(|i| i.2).collect::<Vec<_>>(),
        report.len(),
        report.join("\n    ")
    );
}

fn kill_plan(iteration: usize) -> [Trigger; 2] {
    match iteration {
        // mid first track, then right after a track landed (between tracks).
        0 => [Trigger::MidTrack { min_finals: 0 }, Trigger::AfterFinals(1)],
        // right after the first track landed, then mid a later track.
        1 => [Trigger::AfterFinals(1), Trigger::MidTrack { min_finals: 2 }],
        // after the second track of the first album, then mid track of the second album.
        _ => [Trigger::AfterFinals(2), Trigger::MidTrack { min_finals: TRACKS + 1 }],
    }
}

async fn prove(downloader: &'static str) {
    let t0 = Instant::now();
    let (site, addr) = start_site().await;
    let scratch = tempfile::tempdir().expect("scratch");
    let body_file = scratch.path().join("body.mp3");
    std::fs::write(&body_file, mp3_body()).expect("body");
    let fake_bcdl = write_fake_bcdl(scratch.path(), &body_file);
    for iteration in 0..3 {
        run_scenario(&Scenario { downloader, kills: kill_plan(iteration) }, iteration, &site, addr, &fake_bcdl).await;
    }
    println!("[{downloader}] 3 iterations x 2 SIGKILLs proven in {:.1}s", t0.elapsed().as_secs_f64());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill9_resume_native_downloader() {
    prove("native").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill9_resume_bandcamp_dl_adapter() {
    prove("bcdl").await;
}

/// The stale-`.tmp` truncation scenario on its own: the fake tool (like the real one) renames a
/// pre-existing `.tmp` without fetching it, so a half-length leftover would become a "complete"
/// track. The first half of the test proves the fake reproduces that; the second that the worker
/// (startup recovery + the adapter's pre-attempt purge) never lets it happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_tmp_never_becomes_a_truncated_track() {
    let (_site, addr) = start_site().await;
    let scratch = tempfile::tempdir().expect("scratch");
    let body = mp3_body();
    let body_file = scratch.path().join("body.mp3");
    std::fs::write(&body_file, &body).expect("body");
    let fake = write_fake_bcdl(scratch.path(), &body_file);
    let half = &body[..body.len() / 2];
    let url = format!("http://kill.bandcamp.com:{}/album/album-1", addr.port());

    // Control: the tool itself, run against a directory holding a stale half-length .tmp.
    let control = scratch.path().join("control");
    let album = control.join("Killer").join("album-1");
    std::fs::create_dir_all(&album).expect("mkdir");
    std::fs::write(album.join("02 - track-2.mp3.tmp"), half).expect("stale tmp");
    let st = Command::new(&fake).args(["--base-dir"]).arg(&control).arg(&url).stdout(Stdio::null()).status().expect("run fake");
    assert!(st.success());
    assert_eq!(std::fs::metadata(album.join("02 - track-2.mp3")).expect("track 2").len() as usize, half.len(), "the fake must reproduce the truncation bug");
    assert_ne!(half.len(), body.len());

    // The worker: the same stale .tmp (and a complete track 1) in the item's staging dir.
    let tmp = tempfile::tempdir().expect("tmp");
    let db_path = tmp.path().join("stale.db");
    let data = tmp.path().join("data");
    let dl_root = data.join("downloads");
    let db = Db::open(&db_path).expect("db");
    let store = JobStore::new(db.clone(), None);
    store.create_job(NewJob::new("download", vec![NewItem::url(url.clone(), "album")])).expect("job");
    let staged = dl_root.join(".staging").join("item-1").join("Killer").join("album-1");
    std::fs::create_dir_all(&staged).expect("staging");
    std::fs::write(staged.join("01 - track-1.mp3"), &body).expect("track 1");
    std::fs::write(staged.join("02 - track-2.mp3.tmp"), half).expect("stale tmp");

    let setup = Setup { db_path, data, addr, downloader: "bcdl", bcdl_bin: Some(fake) };
    let mut h = setup.spawn(true);
    h.wait_ready();
    let deadline = Instant::now() + Duration::from_secs(40);
    let status = loop {
        h.drain();
        if let Some(st) = h.child.try_wait().expect("try_wait") {
            break st;
        }
        assert!(Instant::now() < deadline, "the harness did not finish: {:?}", h.log);
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(status.success(), "{status:?} {:?}", h.log);
    assert_eq!(db_items(&db)[0].1, "done");
    let report = verify_final(&dl_root, "bcdl", TRACKS);
    println!("stale .tmp control + worker: truncated by the bare tool, but the worker's tracks are all complete: {report:?}");
}
