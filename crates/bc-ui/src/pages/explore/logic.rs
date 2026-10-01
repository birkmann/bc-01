//! Pure Explore logic: ports of the legacy `bandcampSearch.ts` plus the
//! facet / saved-query / queue-item helpers that were buried in `Explore.tsx`
//! and `ExploreGrid.tsx`. DOM free, so it is tested natively.
use std::collections::{BTreeMap, HashSet};

use bc_types::bandcamp::{ExploreReleaseOut, ExploreTrackOut, ReleaseCardOut, SearchHitOut};
use bc_types::player::{ExploreCard, ItemOrigin, QueueItem};
use serde_json::Value;

/// How many records a tag needs before the query is read as a genre (legacy `TAG_IS_A_GENRE`).
pub const TAG_IS_A_GENRE: i64 = 1000;
/// Covers shown before a discography needs asking for the rest.
pub const DISCOGRAPHY_PREVIEW: usize = 14;
/// A ceiling on one sweep: every release is its own page fetch server side.
pub const MAX_SWEEP: usize = 100;
/// `POST /explore/download` takes at most this many URLs per call.
pub const DOWNLOAD_CHUNK: usize = 200;

/// `encodeURIComponent`, natively testable.
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn band_path(url: &str) -> String {
    format!("/explore/band?url={}", pct_encode(url))
}
pub fn release_path(url: &str) -> String {
    format!("/explore/release?url={}", pct_encode(url))
}
/// Where the same words go on the Explore screen (the library search's "open in Explore" link).
#[allow(dead_code)]
pub fn explore_path(q: &str) -> String {
    format!("/explore?q={}", pct_encode(q))
}
pub fn tag_path(tag: &str) -> String {
    format!("/explore?tag={}", pct_encode(tag))
}

/// Two Bandcamp URLs for the same release, made comparable (scheme and trailing slash).
pub fn release_key(url: &str) -> String {
    let u = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    u.trim_end_matches('/').to_lowercase()
}

/// `https://x.bandcamp.com/album/y` -> `https://x.bandcamp.com`.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}").to_lowercase())
}

// ---------------------------------------------------------------------------
// Search hits (bandcampSearch.ts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupedHits {
    /// Artists and labels: pages, not music.
    pub bands: Vec<SearchHitOut>,
    pub albums: Vec<SearchHitOut>,
    pub tracks: Vec<SearchHitOut>,
}

impl GroupedHits {
    /// Albums first, so "play all" runs the records before the loose tracks.
    pub fn playable(&self) -> Vec<SearchHitOut> {
        self.albums.iter().chain(self.tracks.iter()).cloned().collect()
    }
}

/// One ranked list -> bands, albums, tracks. Fan profiles are dropped (the band view
/// cannot open them) and a second hit for the same page is noise.
pub fn group_hits(hits: &[SearchHitOut]) -> GroupedHits {
    let mut seen = HashSet::new();
    let mut g = GroupedHits::default();
    for hit in hits {
        if hit.kind == "fan" {
            continue;
        }
        if !seen.insert(hit.url.clone()) {
            continue;
        }
        match hit.kind.as_str() {
            "album" => g.albums.push(hit.clone()),
            "track" => g.tracks.push(hit.clone()),
            _ => g.bands.push(hit.clone()),
        }
    }
    g
}

/// An album hit as a grid card. `is_free_download` is the one thing autocomplete does not say.
pub fn hit_to_card(hit: &SearchHitOut) -> ReleaseCardOut {
    ReleaseCardOut {
        url: hit.url.clone(),
        title: hit.name.clone(),
        artist_name: hit.subtitle.clone(),
        item_type: hit.kind.clone(),
        art_url: hit.art_url.clone(),
        release_date: None,
        is_free_download: false,
        in_library: hit.in_library,
        blacklisted: hit.blacklisted,
        library_release_id: hit.library_release_id,
    }
}

/// Dedupe cards by URL, keeping the first of each (pages accumulate).
pub fn dedupe_cards(cards: impl IntoIterator<Item = ReleaseCardOut>) -> Vec<ReleaseCardOut> {
    let mut seen = HashSet::new();
    cards.into_iter().filter(|c| seen.insert(c.url.clone())).collect()
}

