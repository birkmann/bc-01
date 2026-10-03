//! `/library/excluded`: the files removed from the library (kept on disk), and letting them back.
//! The list itself lives in [`bc_maint::excluded`]; restoring is here because it ingests.

use std::collections::BTreeMap;
use std::path::PathBuf;

use axum::Json;
use axum::extract::State;
use bc_libcore::{ApiError, ApiResult, Ctx};
use bc_types::library::maint::{ExcludedOut, RestoreExcludedRequest, RestoredOut};

use crate::ingest::{IngestOptions, best_root, ingest_paths, load_roots};

/// Lift the exclusion of `paths` and ingest the ones still on disk, root by root. A path that is
/// gone, or no longer under a root, is lifted and adds nothing.
pub fn restore(ctx: &Ctx, paths: &[String]) -> ApiResult<RestoredOut> {
    if paths.is_empty() {
        return Err(ApiError::bad("no paths given"));
    }
    let ps = paths.to_vec();
    let restored = ctx.write(move |t| bc_maint::excluded::remove(t, &ps))?;
    let roots = ctx.read(load_roots)?;
    let mut by_root: BTreeMap<i64, Vec<PathBuf>> = BTreeMap::new();
    for p in paths {
        let path = PathBuf::from(p);
        if !path.is_file() {
            continue;
        }
        if let Some(r) = best_root(&roots, &path) {
            by_root.entry(r.id).or_default().push(path);
        }
    }
    let mut out = RestoredOut { restored, ..Default::default() };
    for (root_id, files) in by_root {
        let rep = ingest_paths(ctx, root_id, &files, &IngestOptions::default())?;
        out.tracks_added += rep.tracks_added;
        out.errors.extend(rep.errors);
    }
    Ok(out)
}

pub(crate) async fn list(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<ExcludedOut>>> {
    Ok(Json(ctx.read_async(bc_maint::excluded::list).await?))
}

pub(crate) async fn restore_route(State(ctx): State<Ctx>, Json(body): Json<RestoreExcludedRequest>) -> ApiResult<Json<RestoredOut>> {
    let out = tokio::task::spawn_blocking(move || restore(&ctx, &body.paths)).await.map_err(ApiError::internal)??;
    Ok(Json(out))
}
