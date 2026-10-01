//! Optional essentia Python sidecar: used for tracks that have NO BPM/key yet (new downloads)
//! because the native key/BPM do not reproduce essentia closely (PLAN 7.2 gate missed).
//!
//! A small pool of persistent niced worker processes (`sidecar/essentia_worker.py`) speaks JSON
//! lines over stdin/stdout; workers hold no DB handle. Python is auto-detected
//! (`BC_ESSENTIA_PYTHON`, else `web/backend/.venv/bin/python` found upward from the cwd or the
//! executable) and probed once for `import essentia`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use serde::Deserialize;

pub const VENV_PYTHON: &str = "web/backend/.venv/bin/python";
const WORKER_PY: &str = include_str!("../sidecar/essentia_worker.py");

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SidecarResult {
    pub bpm: Option<f64>,
    pub bpm_confidence: Option<f64>,
    pub beat_offset_ms: Option<f64>,
    #[serde(default)]
    pub bpm_candidates: Vec<f64>,
    pub key_root: Option<i64>,
    pub key_mode: Option<String>,
    pub key_confidence: Option<f64>,
    pub duration_s: Option<f64>,
    pub error: Option<String>,
}

impl SidecarResult {
    pub fn camelot(&self) -> Option<&'static str> {
        bc_music::camelot::to_camelot_opt(self.key_root.map(|k| k as i32), self.key_mode.as_deref())
    }
}

fn find_python() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("BC_ESSENTIA_PYTHON") {
        let p = PathBuf::from(p);
        return p.exists().then_some(p);
    }
    let mut starts: Vec<PathBuf> = Vec::new();
    if let Ok(c) = std::env::current_dir() {
        starts.push(c);
    }
    if let Ok(e) = std::env::current_exe() {
        if let Some(d) = e.parent() {
            starts.push(d.to_path_buf());
        }
    }
    for s in starts {
        for dir in s.ancestors() {
            let cand = dir.join(VENV_PYTHON);
            if cand.exists() {
                return Some(cand);
            }
        }
    }
    None
}

/// The python interpreter that can `import essentia`, probed once per process.
pub fn python() -> Option<&'static Path> {
    static P: OnceLock<Option<PathBuf>> = OnceLock::new();
    P.get_or_init(|| {
        let py = find_python()?;
        let ok = Command::new(&py)
            .args(["-c", "import essentia.standard"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        ok.then_some(py)
    })
    .as_deref()
}

pub fn available() -> bool {
    python().is_some()
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

pub struct EssentiaSidecar {
    python: PathBuf,
    script: PathBuf,
    workers: Vec<Mutex<Option<Worker>>>,
    next: AtomicUsize,
}

fn spawn(python: &Path, script: &Path) -> std::io::Result<Worker> {
    let mut cmd = Command::new(python);
    cmd.arg(script).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: only async-signal-safe libc call between fork and exec; niced like the pool.
        unsafe {
            cmd.pre_exec(|| {
                libc::setpriority(libc::PRIO_PROCESS, 0, crate::pool::WORKER_NICE);
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;
    let stdout = BufReader::new(child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?);
    Ok(Worker { child, stdin, stdout })
}

impl EssentiaSidecar {
    /// `None` when no essentia python is available. `workers` processes are started lazily.
    pub fn detect(workers: usize) -> Option<Self> {
        let python = python()?.to_path_buf();
        let dir = std::env::temp_dir().join(format!("bc-essentia-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let script = dir.join("essentia_worker.py");
        std::fs::write(&script, WORKER_PY).ok()?;
        Some(Self {
            python,
            script,
            workers: (0..workers.max(1)).map(|_| Mutex::new(None)).collect(),
            next: AtomicUsize::new(0),
        })
    }

    /// Blocking: analyse one file (round-robins over the workers; respawns a dead worker once).
    pub fn analyze(&self, path: &Path) -> Result<SidecarResult, String> {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let mut slot = self.workers[i].lock();
        let req = format!("{}\n", serde_json::json!({ "id": 0, "path": path.to_string_lossy() }));
        for attempt in 0..2 {
            if slot.is_none() {
                *slot = Some(spawn(&self.python, &self.script).map_err(|e| e.to_string())?);
            }
            let w = slot.as_mut().expect("spawned above");
            let mut line = String::new();
            let ok = w.stdin.write_all(req.as_bytes()).and_then(|_| w.stdin.flush()).is_ok()
                && matches!(w.stdout.read_line(&mut line), Ok(n) if n > 0);
            if ok {
                let r: SidecarResult = serde_json::from_str(&line).map_err(|e| e.to_string())?;
                return match r.error.clone() {
                    Some(e) => Err(e),
                    None => Ok(r),
                };
            }
            if let Some(mut dead) = slot.take() {
                let _ = dead.child.kill();
                let _ = dead.child.wait();
            }
            if attempt == 1 {
                break;
            }
        }
        Err("essentia worker died".into())
    }
}

impl Drop for EssentiaSidecar {
    fn drop(&mut self) {
        for w in &self.workers {
            if let Some(mut w) = w.lock().take() {
                let _ = w.child.kill();
                let _ = w.child.wait();
            }
        }
    }
}
