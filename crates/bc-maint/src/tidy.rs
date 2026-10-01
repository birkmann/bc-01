//! What a deletion leaves behind (port of `services/library/tidy.py`).
//!
//! Removing music is never just removing rows: the album folder keeps its `cover.jpg`, the empty
//! directories stay, and the cached artwork (in the data dir, keyed by a release id that will
//! never be issued again) lingers. These sweeps are shared so a deletion cannot half-do them: a
//! deletion that skips the sidecars leaves a folder that [`prune_empty_dirs`] then refuses to
//! remove, and the album appears to still be there in every file manager.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use bc_core::paths::shard_path;
use bc_db::rusqlite::Connection;
use bc_libcore::ApiResult;

use crate::util::{canon, has_audio_ext, root_paths, strictly_under_a_root};

/// What bandcamp-dl and the scanner leave beside the music. Only audio is indexed in `files`, so
/// these are invisible to file deletion -- and one leftover `cover.jpg` stops [`prune_empty_dirs`].
pub const SIDECAR_SUFFIXES: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "txt", "nfo", "m3u", "m3u8", "cue", "log"];

/// Walk up from each emptied directory, removing it while it is empty, but never past (or
/// including) a library root.
pub fn prune_empty_dirs(c: &Connection, dirs: &[PathBuf]) -> ApiResult<()> {
    let roots = root_paths(c)?;
    let starts: HashSet<PathBuf> = dirs.iter().map(|d| canon(d)).collect();
    for start in starts {
        let mut current = start;
        while strictly_under_a_root(&current, &roots) {
            match std::fs::read_dir(&current) {
                Ok(mut it) => {
                    if it.next().is_some() {
                        break; // still holds something
                    }
                }
                Err(_) => break,
            }
            if std::fs::remove_dir(&current).is_err() {
                break;
            }
            match current.parent() {
                Some(p) => current = p.to_path_buf(),
                None => break,
            }
        }
    }
    Ok(())
}

/// Remove leftover art and text files from folders that hold no audio left. An allowlist of
/// extensions rather than a blanket wipe, and only once the folder has no audio at all: a folder
/// that still holds music is somebody else's.
pub fn sweep_sidecars(c: &Connection, folders: &[PathBuf]) -> ApiResult<()> {
    let roots = root_paths(c)?;
    let folders: HashSet<PathBuf> = folders.iter().map(|d| canon(d)).collect();
    for folder in folders {
        if !strictly_under_a_root(&folder, &roots) {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&folder) else { continue };
        let entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        if entries.iter().any(|p| has_audio_ext(p)) {
            continue;
        }
        for p in entries {
            let is_sidecar = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| SIDECAR_SUFFIXES.contains(&e.to_ascii_lowercase().as_str()))
                .unwrap_or(false);
            if p.is_file() && is_sidecar {
                let _ = std::fs::remove_file(&p);
            }
        }
    }
    Ok(())
}

/// Every art file a release can own: the webp sizes under the sharded layout and the legacy
/// `{id}.jpg` / `{id}_thumb.jpg` pair.
pub fn art_files(art_dir: &Path, release_id: i64) -> Vec<PathBuf> {
    ["_thumb.webp", "_medium.webp", "_full.webp", ".jpg", "_thumb.jpg"].iter().map(|s| shard_path(art_dir, release_id, s)).collect()
}

/// Drop the cached cover files of deleted releases (a missing file is not an error). The
/// `artwork` row goes with the release (`ON DELETE CASCADE`); use [`delete_artwork_rows`] when
/// the release row stays.
pub fn delete_artwork(art_dir: &Path, release_ids: &[i64]) {
    for id in release_ids {
        for p in art_files(art_dir, *id) {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Delete the `artwork` rows of these releases (inside the caller's write transaction).
pub fn delete_artwork_rows(c: &Connection, release_ids: &[i64]) -> ApiResult<()> {
    c.execute("DELETE FROM artwork WHERE release_id IN (SELECT value FROM json_each(?1))", [crate::util::ids_json(release_ids)])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn sidecars_and_empty_dirs_are_swept_but_never_the_root() {
        let db = test_db();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("music");
        let album = root.join("artist").join("album");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("cover.jpg"), b"x").unwrap();
        std::fs::write(album.join("notes.txt"), b"x").unwrap();
        std::fs::write(album.join("keep.bin"), b"x").unwrap();
        seed_root(&db, root.to_str().unwrap(), "library");
        db.read(|c| {
            sweep_sidecars(c, &[album.clone()]).unwrap();
            assert!(album.join("keep.bin").exists() && !album.join("cover.jpg").exists());
            std::fs::remove_file(album.join("keep.bin")).unwrap();
            prune_empty_dirs(c, &[album.clone()]).unwrap();
            assert!(!album.exists() && !album.parent().unwrap().exists() && root.exists());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_folder_with_audio_is_somebody_elses() {
        let db = test_db();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("music");
        let album = root.join("a");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::write(album.join("01.mp3"), b"x").unwrap();
        std::fs::write(album.join("cover.jpg"), b"x").unwrap();
        seed_root(&db, root.to_str().unwrap(), "library");
        db.read(|c| {
            sweep_sidecars(c, &[album.clone()]).unwrap();
            assert!(album.join("cover.jpg").exists());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn artwork_in_both_layouts_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let files = art_files(tmp.path(), 1234);
        for f in &files {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, b"x").unwrap();
        }
        assert!(files[0].ends_with("0001/1234_thumb.webp"));
        delete_artwork(tmp.path(), &[1234]);
        assert!(files.iter().all(|f| !f.exists()));
    }
}
