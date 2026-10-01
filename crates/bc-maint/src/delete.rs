//! Deleting tracks, releases and labels with their files (port of the delete helpers of
//! `api/routes/library.py`).
//!
//! Hard rules: only files under a registered library root are ever unlinked; anything else is
//! refused (logged, and the release reported as failed). Every release is deleted atomically on
//! its own -- the paths are validated before any file is touched, and the rows only go once every
//! file is gone -- so a permission-denied file two albums into a fifty-album sweep keeps its
//! rows and does not roll back the albums that worked.

use std::path::PathBuf;

use bc_db::rusqlite::{Connection, OptionalExtension};
use bc_libcore::{ApiError, ApiResult, Ctx};
use bc_types::library::{DeleteLabelResult, DeleteReleasesResult, DeletedOut, LibraryChanged, TOPIC_LIBRARY_CHANGED};

use crate::util::{canon, ids_json, root_paths, under_a_root};
use crate::{blacklist, tidy};

/// What one purge removed.
#[derive(Debug, Clone, Default)]
pub struct Purged {
    pub track_ids: Vec<i64>,
    pub files: i64,
}

fn track_ids_of(c: &Connection, release_id: i64) -> ApiResult<Vec<i64>> {
    let mut st = c.prepare("SELECT id FROM tracks WHERE release_id = ?1 ORDER BY id")?;
    Ok(st.query_map([release_id], |r| r.get(0))?.collect::<Result<_, _>>()?)
}

/// Unlink every file of these tracks; returns `(files removed, parent dirs touched)`.
/// Validates every path against the registered roots first, so a refusal leaves everything intact.
pub fn delete_files(c: &Connection, track_ids: &[i64]) -> ApiResult<(i64, Vec<PathBuf>)> {
    if track_ids.is_empty() {
        return Ok((0, vec![]));
    }
    let roots = root_paths(c)?;
    let paths: Vec<String> = {
        let mut st = c.prepare("SELECT path FROM files WHERE track_id IN (SELECT value FROM json_each(?1)) ORDER BY id")?;
        st.query_map([ids_json(track_ids)], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    let mut resolved = Vec::with_capacity(paths.len());
    for p in &paths {
        let path = canon(std::path::Path::new(p));
        if !under_a_root(&path, &roots) {
            tracing::warn!(path = %p, "refusing to delete a file outside every library root");
            return Err(ApiError::bad(format!("refusing to delete {p}: outside every library root")));
        }
        resolved.push(path);
    }
    let (mut removed, mut parents) = (0, Vec::new());
    for (p, path) in paths.iter().zip(resolved) {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
                parents.extend(path.parent().map(PathBuf::from));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => parents.extend(path.parent().map(PathBuf::from)),
            Err(e) => {
                tracing::error!(path = %p, error = %e, "could not delete file");
                return Err(ApiError::bad(format!("could not delete {p}")));
            }
        }
    }
    Ok((removed, parents))
}

/// Give back the tag counts these tracks were holding (`tags.track_count` is maintained
/// incrementally on ingest; nothing else decrements it on delete).
pub fn release_tag_counts(c: &Connection, track_ids: &[i64]) -> ApiResult<()> {
    if track_ids.is_empty() {
        return Ok(());
    }
    c.execute(
        "UPDATE tags SET track_count = MAX(0, track_count - (
             SELECT COUNT(*) FROM track_tags tt WHERE tt.tag_id = tags.id AND tt.track_id IN (SELECT value FROM json_each(?1))))
          WHERE id IN (SELECT tag_id FROM track_tags WHERE track_id IN (SELECT value FROM json_each(?1)))",
        [ids_json(track_ids)],
    )?;
    Ok(())
}

