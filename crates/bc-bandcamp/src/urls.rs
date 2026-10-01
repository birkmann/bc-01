//! Bandcamp URL parsing, normalisation, and naming.
//!
//! Port of `harvest/urls.py`. Python's `urlparse` is emulated by a tiny
//! splitter ([`parse`]) rather than `url::Url`, because the legacy behaviour
//! on malformed input is pinned by the tests: an input without a scheme is a
//! *path with no host*, a host is whatever precedes the first `:` of the
//! netloc, and query values are percent-decoded (`+` -> space) and re-joined
//! unencoded in `normalise`.

use std::sync::LazyLock;

use percent_encoding::percent_decode_str;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

/// What a Bandcamp URL addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UrlKind {
    Album,
    Track,
    Artist,
    Music,
    Artists,
    Fan,
    Discover,
    Unknown,
}

impl UrlKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Track => "track",
            Self::Artist => "artist",
            Self::Music => "music",
            Self::Artists => "artists",
            Self::Fan => "fan",
            Self::Discover => "discover",
            Self::Unknown => "unknown",
        }
    }
}

/// Query keys Bandcamp links accumulate that never change what is addressed.
/// The gen-1 bookmarklet scraped `?action=buy` links, discover results carry
/// `?from=`, label pages add `?label=`/`?tab=`. Keeping any of them would defeat
/// deduplication -- the same release would enter the inbox several times.
pub const STRIP_QUERY_KEYS: &[&str] = &[
    "action",
    "from",
    "label",
    "tab",
    "sig",
    "ref",
    "search_item_id",
    "search_page_id",
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_content",
    "utm_term",
];

/// Reserved bandcamp.com paths that are not fan usernames.
const RESERVED: &[&str] = &[
    "discover", "search", "signup", "login", "help", "about", "artists", "tag", "api", "download",
    "settings", "feed", "campaign", "privacy", "terms",
];

// ---------------------------------------------------------------------------
// urlparse / parse_qs emulation
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Parsed {
    scheme: String,
    netloc: String,
    path: String,
    query: String,
}

/// Emulates Python's `urlparse` (fragment and params are discarded).
fn parse(raw: &str) -> Parsed {
    let cleaned: String = raw.chars().filter(|c| !matches!(c, '\t' | '\r' | '\n')).collect();
    let mut rest: &str = cleaned.trim_matches(|c: char| c <= ' ');
    let mut out = Parsed::default();

    if let Some(i) = rest.find(':') {
        let cand = &rest[..i];
        let mut chars = cand.chars();
        let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if valid {
            out.scheme = cand.to_ascii_lowercase();
            rest = &rest[i + 1..];
        }
    }
    if rest.starts_with("//") {
        let after = &rest[2..];
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        out.netloc = after[..end].to_string();
        rest = &after[end..];
    }
    if let Some(i) = rest.find('#') {
        rest = &rest[..i];
    }
    if let Some(i) = rest.find('?') {
        out.query = rest[i + 1..].to_string();
        rest = &rest[..i];
    }
    out.path = rest.to_string();
    out
}

impl Parsed {
    /// `netloc.split(":")[0].lower()`
    fn host(&self) -> String {
        self.netloc.split(':').next().unwrap_or("").to_lowercase()
    }
    fn segments(&self) -> Vec<&str> {
        self.path.split('/').filter(|s| !s.is_empty()).collect()
    }
    fn scheme_or_https(&self) -> &str {
        if self.scheme.is_empty() { "https" } else { &self.scheme }
    }
}

fn unquote_plus(s: &str) -> String {
    let s = s.replace('+', " ");
    percent_decode_str(&s).decode_utf8_lossy().into_owned()
}

/// `urllib.parse.parse_qs` (blank values dropped), insertion-ordered by key.
fn parse_qs(query: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let Some((k, v)) = pair.split_once('=') else {
            continue; // no '=' => blank value => dropped
        };
        if v.is_empty() {
            continue;
        }
        let (k, v) = (unquote_plus(k), unquote_plus(v));
        match out.iter_mut().find(|(ek, _)| *ek == k) {
            Some((_, vs)) => vs.push(v),
            None => out.push((k, vec![v])),
        }
    }
    out
}

