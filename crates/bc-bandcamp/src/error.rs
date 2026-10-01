//! Error types for the Bandcamp layer. Mirrors `harvest/net.py`'s exception
//! hierarchy; callers match on the variant (e.g. `IdentityExpired` -> 401).

#[derive(Debug, thiserror::Error)]
pub enum HarvestError {
    /// Base `HarvestError`: generic failure (HTTP 5xx, transport, 404 "not found: <url>").
    #[error("{0}")]
    Other(String),
    /// The API returned HTTP 200 with an error body.
    #[error("{message} ({url})")]
    Api {
        message: String,
        url: String,
        error_type: Option<String>,
        /// The body was a bare `{"error": true}` with nothing said about why;
        /// endpoints guarding private data answer that way.
        unspecified: bool,
    },
    /// A fan's list is private / not served.
    #[error("{0}")]
    ListUnavailable(String),
    /// The stored identity cookie is missing or no longer valid.
    #[error("{0}")]
    IdentityExpired(String),
    /// 429/403 from Bandcamp.
    #[error("{0}")]
    RateLimited(String),
    /// A page did not contain the data we expected.
    #[error("{0}")]
    Extraction(String),
    /// Cancelled by the caller (job cancel / sweep stop).
    #[error("cancelled")]
    Cancelled,
    #[error("database: {0}")]
    Db(#[from] bc_db::DbError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = HarvestError> = std::result::Result<T, E>;

impl HarvestError {
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}

impl From<bc_db::rusqlite::Error> for HarvestError {
    fn from(e: bc_db::rusqlite::Error) -> Self {
        Self::Db(bc_db::DbError::Sqlite(e))
    }
}
