//! SQLite access (PLAN §3.3): exactly one writer thread that wraps every write in
//! `BEGIN IMMEDIATE`, plus a pool of read-only WAL connections.
//!
//! Contract used by every other crate:
//! * `db.read(|c| ...)` / `db.read_async(...)` — read-only connection, never on a tokio worker.
//! * `db.write(|tx| ...)` / `db.write_async(...)` — runs on the writer thread inside
//!   `BEGIN IMMEDIATE ... COMMIT`; an `Err` rolls back. Batch background writes in
//!   groups of ~200 rows per call.

pub mod fts;
pub mod migrate;
pub mod util;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
pub use rusqlite;
use rusqlite::{Connection, OpenFlags, Transaction};

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("{0}")]
    Other(String),
    #[error("database worker stopped")]
    Closed,
}

pub type Result<T, E = DbError> = std::result::Result<T, E>;

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send>;

struct Inner {
    path: PathBuf,
    writer: Sender<WriteJob>,
    readers_tx: Sender<Connection>,
    readers_rx: Receiver<Connection>,
    /// Bumped after every committed write: cheap cache invalidation for derived counts.
    generation: AtomicU64,
}

/// Cheap to clone; share one per process.
#[derive(Clone)]
pub struct Db(Arc<Inner>);

pub const READERS: usize = 8;

fn apply_pragmas(c: &Connection, write: bool) -> rusqlite::Result<()> {
    c.busy_timeout(Duration::from_millis(15_000))?;
    // Readers are many (8) and mostly page through mmap: a small private cache each keeps server RSS low.
    // The single writer keeps a larger cache for batch work.
    let (cache_kib, mmap) = if write { (65536, 268_435_456u64) } else { (16384, 268_435_456u64) };
    c.execute_batch(&format!(
        "PRAGMA foreign_keys=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-{cache_kib}; PRAGMA mmap_size={mmap};"
    ))?;
    if write {
        c.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA wal_autocheckpoint=2000;",
        )?;
    }
    Ok(())
}

impl Db {
    /// Open (creating if needed) and migrate. Spawns the writer thread.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| DbError::Other(e.to_string()))?;
        }
        let mut wconn = Connection::open(&path)?;
        apply_pragmas(&wconn, true)?;
        migrate::migrate(&mut wconn)?;

        let (wtx, wrx) = unbounded::<WriteJob>();
        std::thread::Builder::new()
            .name("bc-db-writer".into())
            .spawn(move || {
                while let Ok(job) = wrx.recv() {
                    job(&mut wconn);
                }
                let _ = wconn.execute_batch("PRAGMA optimize;");
            })
            .map_err(|e| DbError::Other(e.to_string()))?;

        let (rtx, rrx) = bounded(READERS);
        for _ in 0..READERS {
            let c = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            apply_pragmas(&c, false)?;
            rtx.send(c).map_err(|_| DbError::Closed)?;
        }
        Ok(Db(Arc::new(Inner { path, writer: wtx, readers_tx: rtx, readers_rx: rrx, generation: AtomicU64::new(1) })))
    }

    /// Monotonic counter bumped after every committed write.
    pub fn generation(&self) -> u64 {
        self.0.generation.load(Ordering::SeqCst)
    }

    pub fn path(&self) -> &Path {
        &self.0.path
    }

    /// Blocking read on a pooled read-only connection.
    pub fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.0.readers_rx.recv().map_err(|_| DbError::Closed)?;
        let out = f(&conn);
        let _ = self.0.readers_tx.send(conn);
        out
    }

    /// Blocking write on the single writer thread inside `BEGIN IMMEDIATE`.
    pub fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = bounded(1);
        let me = self.clone();
        let job: WriteJob = Box::new(move |conn: &mut Connection| {
            let res = (|| {
                let t = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let v = f(&t)?;
                t.commit()?;
                me.0.generation.fetch_add(1, Ordering::SeqCst);
                Ok(v)
            })();
            let _ = tx.send(res);
        });
        self.0.writer.send(job).map_err(|_| DbError::Closed)?;
        rx.recv().map_err(|_| DbError::Closed)?
    }

    pub async fn read_async<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.read(f)).await.map_err(|e| DbError::Other(e.to_string()))?
    }

    pub async fn write_async<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let db = self.clone();
        tokio::task::spawn_blocking(move || db.write(f)).await.map_err(|e| DbError::Other(e.to_string()))?
    }
}

impl Db {
    /// Like [`Db::read`] but with the caller's own error type (anything `From<DbError>`).
    pub fn read_with<T, E: From<DbError>>(&self, f: impl FnOnce(&Connection) -> std::result::Result<T, E>) -> std::result::Result<T, E> {
        let conn = self.0.readers_rx.recv().map_err(|_| E::from(DbError::Closed))?;
        let out = f(&conn);
        let _ = self.0.readers_tx.send(conn);
        out
    }

