//! Pure logic shared by the Artists and Labels pages (no DOM): copy for banners, catalogue
//! filtering, URL helpers, range selection. Ported from the legacy `Labels.tsx` / `Artists.tsx`.
use bc_types::bandcamp::{ReleaseCardOut, RunResult, SweepStatus};
use bc_types::player::ExploreCard;
use bc_types::library::{ArtistSort, LabelSort, SortDir};

use crate::logic::format::format_count;

/// Singular/plural word: `plural(1, "release")` = "release", `plural(2, ..)` = "releases".
pub fn plural(n: i64, word: &str) -> String {
    if n == 1 { word.to_string() } else { format!("{word}s") }
}

/// `3 releases` with a thousands separator.
pub fn count_of(n: i64, word: &str) -> String {
    format!("{} {}", format_count(n), plural(n, word))
}

/// URL without scheme and trailing slash, for display.
pub fn strip_scheme(url: &str) -> String {
    url.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_string()
}

/// Same Bandcamp page (case and trailing slashes ignored).
pub fn same_url(a: Option<&str>, b: &str) -> bool {
    match a {
        Some(a) if !a.is_empty() => a.trim_end_matches('/').eq_ignore_ascii_case(b.trim_end_matches('/')),
        _ => false,
    }
}

/// Grids ask for thumbnails: the API serves `size=full` covers on artist rows, which is
/// far more than a 180px card needs.
pub fn thumb(url: &str) -> String {
    url.replace("size=full", "size=thumb")
}

// ---- sorts ----------------------------------------------------------------------------

pub struct SortDef<S> {
    pub value: &'static str,
    pub label: &'static str,
    pub sort: S,
    pub order: SortDir,
}

pub const ARTIST_SORTS: [SortDef<ArtistSort>; 5] = [
    SortDef { value: "name", label: "A\u{2013}Z", sort: ArtistSort::Name, order: SortDir::Asc },
    SortDef { value: "releases", label: "Most releases", sort: ArtistSort::Releases, order: SortDir::Desc },
    SortDef { value: "tracks", label: "Most tracks", sort: ArtistSort::Tracks, order: SortDir::Desc },
    SortDef { value: "added", label: "Recently added", sort: ArtistSort::Added, order: SortDir::Desc },
    SortDef { value: "plays", label: "Most played", sort: ArtistSort::Plays, order: SortDir::Desc },
];

pub const LABEL_SORTS: [SortDef<LabelSort>; 4] = [
    SortDef { value: "releases", label: "Most releases", sort: LabelSort::Releases, order: SortDir::Desc },
    SortDef { value: "tracks", label: "Most tracks", sort: LabelSort::Tracks, order: SortDir::Desc },
    SortDef { value: "name", label: "A\u{2013}Z", sort: LabelSort::Name, order: SortDir::Asc },
    SortDef { value: "added", label: "Recently added", sort: LabelSort::Added, order: SortDir::Desc },
];

pub fn artist_sort(value: &str) -> &'static SortDef<ArtistSort> {
    ARTIST_SORTS.iter().find(|s| s.value == value).unwrap_or(&ARTIST_SORTS[0])
}

pub fn label_sort(value: &str) -> &'static SortDef<LabelSort> {
    LABEL_SORTS.iter().find(|s| s.value == value).unwrap_or(&LABEL_SORTS[0])
}

pub fn dir_str(d: SortDir) -> &'static str {
    match d {
        SortDir::Asc => "asc",
        SortDir::Desc => "desc",
    }
}

/// Sorts of a label's releases listing: (value, label, order).
pub const RELEASE_SORTS: [(&str, &str, &str); 4] = [
    ("added", "Recently added", "desc"),
    ("year", "Year", "desc"),
    ("title", "Title", "asc"),
    ("artist", "Artist", "asc"),
];

