//! `bc`: command line entry point. `bc import | scan | doctor | serve | analyze`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use bc_core::Config;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bc", version, about = "bc-rust: Bandcamp library manager")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Import the legacy Python app's library into $BC_DATA_DIR/library.db
    Import {
        /// The legacy library.db, or the legacy data dir (default: $BC_LEGACY_DB)
        #[arg(long)]
        from: Option<PathBuf>,
        /// Replace an existing library.db (moved aside, not deleted)
        #[arg(long)]
        force: bool,
        /// Skip the one-off repair passes
        #[arg(long)]
        skip_repairs: bool,
        /// Print the verification report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Scan library roots for new/changed/missing files
    Scan {
        #[arg(long)]
        root: Option<i64>,
    },
    /// Check the database and library: integrity, counts, index plans, orphans
    Doctor {
        /// Build the optional trigram search table over tracks
        #[arg(long)]
        build_trigram: bool,
        /// Rebuild the track FTS index
        #[arg(long)]
        rebuild_fts: bool,
    },
    /// Run the analysis tool (accuracy gate, key-profile fit, energy calibration): passes its arguments to `bc-analysis-tool`
    /// (e.g. `bc analyze --gate`, `--fit-key-profiles`, `--calibrate-energy`)
    Analyze {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Re-run the Bandcamp extractors over the cached pages in cache.db, offline (extractor-fix check
    /// and degradation telemetry: tier counts, canary)
    #[command(name = "bandcamp-replay")]
    BandcampReplay {
        /// Page kind: album | music | fan | discover | stream
        #[arg(long, default_value = "album")]
        kind: String,
        /// Print the report as JSON
        #[arg(long)]
        json: bool,
    },
    /// Serve the HTTP API and the UI (default 127.0.0.1:8420; `--lan` binds all interfaces with device pairing)
    Serve {
        #[arg(long, env = "BC_PORT")]
        port: Option<u16>,
        /// Opt in to LAN mode: bind 0.0.0.0, require a paired device token for other machines
        #[arg(long)]
        lan: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let cli = Cli::parse();
    let config = Config::from_env();
    match cli.cmd {
        Cmd::Import { from, force, skip_repairs, json } => {
            let opts = bc_library::import::ImportOptions { from, force, skip_repairs };
            let report = bc_library::import::run_import(&config, &opts, &|m| eprintln!("{m}")).context("import failed")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_report(&report);
            }
            if !report.ok {
                anyhow::bail!("import finished with verification mismatches (see report)");
            }
        }
        Cmd::Scan { root } => {
            let db = bc_db::Db::open(config.db_path()).context("open library.db")?;
            let ctx = bc_libcore::Ctx::new(db, std::sync::Arc::new(bc_core::EventBus::new()), config.clone());
            bc_scan::roots::ensure_roots(&ctx)?;
            let roots = ctx.read(bc_scan::roots::list_roots)?;
            for r in roots.into_iter().filter(|r| r.enabled && root.is_none_or(|id| id == r.id)) {
                let t = std::time::Instant::now();
                let res = bc_scan::scanner::scan_root(&ctx, r.id, bc_scan::scanner::ScanHooks::none())?;
                println!(
                    "{}: seen {} added {} updated {} missing {} unchanged {} tracks+{} errors {} ({} ms, wall {:?})",
                    r.path, res.files_seen, res.files_added, res.files_updated, res.files_missing, res.files_unchanged, res.tracks_added, res.errors.len(), res.duration_ms, t.elapsed()
                );
            }
        }
        Cmd::Doctor { build_trigram, rebuild_fts } => {
            let db = bc_db::Db::open(config.db_path()).context("open library.db")?;
            let rep = bc_library::doctor::run(&db, &bc_library::doctor::DoctorOptions { build_trigram, rebuild_fts, full_integrity: false })?;
            for c in &rep.checks {
                println!("[{}] {:<34} {}", if c.ok { " ok " } else { "FAIL" }, c.name, c.detail);
            }
            if !rep.ok {
                anyhow::bail!("doctor found problems");
            }
        }
        Cmd::Analyze { args } => {
            let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("bc-analysis-tool")));
            let tool = exe.filter(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("bc-analysis-tool"));
            let status = std::process::Command::new(&tool)
                .args(&args)
                .status()
                .with_context(|| format!("could not run {} (build it with `cargo build -p bc-analysis --bins`)", tool.display()))?;
            if !status.success() {
                anyhow::bail!("analysis tool exited with {status}");
            }
        }
        Cmd::BandcampReplay { kind, json } => {
            let kind = bc_bandcamp::net::PageKind::parse(&kind).with_context(|| format!("unknown page kind {kind:?}"))?;
            let cache = bc_bandcamp::net::PageCache::open(&config.cache_db_path(), bc_bandcamp::net::DEFAULT_MAX_BYTES)
                .context("cannot open cache.db")?
                .shared();
            let r = bc_bandcamp::replay::replay(&cache, kind).context("replay failed")?;
            if json {
                println!("{}", serde_json::json!({"pages": r.pages, "tiers": r.tiers, "canary_missing": r.canary_missing, "unreadable": r.unreadable, "grid_items": r.grid_items, "examples": r.examples}));
            } else {
                println!("{} page(s) replayed; tiers {:?}; canary missing {}; unreadable {}; grid items {}", r.pages, r.tiers, r.canary_missing, r.unreadable, r.grid_items);
                for (k, v) in &r.examples {
                    println!("  {k}: {}", v.join(", "));
                }
            }
        }
        Cmd::Serve { port, lan } => {
            let mut config = config;
            if let Some(p) = port {
                config.port = p;
            }
            if lan {
                config.lan = true;
            }
            let rt = tokio::runtime::Runtime::new().context("start the async runtime")?;
            rt.block_on(bc_server::run(config))?;
        }
    }
    Ok(())
}

fn print_report(r: &bc_library::import::ImportReport) {
    println!("import {} -> {}", r.source, r.target);
    for t in &r.timings {
        println!("  {:<24} {:>8} ms  {}", t.step, t.ms, t.detail);
    }
    println!("tables (legacy -> new):");
    for c in &r.counts {
        println!("  {:<16} {:>9} -> {:>9} {}", c.table, c.legacy, c.new, if c.ok { "ok" } else { "MISMATCH" });
    }
    println!("fts parity: {}/{} identical", r.fts.identical, r.fts.queries);
    for m in &r.fts.mismatches {
        println!("  {m}");
    }
    println!("playlist/set checksum: {} {}", if r.playlists_checksum_legacy == r.playlists_checksum_new { "match" } else { "MISMATCH" }, &r.playlists_checksum_new[..12.min(r.playlists_checksum_new.len())]);
    println!("cookie moved: {:?}; undo journals copied: {}; artwork rows awaiting WebP: {}", r.cookie_moved, r.backups_copied, r.art_rows);
    println!("total {} ms, ok = {}", r.total_ms, r.ok);
}
