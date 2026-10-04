//! On-disk LRU store of `.bcw2` files (native only). Files are sharded as
//! `{dir}/{id/1000}/{id}.bcw2`; when the total size exceeds the cap the least recently used
//! files are rewritten overview-only (the overview is never deleted).

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, TryLockError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::format::{
    EncodeOpts, HEADER_LEN, Header, Waveform, WaveformError, decode_block, read_header,
};

/// blake3 of `size || first 64 KiB || last 64 KiB`, truncated to 16 bytes.
pub fn source_hash(path: &Path) -> io::Result<[u8; 16]> {
    const CH: u64 = 64 * 1024;
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    let mut h = blake3::Hasher::new();
    h.update(&size.to_le_bytes());
    let mut buf = vec![0u8; CH.min(size) as usize];
    f.read_exact(&mut buf)?;
    h.update(&buf);
    if size > 0 {
        f.seek(SeekFrom::Start(size.saturating_sub(CH)))?;
        let n = CH.min(size) as usize;
        buf.resize(n, 0);
        f.read_exact(&mut buf)?;
        h.update(&buf);
    }
    let mut out = [0u8; 16];
    out.copy_from_slice(&h.finalize().as_bytes()[..16]);
    Ok(out)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreStats {
    pub files: u64,
    pub bytes: u64,
    pub overview_only_files: u64,
    pub cap_bytes: u64,
}

/// Eviction stops at this share of the cap (per mille), so a cache sitting at the cap rescans its
/// files once per ~10% of new data rather than on every put.
const EVICT_TO_PERMILLE: u64 = 900;

pub struct WaveformStore {
    dir: PathBuf,
    cap_bytes: u64,
    /// Guards accounting and file writes. `None` = not scanned yet.
    state: Mutex<Option<u64>>,
    /// Held for a whole eviction pass: one at a time, without blocking puts.
    evicting: Mutex<()>,
    tmp_counter: AtomicU64,
}

impl WaveformStore {
    pub fn new(dir: impl Into<PathBuf>, cap_bytes: u64) -> Self {
        Self {
            dir: dir.into(),
            cap_bytes,
            state: Mutex::new(None),
            evicting: Mutex::new(()),
            tmp_counter: AtomicU64::new(0),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    pub fn path_for(&self, track_id: i64) -> PathBuf {
        self.dir
            .join((track_id / 1000).to_string())
            .join(format!("{track_id}.bcw2"))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<u64>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn scan(&self) -> Vec<(PathBuf, u64, SystemTime)> {
        let mut out = Vec::new();
        let Ok(shards) = fs::read_dir(&self.dir) else {
            return out;
        };
        for shard in shards.flatten() {
            let Ok(files) = fs::read_dir(shard.path()) else {
                continue;
            };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().is_some_and(|e| e == "bcw2")
                    && let Ok(m) = f.metadata()
                {
                    out.push((p, m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
                }
            }
        }
        out
    }

    fn total_locked(&self, st: &mut Option<u64>) -> u64 {
        *st.get_or_insert_with(|| self.scan().iter().map(|x| x.1).sum())
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp.{}.{n}", std::process::id()));
        let res = (|| {
            let mut f = File::create(&tmp)?;
            f.write_all(bytes)?;
            f.flush()?;
            fs::rename(&tmp, path)
        })();
        if res.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        res
    }

    /// Store a waveform (compressed, whatever levels it holds). May trigger eviction.
    pub fn put(&self, track_id: i64, w: &Waveform) -> io::Result<u64> {
        let bytes = w.to_bytes(&EncodeOpts::file());
        let path = self.path_for(track_id);
        let over = {
            let mut st = self.lock();
            let total = self.total_locked(&mut st);
            let old = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            self.write_atomic(&path, &bytes)?;
            let new_total = total.saturating_sub(old) + bytes.len() as u64;
            *st = Some(new_total);
            new_total > self.cap_bytes
        };
        if over {
            self.evict_to_cap()?;
        }
        Ok(bytes.len() as u64)
    }

    fn touch(&self, path: &Path) {
        if let Ok(f) = File::options().write(true).open(path) {
            let _ = f.set_modified(SystemTime::now());
        }
    }

    /// Full read (both levels if present). Marks the file as recently used.
    pub fn get(&self, track_id: i64) -> Option<Waveform> {
        let path = self.path_for(track_id);
        let bytes = fs::read(&path).ok()?;
        let w = Waveform::from_bytes(&bytes).ok()?;
        self.touch(&path);
        Some(w)
    }

    /// Reads only the header and the overview block.
    pub fn get_overview_only(&self, track_id: i64) -> Option<Waveform> {
        let path = self.path_for(track_id);
        let mut f = File::open(&path).ok()?;
        let mut head = [0u8; HEADER_LEN];
        f.read_exact(&mut head).ok()?;
        let h = read_header(&head).ok()?;
        if !h.has_overview() {
            return None;
        }
        let mut block = vec![0u8; h.overview_len as usize];
        f.read_exact(&mut block).ok()?;
        let overview = decode_block(&block, h.overview_points as usize, h.is_raw()).ok()?;
        self.touch(&path);
        Some(Waveform {
            sample_rate: h.sample_rate,
            hop_samples: h.hop_samples,
            total_samples: h.total_samples,
            source_hash: h.source_hash,
            overview,
            detail: None,
        })
    }

    /// Header only (64 bytes read).
    pub fn header(&self, track_id: i64) -> Option<Header> {
        let mut f = File::open(self.path_for(track_id)).ok()?;
        let mut head = [0u8; HEADER_LEN];
        f.read_exact(&mut head).ok()?;
        read_header(&head).ok()
    }

    pub fn remove(&self, track_id: i64) -> io::Result<bool> {
        let path = self.path_for(track_id);
        let mut st = self.lock();
        let total = self.total_locked(&mut st);
        let len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        match fs::remove_file(&path) {
            Ok(()) => {
                *st = Some(total.saturating_sub(len));
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// When over the cap, rewrite least-recently-used files overview-only until the total is down
    /// to [`EVICT_TO_PERMILLE`] of it. Returns
    /// the number of files downgraded. Files that are already overview-only are left alone.
    pub fn evict_to_cap(&self) -> io::Result<usize> {
        let _pass = match self.evicting.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
            // another pass is running; puts made meanwhile are accounted and caught by the next one
            Err(TryLockError::WouldBlock) => return Ok(0),
        };
        let (mut files, mut total) = {
            let mut st = self.lock();
            let files = self.scan();
            let total: u64 = files.iter().map(|x| x.1).sum();
            *st = Some(total);
            (files, total)
        };
        if total <= self.cap_bytes {
            return Ok(0);
        }
        let target = self.cap_bytes / 1000 * EVICT_TO_PERMILLE;
        files.sort_by_key(|x| x.2);
        let mut downgraded = 0;
        for (path, len, mtime) in files {
            if total <= target {
                break;
            }
            // decode and re-encode outside the lock: puts keep flowing meanwhile
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(h) = read_header(&bytes) else { continue };
            if !h.has_detail() {
                continue;
            }
            let Ok(w) = Waveform::from_bytes(&bytes) else {
                continue;
            };
            let small = w.to_bytes(&EncodeOpts::file_overview_only());
            let mut st = self.lock();
            // rewritten or read since the scan: no longer this LRU entry
            if fs::metadata(&path).and_then(|m| m.modified()).ok() != Some(mtime) {
                continue;
            }
            self.write_atomic(&path, &small)?;
            // keep the LRU order stable for the rewritten file
            if let Ok(f) = File::options().write(true).open(&path) {
                let _ = f.set_modified(mtime);
            }
            let cur = self.total_locked(&mut st);
            *st = Some(cur.saturating_sub(len) + small.len() as u64);
            total = total.saturating_sub(len) + small.len() as u64;
            downgraded += 1;
        }
        Ok(downgraded)
    }

    /// A snapshot; files are replaced by rename, so no lock is needed (and none is held while every
    /// header is read).
    pub fn stats(&self) -> StoreStats {
        let files = self.scan();
        let mut s = StoreStats {
            cap_bytes: self.cap_bytes,
            ..Default::default()
        };
        for (p, len, _) in files {
            s.files += 1;
            s.bytes += len;
            if let Ok(mut f) = File::open(&p) {
                let mut head = [0u8; HEADER_LEN];
                if f.read_exact(&mut head).is_ok()
                    && read_header(&head).is_ok_and(|h| !h.has_detail())
                {
                    s.overview_only_files += 1;
                }
            }
        }
        s
    }
}

impl From<WaveformError> for io::Error {
    fn from(e: WaveformError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}