fn qs_get<'a>(qs: &'a [(String, Vec<String>)], key: &str) -> Option<&'a Vec<String>> {
    qs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Match `^(?:(?P<sub>[^.]+)\.)?bandcamp\.com$` on a lowercase host.
/// `Some(None)` = bare `bandcamp.com`, `Some(Some(sub))` = a subdomain.
fn match_bandcamp(host: &str) -> Option<Option<String>> {
    let host = host.to_lowercase();
    if host == "bandcamp.com" {
        return Some(None);
    }
    let sub = host.strip_suffix(".bandcamp.com")?;
    if sub.is_empty() || sub.contains('.') {
        return None;
    }
    Some(Some(sub.to_string()))
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// What does this URL address?
pub fn classify(raw: &str) -> UrlKind {
    let parsed = parse(raw);
    if (parsed.scheme != "http" && parsed.scheme != "https") || parsed.netloc.is_empty() {
        return UrlKind::Unknown;
    }
    let host = parsed.host();
    let path = {
        let p = if parsed.path.is_empty() { "/" } else { parsed.path.as_str() };
        let t = p.trim_end_matches('/');
        if t.is_empty() { "/".to_string() } else { t.to_string() }
    };
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let m = match_bandcamp(&host);
    let is_bandcamp = m.is_some();
    let subdomain = m.flatten();

    // /album/ and /track/ are unambiguous on any host, including custom domains.
    let padded = format!("{path}/");
    if padded.contains("/album/") {
        return UrlKind::Album;
    }
    if padded.contains("/track/") {
        return UrlKind::Track;
    }

    if is_bandcamp && matches!(subdomain.as_deref(), None | Some("www")) {
        let Some(first) = segments.first() else {
            return UrlKind::Unknown;
        };
        let first = first.to_lowercase();
        if first == "discover" {
            return UrlKind::Discover;
        }
        if RESERVED.contains(&first.as_str()) {
            return UrlKind::Unknown;
        }
        // bandcamp.com/<username> is a fan page.
        return UrlKind::Fan;
    }

    // An artist/label subdomain or a custom domain.
    match segments.first().map(|s| s.to_lowercase()).as_deref() {
        Some("music") => UrlKind::Music,
        Some("artists") => UrlKind::Artists,
        None => UrlKind::Artist,
        _ => UrlKind::Unknown,
    }
}

/// Which tab a fan URL addresses: `collection`, `wishlist`, or `hidden`.
///
/// Bandcamp puts the tab in the path (`/you/wishlist`) or the query
/// (`?tab=wishlist`), so pasting the link you are actually looking at picks
/// the right list without asking.
pub fn fan_tab(raw: &str) -> &'static str {
    let parsed = parse(raw);
    let segments: Vec<String> = parsed.segments().iter().map(|s| s.to_lowercase()).collect();
    let qs = parse_qs(&parsed.query);
    let tab = qs_get(&qs, "tab").and_then(|v| v.first()).map(|s| s.to_lowercase()).unwrap_or_default();

    for candidate in segments.iter().skip(1).chain(std::iter::once(&tab)) {
        match candidate.as_str() {
            "wishlist" => return "wishlist",
            "hidden" => return "hidden",
            _ => {}
        }
    }
    "collection"
}

/// The fan page itself, with any tab suffix removed.
///
/// Only `/<user>` and `/<user>/wishlist` exist as real pages -- a guessed
/// `/<user>/collection` 404s. Fetching the base page is also strictly better:
/// its pagedata blob carries collection, wishlist *and* hidden data together,
/// so one request serves every tab.
pub fn fan_base_url(raw: &str) -> String {
    let parsed = parse(raw);
    let user = parsed.segments().first().copied().unwrap_or("");
    let s = format!("{}://{}/{}", parsed.scheme_or_https(), parsed.host(), user);
    s.trim_end_matches('/').to_string()
}