    /// Like [`Db::write`] but with the caller's own error type: `Err` rolls the transaction back.
    pub fn write_with<T, E>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> std::result::Result<T, E> + Send + 'static,
    ) -> std::result::Result<T, E>
    where
        T: Send + 'static,
        E: From<DbError> + From<rusqlite::Error> + Send + 'static,
    {
        let (tx, rx) = bounded(1);
        let me = self.clone();
        let job: WriteJob = Box::new(move |conn: &mut Connection| {
            let res = (|| {
                let t = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(E::from)?;
                let v = f(&t)?;
                t.commit().map_err(E::from)?;
                me.0.generation.fetch_add(1, Ordering::SeqCst);
                Ok(v)
            })();
            let _ = tx.send(res);
        });
        self.0.writer.send(job).map_err(|_| E::from(DbError::Closed))?;
        rx.recv().map_err(|_| E::from(DbError::Closed))?
    }
}

impl Db {
    /// Background writes in batches (~200 rows per transaction, PLAN §3.3): `f` runs once per chunk
    /// on the writer thread, each chunk in its own `BEGIN IMMEDIATE`, so interactive writes interleave.
    pub fn write_chunks<I, R>(
        &self,
        items: Vec<I>,
        chunk: usize,
        f: impl Fn(&Transaction<'_>, &[I]) -> Result<R> + Send + Sync + 'static,
    ) -> Result<Vec<R>>
    where
        I: Send + Sync + 'static,
        R: Send + 'static,
    {
        let f = Arc::new(f);
        let items = Arc::new(items);
        let chunk = chunk.max(1);
        let mut out = Vec::new();
        let mut start = 0;
        while start < items.len() {
            let end = (start + chunk).min(items.len());
            let (f, items) = (f.clone(), items.clone());
            out.push(self.write(move |t| f(t, &items[start..end]))?);
            start = end;
        }
        Ok(out)
    }

    /// Run `f` on the writer thread with the raw writer connection (no transaction): for operations that
    /// need the connection itself, e.g. restoring a backup over the live database.
    pub fn with_writer<T: Send + 'static>(&self, f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static) -> Result<T> {
        let (tx, rx) = bounded(1);
        let me = self.clone();
        let job: WriteJob = Box::new(move |conn: &mut Connection| {
            let r = f(conn);
            me.0.generation.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(r);
        });
        self.0.writer.send(job).map_err(|_| DbError::Closed)?;
        rx.recv().map_err(|_| DbError::Closed)?
    }

    /// Checkpoint the WAL (truncate). Runs on the writer thread.
    pub fn checkpoint(&self) -> Result<()> {
        let (tx, rx) = bounded(1);
        let job: WriteJob = Box::new(move |conn: &mut Connection| {
            let r = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").map_err(DbError::from);
            let _ = tx.send(r);
        });
        self.0.writer.send(job).map_err(|_| DbError::Closed)?;
        rx.recv().map_err(|_| DbError::Closed)?
    }
}

/// Settings key/value helpers (legacy `settings` table: key, value JSON text).
pub mod settings {
    use super::*;
    use rusqlite::OptionalExtension;

    pub fn get(c: &Connection, key: &str) -> Result<Option<String>> {
        Ok(c.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten())
    }
    pub fn set(t: &Transaction<'_>, key: &str, value: &str) -> Result<()> {
        t.execute(
            "INSERT INTO settings(key, value, updated_at) VALUES (?1, ?2, CURRENT_TIMESTAMP)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            [key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_through_separate_connection() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        db.write(|t| {
            t.execute("INSERT INTO tags(name, name_key, kind, track_count) VALUES ('Techno','techno','genre',0)", [])?;
            Ok(())
        })
        .unwrap();
        let n: i64 = db.read(|c| Ok(c.query_row("SELECT count(*) FROM tags", [], |r| r.get(0))?)).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn failed_write_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        let r: Result<()> = db.write(|t| {
            t.execute("INSERT INTO tags(name, name_key, kind, track_count) VALUES ('A','a','genre',0)", [])?;
            Err(DbError::Other("boom".into()))
        });
        assert!(r.is_err());
        let n: i64 = db.read(|c| Ok(c.query_row("SELECT count(*) FROM tags", [], |r| r.get(0))?)).unwrap();
        assert_eq!(n, 0);
    }
}

/// `ui_state` key/value store (themes, column prefs, player queue; shared by desktop and phone).
pub mod ui_state {
    use super::*;
    use rusqlite::OptionalExtension;

    pub fn get(c: &Connection, key: &str) -> Result<Option<String>> {
        Ok(c.query_row("SELECT value FROM ui_state WHERE key=?1", [key], |r| r.get::<_, String>(0)).optional()?)
    }
    pub fn set(t: &Transaction<'_>, key: &str, value: &str) -> Result<()> {
        t.execute(
            "INSERT INTO ui_state(key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
            rusqlite::params![key, value, util::now_db()],
        )?;
        Ok(())
    }
    pub fn delete(t: &Transaction<'_>, key: &str) -> Result<()> {
        t.execute("DELETE FROM ui_state WHERE key=?1", [key])?;
        Ok(())
    }
}