/// Delete one release completely: files, rows (tracks, files, tags, history by cascade), FTS
/// rows, the `artwork` row, folder leftovers and cached art. The one shape every "delete this
/// album" path shares, so they cannot drift apart on which things a deletion cleans up. A file
/// that will not go fails before any row is touched, leaving the release for the caller to
/// report and the user to retry.
pub fn purge_release(ctx: &Ctx, release_id: i64) -> ApiResult<Purged> {
    let (folder, track_ids) = ctx.read(|c| {
        let folder: Option<Option<String>> = c.query_row("SELECT folder_path FROM releases WHERE id = ?1", [release_id], |r| r.get(0)).optional()?;
        let folder = folder.ok_or_else(|| ApiError::not_found(format!("release {release_id} not found")))?;
        Ok((folder, track_ids_of(c, release_id)?))
    })?;
    let (files, parents) = ctx.read(|c| delete_files(c, &track_ids))?;
    let ids = track_ids.clone();
    ctx.write(move |t| {
        release_tag_counts(t, &ids)?;
        bc_db::fts::remove_tracks(t, &ids).map_err(ApiError::from)?;
        tidy::delete_artwork_rows(t, &[release_id])?;
        t.execute("DELETE FROM releases WHERE id = ?1", [release_id])?;
        Ok(())
    })?;
    let mut touched = parents;
    touched.extend(folder.map(PathBuf::from));
    ctx.read(|c| {
        tidy::sweep_sidecars(c, &touched)?;
        tidy::prune_empty_dirs(c, &touched)
    })?;
    tidy::delete_artwork(&ctx.config.art_dir(), &[release_id]);
    Ok(Purged { track_ids, files })
}

fn announce(ctx: &Ctx, deleted: &[i64]) {
    if deleted.is_empty() {
        return;
    }
    ctx.bus.invalidate("track", deleted.to_vec());
    ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &LibraryChanged { deleted_tracks: deleted.to_vec(), ..Default::default() });
}

