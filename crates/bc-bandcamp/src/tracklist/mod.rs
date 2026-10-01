//! Turning a DJ tracklist CSV into a folder of the tracks it lists.
//!
//! Two operations, and deliberately no third. *Parsing* is one request
//! ([`parse_upload`]); *matching* is one request per row ([`match_row`]),
//! driven from the client, because Bandcamp's search sits behind a shared token
//! bucket at well under one request a second.
//!
//! Downloading is not here: the confirmed URLs go to `POST /downloads`.
//!
//! The Bandcamp search is injected through [`Searcher`] so the scoring and the
//! escalation logic are testable without a network; the lead adapts the real
//! `sources::search` with a thin impl and a `From` for [`SearchHit`].

mod difflib;
mod matching;
mod parse;
mod sniff;

use async_trait::async_trait;

use crate::error::HarvestError;

pub use matching::{
    LIKELY, MatchResult, STRONG, ScoredHit, bare_name, base_title, match_row, pick_best, rank,
    score_hit, tier_of,
};
pub use parse::{
    MergedTracklist, ParsedFile, TrackRow, TracklistError, decode, merge_files, name_key,
    parse_tracklist, repair_mojibake, suggest_title, title_from_filename,
};

pub const MAX_FILES: usize = 20;
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// A cap on the search pass, not on the CSV. 500 rows is already twenty minutes
/// of rate-limited requests.
pub const MAX_ROWS: usize = 500;

/// Bandcamp autocomplete filter: `t` (tracks only) or everything (`""`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Track,
    All,
}

impl SearchKind {
    /// The legacy filter string (`"t"` / `""`).
    pub fn as_filter(self) -> &'static str {
        match self {
            SearchKind::Track => "t",
            SearchKind::All => "",
        }
    }
}

/// Mirror of the Python search-hit dataclass (minus `location`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchHit {
    /// `artist` | `label` | `album` | `track` | `fan`
    pub kind: String,
    pub name: String,
    pub url: String,
    pub subtitle: String,
    pub art_url: Option<String>,
    pub band_id: Option<i64>,
    pub item_id: Option<i64>,
}

#[async_trait]
pub trait Searcher: Send + Sync {
    async fn search(&self, query: &str, kind: SearchKind, limit: usize) -> Result<Vec<SearchHit>, HarvestError>;
}

// ---------------------------------------------------------------------------
// Upload handling (what the parse route does around the pure parser)
// ---------------------------------------------------------------------------

/// One multipart file part.
#[derive(Debug, Clone, Default)]
pub struct UploadedFile {
    /// `None` when the part carried no filename (`tracklist.csv` is used).
    pub filename: Option<String>,
    pub data: Vec<u8>,
}

/// Request-level failures of the parse route; every variant is a 400.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UploadError {
    #[error("No files uploaded.")]
    NoFiles,
    #[error("Too many files: {0}. At most {MAX_FILES} per upload.")]
    TooManyFiles(usize),
    /// Every file failed; carries each file's error message (the `files` detail).
    #[error("None of these files is a readable tracklist.")]
    NoneReadable(Vec<String>),
    #[error("{0} tracks is more than one pass can search ({MAX_ROWS} max). Upload these in smaller batches.")]
    TooManyRows(usize),
}

/// Result of [`parse_upload`]; maps one-to-one onto the `ParsedTracklists` body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUpload {
    pub suggested_title: String,
    /// Per-file outcome (`rows` there is `file.rows.len()`).
    pub files: Vec<ParsedFile>,
    pub rows: Vec<TrackRow>,
    pub duplicates: usize,
}

/// Parse every uploaded file (a bad file reports its own error and the rest
/// still parse), merge them, and apply the request-level limits.
pub fn parse_upload(uploads: &[UploadedFile]) -> Result<ParsedUpload, UploadError> {
    if uploads.is_empty() {
        return Err(UploadError::NoFiles);
    }
    if uploads.len() > MAX_FILES {
        return Err(UploadError::TooManyFiles(uploads.len()));
    }
    let mut parsed: Vec<ParsedFile> = Vec::with_capacity(uploads.len());
    for upload in uploads {
        let filename = upload.filename.clone().filter(|f| !f.is_empty()).unwrap_or_else(|| "tracklist.csv".into());
        if upload.data.len() > MAX_BYTES {
            parsed.push(ParsedFile::failed(&filename, format!("{filename}: larger than {} KB.", MAX_BYTES / 1024)));
            continue;
        }
        match parse_tracklist(&upload.data, &filename) {
            Ok(file) => parsed.push(file),
            Err(e) => parsed.push(ParsedFile::failed(&filename, e.to_string())),
        }
    }
    if parsed.iter().all(|p| p.error.is_some()) {
        return Err(UploadError::NoneReadable(parsed.iter().filter_map(|p| p.error.clone()).collect()));
    }
    let merged = merge_files(&parsed);
    if merged.rows.len() > MAX_ROWS {
        return Err(UploadError::TooManyRows(merged.rows.len()));
    }
    Ok(ParsedUpload {
        suggested_title: suggest_title(&parsed),
        files: parsed,
        rows: merged.rows,
        duplicates: merged.duplicates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(name: &str, data: &str) -> UploadedFile {
        UploadedFile { filename: Some(name.into()), data: data.as_bytes().to_vec() }
    }

    #[test]
    fn one_bad_file_does_not_sink_the_upload() {
        let out = parse_upload(&[up("good.csv", "Artist,Title\nSlam,Lifetimes\n"), up("bad.csv", "x,y\n1,2\n")]).unwrap();
        assert_eq!(out.rows.len(), 1);
        assert!(out.files[1].error.is_some());
        assert_eq!(out.suggested_title, "good");
    }

    #[test]
    fn request_level_failures() {
        assert_eq!(parse_upload(&[]).unwrap_err(), UploadError::NoFiles);
        let many: Vec<_> = (0..21).map(|i| up(&format!("{i}.csv"), "Artist,Title\na,b\n")).collect();
        assert_eq!(parse_upload(&many).unwrap_err(), UploadError::TooManyFiles(21));
        let err = parse_upload(&[up("bad.csv", "x,y\n1,2\n")]).unwrap_err();
        assert!(matches!(err, UploadError::NoneReadable(ref v) if v.len() == 1));
        let big = UploadedFile { filename: Some("big.csv".into()), data: vec![b'a'; MAX_BYTES + 1] };
        let err = parse_upload(&[big]).unwrap_err();
        assert!(matches!(err, UploadError::NoneReadable(ref v) if v[0] == "big.csv: larger than 2048 KB."));
        let mut csv = String::from("Artist,Title\n");
        for i in 0..501 {
            csv.push_str(&format!("a{i},t{i}\n"));
        }
        assert_eq!(parse_upload(&[up("r.csv", &csv)]).unwrap_err(), UploadError::TooManyRows(501));
    }

    #[test]
    fn missing_filename_defaults() {
        let out = parse_upload(&[UploadedFile { filename: None, data: b"Artist,Title\na,b\n".to_vec() }]).unwrap();
        assert_eq!(out.files[0].filename, "tracklist.csv");
    }
}
