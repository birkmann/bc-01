//! Replaying the extractors over the persistent page cache, offline (`bc bandcamp replay`).
//!
//! Because pages are cached zstd-compressed in `cache.db`, an extractor bug fix can be checked over
//! everything that was ever fetched without re-crawling (re-crawling is what gets an IP blocked).
//! The report is also the degradation telemetry: how many pages of each kind parse at which tier,
//! and how many album pages lost the `"for the curious"` canary.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::Result;
use crate::extract::{self, Tier};
use crate::net::{PageCache, PageKind};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReplayReport {
    pub pages: u64,
    /// Pages per extraction tier (`blob`, `jsonld`, `css`).
    pub tiers: BTreeMap<String, u64>,
    /// Album/track pages whose `data-tralbum` blob lost the canary key (format change early warning).
    pub canary_missing: u64,
    /// Pages the extractor could not read at all (no title / no data).
    pub unreadable: u64,
    /// Music-grid pages: total grid items found.
    pub grid_items: u64,
    /// Example URLs per problem (capped), for debugging.
    pub examples: BTreeMap<String, Vec<String>>,
}

fn note(report: &mut ReplayReport, key: &str, url: &str) {
    let v = report.examples.entry(key.to_string()).or_default();
    if v.len() < 5 {
        v.push(url.to_string());
    }
}

/// Re-run the extractors for `kind` over every cached page (ignoring TTL).
pub fn replay(cache: &Arc<PageCache>, kind: PageKind) -> Result<ReplayReport> {
    let mut rep = ReplayReport::default();
    for page in cache.iter_kind(kind)? {
        let page = page?;
        rep.pages += 1;
        match kind {
            PageKind::Album | PageKind::Stream => {
                let release = extract::parse_tralbum(&page.body, &page.url);
                *rep.tiers.entry(release.tier.as_str().to_string()).or_default() += 1;
                if release.title.is_empty() {
                    rep.unreadable += 1;
                    note(&mut rep, "unreadable", &page.url);
                }
                if extract::tralbum_has_canary(&page.body) == Some(false) {
                    rep.canary_missing += 1;
                    note(&mut rep, "canary_missing", &page.url);
                }
            }
            PageKind::Music => {
                let root = crate::urls::artist_root(&page.url);
                let (items, tier) = extract::parse_music_grid(&page.body, &root);
                *rep.tiers.entry(tier.as_str().to_string()).or_default() += 1;
                rep.grid_items += items.len() as u64;
                if tier == Tier::Css {
                    note(&mut rep, "css_fallback", &page.url);
                }
            }
            PageKind::Fan => match extract::parse_fan_page(&page.body) {
                Ok(_) => *rep.tiers.entry("blob".into()).or_default() += 1,
                Err(_) => {
                    rep.unreadable += 1;
                    note(&mut rep, "unreadable", &page.url);
                }
            },
            PageKind::Discover => {
                let facets = extract::parse_discover_facets(&page.body);
                *rep.tiers.entry(if facets.is_empty() { "css" } else { "blob" }.into()).or_default() += 1;
            }
        }
    }
    Ok(rep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_cached_pages_and_counts_tiers() {
        let cache = PageCache::open_in_memory(1 << 20).unwrap().shared();
        let blob = r#"<html><head></head><body><script data-tralbum="{&quot;current&quot;:{&quot;title&quot;:&quot;T&quot;},&quot;artist&quot;:&quot;A&quot;,&quot;for the curious&quot;:&quot;x&quot;,&quot;trackinfo&quot;:[]}"></script></body></html>"#;
        cache.put("https://a.bandcamp.com/album/t", PageKind::Album, blob, None).unwrap();
        cache.put("https://a.bandcamp.com/album/empty", PageKind::Album, "<html></html>", None).unwrap();
        let rep = replay(&cache, PageKind::Album).unwrap();
        assert_eq!(rep.pages, 2);
        assert_eq!(rep.tiers.values().sum::<u64>(), 2);
        assert!(rep.unreadable >= 1);
        // other kinds are untouched
        assert_eq!(replay(&cache, PageKind::Fan).unwrap().pages, 0);
    }
}