/// `POST /releases/delete`: many releases, optionally blacklisted. One transaction per release;
/// failures are named in the result rather than raised. Blacklisting happens *before* the rows go
/// (the row is where the URL and artist name are read from).
pub fn delete_releases(ctx: &Ctx, ids: &[i64], blacklist_them: bool, reason: Option<&str>) -> ApiResult<DeleteReleasesResult> {
    if ids.is_empty() {
        return Err(ApiError::bad("no releases given"));
    }
    let releases: Vec<(i64, String, Option<String>, String)> = ctx.read(|c| {
        let mut st = c.prepare(
            "SELECT r.id, r.title, r.bandcamp_url, COALESCE(a.name,'') FROM releases r LEFT JOIN artists a ON a.id = r.artist_id
              WHERE r.id IN (SELECT value FROM json_each(?1)) ORDER BY r.id",
        )?;
        Ok(st.query_map([ids_json(ids)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?)
    })?;
    if releases.is_empty() {
        return Err(ApiError::not_found("none of those releases exist"));
    }
    let mut result = DeleteReleasesResult::default();
    let mut entries = Vec::new();
    let mut deleted = Vec::new();
    for (id, title, url, artist) in releases {
        let label = if title.is_empty() { id.to_string() } else { title.clone() };
        if blacklist_them {
            let (u, a, ti, re) = (url.clone(), artist.clone(), title.clone(), reason.unwrap_or("cleanup").to_string());
            match ctx.write(move |t| blacklist::add(t, u.as_deref(), &a, &ti, Some(&re))) {
                Ok(e) => entries.push(e),
                Err(e) => {
                    result.errors.push(format!("{label}: {e}"));
                    continue;
                }
            }
        }
        match purge_release(ctx, id) {
            Ok(p) => {
                result.releases += 1;
                result.tracks += p.track_ids.len() as i64;
                result.files += p.files;
                deleted.extend(p.track_ids);
            }
            Err(e) => {
                tracing::error!(release = %label, error = %e, "bulk delete failed for release");
                result.errors.push(format!("{label}: {e}"));
            }
        }
    }
    result.blacklisted = entries.len() as i64;
    if !entries.is_empty() {
        let es = entries;
        result.inbox_ignored = ctx.write(move |t| blacklist::ignore_matching_inbox(t, &es))? as i64;
    }
    announce(ctx, &deleted);
    Ok(result)
}

/// `DELETE /tracks/{id}`: the audio file goes as well as the row (removing only the row would be
/// pointless: the next scan re-imports the file).
pub fn delete_track(ctx: &Ctx, track_id: i64) -> ApiResult<DeletedOut> {
    ctx.read(|c| {
        c.query_row("SELECT 1 FROM tracks WHERE id = ?1", [track_id], |r| r.get::<_, i64>(0))
            .optional()?
            .map(|_| ())
            .ok_or_else(|| ApiError::not_found(format!("track {track_id} not found")))
    })?;
    let (files, parents) = ctx.read(|c| delete_files(c, &[track_id]))?;
    ctx.write(move |t| {
        release_tag_counts(t, &[track_id])?;
        bc_db::fts::remove_tracks(t, &[track_id]).map_err(ApiError::from)?;
        t.execute("DELETE FROM tracks WHERE id = ?1", [track_id])?;
        Ok(())
    })?;
    ctx.read(|c| tidy::prune_empty_dirs(c, &parents))?;
    announce(ctx, &[track_id]);
    Ok(DeletedOut { tracks: 1, files })
}

/// `DELETE /releases/{id}`: every track, its files and the emptied folders.
pub fn delete_release(ctx: &Ctx, release_id: i64) -> ApiResult<DeletedOut> {
    let p = purge_release(ctx, release_id)?;
    announce(ctx, &p.track_ids);
    Ok(DeletedOut { tracks: p.track_ids.len() as i64, files: p.files })
}

/// `DELETE /labels/{id}`: the label and everything filed under it. Every release goes the way the
/// bulk delete takes it; a release that cannot be deleted is reported (400) and keeps the label
/// with whatever remains, so the removal can be retried. The harvest evidence naming the label is
/// cleared too -- it is what the startup backfill files from, and leaving it would resurrect the
/// folder on the next boot.
pub fn delete_label(ctx: &Ctx, label_id: i64) -> ApiResult<DeleteLabelResult> {
    let (name, key) = ctx.read(|c| {
        c.query_row("SELECT name, name_key FROM labels WHERE id = ?1", [label_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .optional()?
            .ok_or_else(|| ApiError::not_found(format!("label {label_id} not found")))
    })?;
    let releases: Vec<(i64, String)> = ctx.read(|c| {
        let mut st = c.prepare("SELECT id, title FROM releases WHERE label_id = ?1 ORDER BY id")?;
        Ok(st.query_map([label_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?)
    })?;
    let mut result = DeleteLabelResult::default();
    let (mut deleted, mut errors) = (Vec::new(), Vec::new());
    for (id, title) in releases {
        match purge_release(ctx, id) {
            Ok(p) => {
                result.releases += 1;
                result.tracks += p.track_ids.len() as i64;
                result.files += p.files;
                deleted.extend(p.track_ids);
            }
            Err(e) => {
                tracing::error!(release = %title, label = %name, error = %e, "could not delete release while removing label");
                errors.push(format!("{}: {e}", if title.is_empty() { id.to_string() } else { title }));
            }
        }
    }
    announce(ctx, &deleted);
    if !errors.is_empty() {
        let kept = errors.len();
        return Err(ApiError::bad(format!(
            "{kept} release{} of \u{201c}{name}\u{201d} could not be deleted, so the label was kept: {}",
            if kept != 1 { "s" } else { "" },
            errors.join("; ")
        )));
    }
    ctx.write(move |t| {
        let stored: Vec<String> = {
            let mut st = t.prepare("SELECT DISTINCT label_name FROM harvest_items WHERE label_name IS NOT NULL")?;
            let all: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            all.into_iter().filter(|s| bc_db::util::name_key(s) == key).collect()
        };
        for s in stored {
            t.execute("UPDATE harvest_items SET label_name = NULL WHERE label_name = ?1", [s])?;
        }
        t.execute("DELETE FROM labels WHERE id = ?1", [label_id])?;
        Ok(())
    })?;
    Ok(result)
}
