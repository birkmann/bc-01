//! Storage for the Bandcamp identity cookie.
//!
//! There is no programmatic login: Bandcamp gates it behind reCAPTCHA (the login
//! page blob carries `recaptcha_public_key`). The cookie is pasted once.
//!
//! It grants **full account access**, including purchase history, so:
//!
//! * it is stored in the OS keyring when available (feature `os-keyring`,
//!   Secret Service on Linux, the Keychain on macOS), otherwise in `<data_dir>/identity.cookie`
//!   created with mode 0600 (temp file + rename);
//! * it is never returned by any endpoint -- only a redacted [`fingerprint`];
//! * it is never logged: hold it in a [`Secret`] (`Debug`/`Display` print
//!   `<redacted>`) and pass free text through [`redact`];
//! * it is only ever sent to `*.bandcamp.com` (enforced in `net::client`).

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

/// Legacy `settings` row that held the cookie in plaintext in `library.db`.
pub const SETTING_KEY: &str = "bandcamp.identity_cookie";

/// File name (inside the data dir) of the 0600 fallback store.
pub const COOKIE_FILE: &str = "identity.cookie";

const KEYRING_SERVICE: &str = "bc-rust";
const KEYRING_USER: &str = "bandcamp.identity_cookie";

static IDENTITY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"identity=([^;\s]+)").expect("static regex"));

/// Strip a cookie value out of anything that might be logged.
pub fn redact(text: &str) -> String {
    IDENTITY_RE.replace_all(text, "identity=<redacted>").into_owned()
}

/// A secret string that can never be formatted into a log line: `Debug` and
/// `Display` both print `<redacted>`. Use [`Secret::expose`] at the single
/// place the value is actually needed (the `Cookie` header).
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    /// The raw value. Never pass the result to a logging macro.
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl From<String> for Secret {
    fn from(v: String) -> Self {
        Self(v)
    }
}

/// Accept a bare `identity=...`, just its value, or a whole pasted Cookie header.
///
/// Users copy the entire header out of devtools, and some endpoints may need
/// the companion cookies (`client_id`, `session`), so the whole string is
/// kept verbatim rather than extracting just `identity`.
pub fn normalise_cookie(raw: &str) -> String {
    let text = raw.trim().trim_matches(';');
    // Python: text.lower().startswith("cookie:") then split(":", 1)[1].strip()
    let is_header = text.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("cookie:"));
    if is_header {
        return text[7..].trim().to_string();
    }
    // Just the value, copied from the browser's cookie list.
    if !text.is_empty() && !text.contains(['=', ';', ' ']) {
        return format!("identity={text}");
    }
    text.to_string()
}

/// A short, non-reversible label so the UI can show *which* cookie is stored.
pub fn fingerprint(cookie: &str) -> String {
    let value = IDENTITY_RE.captures(cookie).and_then(|c| c.get(1)).map_or(cookie, |m| m.as_str());
    let chars: Vec<char> = value.chars().collect();
    if chars.len() > 6 {
        let tail: String = chars[chars.len() - 6..].iter().collect();
        format!("\u{2026}{tail}")
    } else {
        "\u{2026}".to_string()
    }
}

/// Does the cookie string carry an `identity=` value?
pub fn has_identity(cookie: &str) -> bool {
    IDENTITY_RE.is_match(cookie)
}

/// Where the cookie lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Keyring,
    File,
}

/// The identity cookie store: OS keyring first, 0600 file as fallback.
///
/// All methods are blocking (keyring and file I/O); call from
/// `spawn_blocking` if needed. They never log the cookie.
#[derive(Debug, Clone)]
pub struct CookieStore {
    data_dir: PathBuf,
    use_keyring: bool,
}

