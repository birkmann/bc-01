//! `GET /library/browse`: the folder picker for adding a root. Lists the subfolders of one folder
//! on the machine running bc (never file names, only how many audio files sit in it), plus places
//! to start from: home, the music folder, mounted drives, the filesystem root.

use std::path::{Path, PathBuf};

use bc_db::rusqlite::OptionalExtension;
use bc_libcore::{ApiError, ApiResult, Ctx};
use bc_types::library::{BrowseDir, BrowseOut, BrowsePlace};

use crate::roots::expand_tilde;

/// List `path` (`~` expands; empty means the music folder, else home). `hidden` includes dot folders.
pub fn browse(ctx: &Ctx, path: Option<&str>, hidden: bool) -> ApiResult<BrowseOut> {
    let home = home_dir();
    let music = music_dir(home.as_deref());
    let start = match path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => expand_tilde(p),
        None => music.clone().or_else(|| home.clone()).unwrap_or_else(|| PathBuf::from("/")),
    };
    let dir = std::fs::canonicalize(&start).map_err(|_| ApiError::not_found(format!("folder not found: {}", start.display())))?;
    if !dir.is_dir() {
        return Err(ApiError::bad(format!("not a folder: {}", dir.display())));
    }
    let entries = std::fs::read_dir(&dir).map_err(|e| ApiError::bad(format!("cannot open {}: {e}", dir.display())))?;

    let mut dirs = Vec::new();
    let mut audio_files = 0u32;
    for entry in entries.flatten() {
        // a name that is not UTF-8 cannot travel back as a path string
        let Ok(name) = entry.file_name().into_string() else { continue };
        let p = entry.path();
        // follows symlinks, so a linked folder lists as a folder
        let is_dir = std::fs::metadata(&p).map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
            if hidden || !name.starts_with('.') {
                dirs.push(BrowseDir { name, path: p.to_string_lossy().into_owned() });
            }
        } else if crate::media::is_audio_path(&p) {
            audio_files += 1;
        }
    }
    dirs.sort_by_cached_key(|d| d.name.to_lowercase());

    let path = dir.to_string_lossy().into_owned();
    let lookup = path.clone();
    let is_root = ctx.read(move |c| {
        Ok(c.query_row("SELECT 1 FROM library_roots WHERE path = ?1 AND kind = 'library'", [&lookup], |_| Ok(())).optional()?.is_some())
    })?;
    Ok(BrowseOut {
        parent: dir.parent().map(|p| p.to_string_lossy().into_owned()),
        path,
        dirs,
        audio_files,
        is_root,
        places: places(home.as_deref(), music.as_deref()),
    })
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_dir())
}

/// `XDG_MUSIC_DIR` from `user-dirs.dirs` (localised, e.g. `~/Musik`), else `~/Music`.
fn music_dir(home: Option<&Path>) -> Option<PathBuf> {
    let home = home?;
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    let xdg = std::fs::read_to_string(config.join("user-dirs.dirs")).ok().and_then(|s| xdg_music_dir(&s, home));
    xdg.into_iter().chain([home.join("Music")]).find(|p| p.is_dir() && p != home)
}

/// The `XDG_MUSIC_DIR="$HOME/..."` line of a `user-dirs.dirs` file.
fn xdg_music_dir(contents: &str, home: &Path) -> Option<PathBuf> {
    let value = contents.lines().find_map(|l| l.trim().strip_prefix("XDG_MUSIC_DIR="))?.trim().trim_matches('"');
    match value.strip_prefix("$HOME") {
        Some(rest) => Some(home.join(rest.trim_start_matches('/'))),
        None if value.starts_with('/') => Some(PathBuf::from(value)),
        None => None,
    }
}

fn places(home: Option<&Path>, music: Option<&Path>) -> Vec<BrowsePlace> {
    let place = |label: &str, path: &Path, kind: &str| BrowsePlace {
        label: label.to_string(),
        path: path.to_string_lossy().into_owned(),
        kind: kind.to_string(),
    };
    let mut out = Vec::new();
    if let Some(h) = home {
        out.push(place("Home", h, "home"));
    }
    if let Some(m) = music {
        let label = m.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Music".into());
        out.push(place(&label, m, "music"));
    }
    let mut seen: Vec<PathBuf> = out.iter().map(|p| PathBuf::from(&p.path)).collect();
    for drive in drives() {
        let Ok(real) = std::fs::canonicalize(&drive) else { continue };
        // macOS lists the boot volume in /Volumes as a link to /
        if real == Path::new("/") || seen.contains(&real) {
            continue;
        }
        let label = drive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        out.push(place(&label, &drive, "drive"));
        seen.push(real);
    }
    out.push(place("Computer", Path::new("/"), "root"));
    out
}

/// Mount points of removable and extra drives (folders under the usual mount parents).
fn drives() -> Vec<PathBuf> {
    let user = std::env::var("USER").unwrap_or_default();
    let mut parents = vec![PathBuf::from("/Volumes"), PathBuf::from("/mnt")];
    if !user.is_empty() {
        parents.insert(0, Path::new("/run/media").join(&user));
        parents.insert(1, Path::new("/media").join(&user));
    }
    parents.push(PathBuf::from("/media"));
    let mut out = Vec::new();
    for parent in parents {
        let Ok(rd) = std::fs::read_dir(&parent) else { continue };
        let mut found: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
            // /media/<user> is a parent of its own, listed above
            .filter(|p| !(parent == Path::new("/media") && p.file_name().is_some_and(|n| n == user.as_str())))
            .collect();
        found.sort();
        out.extend(found);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_localised_music_folder() {
        let home = Path::new("/home/u");
        let dirs = "# comment\nXDG_DESKTOP_DIR=\"$HOME/Schreibtisch\"\nXDG_MUSIC_DIR=\"$HOME/Musik\"\n";
        assert_eq!(xdg_music_dir(dirs, home), Some(PathBuf::from("/home/u/Musik")));
        assert_eq!(xdg_music_dir("XDG_MUSIC_DIR=\"/srv/music\"", home), Some(PathBuf::from("/srv/music")));
        assert_eq!(xdg_music_dir("XDG_MUSIC_DIR=\"$HOME/\"", home), Some(PathBuf::from("/home/u/")));
        assert_eq!(xdg_music_dir("XDG_VIDEOS_DIR=\"$HOME/Videos\"", home), None);
    }
}