pub fn release_sort_order(value: &str) -> &'static str {
    RELEASE_SORTS.iter().find(|s| s.0 == value).map(|s| s.2).unwrap_or("desc")
}

// ---- range selection --------------------------------------------------------------------

/// Inclusive index range between the anchor and the clicked folder.
pub fn shift_range(anchor: usize, index: usize) -> (usize, usize) {
    if anchor <= index { (anchor, index) } else { (index, anchor) }
}

// ---- Bandcamp catalogue -----------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CatalogFilter {
    #[default]
    Missing,
    Library,
    All,
}

impl CatalogFilter {
    pub fn key(self) -> &'static str {
        match self {
            CatalogFilter::Missing => "missing",
            CatalogFilter::Library => "library",
            CatalogFilter::All => "all",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "library" => CatalogFilter::Library,
            "all" => CatalogFilter::All,
            _ => CatalogFilter::Missing,
        }
    }
}

/// A release the library does not hold and has not blacklisted.
pub fn is_missing(r: &ReleaseCardOut) -> bool {
    !r.in_library && !r.blacklisted
}

pub fn filter_catalogue(all: &[ReleaseCardOut], f: CatalogFilter) -> Vec<ReleaseCardOut> {
    all.iter()
        .filter(|r| match f {
            CatalogFilter::All => true,
            CatalogFilter::Library => r.in_library,
            CatalogFilter::Missing => is_missing(r),
        })
        .cloned()
        .collect()
}

/// (missing, in library, all)
pub fn catalogue_counts(all: &[ReleaseCardOut]) -> (usize, usize, usize) {
    (all.iter().filter(|r| is_missing(r)).count(), all.iter().filter(|r| r.in_library).count(), all.len())
}

/// Everything to shuffle for a whole artist or label: the Bandcamp catalogue (owned releases play
/// from the library, the rest stream), without what is blacklisted, plus the library releases
/// the catalogue does not list (`library` is `(release id, Bandcamp URL)`).
pub fn whole_catalogue(catalogue: &[ReleaseCardOut], library: &[(i64, Option<String>)]) -> Vec<ExploreCard> {
    let mut cards: Vec<ExploreCard> =
        catalogue.iter().filter(|r| !r.blacklisted).map(|r| ExploreCard { url: r.url.clone(), library_release_id: r.library_release_id }).collect();
    let listed: std::collections::HashSet<i64> = catalogue.iter().filter_map(|r| r.library_release_id).collect();
    cards.extend(
        library
            .iter()
            .filter(|(id, _)| !listed.contains(id))
            .map(|(id, url)| ExploreCard { url: url.clone().unwrap_or_default(), library_release_id: Some(*id) }),
    );
    cards
}

// ---- banner copy --------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Ok,
    Err,
}

/// Summary of a finished "find new releases" harvest.
pub fn find_new_text(name: &str, res: &RunResult) -> String {
    let pending = res.pending_item_ids.len() as i64;
    let mut parts = vec![format!("{} on Bandcamp", format_count(res.seen)), format!("you have {} of them", format_count(res.in_library))];
    if res.queued > 0 {
        parts.push(format!("{} already downloading", format_count(res.queued)));
    }
    if pending > 0 {
        parts.push(format!("{} awaiting download", format_count(pending)));
    }
    if res.new > 0 {
        parts.push(format!("{} newly found", format_count(res.new)));
    }
    format!("{name}: {}.", parts.join(" \u{b7} "))
}

