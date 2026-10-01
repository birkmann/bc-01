//! bandcamp-dl's file naming, ported exactly so that dedup can predict where a
//! release's files land and match files already on disk.
//!
//! Source of truth (installed `bandcamp-downloader` 0.0.17):
//! * `bandcamp_dl/bandcampdownloader.py::template_to_path` and `download_album`
//! * `bandcamp_dl/config.py`: `TEMPLATE`, `OK_CHARS = '-_~'`, `SPACE_CHAR = '-'`
//! * the `slugify` package it imports is **unicode-slugify** 0.1.5 (not
//!   `python-slugify`): NFKC-normalise, keep characters whose Unicode general
//!   category starts with `L` or `N` (or that appear in `ok`), turn category
//!   `Z*` into a plain space, drop everything else (including combining marks,
//!   punctuation and symbols), collapse runs of `[<space_replacement>\s]+`
//!   into one `space_replacement`, lowercase.
//!
//! `--ascii-only` (needs `unidecode`) is not supported: the app never passes it.

use std::path::PathBuf;

use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_normalization::UnicodeNormalization;

/// `config.TEMPLATE`: the hierarchical layout used for every ordinary download.
pub const DEFAULT_TEMPLATE: &str = "%{artist}/%{album}/%{track} - %{title}";

/// No path separators, so a whole batch lands flat in one folder. The artist and
/// album stay in the file name: they are what keeps two albums' "01 - Intro"
/// from colliding once nothing separates them into directories.
pub const FLAT_TEMPLATE: &str = "%{artist} - %{album} - %{track} - %{title}";

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

/// The fields `template_to_path` reads (`track_meta` in `download_album`).
#[derive(Debug, Clone, Default)]
pub struct TrackMeta {
    /// Per-track artist (`track['artist']`); `None` falls back to the album artist.
    pub artist: Option<String>,
    pub albumartist: String,
    pub album: String,
    /// Already stripped of the `"{artist} - "` prefix; see [`strip_artist_prefix`].
    pub title: String,
    /// `None` renders as `Single` (bandcamp-dl compares the track number to the
    /// string `"None"`).
    pub track: Option<u32>,
    pub date: String,
    pub label: String,
}

/// `track['title'].replace(f"{track['artist']} - ", "", 1)`.
///
/// Fidelity quirk kept on purpose: with no track artist Python formats `None`
/// into the needle, so a title containing `"None - "` loses it.
pub fn strip_artist_prefix(title: &str, track_artist: Option<&str>) -> String {
    let needle = format!("{} - ", track_artist.unwrap_or("None"));
    title.replacen(&needle, "", 1)
}

/// Expand `template` for one track exactly as `template_to_path` does (slugified,
/// default options). Returns the path **relative to the base dir and without the
/// `.mp3` extension** bandcamp-dl appends; see [`expected_file`].
pub fn expand_template(template: &str, meta: &TrackMeta) -> PathBuf {
    expand_template_with(template, meta, &SlugOptions::default(), false)
}

/// [`expand_template`] with explicit slug options and the `--no-slugify` switch.
pub fn expand_template_with(
    template: &str,
    meta: &TrackMeta,
    opts: &SlugOptions,
    no_slugify: bool,
) -> PathBuf {
    let f = |s: &str| if no_slugify { s.to_string() } else { slugify_with(s, opts) };
    let mut path = template.to_string();
    // Python order matters: trackartist, artist, album, title, date, label, track.
    let trackartist = f(meta.artist.as_deref().unwrap_or(&meta.albumartist));
    path = path.replace("%{trackartist}", &trackartist);
    path = path.replace("%{artist}", &f(&meta.albumartist));
    path = path.replace("%{album}", &f(&meta.album));
    path = path.replace("%{title}", &f(&meta.title));
    path = path.replace("%{date}", &f(&meta.date));
    path = path.replace("%{label}", &f(&meta.label));
    let track = match meta.track {
        None => "Single".to_string(),
        Some(n) => format!("{n:02}"),
    };
    path = path.replace("%{track}", &track);
    PathBuf::from(path)
}

