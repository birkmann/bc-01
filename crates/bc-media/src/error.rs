//! Error type shared by every `bc-media` module.

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, MediaError>;

/// Everything that can go wrong in tag, artwork and streaming code.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// Filesystem error (open, copy, rename, fsync ...).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The tag library could not parse or serialise the file.
    #[error("tag error: {0}")]
    Tag(String),
    /// The container cannot be read or written by this module.
    #[error("unsupported container: {0}")]
    Unsupported(String),
    /// Image decode / resize / encode failure.
    #[error("image error: {0}")]
    Image(String),
    /// A write could not be completed. The original file is unchanged.
    #[error("tag write failed: {0}")]
    Write(String),
}

impl From<lofty::error::FileParseError> for MediaError {
    fn from(e: lofty::error::FileParseError) -> Self {
        MediaError::Tag(e.to_string())
    }
}

impl From<lofty::error::FileEncodingError> for MediaError {
    fn from(e: lofty::error::FileEncodingError) -> Self {
        MediaError::Tag(e.to_string())
    }
}
