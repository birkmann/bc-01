//! Band profile, label roster and label detection.

use scraper::Html;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::urls::{artist_root, display_name, normalise};
use super::util::{as_int, attr, attr_json, css_all, css_first, node_first, str_or_empty, text_strip, truthy};
use super::{BandProfile, RosterArtist};

/// Port of `parse_roster`: label roster from `/artists`.
///
/// This page uses single-quoted attributes, so it must go through a real HTML parser.
pub fn parse_roster(html_text: &str, root_url: &str) -> Vec<RosterArtist> {
    let doc = Html::parse_document(html_text);
    let mut out = Vec::new();
    for node in css_all(&doc, "li.artists-grid-item") {
        let Some(href) = node_first(node, "a").and_then(|l| attr(l, "href")).filter(|h| !h.is_empty()) else {
            continue;
        };
        let name = node_first(node, "div.artists-grid-name");
        let location = node_first(node, "div.artists-grid-location");
        let band_id = attr(node, "data-item-id").unwrap_or("");
        out.push(RosterArtist {
            name: name.map(|n| text_strip(n, true)).unwrap_or_default(),
            url: normalise(&if href.starts_with("http") { href.to_string() } else { format!("{root_url}{href}") }),
            band_id: if !band_id.is_empty() && band_id.bytes().all(|b| b.is_ascii_digit()) {
                band_id.parse().ok()
            } else {
                None
            },
            location: location.map(|n| text_strip(n, true)),
        });
    }
    out
}

/// Port of `looks_like_label`: a roster grid in the DOM, or Bandcamp's own `is_label` flag
/// in the band blob (for labels whose `/music` page renders no roster grid; not a replacement
/// for the DOM check: some label accounts still carry `is_label: false`).
pub fn looks_like_label(html_text: &str) -> bool {
    looks_like_label_doc(&Html::parse_document(html_text))
}

fn looks_like_label_doc(doc: &Html) -> bool {
    if css_first(doc, "ol.artists-grid").is_some()
        || css_first(doc, r#"[data-test="label-artists"]"#).is_some()
        || css_first(doc, "li.artists-grid-item").is_some()
    {
        return true;
    }
    match attr_json(doc, "script[data-band]", "data-band") {
        Some(Value::Object(band)) => truthy(band.get("is_label")),
        _ => false,
    }
}

/// Port of `parse_band_profile`: the header of an artist or label page, who they are, not
/// what they sell. Every field degrades independently.
pub fn parse_band_profile(html_text: &str, root_url: &str) -> BandProfile {
    let doc = Html::parse_document(html_text);
    let mut profile = BandProfile {
        url: artist_root(root_url),
        name: display_name(root_url),
        ..Default::default()
    };

    if let Some(Value::Object(band)) = attr_json(&doc, "script[data-band]", "data-band") {
        profile.band_id = as_int(band.get("id"));
        if truthy(band.get("name")) {
            profile.name = str_or_empty(band.get("name"));
        }
    }

    if let Some(node) = css_first(&doc, "p#band-name-location span.title")
        .or_else(|| css_first(&doc, "#band-name-location"))
    {
        let text = text_strip(node, true);
        if !text.is_empty() {
            profile.name = text;
        }
    }

    if let Some(location) = css_first(&doc, "p#band-name-location span.location") {
        let t = text_strip(location, true);
        profile.location = (!t.is_empty()).then_some(t);
    }

    if let Some(bio) = css_first(&doc, "#bio-text").or_else(|| css_first(&doc, "p.bio-text")) {
        let t = text_strip(bio, true);
        profile.bio = (!t.is_empty()).then_some(t);
    } else if let Some(meta) = css_first(&doc, r#"meta[property="og:description"]"#) {
        profile.bio = attr(meta, "content").filter(|s| !s.is_empty()).map(str::to_string);
    }

    if let Some(image) = css_first(&doc, "div.band-photo-container img").or_else(|| css_first(&doc, "img.band-photo")) {
        profile.image_url = attr(image, "src").filter(|s| !s.is_empty()).map(str::to_string);
    }

    for link in css_all(&doc, "#band-links a, ol#band-links a") {
        let href = attr(link, "href").unwrap_or("");
        let text = text_strip(link, true);
        if !href.is_empty() && !text.is_empty() {
            let mut m = BTreeMap::new();
            m.insert("label".to_string(), text);
            m.insert("url".to_string(), href.to_string());
            profile.links.push(m);
        }
    }

    profile.is_label = looks_like_label_doc(&doc);
    profile
}
