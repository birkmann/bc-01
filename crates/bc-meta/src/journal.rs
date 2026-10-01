//! Append-only undo journal (JSONL), one file per job: `<backups>/metadata/<job_id>.jsonl`.
//! Written before the file it describes and fsynced per batch: an undo entry for a write that never
//! happened is harmless; a write with no undo entry is not.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;

pub const VERSION: i64 = 1;

pub fn dir(backups: &Path) -> PathBuf {
    backups.join("metadata")
}

pub fn path(backups: &Path, job_id: &str) -> PathBuf {
    // job ids are uuids, but never let one escape the directory
    let safe: String = job_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    dir(backups).join(format!("{safe}.jsonl"))
}

pub struct Writer {
    file: File,
}

impl Writer {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        Ok(Self { file: OpenOptions::new().create(true).append(true).open(path)? })
    }

    pub fn header(&mut self, job_id: &str, params: Value) -> std::io::Result<()> {
        if self.file.metadata()?.len() > 0 {
            return Ok(()); // a resumed job appends to the same file
        }
        self.line(&serde_json::json!({"type": "header", "v": VERSION, "job_id": job_id, "created_at": bc_db::util::iso_now(), "params": params}))
    }

    pub fn entry(&mut self, mut payload: Value) -> std::io::Result<()> {
        if let Some(o) = payload.as_object_mut() {
            o.insert("type".into(), "entry".into());
        }
        self.line(&payload)
    }

    fn line(&mut self, v: &Value) -> std::io::Result<()> {
        let mut s = serde_json::to_string(v).unwrap_or_default();
        s.push('\n');
        self.file.write_all(s.as_bytes())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        self.file.sync_all()
    }
}

/// The entry lines of a journal, skipping the header and any line a kill left half-written.
pub fn read_entries(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() {
                return None;
            }
            match serde_json::from_str::<Value>(l) {
                Ok(v) if v.get("type").and_then(|t| t.as_str()) == Some("entry") => Some(v),
                Ok(_) => None,
                Err(_) => {
                    tracing::warn!(journal = %path.display(), "skipping malformed journal line");
                    None
                }
            }
        })
        .collect()
}
