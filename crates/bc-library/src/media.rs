//! Media routes: `/stream/{track_id}`, `/art/release/{id}`, `/art/track/{id}`. Media is addressed by id and
//! resolved server-side; no client-supplied path is ever accepted.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bc_libcore::{ApiError, ApiResult, Ctx, Q};
use bc_media::artwork::{ArtSize, art_file_path};
use bc_media::stream;
use bc_db::rusqlite::OptionalExtension;
use serde::Deserialize;

pub use bc_libcore::files::{ResolvedFile, resolve_track_file};

/// Called (best effort, off the request path) when an art request had to fall back to the legacy JPEG,
/// so the lazy WebP conversion can take that release next.
pub type LegacyArtHook = Arc<dyn Fn(i64) + Send + Sync>;

#[derive(Clone)]
struct MediaState {
    ctx: Ctx,
    hook: Option<LegacyArtHook>,
}

pub fn router(ctx: Ctx, hook: Option<LegacyArtHook>) -> Router {
    Router::new()
        .route("/stream/{track_id}", get(stream_track))
        .route("/art/release/{id}", get(release_art))
        .route("/art/track/{id}", get(track_art))
        .with_state(MediaState { ctx, hook })
}

async fn stream_track(State(s): State<MediaState>, method: Method, headers: HeaderMap, Path(track_id): Path<i64>) -> ApiResult<Response> {
    let file = s.ctx.read_async(move |c| resolve_track_file(c, track_id)).await?;
    let mime = stream::audio_mime(file.ext.trim_start_matches('.'));
    Ok(stream::serve_file_method(&method, &file.path, &headers, mime).await)
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ArtQ {
    size: Option<String>,
    #[allow(dead_code)]
    v: Option<String>,
}

struct ArtRow {
    sizes: u8,
    cover_path: Option<String>,
}

fn art_row(c: &bc_db::rusqlite::Connection, id: i64) -> ApiResult<ArtRow> {
    let row: Option<(Option<String>, Option<i64>)> = c
        .query_row(
            "SELECT r.cover_path, aw.sizes FROM releases r LEFT JOIN artwork aw ON aw.release_id = r.id WHERE r.id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((cover_path, sizes)) if cover_path.is_some() => Ok(ArtRow { sizes: sizes.unwrap_or(0) as u8, cover_path }),
        _ => Err(ApiError::not_found(format!("no artwork for release {id}"))),
    }
}

async fn release_art(State(s): State<MediaState>, method: Method, headers: HeaderMap, Path(id): Path<i64>, Q(q): Q<ArtQ>) -> ApiResult<Response> {
    let want = q.size.as_deref().map(|z| ArtSize::parse(z).ok_or_else(|| ApiError::unprocessable("size must be thumb, medium or full"))).transpose()?.unwrap_or(ArtSize::Thumb);
    let row = s.ctx.read_async(move |c| art_row(c, id)).await?;
    let art_dir = s.ctx.config.art_dir();
    // Best available rendition at or near the requested size.
    let order: &[ArtSize] = match want {
        ArtSize::Thumb => &[ArtSize::Thumb, ArtSize::Medium, ArtSize::Full],
        ArtSize::Medium => &[ArtSize::Medium, ArtSize::Thumb, ArtSize::Full],
        ArtSize::Full => &[ArtSize::Full, ArtSize::Medium, ArtSize::Thumb],
    };
    for z in order {
        if row.sizes & z.bit() != 0 {
            let p = art_file_path(&art_dir, id, *z);
            if p.is_file() {
                return Ok(stream::serve_art_method(&method, &p, &headers, "image/webp").await);
            }
        }
    }
    // Not converted yet: the legacy JPEG (`cover_path` is the full one, `_thumb` beside it).
    if let Some(cover) = row.cover_path {
        let full = PathBuf::from(&cover);
        let thumb = {
            let stem = full.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            full.with_file_name(format!("{stem}_thumb.jpg"))
        };
        let candidates = if want == ArtSize::Full { [full.clone(), thumb.clone()] } else { [thumb, full] };
        for p in candidates {
            if p.is_file() {
                if let Some(h) = &s.hook {
                    h(id);
                }
                return Ok(stream::serve_art_method(&method, &p, &headers, "image/jpeg").await);
            }
        }
    }
    Err(ApiError::not_found(format!("artwork file missing for release {id}")))
}

async fn track_art(State(s): State<MediaState>, method: Method, headers: HeaderMap, Path(id): Path<i64>, q: Q<ArtQ>) -> ApiResult<Response> {
    let rid: Option<i64> = s
        .ctx
        .read_async(move |c| Ok(c.query_row("SELECT release_id FROM tracks WHERE id = ?1", [id], |r| r.get::<_, Option<i64>>(0)).optional()?.flatten()))
        .await?;
    let Some(rid) = rid else { return Err(ApiError::not_found(format!("no artwork for track {id}"))) };
    release_art(State(s), method, headers, Path(rid), q).await
}

#[allow(dead_code)]
fn _unused(_: impl IntoResponse) {}