/// What a sweep needs from a grid: the card URL and its library copy.
pub fn sweep_cards<'a>(items: impl IntoIterator<Item = (&'a str, Option<i64>)>) -> Vec<ExploreCard> {
    items.into_iter().take(MAX_SWEEP).map(|(url, id)| ExploreCard { url: url.to_string(), library_release_id: id }).collect()
}

// ---------------------------------------------------------------------------
// Facets (Bandcamp's own filter vocabulary)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facet {
    pub id: i64,
    pub label: String,
    pub slug: String,
    /// Subgenres only: the genre they hang under.
    pub parent_slug: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facets {
    pub genres: Vec<Facet>,
    pub subgenres: Vec<Facet>,
    pub slices: Vec<Facet>,
    pub locations: Vec<Facet>,
}

fn facet(id: i64, label: &str, slug: &str) -> Facet {
    Facet { id, label: label.into(), slug: slug.into(), parent_slug: None }
}

/// Used only until the live vocabulary loads (or when it cannot). Free-text genres do not
/// work -- discover matches on names it already knows -- so these are real slugs.
pub fn fallback_genres() -> Vec<Facet> {
    vec![
        facet(10, "electronic", "electronic"),
        facet(23, "rock", "rock"),
        facet(14, "hip-hop/rap", "hip-hop-rap"),
        facet(8, "experimental", "experimental"),
        facet(15, "jazz", "jazz"),
        facet(20, "punk", "punk"),
        facet(18, "metal", "metal"),
        facet(2, "ambient", "ambient"),
    ]
}

pub fn fallback_slices() -> Vec<Facet> {
    vec![facet(2, "new arrivals", "new"), facet(3, "bandcamp picks", "rec"), facet(1, "best-selling", "top")]
}

fn parse_list(v: &Value, key: &str) -> Vec<Facet> {
    let Some(list) = v.get(key).and_then(|l| l.as_array()) else { return vec![] };
    list.iter()
        .filter_map(|f| {
            let slug = match f.get("slug") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => return None,
            };
            let label = f.get("label").or_else(|| f.get("name")).and_then(|l| l.as_str()).unwrap_or(&slug).to_string();
            let id = f.get("id").and_then(|i| i.as_i64()).unwrap_or(0);
            let parent_slug = f.get("parentSlug").or_else(|| f.get("parent_slug")).and_then(|p| p.as_str()).map(str::to_string);
            Some(Facet { id, label, slug, parent_slug })
        })
        .collect()
}

/// Read `GET /explore/genres`, falling back where a list is missing.
pub fn parse_facets(v: Option<&Value>) -> Facets {
    let null = Value::Null;
    let v = v.unwrap_or(&null);
    let mut f = Facets {
        genres: parse_list(v, "genres"),
        subgenres: parse_list(v, "subgenres"),
        slices: parse_list(v, "slices"),
        locations: parse_list(v, "locations"),
    };
    if f.genres.is_empty() {
        f.genres = fallback_genres();
    }
    if f.slices.is_empty() {
        f.slices = fallback_slices();
    }
    f
}

impl Facets {
    pub fn subgenres_of(&self, genre: &str) -> Vec<Facet> {
        self.subgenres.iter().filter(|s| s.parent_slug.as_deref() == Some(genre)).cloned().collect()
    }
    /// The geoname id of a chosen place (slug is the id string in the page blob; fall back to `id`).
    pub fn geoname_of(&self, place: &str) -> Option<i64> {
        if place.is_empty() || place == "0" {
            return None;
        }
        place.parse::<i64>().ok().or_else(|| self.locations.iter().find(|l| l.slug == place).map(|l| l.id).filter(|i| *i != 0))
    }
}

// ---------------------------------------------------------------------------
// The browse query: URL state, discover request, saved-query shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Browse {
    pub genre: String,
    pub slice: String,
    pub tag: String,
    pub place: String,
}

impl Browse {
    pub fn from_params(genre: Option<String>, slice: Option<String>, tag: Option<String>, place: Option<String>) -> Self {
        Self {
            genre: genre.filter(|g| !g.is_empty()).unwrap_or_else(|| "electronic".into()),
            slice: slice.filter(|g| !g.is_empty()).unwrap_or_else(|| "new".into()),
            tag: tag.unwrap_or_default(),
            place: place.filter(|g| !g.is_empty()).unwrap_or_else(|| "0".into()),
        }
    }

