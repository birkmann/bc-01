//! Small shared helpers.

use std::collections::HashSet;

use bc_db::rusqlite::{Connection, OptionalExtension, Transaction};
use bc_libcore::ApiResult;

/// `[1,2,3]` as JSON text for `IN (SELECT value FROM json_each(?))`: any number of ids in one
/// statement, no 999-parameter ceiling.
pub fn ids_json(ids: &[i64]) -> String {
    serde_json::to_string(ids).unwrap_or_else(|_| "[]".into())
}

/// A JSON array of strings, for the same `json_each` trick.
pub fn strs_json<S: AsRef<str>>(vals: &[S]) -> String {
    let v: Vec<&str> = vals.iter().map(|s| s.as_ref()).collect();
    serde_json::to_string(&v).unwrap_or_else(|_| "[]".into())
}

pub fn dedup_sorted(ids: impl IntoIterator<Item = i64>) -> Vec<i64> {
    let set: HashSet<i64> = ids.into_iter().collect();
    let mut v: Vec<i64> = set.into_iter().collect();
    v.sort_unstable();
    v
}

pub fn setting(c: &Connection, key: &str) -> ApiResult<Option<String>> {
    Ok(c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get::<_, String>(0)).optional()?)
}

pub fn set_setting(t: &Transaction<'_>, key: &str, value: &str) -> ApiResult<()> {
    t.execute(
        "INSERT INTO settings(key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        (key, value, bc_db::util::now_db()),
    )?;
    Ok(())
}

/// Whether `path` is audio by extension (used to decide a folder "still holds music").
pub fn has_audio_ext(path: &std::path::Path) -> bool {
    bc_core::audio::is_audio_path(path)
}

/// Canonicalise the longest existing prefix of `p` (so a path that was just deleted can still be
/// compared with a root). Mirrors Python's non-strict `Path.resolve()`.
pub fn canon(p: &std::path::Path) -> std::path::PathBuf {
    let mut existing = p.to_path_buf();
    let mut tail = Vec::new();
    while !existing.exists() {
        match (existing.file_name().map(|s| s.to_owned()), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = existing.canonicalize().unwrap_or(existing);
    for t in tail.into_iter().rev() {
        out.push(t);
    }
    out
}

/// Canonical paths of every registered library root.
pub fn root_paths(c: &Connection) -> ApiResult<Vec<std::path::PathBuf>> {
    let mut st = c.prepare("SELECT path FROM library_roots")?;
    let v: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
    Ok(v.into_iter().map(|p| canon(std::path::Path::new(&p))).collect())
}

/// Whether `path` (already canonical) lies under (or equals) one of the roots.
pub fn under_a_root(path: &std::path::Path, roots: &[std::path::PathBuf]) -> bool {
    roots.iter().any(|r| path.starts_with(r))
}

/// Strictly below a root (the root itself never qualifies).
pub fn strictly_under_a_root(path: &std::path::Path, roots: &[std::path::PathBuf]) -> bool {
    roots.iter().any(|r| path.starts_with(r) && path != r)
}
