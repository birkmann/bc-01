//! bandcamp-dl's slug, ported exactly, shared by the downloaders (which name files with it) and
//! maintenance (which predicts where a release's files land).
//!
//! The `slugify` package bandcamp-dl imports is **unicode-slugify** 0.1.5 (not `python-slugify`):
//! NFKC-normalise, keep characters whose Unicode general category starts with `L` or `N` (or
//! that appear in `ok`), turn category `Z*` into a plain space, drop everything else (including
//! combining marks, punctuation and symbols), collapse runs of `[<space_replacement>\s]+` into
//! one `space_replacement`, lowercase.

use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_normalization::UnicodeNormalization;

/// `config.OK_CHARS`.
pub const OK_CHARS: &str = "-_~";
/// `config.SPACE_CHAR`.
pub const SPACE_CHAR: &str = "-";

/// The knobs `slugify_preset` passes through (bandcamp-dl CLI flags `-c -s -k -u`).
#[derive(Debug, Clone)]
pub struct SlugOptions {
    pub ok_chars: String,
    pub space_char: String,
    pub keep_spaces: bool,
    pub keep_upper: bool,
}

impl Default for SlugOptions {
    fn default() -> Self {
        Self {
            ok_chars: OK_CHARS.to_string(),
            space_char: SPACE_CHAR.to_string(),
            keep_spaces: false,
            keep_upper: false,
        }
    }
}

/// `slugify._sanitize`.
fn sanitize(text: &str, ok: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let cat = get_general_category(c);
        let major = match cat {
            GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
            | GeneralCategory::DecimalNumber
            | GeneralCategory::LetterNumber
            | GeneralCategory::OtherNumber => 'L',
            GeneralCategory::SpaceSeparator
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator => 'Z',
            _ => 'x',
        };
        if major == 'L' || ok.contains(c) {
            out.push(c);
        } else if major == 'Z' {
            out.push(' ');
        }
    }
    // Python str.strip() strips Unicode whitespace; after sanitising only ' ' can
    // be left over, but keep the semantics anyway.
    out.trim().to_string()
}

/// `slugify.slugify(s, ok, only_ascii=False, spaces, lower, space_replacement)`.
pub fn slugify_with(s: &str, opts: &SlugOptions) -> String {
    let normalised: String = s.nfkc().collect();
    let mut new = sanitize(&normalised, &opts.ok_chars);
    if !opts.keep_spaces {
        let mut rep = opts.space_char.clone();
        if !rep.is_empty() && !opts.ok_chars.contains(&rep) {
            rep = opts.ok_chars.chars().next().map(String::from).unwrap_or_default();
        }
        // re.sub('[%s\s]+' % rep, rep, new)
        let is_sep = |c: char| c.is_whitespace() || rep.contains(c);
        let mut collapsed = String::with_capacity(new.len());
        let mut in_run = false;
        for c in new.chars() {
            if is_sep(c) {
                if !in_run {
                    collapsed.push_str(&rep);
                    in_run = true;
                }
            } else {
                collapsed.push(c);
                in_run = false;
            }
        }
        new = collapsed;
    }
    if !opts.keep_upper {
        new = new.to_lowercase();
    }
    new
}

/// bandcamp-dl's default slug (`ok='-_~'`, `-` for spaces, lowercase).
pub fn slugify(s: &str) -> String {
    slugify_with(s, &SlugOptions::default())
}

/// What a name that slugifies to nothing ("!!!", an all-emoji title) is filed under. bandcamp-dl
/// itself breaks there -- an empty artist expands to an absolute `/album/...` path, an empty album
/// to `artist//...` -- so these never move a file it could have written.
pub const EMPTY_ARTIST: &str = "unknown-artist";
pub const EMPTY_ALBUM: &str = "untitled";
pub const EMPTY_TITLE: &str = "track";

/// Longest single path component (file or folder name) a download may produce, in bytes. Most
/// filesystems allow 255; the rest is headroom for the extension, a `.part` suffix while writing
/// and a ` (2)` collision suffix. Anything bandcamp-dl can write without `ENAMETOOLONG` fits, so
/// capping never moves a file it could have produced.
pub const MAX_COMPONENT_BYTES: usize = 240;

/// `s` cut to at most `max` bytes on a character boundary. Returns `s` unchanged when it fits.
pub fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// One path component capped at [`MAX_COMPONENT_BYTES`]. A cut that lands right after a
/// separator drops it, so a name never ends in a dangling `-` or space.
pub fn cap_component(s: &str) -> &str {
    let cut = truncate_bytes(s, MAX_COMPONENT_BYTES);
    if cut.len() == s.len() { cut } else { cut.trim_end_matches(['-', ' ']) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basics() {
        assert_eq!(slugify("Feel The Music (Album)"), "feel-the-music-album");
        assert_eq!(slugify("O.M.Theorem"), "omtheorem");
        assert_eq!(slugify("Randstad & CASKO"), "randstad-casko");
        assert_eq!(slugify("Drafted / Unthone"), "drafted-unthone");
        assert_eq!(slugify("Closing Circle (You Can't Control Me)"), "closing-circle-you-cant-control-me");
        assert_eq!(slugify("  Hello   World  "), "hello-world");
        assert_eq!(slugify("a - b"), "a-b", "spaces and dashes collapse into one dash");
        assert_eq!(slugify("a_b~c"), "a_b~c", "ok chars survive");
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn slugify_keeps_unicode_letters_and_drops_marks() {
        assert_eq!(slugify("Christian Wünsch"), "christian-wünsch");
        assert_eq!(slugify("BRÄLLE"), "brälle");
        // NFKC composes u + combining diaeresis, so it survives as a letter...
        assert_eq!(slugify("Wu\u{308}nsch"), "wünsch");
        // ...but a mark with no precomposed form is category M and is dropped.
        assert_eq!(slugify("a\u{20DD}b"), "ab");
        // Fullwidth forms fold through NFKC.
        assert_eq!(slugify("ＡＢＣ１２３"), "abc123");
        // Non-breaking space is category Zs: becomes a separator.
        assert_eq!(slugify("a\u{a0}b"), "a-b");
        assert_eq!(slugify("北京 (capital)"), "北京-capital");
    }

    #[test]
    fn slugify_options() {
        let keep = SlugOptions { keep_spaces: true, keep_upper: true, ..Default::default() };
        assert_eq!(slugify_with("Hello  World", &keep), "Hello  World");
        let under = SlugOptions { ok_chars: "-_~".into(), space_char: "_".into(), ..Default::default() };
        assert_eq!(slugify_with("a b - c", &under), "a_b_-_c");
        // A space char that is not an ok char falls back to the first ok char.
        let odd = SlugOptions { space_char: "+".into(), ..Default::default() };
        assert_eq!(slugify_with("a b", &odd), "a-b");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_bytes("abc", 10), "abc");
        assert_eq!(truncate_bytes("äöü", 3), "ä", "a 2-byte char is never split");
        let long = "北".repeat(100);
        let capped = cap_component(&long);
        assert!(capped.len() <= MAX_COMPONENT_BYTES);
        assert_eq!(capped.chars().count(), MAX_COMPONENT_BYTES / 3);
        let dashed = format!("{}-x", "a".repeat(MAX_COMPONENT_BYTES - 1));
        assert_eq!(cap_component(&dashed), "a".repeat(MAX_COMPONENT_BYTES - 1), "no dangling separator");
        assert_eq!(cap_component("short-"), "short-", "names that fit are untouched");
    }
}