impl CookieStore {
    /// Keyring-backed when the `os-keyring` feature is on and `BC_NO_KEYRING`
    /// is unset; otherwise file-only. A keyring that errors at runtime (no
    /// D-Bus session, locked collection, ...) transparently falls back to the file.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        let enabled = cfg!(feature = "os-keyring") && std::env::var_os("BC_NO_KEYRING").is_none();
        tracing::info!(
            "identity cookie store: {}",
            if enabled { "OS keyring (file fallback)" } else { "0600 file" }
        );
        Self { data_dir, use_keyring: enabled }
    }

    /// Always use the 0600 file (tests, headless installs).
    pub fn file_only(data_dir: impl Into<PathBuf>) -> Self {
        Self { data_dir: data_dir.into(), use_keyring: false }
    }

    pub fn cookie_path(&self) -> PathBuf {
        self.data_dir.join(COOKIE_FILE)
    }

    /// The stored cookie, if any.
    pub fn load(&self) -> Option<String> {
        if self.use_keyring {
            match keyring_get() {
                Ok(Some(v)) if !v.is_empty() => return Some(v),
                Ok(_) => {}
                Err(e) => tracing::debug!("keyring unavailable on load ({e}); using file"),
            }
        }
        read_file(&self.cookie_path()).filter(|v| !v.is_empty())
    }

    /// Normalise and persist the cookie; returns its [`fingerprint`].
    pub fn store(&self, cookie: &str) -> String {
        let value = normalise_cookie(cookie);
        let fp = fingerprint(&value);
        if self.use_keyring {
            match keyring_set(&value) {
                Ok(()) => {
                    // Read back: a build without a platform backend silently
                    // uses a non-persistent mock store.
                    if matches!(keyring_get(), Ok(Some(ref v)) if *v == value) {
                        let _ = std::fs::remove_file(self.cookie_path());
                        return fp;
                    }
                    tracing::warn!("keyring read-back mismatch; storing the cookie in the 0600 file instead");
                }
                Err(e) => tracing::warn!("keyring unavailable ({e}); storing the cookie in the 0600 file"),
            }
        }
        if let Err(e) = write_file_0600(&self.cookie_path(), &value) {
            tracing::error!("cannot persist identity cookie: {e}");
        }
        fp
    }

    /// Remove the cookie from every backend.
    pub fn clear(&self) {
        if self.use_keyring
            && let Err(e) = keyring_delete() {
                tracing::debug!("keyring delete: {e}");
            }
        match std::fs::remove_file(self.cookie_path()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("cannot remove cookie file: {e}"),
        }
    }

    /// Which backend currently holds the cookie (None if no cookie).
    pub fn backend(&self) -> Option<Backend> {
        if self.use_keyring && matches!(keyring_get(), Ok(Some(ref v)) if !v.is_empty()) {
            return Some(Backend::Keyring);
        }
        read_file(&self.cookie_path()).filter(|v| !v.is_empty()).map(|_| Backend::File)
    }

    /// Move the legacy plaintext `settings` row `bandcamp.identity_cookie`
    /// into this store and delete the row. Best effort; returns whether a
    /// cookie was migrated. An existing stored cookie is not overwritten (the
    /// legacy row is still removed so the secret does not linger in the DB).
    pub fn migrate_from_settings(&self, db: &bc_db::Db) -> bool {
        let legacy = match db.read(|c| bc_db::settings::get(c, SETTING_KEY)) {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("identity migration: cannot read settings: {e}");
                return false;
            }
        };
        let Some(raw) = legacy else { return false };
        // The settings column is JSON text elsewhere; the Python app stored the raw string.
        let value = serde_json::from_str::<String>(&raw).unwrap_or(raw);
        let mut migrated = false;
        if !value.trim().is_empty() {
            if self.load().is_none() {
                self.store(&value);
                migrated = self.load().is_some();
            }
            if !migrated && self.load().is_none() {
                return false; // could not persist: keep the row rather than lose the cookie
            }
        }
        if let Err(e) = db.write(|t| {
            t.execute("DELETE FROM settings WHERE key = ?1", [SETTING_KEY])?;
            Ok(())
        }) {
            tracing::warn!("identity migration: cannot delete legacy settings row: {e}");
        }
        migrated
    }
}

fn read_file(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Write `value` to `path` via a temp file created with mode 0600, then rename.
fn write_file_0600(path: &Path, value: &str) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{COOKIE_FILE}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    let res = f.write_all(value.as_bytes()).and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = res.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Keyring plumbing (compiled out without the feature)
// ---------------------------------------------------------------------------

#[cfg(feature = "os-keyring")]
fn entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|e| e.to_string())
}
#[cfg(feature = "os-keyring")]
fn keyring_get() -> Result<Option<String>, String> {
    match entry()?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}
