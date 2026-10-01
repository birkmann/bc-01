//! Reading a DJ tracklist CSV.
//!
//! The files these come from are radio-show tracklists -- one row per track
//! played, with the artist and title we have to find on Bandcamp and a
//! start/end we keep only so the review table can show the set in playing
//! order.
//!
//! Two things about them are not negotiable:
//!
//! * **The encoding is usually wrong.** Real files carry `Roman FlÃ¼gel` --
//!   UTF-8 bytes that some earlier tool decoded as cp1252 and re-encoded.
//!   Searching Bandcamp for "FlÃ¼gel" finds nothing, so the mojibake has to be
//!   undone before anything else happens.
//! * **The header is not fixed.** Different exporters write
//!   `Title`/`Track`/`Song` for the same column, and the leading index column
//!   is `#` or `No`.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

use super::sniff;

/// This file is not a tracklist we can read. A per-file condition, not a
/// per-request one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TracklistError(pub String);

/// Fold a name to a comparison key (legacy `name_key`): NFKD, drop combining
/// marks, non-alphanumerics to spaces, lowercase, collapse whitespace.
pub fn name_key(value: &str) -> String {
    let kept: String = value
        .nfkd()
        .filter(|c| canonical_combining_class(*c) == 0)
        .map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' })
        .collect();
    kept.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackRow {
    pub artist: String,
    pub title: String,
    pub label: String,
    pub start: String,
    pub end: String,
    pub source_file: String,
    pub seq: u32,
}

impl TrackRow {
    /// Convenience for a row with only the fields the matcher reads.
    pub fn new(artist: impl Into<String>, title: impl Into<String>, label: impl Into<String>) -> Self {
        Self { artist: artist.into(), title: title.into(), label: label.into(), ..Self::default() }
    }

    /// What makes two rows the same track, across files and spellings.
    pub fn key(&self) -> (String, String) {
        (name_key(&self.artist), name_key(&self.title))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedFile {
    pub filename: String,
    /// Crate-name suggestion taken from the filename.
    pub title: String,
    pub rows: Vec<TrackRow>,
    /// Rows with neither an artist nor a title -- footers and the like.
    pub skipped: usize,
    pub error: Option<String>,
}

impl ParsedFile {
    pub fn new(filename: impl Into<String>, title: impl Into<String>) -> Self {
        Self { filename: filename.into(), title: title.into(), ..Self::default() }
    }

    pub fn failed(filename: &str, error: impl Into<String>) -> Self {
        Self {
            filename: filename.to_string(),
            title: title_from_filename(filename),
            error: Some(error.into()),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MergedTracklist {
    pub rows: Vec<TrackRow>,
    pub duplicates: usize,
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// The residue of UTF-8 read as cp1252: a lead byte that survived as Ã/Â/â
/// followed by a continuation byte that survived as a Latin-1 supplement char.
static MOJIBAKE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("Ã[\u{80}-\u{bf}]|Â[\u{80}-\u{bf}]|â€").expect("static regex"));

/// cp1252 0x80..=0x9F; `None` = undefined in Python's codec.
const CP1252_HIGH: [Option<char>; 32] = [
    Some('\u{20AC}'), None, Some('\u{201A}'), Some('\u{0192}'), Some('\u{201E}'), Some('\u{2026}'),
    Some('\u{2020}'), Some('\u{2021}'), Some('\u{02C6}'), Some('\u{2030}'), Some('\u{0160}'),
    Some('\u{2039}'), Some('\u{0152}'), None, Some('\u{017D}'), None, None, Some('\u{2018}'),
    Some('\u{2019}'), Some('\u{201C}'), Some('\u{201D}'), Some('\u{2022}'), Some('\u{2013}'),
    Some('\u{2014}'), Some('\u{02DC}'), Some('\u{2122}'), Some('\u{0161}'), Some('\u{203A}'),
    Some('\u{0153}'), None, Some('\u{017E}'), Some('\u{0178}'),
];

fn cp1252_decode(data: &[u8]) -> Option<String> {
    let mut out = String::with_capacity(data.len());
    for &b in data {
        match b {
            0x80..=0x9f => out.push(CP1252_HIGH[(b - 0x80) as usize]?),
            _ => out.push(b as char),
        }
    }
    Some(out)
}

fn cp1252_encode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        let cp = c as u32;
        if cp < 0x80 || (0xA0..=0xFF).contains(&cp) {
            out.push(cp as u8);
        } else {
            let idx = CP1252_HIGH.iter().position(|m| *m == Some(c))?;
            out.push(0x80 + idx as u8);
        }
    }
    Some(out)
}

/// Undo one round of "UTF-8 decoded as cp1252", where that is what happened.
///
/// Only attempted when the tell-tale sequences are present, and only kept when
/// the round-trip both succeeds and leaves fewer of them than it found. That
/// second test is what stops a file legitimately containing "Â£" from being
/// mangled by a repair it never needed.
pub fn repair_mojibake(text: &str) -> String {
    let before = MOJIBAKE.find_iter(text).count();
    if before == 0 {
        return text.to_string();
    }
    let repaired = cp1252_encode(text).and_then(|b| String::from_utf8(b).ok());
    match repaired {
        Some(r) if MOJIBAKE.find_iter(&r).count() < before => r,
        _ => text.to_string(),
    }
}

/// Bytes to text, preferring UTF-8 and never failing.
///
/// `utf-8-sig` first so a BOM written by Excel does not end up glued to the
/// first header name. cp1252 next because that is what everything else on a
/// Windows-authored CSV turns out to be; latin-1 last because it cannot fail,
/// which keeps a genuinely broken file readable enough to report on.
pub fn decode(data: &[u8]) -> String {
    let body = data.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(data);
    if let Ok(s) = std::str::from_utf8(body) {
        return repair_mojibake(s);
    }
    if let Some(s) = cp1252_decode(data) {
        return repair_mojibake(&s);
    }
    let latin1: String = data.iter().map(|&b| b as char).collect();
    repair_mojibake(&latin1)
}

// ---------------------------------------------------------------------------
// Header mapping
// ---------------------------------------------------------------------------

const ALIASES: [(&str, &[&str]); 5] = [
    ("artist", &["artist", "artists", "kunstler", "künstler", "interpret", "act"]),
    ("title", &["title", "titel", "track", "track name", "trackname", "name", "song"]),
    ("label", &["label", "record label", "imprint"]),
    ("start", &["start", "start time", "begin", "from", "time"]),
    ("end", &["end", "end time", "stop", "until", "to"]),
];

fn fold_header(raw: &str) -> String {
    raw.replace('\u{feff}', "").trim().to_lowercase()
}

/// Which CSV column (by header spelling) answers which of our fields. First
/// match wins, so a file carrying both `Track` and `Title` uses whichever its
/// author put first.
fn map_columns(fieldnames: &[String]) -> Vec<(&'static str, String)> {
    let mut mapping: Vec<(&'static str, String)> = Vec::new();
    for raw in fieldnames {
        let folded = fold_header(raw);
        for (ours, names) in ALIASES {
            if !mapping.iter().any(|(k, _)| *k == ours) && names.contains(&folded.as_str()) {
                mapping.push((ours, raw.clone()));
                break;
            }
        }
    }
    mapping
}

const PLACEHOLDERS: [&str; 7] = ["", "n/a", "na", "-", "--", "unknown", "?"];

/// Cell value for a mapped column. As with `csv.DictReader`, when a header
/// name is repeated the *last* such column supplies the value.
fn cell(record: &csv::StringRecord, fieldnames: &[String], column: Option<&String>) -> String {
    let Some(column) = column else { return String::new() };
    let Some(idx) = fieldnames.iter().rposition(|f| f == column) else { return String::new() };
    let value = record.get(idx).unwrap_or("").trim();
    if PLACEHOLDERS.contains(&value.to_lowercase().as_str()) { String::new() } else { value.to_string() }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// The crate-name suggestion: the filename, minus its extension and path. Left
/// as the author wrote it and not title-cased.
pub fn title_from_filename(filename: &str) -> String {
    let normalised = filename.replace('\\', "/");
    let stem = normalised.split('/').rfind(|p| !p.is_empty() && *p != ".").unwrap_or("");
    let stem = match stem.rsplit_once('.') {
        Some((head, _)) => head,
        None => stem,
    };
    stem.trim().to_string()
}

/// One uploaded CSV as rows we can search Bandcamp for.
///
/// Errors when the file carries no artist or no title column, naming what it
/// did find -- "missing Title" is actionable, "0 rows" is not.
pub fn parse_tracklist(data: &[u8], filename: &str) -> Result<ParsedFile, TracklistError> {
    let mut parsed = ParsedFile::new(filename, title_from_filename(filename));
    let text = decode(data);

    // Python's DictReader takes a *blank first line* as an empty header row.
    let leading_blank = text.starts_with('\n') || text.starts_with('\r');

    let sample: String = text.chars().take(2048).collect();
    // A single-column file, or one the sniffer cannot read: comma is both the
    // overwhelming default and harmless when there is nothing to split.
    let dialect = sniff::sniff(&sample).unwrap_or(sniff::Dialect {
        delimiter: b',',
        quote: b'"',
        double_quote: true,
        skip_initial_space: false,
    });
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(dialect.delimiter)
        .quote(dialect.quote)
        .double_quote(dialect.double_quote)
        .from_reader(text.as_bytes());
    let mut records = reader.records();

    let fieldnames: Vec<String> = if leading_blank {
        Vec::new()
    } else {
        match records.next() {
            Some(Ok(rec)) => rec.iter().map(str::to_string).collect(),
            Some(Err(e)) => return Err(TracklistError(format!("{filename}: malformed CSV: {e}"))),
            None => Vec::new(),
        }
    };
    let columns = map_columns(&fieldnames);
    let col = |name: &str| columns.iter().find(|(k, _)| *k == name).map(|(_, v)| v);

    if col("artist").is_none() || col("title").is_none() {
        let found = if fieldnames.is_empty() {
            "no header row".to_string()
        } else {
            fieldnames.iter().map(|f| fold_header(f)).collect::<Vec<_>>().join(", ")
        };
        let missing = ["artist", "title"]
            .into_iter()
            .filter(|c| col(c).is_none())
            .collect::<Vec<_>>()
            .join(" and ");
        return Err(TracklistError(format!("{filename}: no {missing} column. Columns found: {found}.")));
    }

    for record in records {
        let record = record.map_err(|e| TracklistError(format!("{filename}: malformed CSV: {e}")))?;
        let artist = cell(&record, &fieldnames, col("artist"));
        let title = cell(&record, &fieldnames, col("title"));
        if artist.is_empty() && title.is_empty() {
            parsed.skipped += 1;
            continue;
        }
        parsed.rows.push(TrackRow {
            artist,
            title,
            label: cell(&record, &fieldnames, col("label")),
            start: cell(&record, &fieldnames, col("start")),
            end: cell(&record, &fieldnames, col("end")),
            source_file: filename.to_string(),
            seq: 0,
        });
    }
    Ok(parsed)
}

/// Every file's rows as one list, each track once. First occurrence wins and
/// keeps its file, which keeps the review table in upload order.
pub fn merge_files(files: &[ParsedFile]) -> MergedTracklist {
    let mut merged = MergedTracklist::default();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for parsed in files {
        for row in &parsed.rows {
            if !seen.insert(row.key()) {
                merged.duplicates += 1;
                continue;
            }
            let mut r = row.clone();
            r.seq = merged.rows.len() as u32 + 1;
            merged.rows.push(r);
        }
    }
    merged
}

/// What to prefill the crate name with. One file names itself; several do not,
/// and a wrong prefill is worse than an empty field.
pub fn suggest_title(files: &[ParsedFile]) -> String {
    let usable: Vec<&ParsedFile> = files.iter().filter(|f| f.error.is_none()).collect();
    if usable.len() == 1 { usable[0].title.clone() } else { String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLUBROOM_418: &str = "#,Start,End,Artist,Title,Label
1,00:06:03,00:10:16,Luke Alessi,Sex Machine,Coffee Cola
2,00:22:29,00:29:10,Boogie Vice,2AM (Extended Mix),DFTD
3,00:34:26,00:40:36,Roman Flügel,Tippex In My Eye (Dance Mix),Phantasy Sound
4,00:40:37,00:44:42,Mark Broom,MXM,Rekids
5,00:52:20,01:00:09,Slam,Lifetimes,Soma Records
";
    const CLUBROOM_419: &str = "#,Start,End,Artist,Title,Label
8,00:42:31,00:49:26,Dj Honesty,Wired,Syncrophone
9,00:50:01,00:53:25,Norty Cotto,Stand On Up,Naughty Boy Music
";
    const CLUBROOM_421: &str = "#,Start,End,Artist,Title,Label
1,00:01:12,00:04:30,Norty Cotto,Stand On Up,Naughty Boy Music
2,00:14:34,00:21:21,Dj Honesty,Wired,Syncrophone
3,00:27:29,00:32:19,JB Martinz,Drop It Heavy,SB Recordings
";

    /// The bytes a real export produces: UTF-8 read as cp1252, written as UTF-8.
    fn mangled(text: &str) -> Vec<u8> {
        cp1252_decode(text.as_bytes()).unwrap().into_bytes()
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn double_encoded_utf8_is_repaired() {
        let data = mangled(CLUBROOM_418);
        assert!(contains(&data, b"Fl\xc3\x83\xc2\xbcgel"), "fixture is not actually mangled");
        let parsed = parse_tracklist(&data, "clubroom-418.csv").unwrap();
        assert_eq!(parsed.rows[2].artist, "Roman Flügel");
    }

    #[test]
    fn a_correctly_encoded_file_is_left_alone() {
        let utf8 = CLUBROOM_418.as_bytes().to_vec();
        // cp1252 and latin-1 agree for ü (0xFC)
        let single: Vec<u8> = CLUBROOM_418.chars().map(|c| c as u32 as u8).collect();
        for data in [utf8, single] {
            let parsed = parse_tracklist(&data, "x.csv").unwrap();
            assert_eq!(parsed.rows[2].artist, "Roman Flügel");
        }
    }

    #[test]
    fn a_utf8_bom_does_not_stick_to_the_first_header() {
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice(CLUBROOM_418.as_bytes());
        let parsed = parse_tracklist(&data, "x.csv").unwrap();
        assert_eq!(parsed.rows.len(), 5);
        assert_eq!(parsed.rows[0].artist, "Luke Alessi");
    }

    #[test]
    fn text_that_only_looks_like_mojibake_is_not_mangled() {
        assert_eq!(repair_mojibake("a fiver, £5"), "a fiver, £5");
        // Literal "Â£" would be "repaired" to "£" only if that reduced the tell-tales;
        // an unencodable char keeps the text as-is.
        assert_eq!(repair_mojibake("Ã\u{4e2d}"), "Ã\u{4e2d}");
    }

    #[test]
    fn undecodable_bytes_still_produce_text() {
        assert!(decode(b"\xff\xfe artist").trim().ends_with("artist"));
        // 0x81 is undefined in cp1252, so this falls through to latin-1.
        assert!(decode(b"\x81 artist").trim().ends_with("artist"));
    }

    #[test]
    fn header_aliases_are_accepted() {
        let parsed = parse_tracklist(b"No,Track,Artist\n1,Wired,Dj Honesty\n", "x.csv").unwrap();
        assert_eq!(parsed.rows[0].title, "Wired");
        assert_eq!(parsed.rows[0].artist, "Dj Honesty");
    }

    #[test]
    fn a_semicolon_delimited_file_is_read() {
        let parsed = parse_tracklist(b"Artist;Title;Label\nSlam;Lifetimes;Soma\n", "x.csv").unwrap();
        assert_eq!(parsed.rows[0].label, "Soma");
    }

    #[test]
    fn a_file_without_an_artist_or_title_column_names_what_it_found() {
        let err = parse_tracklist(b"Start,End,Label\n1,2,3\n", "mystery.csv").unwrap_err();
        assert!(err.to_string().contains("artist and title"));
        assert!(err.to_string().contains("start, end, label"));
    }

    #[test]
    fn placeholder_labels_are_dropped() {
        let parsed = parse_tracklist(b"Artist,Title,Label\nMr. G,City Heat,N/A\n", "x.csv").unwrap();
        assert_eq!(parsed.rows[0].label, "");
    }

    #[test]
    fn blank_rows_are_counted_rather_than_kept() {
        let parsed = parse_tracklist(b"Artist,Title\nSlam,Lifetimes\n,\n,\n", "x.csv").unwrap();
        assert_eq!(parsed.rows.len(), 1);
        assert_eq!(parsed.skipped, 2);
    }

    #[test]
    fn quoted_fields_with_delimiters_survive() {
        let parsed =
            parse_tracklist(b"Artist,Title\n\"Heavy, The\",\"Say \"\"Hi\"\"\"\nSlam,Lifetimes\n", "x.csv")
                .unwrap();
        assert_eq!(parsed.rows[0].artist, "Heavy, The");
        assert_eq!(parsed.rows[0].title, "Say \"Hi\"");
    }

    #[test]
    fn an_empty_or_headerless_file_says_so() {
        let err = parse_tracklist(b"", "e.csv").unwrap_err();
        assert!(err.to_string().contains("no header row"));
        let err = parse_tracklist(b"\nArtist,Title\n", "e.csv").unwrap_err();
        assert!(err.to_string().contains("no header row"));
    }

    #[test]
    fn a_track_two_sets_played_downloads_once() {
        let files = [
            parse_tracklist(&mangled(CLUBROOM_418), "clubroom-418.csv").unwrap(),
            parse_tracklist(CLUBROOM_419.as_bytes(), "clubroom-419.csv").unwrap(),
            parse_tracklist(CLUBROOM_421.as_bytes(), "club-room-no-421.csv").unwrap(),
        ];
        let merged = merge_files(&files);
        assert_eq!(merged.rows.len(), 8);
        assert_eq!(merged.duplicates, 2);
        let seqs: Vec<u32> = merged.rows.iter().map(|r| r.seq).collect();
        assert_eq!(seqs, (1..=8).collect::<Vec<u32>>());
    }

    #[test]
    fn the_first_file_to_play_a_track_keeps_it() {
        let files = [
            parse_tracklist(CLUBROOM_419.as_bytes(), "419.csv").unwrap(),
            parse_tracklist(CLUBROOM_421.as_bytes(), "421.csv").unwrap(),
        ];
        let merged = merge_files(&files);
        let wired = merged.rows.iter().find(|r| r.title == "Wired").unwrap();
        assert_eq!(wired.source_file, "419.csv");
    }

    #[test]
    fn duplicates_are_folded_across_spellings() {
        let files = [
            parse_tracklist(b"Artist,Title\nDj Honesty,Wired\n", "a.csv").unwrap(),
            parse_tracklist(b"Artist,Title\nDJ HONESTY,wired\n", "b.csv").unwrap(),
        ];
        assert_eq!(merge_files(&files).duplicates, 1);
    }

    #[test]
    fn the_filename_becomes_the_suggested_crate_name() {
        assert_eq!(
            title_from_filename("clubroom-418-with-anja-schneider.csv"),
            "clubroom-418-with-anja-schneider"
        );
        assert_eq!(title_from_filename("C:\\sets\\a b.csv"), "a b");
    }

    #[test]
    fn several_files_suggest_nothing() {
        let files = [ParsedFile::new("a.csv", "a"), ParsedFile::new("b.csv", "b")];
        assert_eq!(suggest_title(&files), "");
        assert_eq!(suggest_title(&files[..1]), "a");
    }

    #[test]
    fn a_file_that_failed_to_parse_does_not_name_the_crate() {
        let mut broken = ParsedFile::new("broken.csv", "broken");
        broken.error = Some("nope".into());
        let files = [broken, ParsedFile::new("good.csv", "good")];
        assert_eq!(suggest_title(&files), "good");
    }
}