    /// Discover ANDs its filters, so keeping the genre next to an explicitly picked tag is
    /// how "black metal" under the default "electronic" returns nothing: a tag replaces the genre.
    pub fn discover_pairs(&self, facets: &Facets, cursor: &str, size: usize) -> Vec<(String, String)> {
        let mut p: Vec<(String, String)> = vec![];
        if self.tag.is_empty() {
            p.push(("genre".into(), self.genre.clone()));
        } else {
            p.push(("tags".into(), self.tag.clone()));
        }
        p.push(("slice".into(), self.slice.clone()));
        if let Some(g) = facets.geoname_of(&self.place) {
            p.push(("geoname_id".into(), g.to_string()));
        }
        p.push(("cursor".into(), cursor.to_string()));
        p.push(("size".into(), size.to_string()));
        p
    }

    /// This page's own URL state, verbatim, so recalling a saved query is just navigation.
    pub fn explore_params(&self, q: &str) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        if !q.is_empty() {
            m.insert("q".into(), Value::String(q.into()));
            return m;
        }
        m.insert("genre".into(), self.genre.clone().into());
        m.insert("slice".into(), self.slice.clone().into());
        if !self.tag.is_empty() {
            m.insert("tag".into(), self.tag.clone().into());
        }
        if self.place != "0" {
            m.insert("place".into(), self.place.clone().into());
        }
        m
    }

    /// The discover call a feed sweep replays.
    pub fn api_params(&self, q: &str, facets: &Facets) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        if !q.is_empty() {
            return m;
        }
        if self.tag.is_empty() {
            m.insert("genre".into(), self.genre.clone().into());
        } else {
            m.insert("tags".into(), Value::Array(vec![self.tag.clone().into()]));
        }
        m.insert("slice".into(), self.slice.clone().into());
        if let Some(g) = facets.geoname_of(&self.place) {
            m.insert("geoname_id".into(), g.into());
        }
        m
    }

    pub fn save_label(&self, q: &str, facets: &Facets) -> String {
        if !q.is_empty() {
            return format!("\u{201c}{q}\u{201d}");
        }
        let place = if self.place != "0" { facets.locations.iter().find(|l| l.slug == self.place).map(|l| l.label.clone()) } else { None };
        let what = if self.tag.is_empty() { &self.genre } else { &self.tag }.replace('-', " ");
        let slice = facets.slices.iter().find(|s| s.slug == self.slice).map(|s| s.label.clone()).unwrap_or_else(|| self.slice.clone());
        [Some(what), Some(slice), place].into_iter().flatten().collect::<Vec<_>>().join(" \u{b7} ")
    }
}

/// A canonical key for "is the current query already saved?" (sorted, scalar-stringified).
pub fn query_key(p: &BTreeMap<String, Value>) -> String {
    p.iter()
        .map(|(k, v)| format!("{k}={}", v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
        .collect::<Vec<_>>()
        .join("&")
}

/// `/explore?...` for a saved query's params.
pub fn saved_query_path(p: &BTreeMap<String, Value>) -> String {
    let s: Vec<String> = p
        .iter()
        .map(|(k, v)| format!("{}={}", pct_encode(k), pct_encode(&v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))))
        .collect();
    if s.is_empty() { "/explore".into() } else { format!("/explore?{}", s.join("&")) }
}

// ---------------------------------------------------------------------------
// Playable items
// ---------------------------------------------------------------------------

/// A stable synthetic id for a Bandcamp stream: negative so it never collides with the
/// library's autoincrementing ids, derived from Bandcamp's own track id when present so the
/// same track is the same id across sessions and pages.
pub fn stream_track_id(release_url: &str, t: &ExploreTrackOut, index: usize) -> i64 {
    if let Some(id) = t.bc_track_id.filter(|i| *i > 0) {
        return -id;
    }
    // FNV-1a over url + index, folded into 52 bits.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in release_url.bytes().chain((index as u64).to_le_bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    -(((h >> 12) as i64).max(1))
}

/// The playable rows of a release: the Bandcamp stream for every track that has one.
pub fn stream_items(r: &ExploreReleaseOut) -> Vec<QueueItem> {
    r.tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.stream_url.as_deref().is_some_and(|s| !s.is_empty()))
        .map(|(i, t)| stream_item(r, t, i))
        .collect()
}

