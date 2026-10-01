//! TEST FIXTURE ONLY -- runs the download worker until killed (the kill -9 resume proof,
//! `tests/kill9_resume.rs`). Never shipped.
//!
//! ```text
//! dl_harness --db <path> --data <dir> --bandcamp-origin http://bandcamp.com:PORT
//!            --addr 127.0.0.1:PORT --downloader native|bcdl [--bcdl-bin <path>]
//!            [--hosts a.bandcamp.com,b.bandcamp.com] [--exit-when-idle]
//! ```
//!
//! Builds the `Db`, the `JobsService` (crash recovery runs in `start`), and the download worker
//! over a `NoLibrary` (files are merged into `<data>/downloads` but not indexed), with the shared
//! client pointed at the fake Bandcamp at `--addr` (every `--hosts` name resolves there). The
//! downloader is chosen the way production does it: the `downloads.downloader` setting, written
//! here from `--downloader`. Prints `READY` once started, then one line per item status change
//! (`ITEM <id> <status> attempts=<n> class=<error_class>`), until killed -- or, with
//! `--exit-when-idle`, until no download item is pending or running.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bc_bandcamp::download::library_port::NoLibrary;
use bc_bandcamp::download::worker::{DOWNLOADER_KEY, DownloadDeps, DownloadWorker};
use bc_bandcamp::net::{BandcampClient, ClientOptions};
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_jobs::JobsService;

struct Args {
    db: PathBuf,
    data: PathBuf,
    origin: String,
    addr: SocketAddr,
    downloader: String,
    bcdl_bin: Option<String>,
    hosts: Vec<String>,
    exit_when_idle: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let mut flags: Vec<String> = Vec::new();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == "--exit-when-idle" {
            flags.push(a);
            continue;
        }
        let v = it.next().ok_or_else(|| format!("missing value for {a}"))?;
        map.insert(a, v);
    }
    let get = |k: &str| map.get(k).cloned().ok_or_else(|| format!("missing {k}"));
    Ok(Args {
        db: PathBuf::from(get("--db")?),
        data: PathBuf::from(get("--data")?),
        origin: get("--bandcamp-origin")?,
        addr: get("--addr")?.parse().map_err(|e| format!("bad --addr: {e}"))?,
        downloader: get("--downloader")?,
        bcdl_bin: map.get("--bcdl-bin").cloned(),
        hosts: map
            .get("--hosts")
            .map(|h| h.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| vec!["bandcamp.com".into(), "kill.bandcamp.com".into()]),
        exit_when_idle: flags.iter().any(|f| f == "--exit-when-idle"),
    })
}

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("dl_harness: {e}");
            std::process::exit(2);
        }
    };
    let mut cfg = Config::from_env();
    cfg.data_dir = args.data.clone();
    cfg.download_dir = args.data.join("downloads");
    cfg.download_concurrency = 1;
    if let Some(bin) = &args.bcdl_bin {
        cfg.bandcamp_dl_bin = bin.clone();
    }
    let _ = std::fs::create_dir_all(&cfg.download_dir);

    let db = match Db::open(&args.db) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("dl_harness: cannot open the database: {e}");
            std::process::exit(2);
        }
    };
    let choice = if args.downloader == "bcdl" { "bandcamp-dl" } else { "native" };
    let wrote = db.write(move |t| bc_db::settings::set(t, DOWNLOADER_KEY, choice));
    if let Err(e) = wrote {
        eprintln!("dl_harness: cannot write the downloader setting: {e}");
        std::process::exit(2);
    }

    let bus = Arc::new(EventBus::new());
    let jobs = JobsService::new(db.clone(), bus.clone());
    let client = BandcampClient::new(ClientOptions {
        rate_per_sec: 1000.0,
        burst: 1000,
        reserved_rate_per_sec: 1000.0,
        reserved_burst: 1000,
        resolve: args.hosts.iter().map(|h| (h.clone(), args.addr)).collect(),
        api_origin: Some(args.origin.clone()),
        backoff_scale: 0.01,
        ..ClientOptions::default()
    });
    let deps = Arc::new(DownloadDeps::new(db.clone(), bus.clone(), cfg, Some(client)).with_library(Arc::new(NoLibrary)));
    let worker = DownloadWorker::new(deps, jobs.store().clone());
    jobs.add_hooks(worker.hooks());

    // Crash recovery first (requeues what a SIGKILL left `running`), then the worker, whose
    // `start` kills orphaned bandcamp-dl children and purges stale staging before it claims.
    jobs.start().await;
    worker.start().await;
    println!("READY");

    let mut last: HashMap<i64, String> = HashMap::new();
    let mut idle_since: Option<std::time::Instant> = None;
    loop {
        let rows: Vec<(i64, String, i64, Option<String>)> = db
            .read(|c| {
                let mut st = c.prepare(
                    "SELECT ji.id, ji.status, ji.attempts, ji.error_class FROM job_items ji JOIN jobs j ON j.id = ji.job_id \
                     WHERE j.kind = 'download' ORDER BY ji.id",
                )?;
                Ok(st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?)
            })
            .unwrap_or_default();
        let mut open = false;
        for (id, status, attempts, class) in rows {
            let line = format!("ITEM {id} {status} attempts={attempts} class={}", class.unwrap_or_default());
            if matches!(status.as_str(), "pending" | "running") {
                open = true;
            }
            if last.get(&id) != Some(&line) {
                println!("{line}");
                last.insert(id, line);
            }
        }
        if args.exit_when_idle {
            if open {
                idle_since = None;
            } else {
                let since = *idle_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() > Duration::from_millis(600) {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    worker.stop().await;
    println!("DONE");
}
