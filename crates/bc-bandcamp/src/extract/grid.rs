//! `/music` grid and the "you may also like" strip.

use regex::Regex;
use scraper::{ElementRef, Html};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

use crate::urls::normalise;
use super::util::{as_int, attr, attr_json, css_all, node_first, str_or_empty, text_strip};
use super::{GridItem, Tier};

fn art_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"/img/a(\d+)_").expect("static regex"))
}

/// Port of `parse_music_grid`: full discography from an artist or label `/music` page.
///
/// The one extractor whose sources are not a ladder. Bandcamp splits the grid into two
/// **disjoint** halves: the newest releases are server-rendered as `li.music-grid-item`, and
/// everything older sits in `ol#music-grid[data-client-items]`. Reading only the attribute
/// returned every record except the newest sixteen while reporting the count as exact, so
/// both halves are read and unioned, rendered first (the page's own order). The tier answers
/// the only question left: `Blob` means the attribute was there (even an empty list: "and
/// that is all") and the list is the whole catalogue, `Css` means it was not and only the
/// rendered first page could be read.
pub fn parse_music_grid(html_text: &str, root_url: &str) -> (Vec<GridItem>, Tier) {
    let doc = Html::parse_document(html_text);
    let tail = attr_json(&doc, "ol#music-grid", "data-client-items");

    let mut out: Vec<GridItem> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let all = rendered_items(&doc, root_url).into_iter().chain(client_items(tail.as_ref(), root_url));
    for item in all {
        match seen.get(&item.page_url) {
            None => {
                seen.insert(item.page_url.clone(), out.len());
                out.push(item);
            }
            Some(&i) => fill_blanks(&mut out[i], &item),
        }
    }
    let tier = if matches!(tail, Some(Value::Array(_))) { Tier::Blob } else { Tier::Css };
    (out, tier)
}

/// Port of `_grid_url`: absolute, canonical URL for one grid entry (both halves are
/// deduplicated against each other by URL, so both go through `normalise`).
fn grid_url(root_url: &str, raw: &str) -> String {
    let absolute = if raw.starts_with("http") {
        raw.to_string()
    } else {
        format!("{}/{}", root_url.trim_end_matches('/'), raw.trim_start_matches('/'))
    };
    normalise(&absolute)
}

/// Port of `_client_items`: the tail, everything the page has but did not render.
fn client_items(raw: Option<&Value>, root_url: &str) -> Vec<GridItem> {
    let Some(Value::Array(entries)) = raw else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries {
        let Value::Object(entry) = entry else { continue };
        let page_url = str_or_empty(entry.get("page_url"));
        if page_url.is_empty() {
            continue;
        }
        let item_type = str_or_empty(entry.get("type"));
        out.push(GridItem {
            page_url: grid_url(root_url, &page_url),
            title: str_or_empty(entry.get("title")),
            artist: str_or_empty(entry.get("artist")),
            item_type: if item_type.is_empty() { "album".into() } else { item_type },
            bc_item_id: as_int(entry.get("id")),
            band_id: as_int(entry.get("band_id")),
            art_id: as_int(entry.get("art_id")),
            art_url: None,
        });
    }
    out
}

/// Port of `_rendered_items`: the head, the newest releases served in the HTML itself.
/// Scoped to the grid's own `<ol>`, with the bare selector kept for themed pages that drop the id.
fn rendered_items(doc: &Html, root_url: &str) -> Vec<GridItem> {
    let mut nodes = css_all(doc, "ol#music-grid li.music-grid-item");
    if nodes.is_empty() {
        nodes = css_all(doc, "li.music-grid-item");
    }
    let mut out = Vec::new();
    for node in nodes {
        let Some(href) = node_first(node, "a").and_then(|l| attr(l, "href")).filter(|h| !h.is_empty()) else {
            continue;
        };
        let raw_id = attr(node, "data-item-id").unwrap_or("");
        let item_type = if raw_id.starts_with("track") { "track" } else { "album" };
        let bc_id = raw_id.rsplit('-').next().unwrap_or("");

        // The title cell holds the per-item artist in a child span, so reading the cell as one
        // string runs the two together. Only the cell's own text is the title.
        let (mut title, mut artist) = (String::new(), String::new());
        if let Some(title_node) = node_first(node, "p.title") {
            title = text_strip(title_node, false);
            if title.is_empty() {
                title = text_strip(title_node, true);
            }
            if let Some(over) = node_first(title_node, "span.artist-override") {
                artist = text_strip(over, true);
            }
        }

        let band_id = attr(node, "data-band-id")
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|n| *n != 0);
        let (art_id, art_url) = grid_art(node);
        out.push(GridItem {
            page_url: grid_url(root_url, href),
            title,
            artist,
            item_type: item_type.into(),
            bc_item_id: if !bc_id.is_empty() && bc_id.bytes().all(|b| b.is_ascii_digit()) {
                bc_id.parse().ok()
            } else {
                None
            },
            band_id,
            art_id,
            art_url,
        });
    }
    out
}

