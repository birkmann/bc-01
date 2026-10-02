//! Path safety (web/PLAN.md §3.3): compare resolved ancestry, never string prefixes.

use std::path::{Component, Path, PathBuf};

#[derive(Debug, thiserror::Error)]
#[error("unsafe path: {0}")]
pub struct UnsafePath(pub String);

/// Join `rel` under `base`, rejecting absolute paths, `..` escapes and symlink escapes.
pub fn safe_join(base: &Path, rel: &Path) -> Result<PathBuf, UnsafePath> {
    if rel.is_absolute() {
        return Err(UnsafePath(rel.display().to_string()));
    }
    for c in rel.components() {
        if matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)) {
            return Err(UnsafePath(rel.display().to_string()));
        }
    }
    let joined = base.join(rel);
    is_within(base, &joined)?;
    Ok(joined)
}

/// True ancestry check on canonicalised paths (resolves symlinks for the
/// longest existing prefix, so not-yet-created files can be checked too).
pub fn is_within(base: &Path, candidate: &Path) -> Result<(), UnsafePath> {
    // Both sides resolve the same way: a base that does not exist yet under a symlinked parent
    // (macOS: /var → /private/var) must still contain its own children.
    let base_c = canonicalize_existing_prefix(base);
    let cand_c = canonicalize_existing_prefix(candidate);
    if cand_c.starts_with(&base_c) {
        Ok(())
    } else {
        Err(UnsafePath(candidate.display().to_string()))
    }
}

fn canonicalize_existing_prefix(p: &Path) -> PathBuf {
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

/// Sanitise a single directory/file name, keeping Unicode.
pub fn safe_subdir_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == '\0' || c.is_control() { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() { "_".into() } else { trimmed.chars().take(200).collect() }
}

/// `{dir}/{id/1000:04}/{id}{suffix}` shard layout used by art and waveform caches.
pub fn shard_path(dir: &Path, id: i64, suffix: &str) -> PathBuf {
    dir.join(format!("{:04}", id / 1000)).join(format!("{id}{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_prefix_sibling() {
        let tmp = std::env::temp_dir().join(format!("bc-paths-{}", std::process::id()));
        let base = tmp.join("downloads");
        let evil = tmp.join("downloads-evil");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&evil).unwrap();
        assert!(is_within(&base, &evil.join("x")).is_err());
        assert!(safe_join(&base, Path::new("../downloads-evil/x")).is_err());
        assert!(safe_join(&base, Path::new("/etc/passwd")).is_err());
        assert!(safe_join(&base, Path::new("a/b.mp3")).is_ok());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn sanitises_names() {
        assert_eq!(safe_subdir_name("a/b"), "a_b");
        assert_eq!(safe_subdir_name(".."), "_");
        assert_eq!(safe_subdir_name("Björk"), "Björk");
    }
}
