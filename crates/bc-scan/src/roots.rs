//! Library roots: registration, listing, patching, removal (the one place a path is accepted).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bc_db::rusqlite::{Connection, OptionalExtension, params};
use bc_libcore::{ApiError, ApiResult, Ctx};
use bc_types::library::{RootOut, RootPatch};

/// Bumped on every root add/remove/patch; the watcher compares it to know when to re-read roots.
pub static ROOTS_EPOCH: AtomicU64 = AtomicU64::new(0);

pub(crate) fn bump_epoch() {
    ROOTS_EPOCH.fetch_add(1, Ordering::SeqCst);
}

const SELECT_ROOT: &str = "SELECT r.id, r.path, r.kind, r.enabled, r.watch, r.last_scan_at, r.last_scan_ms,
        (SELECT COUNT(*) FROM files f WHERE f.root_id = r.id) FROM library_roots r";

fn map_root(r: &bc_db::rusqlite::Row<'_>) -> bc_db::rusqlite::Result<RootOut> {
    Ok(RootOut {
        id: r.get(0)?,
        path: r.get(1)?,
        kind: r.get(2)?,
        enabled: r.get(3)?,
        watch: r.get(4)?,
        last_scan_at: r.get::<_, Option<String>>(5)?.map(|s| bc_db::util::iso(&s)),
        last_scan_ms: r.get(6)?,
        track_count: r.get(7)?,
    })
}

/// All roots with their file counts.
pub fn list_roots(c: &Connection) -> ApiResult<Vec<RootOut>> {
    let mut st = c.prepare(&format!("{SELECT_ROOT} ORDER BY r.id"))?;
    Ok(st.query_map([], map_root)?.collect::<Result<_, _>>()?)
}

pub fn get_root(c: &Connection, id: i64) -> ApiResult<Option<RootOut>> {
    Ok(c.query_row(&format!("{SELECT_ROOT} WHERE r.id = ?1"), [id], map_root).optional()?)
}

/// Register a root: canonicalised, must be an existing directory, idempotent for a known path.
pub fn add_root(ctx: &Ctx, path: &str, kind: &str) -> ApiResult<RootOut> {
    if kind != "library" && kind != "downloads" {
        return Err(ApiError::bad(format!("unknown root kind: {kind}")));
    }
    let p = expand_tilde(path);
    let resolved = std::fs::canonicalize(&p).map_err(|_| ApiError::bad(format!("path does not exist: {path}")))?;
    if !resolved.is_dir() {
        return Err(ApiError::bad(format!("not a directory: {}", resolved.display())));
    }
    let rp = resolved.to_string_lossy().into_owned();
    let kind = kind.to_string();
    let id = ctx.write(move |tx| {
        let existing: Option<i64> =
            tx.query_row("SELECT id FROM library_roots WHERE path = ?1", [&rp], |r| r.get(0)).optional()?;
        if let Some(id) = existing {
            return Ok(id);
        }
        tx.execute("INSERT INTO library_roots(path, kind, watch, enabled) VALUES (?1, ?2, 0, 1)", params![rp, kind])?;
        Ok(tx.last_insert_rowid())
    })?;
    bump_epoch();
    ctx.read(|c| get_root(c, id))?.ok_or_else(|| ApiError::internal("root vanished"))
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// Remove a root; its `files` rows cascade (tracks/releases stay, as in the legacy app).
pub fn remove_root(ctx: &Ctx, id: i64) -> ApiResult<()> {
    let n = ctx.write(move |tx| Ok(tx.execute("DELETE FROM library_roots WHERE id = ?1", [id])?))?;
    if n == 0 {
        return Err(ApiError::not_found(format!("root {id} not found")));
    }
    bump_epoch();
    ctx.bus.invalidate("track", vec![]);
    ctx.bus.publish(bc_types::library::TOPIC_LIBRARY_CHANGED, &bc_types::library::LibraryChanged::default());
    Ok(())
}

pub fn patch_root(ctx: &Ctx, id: i64, patch: RootPatch) -> ApiResult<RootOut> {
    let n = ctx.write(move |tx| {
        let mut n = 0;
        if let Some(e) = patch.enabled {
            n += tx.execute("UPDATE library_roots SET enabled = ?1 WHERE id = ?2", params![e, id])?;
        }
        if let Some(w) = patch.watch {
            n += tx.execute("UPDATE library_roots SET watch = ?1 WHERE id = ?2", params![w, id])?;
        }
        if patch.enabled.is_none() && patch.watch.is_none() {
            n += tx.query_row("SELECT COUNT(*) FROM library_roots WHERE id = ?1", [id], |r| r.get::<_, i64>(0))? as usize;
        }
        Ok(n)
    })?;
    if n == 0 {
        return Err(ApiError::not_found(format!("root {id} not found")));
    }
    bump_epoch();
    ctx.read(|c| get_root(c, id))?.ok_or_else(|| ApiError::not_found(format!("root {id} not found")))
}

/// Register the configured library root (`kind='library'`) and download dir (`kind='downloads'`)
/// when they are directories (port of `_ensure_root`). Idempotent. Returns the root ids touched.
pub fn ensure_roots(ctx: &Ctx) -> ApiResult<Vec<i64>> {
    let mut ids = Vec::new();
    let mut candidates: Vec<(&Path, &str)> = Vec::new();
    if let Some(p) = ctx.config.library_root.as_deref() {
        candidates.push((p, "library"));
    }
    candidates.push((ctx.config.download_dir.as_path(), "downloads"));
    for (p, kind) in candidates {
        if p.is_dir() {
            match add_root(ctx, &p.to_string_lossy(), kind) {
                Ok(r) => ids.push(r.id),
                Err(e) => tracing::warn!(path = %p.display(), error = %e, "cannot register root"),
            }
        }
    }
    Ok(ids)
}