/// Banner line for a label sweep; `None` while idle.
pub fn sweep_text(s: &SweepStatus) -> Option<(Tone, String)> {
    let total = s.total.unwrap_or(0);
    match s.phase.as_str() {
        "harvesting" => {
            let scope = if s.scope == "selection" { "the selected labels" } else { "labels" };
            let found = if s.new > 0 { format!(" {} newly found so far.", format_count(s.new)) } else { String::new() };
            let now = s.current.as_ref().map(|c| format!(", now {c}")).unwrap_or_default();
            Some((Tone::Ok, format!("Checking {scope} for new releases \u{2014} {} of {} done{now}\u{2026}{found}", format_count(s.done), format_count(total))))
        }
        "queueing" => Some((Tone::Ok, "Queueing the new releases for download\u{2026}".into())),
        "failed" => Some((Tone::Err, format!("Label sweep failed: {}", s.error.clone().unwrap_or_else(|| "unknown error".into())))),
        "done" => {
            let stopped = s.error.as_deref() == Some("Stopped");
            let skipped = if s.no_url > 0 {
                format!(" {} {} no Bandcamp page and {} skipped.", format_count(s.no_url), if s.no_url == 1 { "label has" } else { "labels have" }, if s.no_url == 1 { "was" } else { "were" })
            } else {
                String::new()
            };
            let failed = if s.errors.is_empty() { String::new() } else { format!(" {} could not be checked.", format_count(s.errors.len() as i64)) };
            let lead = if stopped {
                format!("Stopped after {} of {} labels", format_count(s.done), format_count(total))
            } else {
                format!("Checked {} labels", format_count(total))
            };
            Some((Tone::Ok, format!("{lead}: {} newly found, {} queued for download.{skipped}{failed}", format_count(s.new), format_count(s.queued))))
        }
        _ => None,
    }
}

/// Banner line for the "find missing labels" resolver.
pub fn resolve_text(s: &bc_types::bandcamp::LabelResolveStatus) -> Option<(Tone, String)> {
    if s.running {
        return Some((Tone::Ok, format!("Asking label pages what they are \u{2014} {} of {} checked, {} labels found so far\u{2026}", format_count(s.seen), format_count(s.total.unwrap_or(0)), format_count(s.resolved))));
    }
    match s.phase.as_str() {
        "failed" => Some((Tone::Err, format!("Finding labels failed: {}", s.error.clone().unwrap_or_else(|| "unknown error".into())))),
        "done" if s.resolved > 0 => Some((
            Tone::Ok,
            format!("Found {} and filed {} under {}.", count_of(s.resolved, "label"), count_of(s.filed, "release"), if s.resolved == 1 { "it" } else { "them" }),
        )),
        "done" => Some((Tone::Ok, format!("Checked {}: nothing new to file.", count_of(s.total.unwrap_or(0), "Bandcamp page")))),
        _ => None,
    }
}

/// Years of an artist: `1998`, `1998\u{2013}2004` or none.
pub fn year_span(min: Option<i64>, max: Option<i64>) -> Option<String> {
    let min = min?;
    Some(match max {
        Some(m) if m != min => format!("{min}\u{2013}{m}"),
        _ => min.to_string(),
    })
}

/// URL of the Explore band page for a Bandcamp URL.
pub fn band_path(url: &str) -> String {
    format!("/explore/band?url={}", crate::util::enc(url))
}