/// Canonical form: lowercase host, no tracking query, no trailing slash.
/// Remaining query keys are sorted; only the first value of each is kept.
pub fn normalise(raw: &str) -> String {
    let parsed = parse(raw);
    let host = parsed.host();
    let path = parsed.path.trim_end_matches('/');

    let mut kept: Vec<(String, String)> = parse_qs(&parsed.query)
        .into_iter()
        .filter(|(k, _)| !STRIP_QUERY_KEYS.contains(&k.to_lowercase().as_str()))
        .filter_map(|(k, v)| v.into_iter().next().map(|first| (k, first)))
        .collect();
    kept.sort_by(|a, b| a.0.cmp(&b.0));
    let query = kept.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");

    let base = format!("{}://{}{}", parsed.scheme_or_https(), host, path);
    if query.is_empty() { base } else { format!("{base}?{query}") }
}

/// The artist/label root for any of its pages.
pub fn artist_root(raw: &str) -> String {
    let parsed = parse(raw);
    format!("{}://{}", parsed.scheme_or_https(), parsed.host())
}

/// The `<sub>` of `<sub>.bandcamp.com` (never `www`), if any.
pub fn subdomain_of(raw: &str) -> Option<String> {
    let host = parse(raw).host();
    match match_bandcamp(&host).flatten() {
        Some(sub) if sub != "www" => Some(sub),
        _ => None,
    }
}

static WS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("static regex"));
static DASHES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"-{2,}").expect("static regex"));
static NON_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-z0-9]+").expect("static regex"));

/// A filesystem-friendly name for a source.
///
/// Replaces the gen-1 `extractPrefixFromURL`. That version used
/// `^(.*?)\.bandcamp\.com$`, which yields nothing for a custom domain --
/// those fall back to the host with dots replaced.
pub fn display_name(raw: &str) -> String {
    let parsed = parse(raw);
    let host = parsed.host();
    let segments = parsed.segments();
    let mut parts: Vec<String> = Vec::new();

    let sub = subdomain_of(raw);
    let is_bandcamp = match_bandcamp(&host).is_some();

    if segments.first().is_some_and(|s| s.eq_ignore_ascii_case("discover")) {
        parts.push("discover".into());
        parts.extend(segments.iter().skip(1).take(2).map(|s| s.to_string()));
    } else if let Some(sub) = sub {
        parts.push(sub);
    } else if is_bandcamp && !segments.is_empty() {
        parts.push(format!("fan-{}", segments[0]));
    } else if !host.is_empty() {
        parts.push(host.replace('.', "-"));
    }

    let qs = parse_qs(&parsed.query);
    if let Some(tags) = qs_get(&qs, "tags")
        && let Some(first) = tags.first() {
            parts.push(WS.replace_all(first, "-").into_owned());
        }

    let joined = parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    let name = if joined.is_empty() { "bandcamp".to_string() } else { joined };
    DASHES.replace_all(&name, "-").trim_matches('-').to_string()
}

/// Give a pasted address its scheme back.
///
/// People paste what the address bar shows -- `player.bandcamp.com`,
/// `bandcamp.com/you/wishlist` -- and every parser here reads an input
/// without `https://` as a path with no host at all, classifying it
/// `unknown`. Only text whose first segment looks like a hostname is
/// touched; anything else passes through untouched.
pub fn coerce(raw: &str) -> String {
    let cleaned = raw.trim();
    if cleaned.is_empty() || cleaned.contains("://") {
        return cleaned.to_string();
    }
    let head = cleaned.split('/').next().unwrap_or("");
    if head.contains('.') && !head.contains(' ') {
        return format!("https://{cleaned}");
    }
    cleaned.to_string()
}