#[cfg(feature = "os-keyring")]
fn keyring_set(v: &str) -> Result<(), String> {
    entry()?.set_password(v).map_err(|e| e.to_string())
}
#[cfg(feature = "os-keyring")]
fn keyring_delete() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(not(feature = "os-keyring"))]
fn keyring_get() -> Result<Option<String>, String> {
    Err("os-keyring feature disabled".into())
}
#[cfg(not(feature = "os-keyring"))]
fn keyring_set(_: &str) -> Result<(), String> {
    Err("os-keyring feature disabled".into())
}
#[cfg(not(feature = "os-keyring"))]
fn keyring_delete() -> Result<(), String> {
    Err("os-keyring feature disabled".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_hides_identity_value() {
        assert_eq!(
            redact("Cookie: client_id=1; identity=abc123def; x=y"),
            "Cookie: client_id=1; identity=<redacted>; x=y"
        );
        assert_eq!(redact("nothing here"), "nothing here");
    }

    #[test]
    fn normalise_accepts_bare_and_header_forms() {
        assert_eq!(normalise_cookie("  identity=abc;  "), "identity=abc");
        assert_eq!(normalise_cookie("Cookie: identity=abc; client_id=9"), "identity=abc; client_id=9");
        assert_eq!(normalise_cookie("cookie:identity=abc"), "identity=abc");
        assert_eq!(normalise_cookie(" 7%09abc%7B%22id%22%3A1%7D "), "identity=7%09abc%7B%22id%22%3A1%7D");
    }

    #[test]
    fn fingerprint_and_has_identity() {
        assert_eq!(fingerprint("identity=0123456789abcdef; x=1"), "\u{2026}abcdef");
        assert_eq!(fingerprint("identity=abc"), "\u{2026}");
        assert_eq!(fingerprint("0123456789"), "\u{2026}456789");
        assert!(has_identity("a=b; identity=zzz"));
        assert!(!has_identity("a=b"));
    }

    #[test]
    fn secret_never_formats() {
        let s = Secret::new("identity=hunter2");
        assert_eq!(format!("{s}"), "<redacted>");
        assert_eq!(format!("{s:?}"), "<redacted>");
        assert_eq!(format!("{:?}", Some(&s)), "Some(<redacted>)");
        assert_eq!(s.expose(), "identity=hunter2");
    }

    #[test]
    fn file_store_round_trip_and_perms() {
        let dir = tempfile::tempdir().unwrap();
        let store = CookieStore::file_only(dir.path());
        assert_eq!(store.load(), None);
        let fp = store.store("Cookie: identity=0123456789abcdef");
        assert_eq!(fp, "\u{2026}abcdef");
        assert_eq!(store.load().as_deref(), Some("identity=0123456789abcdef"));
        assert_eq!(store.backend(), Some(Backend::File));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.cookie_path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // overwrite keeps 0600 and leaves no temp files
        store.store("identity=newvalue123");
        assert_eq!(store.load().as_deref(), Some("identity=newvalue123"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.cookie_path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1);
        store.clear();
        assert_eq!(store.load(), None);
    }

    #[test]
    fn migrate_moves_the_legacy_settings_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = bc_db::Db::open(dir.path().join("library.db")).unwrap();
        db.write(|t| bc_db::settings::set(t, SETTING_KEY, "identity=legacyvalue99")).unwrap();
        let store = CookieStore::file_only(dir.path());
        assert!(store.migrate_from_settings(&db));
        assert_eq!(store.load().as_deref(), Some("identity=legacyvalue99"));
        let left = db.read(|c| bc_db::settings::get(c, SETTING_KEY)).unwrap();
        assert_eq!(left, None);
        // nothing left to migrate
        assert!(!store.migrate_from_settings(&db));
    }

    /// Manual probe (touches the real OS keyring with a throwaway entry):
    /// `cargo test -p bc-bandcamp --lib keyring_probe -- --ignored --nocapture`
    #[cfg(feature = "os-keyring")]
    #[test]
    #[ignore]
    fn keyring_probe() {
        let e = keyring::Entry::new("bc-rust-probe", "probe").unwrap();
        match e.set_password("x") {
            Ok(()) => {
                println!("keyring set ok; get = {:?}", e.get_password());
                let _ = e.delete_credential();
            }
            Err(err) => println!("keyring unavailable: {err}"),
        }
    }
}