pub fn stream_item(r: &ExploreReleaseOut, t: &ExploreTrackOut, index: usize) -> QueueItem {
    QueueItem {
        track_id: stream_track_id(&r.url, t, index),
        title: t.title.clone(),
        artist: Some(t.artist.clone().filter(|a| !a.is_empty()).unwrap_or_else(|| r.artist_name.clone())),
        album: Some(r.title.clone()),
        track_no: t.track_num.map(|n| n as i32),
        duration_ms: t.duration_sec.map(|s| (s * 1000.0).round() as i64),
        art_url: r.art_url.clone(),
        stream_url: t.stream_url.clone(),
        origin: ItemOrigin::Bandcamp,
        page_url: Some(r.url.clone()),
        ..Default::default()
    }
}

/// The price line of a release page.
pub fn price_label(r: &ExploreReleaseOut) -> String {
    if r.is_free_download {
        "Free / name your price".into()
    } else if r.price.is_some_and(|p| p > 0.0) {
        let p = r.price.unwrap_or(0.0);
        let p = if p.fract() == 0.0 { format!("{p:.0}") } else { format!("{p:.2}") };
        format!("{p} {}", r.currency.clone().unwrap_or_default()).trim().to_string()
    } else if r.is_purchasable {
        "Paid".into()
    } else {
        "Not for sale".into()
    }
}

/// Label of the "download the rest" button.
pub fn catalog_label(missing: usize, exact: bool) -> String {
    if exact && missing == 0 {
        "All in your library".into()
    } else if exact {
        format!("Download {missing} missing")
    } else if missing > 0 {
        format!("Download {missing}+ missing")
    } else {
        "Download missing".into()
    }
}

pub fn catalog_result(queued: i64, skipped_in_library: i64, detail: &str) -> String {
    if queued > 0 {
        let mut s = format!("Queued {queued} release(s)");
        if skipped_in_library > 0 {
            s.push_str(&format!(", skipped {skipped_in_library} already in library"));
        }
        if detail.is_empty() {
            s.push('.');
        } else {
            s.push_str(&format!(". {detail}"));
        }
        s
    } else if detail.is_empty() {
        "Nothing to queue.".into()
    } else {
        detail.to_string()
    }
}

