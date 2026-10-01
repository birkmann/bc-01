//! Bandcamp URL parsing, normalisation and naming (port of `harvest/urls.py`, the parts the
//! library layer needs). WS2 owns the full client-side copy; this one is deliberately small and
//! dependency free so the repair passes and routes of this crate do not wait on it.

use std::collections::BTreeMap;

/// Query keys Bandcamp links accumulate that never change what is addressed.
const STRIP_QUERY_KEYS: &[&str] = &[
    "action", "from", "label", "tab", "sig", "ref", "search_item_id", "search_page_id", "utm_source", "utm_medium",
    "utm_campaign", "utm_content", "utm_term",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

struct Parts<'a> {
    scheme: String,
    netloc: &'a str,
    path: &'a str,
    query: &'a str,
}

fn parse(raw: &str) -> Parts<'_> {
    let s = raw.trim();
    let s = s.split('#').next().unwrap_or("");
    let (scheme, rest) = match s.find("://") {
        Some(i) if s[..i].chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) => (s[..i].to_ascii_lowercase(), &s[i + 3..]),
        _ => (String::new(), s),
    };
    if scheme.is_empty() {
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        return Parts { scheme, netloc: "", path, query };
    }
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let netloc = &rest[..end];
    let rest = &rest[end..];
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    Parts { scheme, netloc, path, query }
}

fn host_of(netloc: &str) -> String {
    netloc.split(':').next().unwrap_or("").to_ascii_lowercase()
}

/// `(subdomain, is_bandcamp)` of a lowercase host.
fn bandcamp_host(host: &str) -> (Option<String>, bool) {
    if host == "bandcamp.com" {
        return (None, true);
    }
    match host.strip_suffix(".bandcamp.com") {
        Some(sub) if !sub.is_empty() && !sub.contains('.') => (Some(sub.to_string()), true),
        _ => (None, false),
    }
}

fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        (c as char).to_digit(16).map(|d| d as u8)
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `parse_qs` (blank values dropped, first value wins on lookup, keys in sorted order).
fn parse_qs(q: &str) -> BTreeMap<String, Vec<String>> {
    let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pair in q.split('&') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        if v.is_empty() {
            continue;
        }
        m.entry(percent_decode(k)).or_default().push(percent_decode(v));
    }
    m
}

/// What does this URL address?
pub fn classify(raw: &str) -> UrlKind {
    let p = parse(raw);
    if (p.scheme != "http" && p.scheme != "https") || p.netloc.is_empty() {
        return UrlKind::Unknown;
    }
    let host = host_of(p.netloc);
    let trimmed = p.path.trim_end_matches('/');
    let path = if trimmed.is_empty() { "/" } else { trimmed };
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let (sub, is_bc) = bandcamp_host(&host);
    let with_slash = format!("{path}/");
    if with_slash.contains("/album/") {
        return UrlKind::Album;
    }
    if with_slash.contains("/track/") {
        return UrlKind::Track;
    }
    if is_bc && matches!(sub.as_deref(), None | Some("www")) {
        let Some(first) = segments.first() else { return UrlKind::Unknown };
        let first = first.to_ascii_lowercase();
        if first == "discover" {
            return UrlKind::Discover;
        }
        const RESERVED: &[&str] = &[
            "discover", "search", "signup", "login", "help", "about", "artists", "tag", "api", "download", "settings", "feed", "campaign",
            "privacy", "terms",
        ];
        if RESERVED.contains(&first.as_str()) {
            return UrlKind::Unknown;
        }
        return UrlKind::Fan;
    }
    match segments.first().map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("music") => UrlKind::Music,
        Some("artists") => UrlKind::Artists,
        None => UrlKind::Artist,
        _ => UrlKind::Unknown,
    }
}

/// `/track/` URL (kind used for queue items and strays).
pub fn is_track(raw: &str) -> bool {
    classify(raw) == UrlKind::Track
}

