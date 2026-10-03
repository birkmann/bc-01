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

pub use bc_core::slug::{EMPTY_ALBUM, EMPTY_ARTIST, EMPTY_TITLE, OK_CHARS, SPACE_CHAR, SlugOptions, slugify, slugify_with};

/// `config.TEMPLATE`: the hierarchical layout used for every ordinary download.
pub const DEFAULT_TEMPLATE: &str = bc_core::config::DEFAULT_DOWNLOAD_TEMPLATE;

/// No path separators, so a whole batch lands flat in one folder. The artist and
/// album stay in the file name: they are what keeps two albums' "01 - Intro"
/// from colliding once nothing separates them into directories.
pub const FLAT_TEMPLATE: &str = "%{artist} - %{album} - %{track} - %{title}";

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

/// Expand `template` for one track as `template_to_path` does (slugified, default options).
/// Returns the path **relative to the base dir and without the `.mp3` extension** bandcamp-dl
/// appends; see [`expected_file`].
///
/// Two departures, both only where bandcamp-dl would fail outright: an empty artist, album or
/// title becomes [`EMPTY_ARTIST`] / [`EMPTY_ALBUM`] / [`EMPTY_TITLE`], and each path component
/// is capped at [`bc_core::slug::MAX_COMPONENT_BYTES`] so a long name cannot hit `ENAMETOOLONG`.
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
    let or = |s: String, empty: &str| if s.trim().is_empty() { empty.to_string() } else { s };
    let mut path = template.to_string();
    // Python order matters: trackartist, artist, album, title, date, label, track.
    let trackartist = or(f(meta.artist.as_deref().unwrap_or(&meta.albumartist)), EMPTY_ARTIST);
    path = path.replace("%{trackartist}", &trackartist);
    path = path.replace("%{artist}", &or(f(&meta.albumartist), EMPTY_ARTIST));
    path = path.replace("%{album}", &or(f(&meta.album), EMPTY_ALBUM));
    path = path.replace("%{title}", &or(f(&meta.title), EMPTY_TITLE));
    path = path.replace("%{date}", &f(&meta.date));
    path = path.replace("%{label}", &f(&meta.label));
    let track = match meta.track {
        None => "Single".to_string(),
        Some(n) => format!("{n:02}"),
    };
    path = path.replace("%{track}", &track);
    path.split('/').map(bc_core::slug::cap_component).collect::<Vec<_>>().join("/").into()
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
    fn names_that_slugify_to_nothing_get_a_placeholder() {
        assert_eq!(expand_template(DEFAULT_TEMPLATE, &meta("!!!", "Louden Up Now", Some(1), "Pardon My Freedom")), PathBuf::from("unknown-artist/louden-up-now/01 - pardon-my-freedom"));
        assert_eq!(expand_template(DEFAULT_TEMPLATE, &meta("Artist", "\u{1F525}\u{1F525}", Some(2), "???")), PathBuf::from("artist/untitled/02 - track"));
        let mut m = meta("Alb", "A", Some(1), "T");
        m.artist = Some("***".into());
        assert_eq!(expand_template("%{trackartist}", &m), PathBuf::from("unknown-artist"));
        // Optional fields stay empty: they are not path anchors.
        m.date = String::new();
        assert_eq!(expand_template("%{album} %{date}", &m), PathBuf::from("a "));
    }

    #[test]
    fn every_component_fits_a_filesystem_name() {
        let long = "北京".repeat(150);
        let p = expand_template(DEFAULT_TEMPLATE, &meta(&long, &long, Some(1), &long));
        for c in p.components() {
            let n = c.as_os_str().len();
            assert!(n <= bc_core::slug::MAX_COMPONENT_BYTES, "component of {n} bytes");
        }
        let flat = expand_template(FLAT_TEMPLATE, &meta(&long, &long, Some(1), &long));
        assert_eq!(flat.components().count(), 1);
        assert!(flat.as_os_str().len() <= bc_core::slug::MAX_COMPONENT_BYTES);
        assert!(flat.to_string_lossy().starts_with("北京"), "the cut keeps the front of the name");
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