/// Which local track corresponds to a Bandcamp track row: by track number when both sides
/// carry one, positionally when the two lists line up whole. `None` falls back to the stream.
pub fn local_match(track_num: Option<i64>, index: usize, local_nums: &[Option<i64>], remote_len: usize) -> Option<usize> {
    if local_nums.is_empty() {
        return None;
    }
    if let Some(n) = track_num {
        if let Some(i) = local_nums.iter().position(|l| *l == Some(n)) {
            return Some(i);
        }
    }
    (local_nums.len() == remote_len).then_some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: &str, name: &str, url: &str) -> SearchHitOut {
        SearchHitOut { kind: kind.into(), name: name.into(), url: url.into(), ..Default::default() }
    }

    #[test]
    fn groups_one_ranked_list_into_bands_albums_and_tracks() {
        let g = group_hits(&[
            hit("track", "Her", "https://c.bandcamp.com/track/her"),
            hit("album", "Volume One", "https://c.bandcamp.com/album/v1"),
            hit("artist", "Guy J", "https://guyj.bandcamp.com"),
            hit("label", "Cocoon", "https://c.bandcamp.com"),
        ]);
        assert_eq!(g.bands.iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["Guy J", "Cocoon"]);
        assert_eq!(g.albums.len(), 1);
        assert_eq!(g.tracks.len(), 1);
        assert_eq!(g.playable().iter().map(|h| h.name.as_str()).collect::<Vec<_>>(), ["Volume One", "Her"]);
    }

    #[test]
    fn keeps_the_first_of_two_hits_for_the_same_page() {
        let g = group_hits(&[hit("album", "Same", "https://c.bandcamp.com/album/same"), hit("track", "Same", "https://c.bandcamp.com/album/same")]);
        assert_eq!(g.albums.len(), 1);
        assert_eq!(g.tracks.len(), 0);
    }

    #[test]
    fn drops_fan_profiles() {
        let g = group_hits(&[hit("fan", "numajohn", "https://bandcamp.com/numajohn"), hit("artist", "Unca John", "https://uncajohn.bandcamp.com")]);
        assert_eq!(g.bands.len(), 1);
        assert_eq!(g.bands[0].name, "Unca John");
    }

    #[test]
    fn hit_to_card_carries_the_hit_and_leaves_unknowns_unclaimed() {
        let mut h = hit("album", "Volume One", "https://c.bandcamp.com/album/v1");
        h.subtitle = "Guy J".into();
        h.in_library = true;
        h.library_release_id = Some(7);
        let c = hit_to_card(&h);
        assert_eq!(c.title, "Volume One");
        assert_eq!(c.artist_name, "Guy J");
        assert!(!c.is_free_download);
        assert!(c.in_library);
        assert_eq!(c.library_release_id, Some(7));
    }

    #[test]
    fn explore_path_encodes() {
        assert_eq!(explore_path("black metal & more"), "/explore?q=black%20metal%20%26%20more");
        assert_eq!(release_path("https://a.bandcamp.com/album/x"), "/explore/release?url=https%3A%2F%2Fa.bandcamp.com%2Falbum%2Fx");
    }

    #[test]
    fn release_keys_ignore_scheme_slash_and_case() {
        assert_eq!(release_key("https://A.bandcamp.com/album/x/"), release_key("http://a.bandcamp.com/album/x"));
        assert_ne!(release_key("https://a.bandcamp.com/album/x"), release_key("https://a.bandcamp.com/album/y"));
    }

    #[test]
    fn origin_of_strips_the_path() {
        assert_eq!(origin_of("https://Foo.bandcamp.com/album/x?a=1").as_deref(), Some("https://foo.bandcamp.com"));
        assert_eq!(origin_of("nonsense"), None);
    }

    #[test]
    fn facets_parse_and_fall_back() {
        let v: Value = serde_json::json!({
            "genres": [{"id": 10, "label": "electronic", "slug": "electronic"}],
            "subgenres": [{"id": 1, "label": "house", "slug": "house", "parentSlug": "electronic"}, {"id": 2, "label": "emo", "slug": "emo", "parentSlug": "rock"}],
            "locations": [{"id": 5128581, "label": "New York", "slug": "5128581"}],
        });
        let f = parse_facets(Some(&v));
        assert_eq!(f.genres.len(), 1);
        assert_eq!(f.slices, fallback_slices());
        assert_eq!(f.subgenres_of("electronic").len(), 1);
        assert_eq!(f.geoname_of("5128581"), Some(5128581));
        assert_eq!(f.geoname_of("0"), None);
        let empty = parse_facets(None);
        assert_eq!(empty.genres, fallback_genres());
    }

    #[test]
    fn a_picked_tag_replaces_the_genre_in_the_discover_call() {
        let facets = parse_facets(None);
        let b = Browse::from_params(None, None, Some("minimal".into()), None);
        let p = b.discover_pairs(&facets, "*", 48);
        assert!(p.contains(&("tags".into(), "minimal".into())));
        assert!(!p.iter().any(|(k, _)| k == "genre"));
        let b = Browse::from_params(Some("jazz".into()), None, None, None);
        assert!(b.discover_pairs(&facets, "*", 48).contains(&("genre".into(), "jazz".into())));
    }

    #[test]
    fn saved_query_shapes_and_labels() {
        let facets = parse_facets(None);
        let b = Browse::from_params(Some("electronic".into()), Some("top".into()), Some("deep-house".into()), None);
        let ep = b.explore_params("");
        assert_eq!(ep.get("tag").and_then(|v| v.as_str()), Some("deep-house"));
        assert!(!ep.contains_key("place"));
        assert_eq!(b.api_params("", &facets).get("tags"), Some(&serde_json::json!(["deep-house"])));
        assert_eq!(b.save_label("", &facets), "deep house \u{b7} best-selling");
        assert_eq!(b.save_label("minimal", &facets), "\u{201c}minimal\u{201d}");
        assert_eq!(b.explore_params("minimal").len(), 1);
        assert!(b.api_params("minimal", &facets).is_empty());
        assert_eq!(saved_query_path(&b.explore_params("minimal")), "/explore?q=minimal");
        assert_eq!(query_key(&b.explore_params("")), "genre=electronic&slice=top&tag=deep-house");
    }

    fn track(title: &str, id: Option<i64>, url: Option<&str>) -> ExploreTrackOut {
        ExploreTrackOut { title: title.into(), bc_track_id: id, stream_url: url.map(str::to_string), ..Default::default() }
    }

    #[test]
    fn stream_ids_are_negative_and_stable() {
        let t = track("a", Some(42), Some("/s"));
        assert_eq!(stream_track_id("u", &t, 0), -42);
        let n = track("b", None, Some("/s"));
        let a = stream_track_id("https://x/y", &n, 3);
        assert!(a < 0);
        assert_eq!(a, stream_track_id("https://x/y", &n, 3));
        assert_ne!(a, stream_track_id("https://x/y", &n, 4));
    }

    #[test]
    fn only_streaming_tracks_are_playable() {
        let r = ExploreReleaseOut {
            url: "https://a.bandcamp.com/album/x".into(),
            title: "X".into(),
            artist_name: "A".into(),
            tracks: vec![track("one", Some(1), Some("/api/explore/stream?release=u&track=1")), track("two", Some(2), None), track("three", Some(3), Some(""))],
            ..Default::default()
        };
        let items = stream_items(&r);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "one");
        assert_eq!(items[0].origin, ItemOrigin::Bandcamp);
        assert_eq!(items[0].page_url.as_deref(), Some("https://a.bandcamp.com/album/x"));
        assert_eq!(items[0].artist.as_deref(), Some("A"));
    }

    #[test]
    fn price_lines() {
        let mut r = ExploreReleaseOut { is_purchasable: true, ..Default::default() };
        assert_eq!(price_label(&r), "Paid");
        r.price = Some(7.0);
        r.currency = Some("EUR".into());
        assert_eq!(price_label(&r), "7 EUR");
        r.price = Some(7.5);
        assert_eq!(price_label(&r), "7.50 EUR");
        r.is_free_download = true;
        assert_eq!(price_label(&r), "Free / name your price");
        let n = ExploreReleaseOut::default();
        assert_eq!(price_label(&n), "Not for sale");
    }

    #[test]
    fn catalogue_button_says_what_it_knows() {
        assert_eq!(catalog_label(0, true), "All in your library");
        assert_eq!(catalog_label(3, true), "Download 3 missing");
        assert_eq!(catalog_label(3, false), "Download 3+ missing");
        assert_eq!(catalog_label(0, false), "Download missing");
        assert_eq!(catalog_result(4, 2, ""), "Queued 4 release(s), skipped 2 already in library.");
        assert_eq!(catalog_result(0, 0, ""), "Nothing to queue.");
        assert_eq!(catalog_result(0, 0, "Cookie needed"), "Cookie needed");
    }

    #[test]
    fn local_tracks_join_by_number_then_by_position() {
        let nums = [Some(1), Some(2), Some(3)];
        assert_eq!(local_match(Some(2), 0, &nums, 3), Some(1));
        assert_eq!(local_match(None, 2, &nums, 3), Some(2));
        // a bonus track with no counterpart falls back to the stream
        assert_eq!(local_match(Some(9), 3, &nums, 4), None);
        assert_eq!(local_match(Some(1), 0, &[], 3), None);
    }

    #[test]
    fn cards_dedupe_and_sweeps_are_capped() {
        let c = |u: &str| ReleaseCardOut { url: u.into(), ..Default::default() };
        assert_eq!(dedupe_cards(vec![c("a"), c("b"), c("a")]).len(), 2);
        let urls: Vec<String> = (0..150).map(|i| format!("u{i}")).collect();
        assert_eq!(sweep_cards(urls.iter().map(|u| (u.as_str(), None))).len(), MAX_SWEEP);
    }
}
