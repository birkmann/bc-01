//! Minimal Bandcamp URL canonicalisation (port of `harvest.urls.normalise`): lowercase host,
//! tracking query stripped, no trailing slash.

const STRIP_QUERY_KEYS: &[&str] = &[
    "action", "from", "label", "tab", "sig", "ref", "search_item_id", "search_page_id", "utm_source", "utm_medium", "utm_campaign", "utm_term",
    "utm_content", "fbclid", "gclid",
];

pub fn normalise(raw: &str) -> String {
    let raw = raw.trim();
    let (scheme, rest) = match raw.split_once("://") {
        Some((s, r)) => (if s.is_empty() { "https".to_string() } else { s.to_lowercase() }, r),
        None => ("https".to_string(), raw.trim_start_matches("//")),
    };
    let rest = rest.split('#').next().unwrap_or("");
    let (before_q, query) = match rest.split_once('?') {
        Some((a, q)) => (a, q),
        None => (rest, ""),
    };
    let (host, path) = match before_q.split_once('/') {
        Some((h, p)) => (h, format!("/{p}")),
        None => (before_q, String::new()),
    };
    let host = host.split(':').next().unwrap_or("").to_lowercase();
    let path = path.trim_end_matches('/').to_string();
    let mut kept: Vec<(String, String)> = query
        .split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            if k.is_empty() || v.is_empty() || STRIP_QUERY_KEYS.contains(&k.to_lowercase().as_str()) {
                None
            } else {
                Some((k.to_string(), v.to_string()))
            }
        })
        .collect();
    kept.sort();
    kept.dedup_by(|a, b| a.0 == b.0);
    let base = format!("{scheme}://{host}{path}");
    if kept.is_empty() {
        base
    } else {
        format!("{base}?{}", kept.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical() {
        assert_eq!(normalise(" HTTPS://Artist.Bandcamp.com/album/x/?utm_source=a&from=b#frag "), "https://artist.bandcamp.com/album/x");
        assert_eq!(normalise("artist.bandcamp.com/music"), "https://artist.bandcamp.com/music");
        assert_eq!(normalise("https://a.bandcamp.com/album/x?z=1&b=2"), "https://a.bandcamp.com/album/x?b=2&z=1");
    }
}
