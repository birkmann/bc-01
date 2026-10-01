//! Fan pages, collectors ("supported by") and discover facets.

use scraper::Html;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

use super::util::{attr_json, int_or_zero, obj, str_or_empty, truthy};
use super::{Collector, Collectors, FanPage};
use crate::HarvestError;

/// Port of `parse_fan_page`: parse a fan page's embedded state.
///
/// The blob rides on `div#pagedata[data-blob]` -- *not* a `<script>`. Selectors are tried in
/// order so a future move between elements degrades instead of breaking outright. Errors with
/// `HarvestError::Extraction` (Python `ExtractionErrorLocal`) when no blob is found.
pub fn parse_fan_page(html_text: &str) -> Result<FanPage, HarvestError> {
    let doc = Html::parse_document(html_text);

    let mut blob = None;
    for selector in ["#pagedata[data-blob]", "div#pagedata", "script#pagedata", "[data-blob]"] {
        let found = attr_json(&doc, selector, "data-blob");
        if matches!(found, Some(Value::Object(_))) {
            blob = found;
            break;
        }
    }
    let Some(Value::Object(blob)) = blob else {
        return Err(HarvestError::Extraction("fan page carries no pagedata blob".into()));
    };

    let fan_data = obj(blob.get("fan_data"));
    let collection = obj(blob.get("collection_data"));
    let wishlist = obj(blob.get("wishlist_data"));
    let hidden = obj(blob.get("hidden_data"));
    let cache = obj(blob.get("item_cache"));

    let mut last_tokens = BTreeMap::new();
    for (tab, data) in [("collection", &collection), ("wishlist", &wishlist), ("hidden", &hidden)] {
        if truthy(data.get("last_token")) {
            last_tokens.insert(tab.to_string(), str_or_empty(data.get("last_token")));
        }
    }

    let username = str_or_empty(fan_data.get("username"));
    let name = str_or_empty(fan_data.get("name"));
    Ok(FanPage {
        fan_id: int_or_zero(fan_data.get("fan_id")),
        display_name: if name.is_empty() { username.clone() } else { name },
        username,
        collection_count: int_or_zero(collection.get("item_count")),
        wishlist_count: int_or_zero(wishlist.get("item_count")),
        hidden_count: int_or_zero(hidden.get("item_count")),
        item_cache: cache,
        last_tokens,
    })
}

/// Port of `_collector`: one buyer row, `None` when it has no username.
fn collector(entry: &Map<String, Value>) -> Option<Collector> {
    let Some(Value::String(username)) = entry.get("username") else { return None };
    if username.is_empty() {
        return None;
    }
    let int_only = |k: &str| match entry.get(k) {
        Some(Value::Number(n)) => n.as_i64(),
        _ => None,
    };
    let name = str_or_empty(entry.get("name"));
    let name = if name.is_empty() { username.as_str() } else { name.as_str() }.trim().to_string();
    let trimmed = |k: &str| match entry.get(k) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    };
    Some(Collector {
        username: username.clone(),
        name: if name.is_empty() { username.clone() } else { name },
        fan_id: int_only("fan_id"),
        image_id: int_only("image_id"),
        token: if truthy(entry.get("token")) { Some(str_or_empty(entry.get("token"))) } else { None },
        why: trimmed("why"),
        fav_track: trimmed("fav_track_title"),
    })
}

/// Port of `collectors_from_results`: the rows of a `tralbumcollectors` API page (or the
/// page blob's lists). Non-list input gives an empty vector.
pub fn collectors_from_results(rows: Option<&Value>) -> Vec<Collector> {
    let Some(Value::Array(rows)) = rows else { return Vec::new() };
    rows.iter()
        .filter_map(|e| match e {
            Value::Object(m) => collector(m),
            _ => None,
        })
        .collect()
}

/// Port of `parse_collectors`: the "supported by" grid and reviews a release page embeds
/// (`div#collectors-data[data-blob]`). A page with no section parses to an empty result.
pub fn parse_collectors(html_text: &str) -> Collectors {
    let doc = Html::parse_document(html_text);
    let Some(Value::Object(blob)) = attr_json(&doc, "div#collectors-data", "data-blob") else {
        return Collectors::default();
    };
    Collectors {
        thumbs: collectors_from_results(blob.get("thumbs")),
        reviews: collectors_from_results(blob.get("reviews")),
        more_thumbs: truthy(blob.get("more_thumbs_available")),
        more_reviews: truthy(blob.get("more_reviews_available")),
    }
}

/// Port of `parse_discover_facets`: snapshot the discover filter vocabulary embedded in the page.
///
/// The blob rides on `div#DiscoverApp[data-blob]` (not a `<script>`, no `id="pagedata"`);
/// selectors are tried in order so a move between elements degrades rather than breaks.
/// Returns an empty map when no blob is found.
pub fn parse_discover_facets(html_text: &str) -> BTreeMap<String, Vec<Value>> {
    let doc = Html::parse_document(html_text);
    let mut blob = None;
    for selector in ["div#DiscoverApp[data-blob]", "script#pagedata", "[data-blob]"] {
        let found = attr_json(&doc, selector, "data-blob");
        if matches!(found, Some(Value::Object(_))) {
            blob = found;
            break;
        }
    }
    let Some(Value::Object(blob)) = blob else { return BTreeMap::new() };

    let state = obj(obj(blob.get("appData")).get("initialState"));
    let mut out = BTreeMap::new();
    for key in ["genres", "subgenres", "slices", "times", "locations", "categories"] {
        if let Some(Value::Array(v)) = state.get(key) {
            out.insert(key.to_string(), v.clone());
        }
    }
    out
}