/// The tag filters a discover URL carries, in API-normalised form.
///
/// Bandcamp puts the genre in the path (`/discover/techno`, optionally a
/// subgenre after it) and extra filters in `?tags=`. Without reading them, a
/// pasted `bandcamp.com/discover/techno` would harvest the generic feed --
/// every genre -- which is never what pasting that link meant.
pub fn discover_tags(raw: &str) -> Vec<String> {
    let parsed = parse(raw);
    let segments = parsed.segments();
    let mut found: Vec<String> = Vec::new();
    if segments.first().is_some_and(|s| s.eq_ignore_ascii_case("discover")) {
        found.extend(segments.iter().skip(1).take(2).map(|s| s.to_string()));
    }
    let qs = parse_qs(&parsed.query);
    for value in qs_get(&qs, "tags").into_iter().flatten() {
        found.extend(value.split(',').map(str::to_string));
    }

    let mut tags: Vec<String> = Vec::new();
    for tag in found.iter().map(|t| norm_tag(t)) {
        if !tag.is_empty() && !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

/// A tag in the form the discover API matches on.
///
/// Discover filters on Bandcamp's *normalised* tag names, and silently returns
/// zero results for anything else: `dub techno` reported 0 releases live
/// while `dub-techno` reported 27,684. Since release pages present tags in
/// display form ("Dub Techno"), clicking one is otherwise guaranteed to land
/// on an empty feed.
///
/// Ampersands are not special-cased. `drum & bass` normalises to
/// `drum-bass` (103k releases) and `R&B` to `r-b` (75k); Bandcamp also
/// holds `drum-n-bass` and `rnb` as separate user tags, so there is no
/// single correct expansion to pick -- the mechanical form is the largest and
/// the most predictable.
pub fn norm_tag(raw: &str) -> String {
    NON_TAG.replace_all(&raw.trim().to_lowercase(), "-").trim_matches('-').to_string()
}

/// Default cover-art size format (700px).
pub const DEFAULT_ART_SIZE: u32 = 16;

/// Cover art URL from an `art_id` at the default size (16 = 700px).
pub fn build_art_url(art_id: Option<i64>) -> Option<String> {
    build_art_url_sized(art_id, DEFAULT_ART_SIZE)
}

/// Cover art URL from an `art_id`.
///
/// Format numbers are community knowledge rather than documented: 0 original,
/// 10 = 1200px, 16 = 700px, 7 = 150px. Callers should treat this as an
/// optimisation and fall back to a scraped `<img src>` if it 404s.
pub fn build_art_url_sized(art_id: Option<i64>, size: u32) -> Option<String> {
    match art_id {
        None | Some(0) => None,
        Some(id) => Some(format!("https://f4.bcbits.com/img/a{id}_{size}.jpg")),
    }
}

/// A fan's avatar at the grid size (50): the same image host as covers.
pub fn build_fan_image_url(image_id: Option<i64>) -> Option<String> {
    build_fan_image_url_sized(image_id, 50)
}

/// A fan's avatar: the same image host as covers, `_50` for the grid.
pub fn build_fan_image_url_sized(image_id: Option<i64>, size: u32) -> Option<String> {
    image_id.map(|id| format!("https://f4.bcbits.com/img/{id:010}_{size}.jpg"))
}

/// Band or label photo URL at the default size (16).
pub fn build_band_image_url(img_id: Option<i64>) -> Option<String> {
    build_band_image_url_sized(img_id, DEFAULT_ART_SIZE)
}

/// Band or label photo URL from an `img_id`.
///
/// Same host and format numbers as release art, but band images live under a
/// bare id -- the `a` prefix that release art needs 404s for them.
pub fn build_band_image_url_sized(img_id: Option<i64>, size: u32) -> Option<String> {
    match img_id {
        None | Some(0) => None,
        Some(id) => Some(format!("https://f4.bcbits.com/img/{id}_{size}.jpg")),
    }
}

fn truthy(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null | Value::Bool(false) => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        Value::Number(n) if n.as_f64() == Some(0.0) => None,
        other => Some(other.to_string()),
    }
}

/// Rebuild a release URL from a collection item's `url_hints` (a JSON object).
///
/// Collection rows sometimes carry a null `item_url` while still having
/// hints, so this is the fallback path rather than dead code.
pub fn item_key_to_url(url_hints: &Value, item_type: &str) -> Option<String> {
    let subdomain = truthy(url_hints.get("subdomain"));
    let custom = truthy(url_hints.get("custom_domain"));
    let slug = truthy(url_hints.get("slug"))?;

    let host = match (custom, subdomain) {
        (Some(c), _) => c,
        (None, Some(s)) => format!("{s}.bandcamp.com"),
        (None, None) => return None,
    };
    let kind = if matches!(item_type, "t" | "track") { "track" } else { "album" };
    Some(format!("https://{host}/{kind}/{slug}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_classify() {
        let cases = [
            ("https://artist.bandcamp.com/album/thing", "album"),
            ("https://artist.bandcamp.com/track/thing", "track"),
            ("https://artist.bandcamp.com", "artist"),
            ("https://artist.bandcamp.com/", "artist"),
            ("https://artist.bandcamp.com/music", "music"),
            ("https://label.bandcamp.com/artists", "artists"),
            ("https://bandcamp.com/someuser", "fan"),
            ("https://bandcamp.com/discover/techno", "discover"),
            // Custom domains are real: path shape decides, not the host.
            ("https://music.example.com/album/thing", "album"),
            ("https://bandcamp.com/login", "unknown"),
            ("https://bandcamp.com/search", "unknown"),
            ("not a url", "unknown"),
            ("ftp://artist.bandcamp.com/album/x", "unknown"),
            ("", "unknown"),
        ];
        for (url, expected) in cases {
            assert_eq!(classify(url).as_str(), expected, "{url}");
        }
    }

    #[test]
    fn test_normalise_strips_tracking_noise() {
        let cases = [
            ("https://a.bandcamp.com/album/x?action=buy", "https://a.bandcamp.com/album/x"),
            ("https://a.bandcamp.com/album/x?from=discover_page", "https://a.bandcamp.com/album/x"),
            ("https://a.bandcamp.com/album/x?label=123&tab=music", "https://a.bandcamp.com/album/x"),
            ("https://A.Bandcamp.COM/album/x/", "https://a.bandcamp.com/album/x"),
            ("https://a.bandcamp.com/album/x?utm_source=newsletter", "https://a.bandcamp.com/album/x"),
        ];
        for (raw, expected) in cases {
            assert_eq!(normalise(raw), expected, "{raw}");
        }
    }

    #[test]
    fn test_normalise_is_idempotent_and_dedupes() {
        let variants = [
            "https://a.bandcamp.com/album/x",
            "https://a.bandcamp.com/album/x/",
            "https://a.bandcamp.com/album/x?action=buy",
            "https://A.bandcamp.com/album/x?from=discover_page&tab=music",
        ];
        let set: std::collections::HashSet<String> = variants.iter().map(|v| normalise(v)).collect();
        assert_eq!(set.len(), 1);
        let once = set.into_iter().next().unwrap();
        assert_eq!(normalise(&once), once);
    }

    #[test]
    fn test_normalise_keeps_meaningful_query_params() {
        assert!(normalise("https://bandcamp.com/discover?tags=techno").contains("tags=techno"));
    }

    #[test]
    fn normalise_sorts_keys_and_keeps_first_value() {
        assert_eq!(
            normalise("https://a.bandcamp.com/x?b=2&a=1&b=3&c="),
            "https://a.bandcamp.com/x?a=1&b=2"
        );
        // no scheme: the whole input is a path with no host (legacy quirk)
        assert_eq!(normalise("bandcamp.com/x"), "https://bandcamp.com/x");
    }

    #[test]
    fn test_artist_root_and_subdomain() {
        assert_eq!(artist_root("https://a.bandcamp.com/album/x"), "https://a.bandcamp.com");
        assert_eq!(subdomain_of("https://hyperdub.bandcamp.com/music").as_deref(), Some("hyperdub"));
        assert_eq!(subdomain_of("https://bandcamp.com/user"), None);
        assert_eq!(subdomain_of("https://www.bandcamp.com/user"), None);
        assert_eq!(subdomain_of("https://music.example.com/album/x"), None);
    }

    #[test]
    fn test_display_name() {
        let cases = [
            ("https://hyperdub.bandcamp.com/music", "hyperdub"),
            ("https://bandcamp.com/discover/techno", "discover-techno"),
            ("https://bandcamp.com/someuser", "fan-someuser"),
            // The gen-1 regex returned nothing for a custom domain.
            ("https://music.example.com/album/x", "music-example-com"),
            ("https://bandcamp.com/discover?tags=dub techno", "discover-dub-techno"),
        ];
        for (url, expected) in cases {
            assert_eq!(display_name(url), expected, "{url}");
        }
    }

    #[test]
    fn test_build_art_url() {
        assert_eq!(
            build_art_url(Some(2906676871)).as_deref(),
            Some("https://f4.bcbits.com/img/a2906676871_16.jpg")
        );
        assert_eq!(build_art_url(None), None);
        assert_eq!(build_art_url(Some(0)), None);
        assert_eq!(
            build_fan_image_url(Some(12345)).as_deref(),
            Some("https://f4.bcbits.com/img/0000012345_50.jpg")
        );
        assert_eq!(build_fan_image_url(None), None);
        assert_eq!(
            build_band_image_url(Some(777)).as_deref(),
            Some("https://f4.bcbits.com/img/777_16.jpg")
        );
        assert_eq!(build_band_image_url(Some(0)), None);
    }

    #[test]
    fn test_item_key_to_url_rebuilds_from_hints() {
        assert_eq!(
            item_key_to_url(&json!({"subdomain": "somatic", "slug": "grid-failure"}), "a").as_deref(),
            Some("https://somatic.bandcamp.com/album/grid-failure")
        );
        assert_eq!(
            item_key_to_url(&json!({"custom_domain": "music.example.com", "slug": "x"}), "t").as_deref(),
            Some("https://music.example.com/track/x")
        );
        assert_eq!(item_key_to_url(&json!({"subdomain": "a"}), "a"), None);
        assert_eq!(item_key_to_url(&json!({"slug": "x"}), "a"), None);
    }

    #[test]
    fn test_fan_tab() {
        let cases = [
            ("https://bandcamp.com/someone", "collection"),
            ("https://bandcamp.com/someone/wishlist", "wishlist"),
            ("https://bandcamp.com/someone?tab=wishlist", "wishlist"),
            ("https://bandcamp.com/someone/hidden", "hidden"),
            // A username that happens to look like a tab must not confuse it.
            ("https://bandcamp.com/wishlist", "collection"),
        ];
        for (url, expected) in cases {
            assert_eq!(fan_tab(url), expected, "{url}");
        }
    }

    #[test]
    fn test_fan_base_url_strips_the_tab() {
        for url in [
            "https://bandcamp.com/someone",
            "https://bandcamp.com/someone/wishlist",
            "https://bandcamp.com/someone/hidden",
            "https://bandcamp.com/someone?tab=wishlist",
        ] {
            assert_eq!(fan_base_url(url), "https://bandcamp.com/someone", "{url}");
        }
        assert_eq!(fan_base_url("https://bandcamp.com/"), "https://bandcamp.com");
    }

    #[test]
    fn test_discover_tags() {
        let cases: [(&str, &[&str]); 6] = [
            ("https://bandcamp.com/discover/techno", &["techno"]),
            ("https://bandcamp.com/discover/electronic/dub-techno", &["electronic", "dub-techno"]),
            ("https://bandcamp.com/discover?tags=dub techno", &["dub-techno"]),
            ("https://bandcamp.com/discover/techno?tags=hardgroove,acid", &["techno", "hardgroove", "acid"]),
            ("https://bandcamp.com/discover", &[]),
            // Duplicates fold once normalised.
            ("https://bandcamp.com/discover/techno?tags=Techno", &["techno"]),
        ];
        for (url, expected) in cases {
            assert_eq!(discover_tags(url), expected, "{url}");
        }
    }

    #[test]
    fn test_coerce_restores_the_scheme() {
        let cases = [
            ("player.bandcamp.com", "https://player.bandcamp.com"),
            ("bandcamp.com/you/wishlist", "https://bandcamp.com/you/wishlist"),
            ("  bandcamp.com/discover/techno  ", "https://bandcamp.com/discover/techno"),
            ("https://player.bandcamp.com", "https://player.bandcamp.com"),
            // Not addresses: pass through untouched.
            ("not a url", "not a url"),
            ("", ""),
        ];
        for (raw, expected) in cases {
            assert_eq!(coerce(raw), expected, "{raw:?}");
        }
    }

    #[test]
    fn test_coerced_paste_classifies() {
        assert_eq!(classify(&coerce("player.bandcamp.com")), UrlKind::Artist);
        assert_eq!(classify(&coerce("bandcamp.com/discover/techno")), UrlKind::Discover);
    }

    #[test]
    fn norm_tag_is_mechanical() {
        assert_eq!(norm_tag("Dub Techno"), "dub-techno");
        assert_eq!(norm_tag("drum & bass"), "drum-bass");
        assert_eq!(norm_tag("R&B"), "r-b");
        assert_eq!(norm_tag("  --x--  "), "x");
    }
}
