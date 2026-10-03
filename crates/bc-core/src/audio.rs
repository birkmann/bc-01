//! What counts as an audio file, by extension. One list for the scanner, the downloaders and
//! maintenance: when they each kept their own, a purchase of `.aif` files was unpacked by one and
//! then deleted as "no audio" by another.

use std::path::Path;

/// Lower case, without the leading dot.
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "m4a", "mp4", "aac", "ogg", "opus", "wav", "aiff", "aif", "wma"];

/// True when `ext` (no leading dot, any case) is an audio extension.
pub fn is_audio_ext(ext: &str) -> bool {
    AUDIO_EXTENSIONS.iter().any(|e| e.eq_ignore_ascii_case(ext))
}

/// True when `path` has an audio extension (case-insensitive).
pub fn is_audio_path(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(is_audio_ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_match_case_insensitively() {
        assert!(is_audio_path(Path::new("a/01 - x.FLAC")));
        assert!(is_audio_path(Path::new("x.aif")));
        assert!(!is_audio_path(Path::new("cover.jpg")));
        assert!(!is_audio_path(Path::new("noext")));
    }
}
