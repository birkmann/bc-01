use std::path::PathBuf;

/// Environment configuration, same `BC_*` names as the Python app (PLAN §3.4).
/// Runtime-changeable settings live in the `settings` table instead.
#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub library_root: Option<PathBuf>,
    pub download_dir: PathBuf,
    pub host: String,
    pub port: u16,
    pub lan: bool,
    pub bandcamp_dl_bin: String,
    pub download_concurrency: usize,
    pub download_timeout_s: u64,
    pub download_template: String,
    pub harvest_rate_per_sec: f64,
    pub harvest_burst: u32,
    pub harvest_concurrency: usize,
    pub harvest_user_agent: String,
    pub discover_run_limit: usize,
    pub analysis_workers: usize,
    pub ffmpeg_bin: String,
    /// Legacy Python DB for `bc import` (read-only).
    pub legacy_db: Option<PathBuf>,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}
fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Self {
        let data_dir = env("BC_DATA_DIR").map(PathBuf::from).unwrap_or_else(default_data_dir);
        let download_dir = env("BC_DOWNLOAD_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("downloads"));
        Self {
            library_root: env("BC_LIBRARY_ROOT").map(PathBuf::from),
            download_dir,
            host: env("BC_HOST").unwrap_or_else(|| "127.0.0.1".into()),
            port: env_parse("BC_PORT", 8420),
            lan: env_parse("BC_LAN", false),
            bandcamp_dl_bin: env("BC_BANDCAMP_DL_BIN").unwrap_or_else(|| "bandcamp-dl".into()),
            download_concurrency: env_parse::<usize>("BC_DOWNLOAD_CONCURRENCY", 2).clamp(1, 8),
            download_timeout_s: env_parse("BC_DOWNLOAD_TIMEOUT_S", 2700),
            download_template: env("BC_DOWNLOAD_TEMPLATE")
                .unwrap_or_else(|| "%{artist}/%{album}/%{track} - %{title}".into()),
            harvest_rate_per_sec: env_parse("BC_HARVEST_RATE_PER_SEC", 0.67),
            harvest_burst: env_parse("BC_HARVEST_BURST", 5),
            harvest_concurrency: env_parse("BC_HARVEST_CONCURRENCY", 4),
            harvest_user_agent: env("BC_HARVEST_USER_AGENT").unwrap_or_else(|| {
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36".into()
            }),
            discover_run_limit: env_parse("BC_DISCOVER_RUN_LIMIT", 500),
            analysis_workers: env_parse("BC_ANALYSIS_WORKERS", 0),
            ffmpeg_bin: env("BC_FFMPEG_BIN").unwrap_or_else(|| "ffmpeg".into()),
            legacy_db: env("BC_LEGACY_DB").map(PathBuf::from),
            data_dir,
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("library.db")
    }
    pub fn cache_db_path(&self) -> PathBuf {
        self.data_dir.join("cache.db")
    }
    pub fn art_dir(&self) -> PathBuf {
        self.data_dir.join("cache").join("art")
    }
    pub fn waveform_dir(&self) -> PathBuf {
        self.data_dir.join("cache").join("waveforms")
    }
    pub fn backups_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }
    pub fn analysis_threads(&self) -> usize {
        if self.analysis_workers > 0 {
            self.analysis_workers
        } else {
            std::thread::available_parallelism().map(|n| n.get().saturating_sub(1).max(1)).unwrap_or(1)
        }
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        for d in [self.data_dir.clone(), self.art_dir(), self.waveform_dir(), self.backups_dir(), self.download_dir.clone()] {
            std::fs::create_dir_all(d)?;
        }
        Ok(())
    }
}

fn default_data_dir() -> PathBuf {
    // macOS keeps app data under ~/Library/Application Support, not the XDG layout.
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Library/Application Support/bc-rust");
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    // Distinct from the Python app's `bcapp` dir so the original is never touched.
    base.join("bc-rust")
}
