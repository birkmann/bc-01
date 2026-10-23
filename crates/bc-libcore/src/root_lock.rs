//! Exclusive work on a library root. A move, a scan and a watcher ingest each hold the roots they
//! touch, so they never run over the same files at once: two moves of one root (the second one
//! re-moved files the first had already moved, onto themselves, and deleted them), or a watcher
//! ingest racing a move for the rows it is rewriting.
//!
//! Keyed by database file, so that several databases in one process (tests) do not share locks.

use std::collections::BTreeMap;
use std::path::PathBuf;

use bc_db::Db;
use parking_lot::Mutex;

static HELD: Mutex<BTreeMap<(PathBuf, i64), &'static str>> = Mutex::new(BTreeMap::new());

/// Held roots, released on drop.
#[derive(Debug)]
pub struct RootGuard {
    keys: Vec<(PathBuf, i64)>,
}

impl Drop for RootGuard {
    fn drop(&mut self) {
        let mut held = HELD.lock();
        for k in &self.keys {
            held.remove(k);
        }
    }
}

/// Take every root in `ids` for `purpose` (`"move"`, `"scan"`, `"watch"`), or none of them:
/// `Err((root_id, purpose))` names a root already held and what holds it.
pub fn try_lock(db: &Db, ids: &[i64], purpose: &'static str) -> Result<RootGuard, (i64, &'static str)> {
    let mut held = HELD.lock();
    let mut keys: Vec<(PathBuf, i64)> = ids.iter().map(|id| (db.path().to_path_buf(), *id)).collect();
    if let Some(p) = keys.iter().find_map(|k| held.get(k).map(|p| (k.1, *p))) {
        return Err(p);
    }
    keys.sort_unstable();
    keys.dedup();
    for k in &keys {
        held.insert(k.clone(), purpose);
    }
    Ok(RootGuard { keys })
}

/// What holds root `id`, if anything.
pub fn holder(db: &Db, id: i64) -> Option<&'static str> {
    HELD.lock().get(&(db.path().to_path_buf(), id)).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_or_nothing_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let (db, other) = (Db::open(dir.path().join("a.db")).unwrap(), Db::open(dir.path().join("b.db")).unwrap());
        let a = try_lock(&db, &[1, 2], "move").unwrap();
        assert_eq!(try_lock(&db, &[3, 2], "scan").unwrap_err(), (2, "move"));
        assert_eq!(holder(&db, 3), None, "a refused lock takes nothing");
        assert!(try_lock(&other, &[2], "scan").is_ok(), "another database's root 2 is another root");
        drop(a);
        assert!(try_lock(&db, &[2], "scan").is_ok());
        assert_eq!(holder(&db, 2), None);
    }
}