/// Full expected audio path: `<base_dir>/<expanded>.mp3`.
pub fn expected_file(base_dir: &std::path::Path, template: &str, meta: &TrackMeta) -> PathBuf {
    let mut rel = expand_template(template, meta).into_os_string();
    rel.push(".mp3");
    base_dir.join(rel)
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

    fn meta(artist: &str, album: &str, track: Option<u32>, title: &str) -> TrackMeta {
        TrackMeta {
            artist: None,
            albumartist: artist.into(),
            album: album.into(),
            title: title.into(),
            track,
            date: "2024".into(),
            label: "Some Label".into(),
        }
    }

    /// Each case is a real `files.path` of the existing library (relative to its
    /// `/mnt/storage/bandcamp` root) with the tags the row carries.
    #[test]
    fn layout_matches_existing_library_paths() {
        let cases = [
            ("Paul Johnson", "Feel The Music (Album)", 6, "Summer Heat", "paul-johnson/feel-the-music-album/06 - summer-heat"),
            ("Paul Johnson", "Feel The Music (Album)", 7, "I Wonder Why", "paul-johnson/feel-the-music-album/07 - i-wonder-why"),
            (
                "Room Trax",
                "[ROOM043] Joline Scheffler - Anisoptera EP",
                5,
                "Joline Scheffler - Anisoptera (Franz Jäger Remix)",
                "room-trax/room043-joline-scheffler-anisoptera-ep/05 - joline-scheffler-anisoptera-franz-jäger-remix",
            ),
            (
                "Room Trax",
                "[ROOM043] Joline Scheffler - Anisoptera EP",
                6,
                "Closing Circle (You Can't Control Me)",
                "room-trax/room043-joline-scheffler-anisoptera-ep/06 - closing-circle-you-cant-control-me",
            ),
            ("O.M.Theorem", "Lemma1", 2, "Lemma1-A2", "omtheorem/lemma1/02 - lemma1-a2"),
            ("Randstad & CASKO", "Exquisite Corpse", 3, "Randstad & CASKO - Exquisite", "randstad-casko/exquisite-corpse/03 - randstad-casko-exquisite"),
            (
                "Drafted / Unthone",
                "Different Forms of Expression (incl. Tadeo & Andrea remixes) [MMAUDIO005]",
                2,
                "Ethereal Gates (Tadeo Remix)",
                "drafted-unthone/different-forms-of-expression-incl-tadeo-andrea-remixes-mmaudio005/02 - ethereal-gates-tadeo-remix",
            ),
            (
                "Jose Pouj | Christian Wünsch | P.E.A.R.L.",
                "Structural Abnormalities",
                3,
                "Structural Abnormalities - Christian Wünsch Remix",
                "jose-pouj-christian-wünsch-pearl/structural-abnormalities/03 - structural-abnormalities-christian-wünsch-remix",
            ),
            (
                "Stanislav Tolkachev, Stave Karim Maas, Magna Pia, Wrong Assessment, Danilenko, The Extraverse",
                "V/A - Bats In Pool (Vol.I)",
                2,
                "Stave, Karim Maas - False Architecture",
                "stanislav-tolkachev-stave-karim-maas-magna-pia-wrong-assessment-danilenko-the-extraverse/va-bats-in-pool-voli/02 - stave-karim-maas-false-architecture",
            ),
            ("BRÄLLE,no more tears,INNSIGNN", "No More Tears", 1, "BRÄLLE - No More Tears", "brälleno-more-tearsinnsignn/no-more-tears/01 - brälle-no-more-tears"),
        ];
        for (artist, album, n, title, expected) in cases {
            let got = expand_template(DEFAULT_TEMPLATE, &meta(artist, album, Some(n), title));
            assert_eq!(got, PathBuf::from(expected), "{artist} / {album} / {title}");
        }
    }

    #[test]
    fn track_number_is_zero_padded_and_single_has_a_word() {
        assert_eq!(expand_template("%{track}", &meta("a", "b", Some(7), "t")), PathBuf::from("07"));
        assert_eq!(expand_template("%{track}", &meta("a", "b", Some(123), "t")), PathBuf::from("123"));
        assert_eq!(expand_template("%{track} - %{title}", &meta("a", "b", None, "Solo")), PathBuf::from("Single - solo"));
    }

    #[test]
    fn flat_template_has_no_separators_and_expands_flat() {
        assert!(!FLAT_TEMPLATE.contains('/'));
        let p = expand_template(FLAT_TEMPLATE, &meta("Somatic", "Grid Failure", Some(1), "Opening Drift"));
        assert_eq!(p, PathBuf::from("somatic - grid-failure - 01 - opening-drift"));
    }

    #[test]
    fn other_tokens() {
        let mut m = meta("Alb Artist", "Album", Some(1), "Title");
        m.artist = Some("Track Artist".into());
        assert_eq!(
            expand_template("%{trackartist}|%{artist}|%{date}|%{label}", &m),
            PathBuf::from("track-artist|alb-artist|2024|some-label")
        );
        // No track artist: falls back to the album artist.
        m.artist = None;
        assert_eq!(expand_template("%{trackartist}", &m), PathBuf::from("alb-artist"));
        // --no-slugify keeps the raw values.
        let raw = expand_template_with("%{artist}/%{title}", &m, &SlugOptions::default(), true);
        assert_eq!(raw, PathBuf::from("Alb Artist/Title"));
    }

    #[test]
    fn artist_prefix_is_stripped_once() {
        assert_eq!(strip_artist_prefix("Joline - Anisoptera", Some("Joline")), "Anisoptera");
        assert_eq!(strip_artist_prefix("Joline - A - Joline - B", Some("Joline")), "A - Joline - B");
        assert_eq!(strip_artist_prefix("No Prefix", Some("Joline")), "No Prefix");
        assert_eq!(strip_artist_prefix("None - Odd", None), "Odd", "python formats None into the needle");
    }

    #[test]
    fn expected_file_appends_mp3() {
        let p = expected_file(std::path::Path::new("/base"), DEFAULT_TEMPLATE, &meta("A B", "C", Some(1), "D"));
        assert_eq!(p, PathBuf::from("/base/a-b/c/01 - d.mp3"));
    }
}