/// Port of `_fill_blanks`: merge a release that turns up in both halves rather than dropping it.
fn fill_blanks(kept: &mut GridItem, extra: &GridItem) {
    if kept.title.is_empty() && !extra.title.is_empty() {
        kept.title = extra.title.clone();
    }
    if kept.artist.is_empty() && !extra.artist.is_empty() {
        kept.artist = extra.artist.clone();
    }
    if kept.bc_item_id.is_none_or(|v| v == 0) && extra.bc_item_id.is_some_and(|v| v != 0) {
        kept.bc_item_id = extra.bc_item_id;
    }
    if kept.band_id.is_none_or(|v| v == 0) && extra.band_id.is_some_and(|v| v != 0) {
        kept.band_id = extra.band_id;
    }
    if kept.art_id.is_none_or(|v| v == 0) && extra.art_id.is_some_and(|v| v != 0) {
        kept.art_id = extra.art_id;
    }
    if kept.art_url.as_deref().is_none_or(str::is_empty) && extra.art_url.as_deref().is_some_and(|s| !s.is_empty()) {
        kept.art_url = extra.art_url.clone();
    }
}

/// Port of `_grid_art`: cover art for one rendered grid item as `(art_id, art_url)`.
///
/// Only the first row has a real `src`; everything below the fold is lazy-loaded (placeholder
/// gif in `src`, image in `data-original`). An id is preferred over the URL because the grid's
/// thumbnails are tiny; the URL is the fallback so covers degrade to small, not to nothing.
fn grid_art(node: ElementRef<'_>) -> (Option<i64>, Option<String>) {
    let Some(img) = node_first(node, "div.art img") else { return (None, None) };
    let src = attr(img, "data-original")
        .filter(|s| !s.is_empty())
        .or_else(|| attr(img, "src"))
        .unwrap_or("");
    if let Some(m) = art_id_re().captures(src) {
        return (m[1].parse().ok(), None);
    }
    if src.starts_with("http") { (None, Some(src.to_string())) } else { (None, None) }
}

/// Port of `parse_recommendations`: Bandcamp's own "if you like this, you may also like" strip.
///
/// CSS-only by nature: the strip is rendered markup with no JSON blob behind it, so the data
/// lives in the `data-*` attributes of each `<li>`.
pub fn parse_recommendations(html_text: &str) -> Vec<GridItem> {
    let doc = Html::parse_document(html_text);
    let mut out = Vec::new();
    for node in css_all(&doc, "li.recommended-album") {
        let Some(href) = node_first(node, "a.album-link").and_then(|l| attr(l, "href")) else { continue };
        if href.is_empty() || !href.starts_with("http") {
            continue;
        }
        let art_id = node_first(node, "img.album-art")
            .and_then(|i| attr(i, "src"))
            .and_then(|s| art_id_re().captures(s))
            .and_then(|m| m[1].parse().ok());
        let item_id = attr(node, "data-albumid").unwrap_or("");
        out.push(GridItem {
            page_url: normalise(href),
            title: attr(node, "data-albumtitle").unwrap_or("").trim().to_string(),
            artist: attr(node, "data-artist").unwrap_or("").trim().to_string(),
            item_type: if href.contains("/track/") { "track" } else { "album" }.into(),
            bc_item_id: if !item_id.is_empty() && item_id.bytes().all(|b| b.is_ascii_digit()) {
                item_id.parse().ok()
            } else {
                None
            },
            band_id: None,
            art_id,
            art_url: None,
        });
    }
    out
}

