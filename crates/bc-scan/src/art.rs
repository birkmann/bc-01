//! Cover art conversion: legacy JPEG cache -> WebP set (thumb/medium/full) with hash,
//! blurhash and dominant colour recorded in `artwork`.
//!
//! The importer inserts `artwork(source='legacy', sizes=0)` rows for every release whose cover
//! lives in the old Python JPEG cache. [`convert_legacy_art`] converts them in the background at
//! low priority; [`convert_one`] converts a single release on demand (the art route calls it when
//! it finds a release still on the legacy JPEG).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use bc_db::rusqlite::{OptionalExtension, params};
use bc_libcore::{ApiError, ApiResult, Ctx, JobHandle};
use bc_types::library::{ArtProgress, TOPIC_ART_PROGRESS};
use rayon::prelude::*;

use crate::media;

struct Converted {
    release_id: i64,
    art: media::ProcessedArt,
    mask: u8,
    full: PathBuf,
}

fn convert_file(art_dir: &Path, release_id: i64, src: &Path) -> Result<Converted, String> {
    let data = std::fs::read(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let art = media::process_cover(&data).map_err(|e| e.to_string())?;
    let c = media::CoverArt { source: "legacy-webp", processed: art };
    let mask = media::write_cover_files(art_dir, release_id, &c)?;
    Ok(Converted { release_id, full: media::full_art_path(art_dir, release_id), art: c.processed, mask })
}

fn apply(tx: &bc_db::rusqlite::Transaction<'_>, rows: &[Converted]) -> ApiResult<()> {
    for c in rows {
        tx.prepare_cached(
            "UPDATE artwork SET hash = ?1, version = ?2, blurhash = ?3, color = ?4, width = ?5, height = ?6,
                    sizes = ?7, source = 'legacy-webp', updated_at = CURRENT_TIMESTAMP
              WHERE release_id = ?8",
        )?
        .execute(params![
            c.art.hash,
            media::version_string(&c.art, c.mask),
            c.art.blurhash,
            c.art.color,
            c.art.width,
            c.art.height,
            c.mask as i64,
            c.release_id
        ])?;
        tx.prepare_cached("UPDATE releases SET cover_path = ?1 WHERE id = ?2")?
            .execute(params![c.full.to_string_lossy(), c.release_id])?;
    }
    Ok(())
}

/// Convert one release's legacy JPEG (`releases.cover_path`) to WebP now. A no-op `Ok` when the
/// release is already converted or has no legacy artwork row. Blocking.
pub fn convert_one(ctx: &Ctx, release_id: i64) -> ApiResult<()> {
    let row: Option<(Option<String>, i64, Option<String>)> = ctx.read(|c| {
        Ok(c.query_row(
            "SELECT r.cover_path, COALESCE(a.sizes, 0), a.source FROM releases r LEFT JOIN artwork a ON a.release_id = r.id WHERE r.id = ?1",
            [release_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?)
    })?;
    let Some((cover, sizes, source)) = row else { return Err(ApiError::not_found(format!("release {release_id} not found"))) };
    if sizes != 0 || source.as_deref() != Some("legacy") {
        return Ok(());
    }
    let src = PathBuf::from(cover.ok_or_else(|| ApiError::not_found("release has no cover"))?);
    let conv = convert_file(&ctx.config.art_dir(), release_id, &src).map_err(ApiError::internal)?;
    ctx.write(move |tx| apply(tx, &[conv]))?;
    ctx.bus.invalidate("release", vec![release_id]);
    Ok(())
}

const SHARED_FOLDER_ART_MIGRATION: &str = "shared_folder_art_v1";

/// Re-extract the covers of releases that share a folder with other releases (a flat download
/// folder). Older scans took one cover per folder, so every release in such a folder showed the
/// first file's art. Each release now takes the embedded art of its own files; one without any
/// loses its cover (a sidecar image in a shared folder belongs to no release in particular).
/// Runs once (data-migration marker); returns the releases changed. Blocking.
pub fn repair_shared_folder_art(ctx: &Ctx) -> ApiResult<usize> {
    if ctx.read(|c| Ok(bc_db::migrate::data_migration_done(c, SHARED_FOLDER_ART_MIGRATION)?))? {
        return Ok(0);
    }
    let rows: Vec<(i64, String, Vec<String>)> = ctx.read(|c| {
        let mut st = c.prepare(
            "SELECT r.id, r.folder_path FROM releases r JOIN artwork a ON a.release_id = r.id
              WHERE a.source IN ('embedded', 'sidecar')
                AND r.folder_path IN (SELECT folder_path FROM releases WHERE folder_path IS NOT NULL
                                       GROUP BY folder_path HAVING COUNT(*) > 1)
              ORDER BY r.id",
        )?;
        let ids: Vec<(i64, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        let mut files = c.prepare(
            "SELECT f.path FROM files f JOIN tracks t ON t.id = f.track_id
              WHERE t.release_id = ?1 AND f.missing_since IS NULL
              ORDER BY t.disc_no, t.track_no, f.path LIMIT 3",
        )?;
        let mut out = Vec::with_capacity(ids.len());
        for (id, folder) in ids {
            let paths = files.query_map([id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            out.push((id, folder, paths));
        }
        Ok(out)
    })?;
    let art_dir = ctx.config.art_dir();
    let found: Vec<(i64, Option<Converted>)> = rows
        .into_par_iter()
        .map(|(id, folder, paths)| {
            let paths: Vec<&Path> = paths.iter().map(Path::new).collect();
            let conv = media::find_cover(Path::new(&folder), &paths, false).and_then(|c| {
                let mask = media::write_cover_files(&art_dir, id, &c)
                    .map_err(|e| tracing::warn!(release_id = id, error = %e, "writing cover art failed"))
                    .ok()?;
                Some(Converted { release_id: id, full: media::full_art_path(&art_dir, id), mask, art: c.processed })
            });
            (id, conv)
        })
        .collect();
    let (with, without): (Vec<_>, Vec<_>) = found.into_iter().partition(|(_, c)| c.is_some());
    let with: Vec<Converted> = with.into_iter().filter_map(|(_, c)| c).collect();
    let without: Vec<i64> = without.into_iter().map(|(id, _)| id).collect();
    let mut changed: Vec<i64> = with.iter().map(|c| c.release_id).collect();
    changed.extend(&without);
    bc_maint::tidy::delete_artwork(&art_dir, &without);
    ctx.write(move |tx| {
        for c in &with {
            tx.prepare_cached(
                "UPDATE artwork SET hash = ?1, version = ?2, blurhash = ?3, color = ?4, width = ?5, height = ?6,
                        sizes = ?7, source = 'embedded', updated_at = CURRENT_TIMESTAMP
                  WHERE release_id = ?8",
            )?
            .execute(params![
                c.art.hash,
                media::version_string(&c.art, c.mask),
                c.art.blurhash,
                c.art.color,
                c.art.width,
                c.art.height,
                c.mask as i64,
                c.release_id
            ])?;
            tx.prepare_cached("UPDATE releases SET cover_path = ?1 WHERE id = ?2")?.execute(params![c.full.to_string_lossy(), c.release_id])?;
        }
        bc_maint::tidy::delete_artwork_rows(tx, &without)?;
        tx.execute("UPDATE releases SET cover_path = NULL WHERE id IN (SELECT value FROM json_each(?1))", [serde_json::to_string(&without).unwrap_or_default()])?;
        Ok(bc_db::migrate::mark_data_migration(tx, SHARED_FOLDER_ART_MIGRATION)?)
    })?;
    let n = changed.len();
    if n > 0 {
        tracing::info!(releases = n, "re-extracted cover art in shared folders");
        ctx.bus.invalidate("release", changed);
    }
    Ok(n)
}

fn low_priority_pool(threads: usize) -> Option<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("bc-art-{i}"))
        .start_handler(|_| {
            // Nice the converter: interactive reads and the player come first. Failure is harmless.
            // SAFETY: plain syscalls with no pointers; gettid has no preconditions.
            #[cfg(target_os = "linux")]
            unsafe {
                let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
                libc::setpriority(libc::PRIO_PROCESS, tid, 10);
            }
            // macOS has no per-thread nice (PRIO_PROCESS would renice the whole app); the utility
            // QoS class is its equivalent. SAFETY: only affects the calling thread.
            #[cfg(target_os = "macos")]
            unsafe {
                libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
            }
        })
        .build()
        .ok()
}

/// Result of a legacy conversion run.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ArtConvertResult {
    pub converted: i64,
    pub failed: i64,
    pub total: i64,
    pub cancelled: bool,
}

/// Convert every `artwork(source='legacy', sizes=0)` row to WebP, in batches of 200, on a bounded
/// low-priority pool (`analysis_threads / 2`, min 1). Polls `handle.cancelled()` between batches,
/// publishes `library.art.progress` at most 4 times a second and finishes `handle` with an
/// [`ArtConvertResult`]. `legacy_art_dir` is the old `cache/art` directory.
pub async fn convert_legacy_art(ctx: &Ctx, legacy_art_dir: &Path, handle: JobHandle) {
    let (ctx, dir) = (ctx.clone(), legacy_art_dir.to_path_buf());
    let h2 = handle.clone();
    let res = tokio::task::spawn_blocking(move || convert_legacy_blocking(&ctx, &dir, &h2)).await;
    match res {
        Ok(Ok(r)) => handle.finish_ok(serde_json::to_value(&r).unwrap_or_default()),
        Ok(Err(e)) => handle.finish_err(e),
        Err(e) => handle.finish_err(e),
    }
}

/// Blocking body of [`convert_legacy_art`].
pub fn convert_legacy_blocking(ctx: &Ctx, legacy_art_dir: &Path, handle: &JobHandle) -> ApiResult<ArtConvertResult> {
    let ids: Vec<i64> = ctx.read(|c| {
        let mut st = c.prepare("SELECT release_id FROM artwork WHERE source = 'legacy' AND sizes = 0 ORDER BY release_id")?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
    })?;
    let total = ids.len() as i64;
    let mut out = ArtConvertResult { total, ..Default::default() };
    if ids.is_empty() {
        return Ok(out);
    }
    let threads = (ctx.config.analysis_threads() / 2).max(1);
    let pool = low_priority_pool(threads).ok_or_else(|| ApiError::internal("cannot build art pool"))?;
    let art_dir = ctx.config.art_dir();
    let mut last = Instant::now() - Duration::from_secs(1);
    let done = AtomicI64::new(0);
    let _ = AtomicBool::new(false);
    for chunk in ids.chunks(200) {
        if handle.cancelled() {
            out.cancelled = true;
            break;
        }
        // Source path: the legacy cache layout, falling back to what the importer stored.
        let srcs: Vec<(i64, PathBuf)> = ctx.read(|c| {
            let mut v = Vec::with_capacity(chunk.len());
            for id in chunk {
                let cp: Option<String> =
                    c.query_row("SELECT cover_path FROM releases WHERE id = ?1", [id], |r| r.get(0)).optional()?.flatten();
                let legacy = media::legacy_jpeg_path(legacy_art_dir, *id);
                let src = if legacy.exists() { legacy } else { cp.map(PathBuf::from).unwrap_or(legacy) };
                v.push((*id, src));
            }
            Ok(v)
        })?;
        let results: Vec<Result<Converted, String>> =
            pool.install(|| srcs.par_iter().map(|(id, src)| convert_file(&art_dir, *id, src)).collect());
        let mut ok = Vec::new();
        for r in results {
            match r {
                Ok(c) => ok.push(c),
                Err(e) => {
                    out.failed += 1;
                    tracing::debug!(error = %e, "legacy art conversion failed");
                }
            }
        }
        out.converted += ok.len() as i64;
        if !ok.is_empty() {
            ctx.write(move |tx| apply(tx, &ok))?;
        }
        done.fetch_add(chunk.len() as i64, Ordering::Relaxed);
        if last.elapsed() >= Duration::from_millis(250) {
            last = Instant::now();
            let d = done.load(Ordering::Relaxed);
            handle.progress(d, Some(total), None);
            ctx.bus.publish(
                TOPIC_ART_PROGRESS,
                &ArtProgress { job_id: handle.id.clone(), done: d, total, failed: out.failed, finished: false },
            );
        }
    }
    ctx.bus.publish(
        TOPIC_ART_PROGRESS,
        &ArtProgress { job_id: handle.id.clone(), done: done.load(Ordering::Relaxed), total, failed: out.failed, finished: true },
    );
    ctx.bus.invalidate("release", vec![]);
    Ok(out)
}
