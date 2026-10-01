//! Secret storage (the Bandcamp identity cookie): the OS keyring (`keyring` crate; Secret Service on
//! Linux), falling back to a `0600` file under `<data_dir>/secrets/` when no keyring is reachable
//! (headless machines). Values are never logged and never returned by any API.
//!
//! WS2 (bandcamp) uses [`Secrets::get`]/[`Secrets::set`] with the name [`BANDCAMP_COOKIE`].

use std::io::Write;
use std::path::{Path, PathBuf};

pub const SERVICE: &str = "bc-rust";
pub const BANDCAMP_COOKIE: &str = "bandcamp.identity_cookie";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Keyring,
    File,
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret store: {0}")]
    Io(String),
}

#[derive(Clone)]
pub struct Secrets {
    dir: PathBuf,
    prefer_keyring: bool,
}

impl Secrets {
    /// `BC_SECRETS=file` forces the file store (CI, headless boxes, tests).
    pub fn new(data_dir: &Path) -> Self {
        let prefer_keyring = std::env::var("BC_SECRETS").map(|v| v != "file").unwrap_or(true);
        Self { dir: data_dir.join("secrets"), prefer_keyring }
    }

    pub fn file_only(data_dir: &Path) -> Self {
        Self { dir: data_dir.join("secrets"), prefer_keyring: false }
    }

    fn file(&self, name: &str) -> PathBuf {
        let safe: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '_' }).collect();
        self.dir.join(safe)
    }

    fn entry(name: &str) -> Option<keyring::Entry> {
        keyring::Entry::new(SERVICE, name).ok()
    }

    /// Store `value`; returns which backend took it.
    pub fn set(&self, name: &str, value: &str) -> Result<Backend, SecretError> {
        if self.prefer_keyring
            && let Some(e) = Self::entry(name)
            && e.set_password(value).is_ok()
            && e.get_password().map(|v| v == value).unwrap_or(false)
        {
            let _ = std::fs::remove_file(self.file(name));
            return Ok(Backend::Keyring);
        }
        self.set_file(name, value)?;
        Ok(Backend::File)
    }

    fn set_file(&self, name: &str, value: &str) -> Result<(), SecretError> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::create_dir_all(&self.dir).map_err(|e| SecretError::Io(e.to_string()))?;
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
        let path = self.file(name);
        let tmp = path.with_extension("tmp");
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).map_err(|e| SecretError::Io(e.to_string()))?;
        f.write_all(value.as_bytes()).map_err(|e| SecretError::Io(e.to_string()))?;
        f.sync_all().map_err(|e| SecretError::Io(e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| SecretError::Io(e.to_string()))
    }

    pub fn get(&self, name: &str) -> Option<String> {
        if self.prefer_keyring
            && let Some(e) = Self::entry(name)
            && let Ok(v) = e.get_password()
        {
            return Some(v);
        }
        std::fs::read_to_string(self.file(name)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    pub fn delete(&self, name: &str) {
        if let Some(e) = Self::entry(name) {
            let _ = e.delete_credential();
        }
        let _ = std::fs::remove_file(self.file(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn file_fallback_is_0600_and_roundtrips() {
        let d = tempfile::tempdir().unwrap();
        let s = Secrets::file_only(d.path());
        assert_eq!(s.set(BANDCAMP_COOKIE, "identity=abc").unwrap(), Backend::File);
        assert_eq!(s.get(BANDCAMP_COOKIE).as_deref(), Some("identity=abc"));
        let meta = std::fs::metadata(d.path().join("secrets").join(BANDCAMP_COOKIE)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        s.delete(BANDCAMP_COOKIE);
        assert_eq!(s.get(BANDCAMP_COOKIE), None);
    }
}