/// Canonical form: lowercase host, no tracking query, no trailing slash.
pub fn normalise(raw: &str) -> String {
    let p = parse(raw);
    let host = host_of(p.netloc);
    let path = p.path.trim_end_matches('/');
    let kept: Vec<String> = parse_qs(p.query)
        .into_iter()
        .filter(|(k, _)| !STRIP_QUERY_KEYS.contains(&k.to_ascii_lowercase().as_str()))
        .map(|(k, v)| format!("{k}={}", v[0]))
        .collect();
    let scheme = if p.scheme.is_empty() { "https" } else { p.scheme.as_str() };
    let base = format!("{scheme}://{host}{path}");
    if kept.is_empty() { base } else { format!("{base}?{}", kept.join("&")) }
}

/// Canonical dedupe key: harvest-normalised, then whole-string lowercase.
pub fn url_key(raw: &str) -> String {
    normalise(raw).to_lowercase()
}

/// A filesystem-friendly name for a source (`fan-<user>`, the artist subdomain, ...).
pub fn display_name(raw: &str) -> String {
    let p = parse(raw);
    let host = host_of(p.netloc);
    let segments: Vec<&str> = p.path.split('/').filter(|s| !s.is_empty()).collect();
    let (sub, is_bc) = bandcamp_host(&host);
    let sub = sub.filter(|s| s != "www");
    let mut parts: Vec<String> = Vec::new();
    if segments.first().map(|s| s.eq_ignore_ascii_case("discover")).unwrap_or(false) {
        parts.push("discover".into());
        parts.extend(segments.iter().skip(1).take(2).map(|s| s.to_string()));
    } else if let Some(sub) = sub {
        parts.push(sub);
    } else if is_bc && !segments.is_empty() {
        parts.push(format!("fan-{}", segments[0]));
    } else if !host.is_empty() {
        parts.push(host.replace('.', "-"));
    }
    if let Some(tags) = parse_qs(p.query).get("tags").and_then(|v| v.first().cloned()) {
        parts.push(tags.split_whitespace().collect::<Vec<_>>().join("-"));
    }
    let joined = parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    let name = if joined.is_empty() { "bandcamp".to_string() } else { joined };
    let mut out = String::new();
    let mut prev_dash = false;
    for c in name.chars() {
        if c == '-' {
            if !prev_dash {
                out.push(c);
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

/// The folder a fan's downloads go under (`shelf_name` in the legacy fans service).
pub fn shelf_name(fan_url: &str, username: &str) -> String {
    let d = display_name(fan_url);
    if d.is_empty() { format!("fan-{username}") } else { d }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_kinds() {
        assert_eq!(classify("https://a.bandcamp.com/album/x"), UrlKind::Album);
        assert_eq!(classify("https://a.bandcamp.com/track/x/"), UrlKind::Track);
        assert_eq!(classify("https://evil.example.com/x"), UrlKind::Unknown);
        assert_eq!(classify("https://bandcamp.com/yassinepeixoto"), UrlKind::Fan);
        assert_eq!(classify("https://bandcamp.com/discover/techno"), UrlKind::Discover);
        assert_eq!(classify("https://a.bandcamp.com"), UrlKind::Artist);
        assert_eq!(classify("a.bandcamp.com/album/x"), UrlKind::Unknown);
    }

    #[test]
    fn normalise_strips_tracking_and_case() {
        assert_eq!(normalise("HTTPS://A.Bandcamp.com/album/X/?from=embed&b=2&a=1"), "https://a.bandcamp.com/album/X?a=1&b=2");
        assert_eq!(url_key("https://A.bandcamp.com/album/X"), "https://a.bandcamp.com/album/x");
    }

    #[test]
    fn shelf_names() {
        assert_eq!(shelf_name("https://bandcamp.com/yassinepeixoto", "yassinepeixoto"), "fan-yassinepeixoto");
        assert_eq!(display_name("https://soma.bandcamp.com/album/x"), "soma");
        assert_eq!(display_name("https://music.example.com/x"), "music-example-com");
    }
}