pub fn release_path(url: &str) -> String {
    format!("/explore/release?url={}", crate::util::enc(url))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(in_library: bool, blacklisted: bool) -> ReleaseCardOut {
        ReleaseCardOut { url: "u".into(), title: "t".into(), artist_name: "a".into(), item_type: "album".into(), art_url: None, release_date: None, is_free_download: false, in_library, blacklisted, library_release_id: None }
    }

    #[test]
    fn whole_catalogue_streams_missing_plays_owned_and_adds_library_only_releases() {
        let owned = ReleaseCardOut { url: "o".into(), library_release_id: Some(7), ..card(true, false) };
        let missing = ReleaseCardOut { url: "m".into(), ..card(false, false) };
        let banned = ReleaseCardOut { url: "b".into(), ..card(false, true) };
        let cards = whole_catalogue(&[owned, missing, banned], &[(7, Some("o".into())), (9, None)]);
        assert_eq!(
            cards,
            vec![
                ExploreCard { url: "o".into(), library_release_id: Some(7) },
                ExploreCard { url: "m".into(), library_release_id: None },
                ExploreCard { url: String::new(), library_release_id: Some(9) },
            ]
        );
    }

    #[test]
    fn plural_forms() {
        assert_eq!(plural(1, "release"), "release");
        assert_eq!(plural(0, "release"), "releases");
        assert_eq!(count_of(1, "track"), "1 track");
    }

    #[test]
    fn urls() {
        assert_eq!(strip_scheme("https://a.bandcamp.com/"), "a.bandcamp.com");
        assert!(same_url(Some("https://A.bandcamp.com/"), "https://a.bandcamp.com"));
        assert!(!same_url(None, "x"));
        assert_eq!(thumb("/api/art/release/1?size=full&v=0"), "/api/art/release/1?size=thumb&v=0");
    }

    #[test]
    fn sorts_resolve() {
        assert_eq!(artist_sort("plays").sort, ArtistSort::Plays);
        assert_eq!(artist_sort("nope").sort, ArtistSort::Name);
        assert_eq!(label_sort("name").order, SortDir::Asc);
        assert_eq!(label_sort("").sort, LabelSort::Releases);
        assert_eq!(release_sort_order("title"), "asc");
    }

    #[test]
    fn range_is_inclusive_either_way() {
        assert_eq!(shift_range(2, 5), (2, 5));
        assert_eq!(shift_range(5, 2), (2, 5));
        assert_eq!(shift_range(3, 3), (3, 3));
    }

    #[test]
    fn catalogue_filters() {
        let all = vec![card(false, false), card(true, false), card(false, true)];
        assert_eq!(catalogue_counts(&all), (1, 1, 3));
        assert_eq!(filter_catalogue(&all, CatalogFilter::Missing).len(), 1);
        assert_eq!(filter_catalogue(&all, CatalogFilter::Library).len(), 1);
        assert_eq!(filter_catalogue(&all, CatalogFilter::All).len(), 3);
        assert_eq!(CatalogFilter::parse("library"), CatalogFilter::Library);
        assert_eq!(CatalogFilter::parse("x"), CatalogFilter::Missing);
    }

    #[test]
    fn find_new_summary() {
        let res = RunResult { kind: "label".into(), label: "L".into(), seen: 12, new: 3, already_known: 0, in_library: 9, queued: 0, pending_item_ids: vec![1, 2, 3], errors: vec![], tier_counts: Default::default() };
        assert_eq!(find_new_text("Foo", &res), "Foo: 12 on Bandcamp \u{b7} you have 9 of them \u{b7} 3 awaiting download \u{b7} 3 newly found.");
    }

    #[test]
    fn sweep_copy() {
        let mut s = SweepStatus { phase: "harvesting".into(), scope: "selection".into(), running: true, done: 2, total: Some(8), current: Some("X".into()), new: 4, ..Default::default() };
        let (t, text) = sweep_text(&s).unwrap();
        assert_eq!(t, Tone::Ok);
        assert!(text.contains("the selected labels") && text.contains("2 of 8") && text.contains("now X") && text.contains("4 newly found"));
        s.phase = "done".into();
        s.no_url = 1;
        s.error = Some("Stopped".into());
        let (_, text) = sweep_text(&s).unwrap();
        assert!(text.starts_with("Stopped after 2 of 8 labels") && text.contains("1 label has no Bandcamp page and was skipped"));
        s.phase = "idle".into();
        assert!(sweep_text(&s).is_none());
        s.phase = "failed".into();
        assert_eq!(sweep_text(&s).unwrap().0, Tone::Err);
    }

    #[test]
    fn years() {
        assert_eq!(year_span(Some(1998), Some(1998)).as_deref(), Some("1998"));
        assert_eq!(year_span(Some(1998), Some(2004)).as_deref(), Some("1998\u{2013}2004"));
        assert_eq!(year_span(None, None), None);
    }
}
