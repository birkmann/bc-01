//! Parsing Bandcamp pages (port of `bcapp/harvest/extract.py`).
//!
//! Every extractor is a ladder, tried in order, recording which tier succeeded:
//!
//! 1. [`Tier::Blob`]   -- embedded JSON (`data-tralbum`, `data-client-items`, `pagedata`). Authoritative.
//! 2. [`Tier::JsonLd`] -- `application/ld+json`, the public SEO standard; the most stable fallback.
//! 3. [`Tier::Css`]    -- the gen-2 selectors, kept verbatim as the floor.
//!
//! The tier is persisted per item so a degradation dashboard can show "37% of pages fell
//! back to CSS this week" -- the early warning that the markup changed.
//!
//! One extractor is not a ladder: on a `/music` page the blob and the DOM are *halves*, not
//! alternatives, so [`parse_music_grid`] unions them and its tier says whether the attribute
//! was there rather than which source won.
//!
//! Every top-level parser takes `&str` HTML and performs no I/O, so the page cache can
//! replay extractors offline. Field names of the output structs mirror the Python
//! dataclasses exactly.

#![allow(clippy::collapsible_if)]

mod fan;
mod grid;
mod profile;
mod tralbum;
mod util;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub use fan::{
    collectors_from_results, parse_collectors, parse_discover_facets, parse_fan_page,
};
pub use grid::{parse_music_grid, parse_recommendations};
pub use profile::{looks_like_label, parse_band_profile, parse_roster};
pub use tralbum::{artist_parts, parse_track_album, parse_tralbum, tralbum_has_canary};
pub use util::{attr_json, parse_bc_date};

/// Bandcamp's own note to scrapers, present as a literal key in `data-tralbum`.
/// Verified present on a live album page; its disappearance means the blob format changed
/// and the extractors need review.
pub const CANARY_KEY: &str = "for the curious";

/// Which rung of the ladder produced a result. Persisted as `"blob"`, `"jsonld"`, `"css"`
/// (the legacy strings), plus `"api"` for results built from API/list data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Tier {
    #[default]
    #[serde(rename = "blob")]
    Blob,
    #[serde(rename = "jsonld")]
    JsonLd,
    #[serde(rename = "css")]
    Css,
    #[serde(rename = "api")]
    Api,
}

impl Tier {
    /// The persisted string (`Tier` literal in Python).
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Blob => "blob",
            Tier::JsonLd => "jsonld",
            Tier::Css => "css",
            Tier::Api => "api",
        }
    }
    /// Parse a persisted tier string (`"json_ld"` accepted as an alias).
    pub fn parse(s: &str) -> Option<Tier> {
        match s {
            "blob" => Some(Tier::Blob),
            "jsonld" | "json_ld" => Some(Tier::JsonLd),
            "css" => Some(Tier::Css),
            "api" => Some(Tier::Api),
            _ => None,
        }
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Port of `HarvestedTrack`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HarvestedTrack {
    pub title: String,
    pub track_num: Option<i64>,
    pub duration_sec: Option<f64>,
    pub url: Option<String>,
    pub artist: Option<String>,
    pub bc_track_id: Option<i64>,
    pub has_lyrics: bool,
    /// The `mp3-128` CDN URL Bandcamp's own player uses. Absent on preorders, private
    /// streams and a few label-gated releases, hence optional.
    pub stream_url: Option<String>,
}

impl HarvestedRelease {
    /// What this page says is out now: a track with no stream `file` is not released yet (the
    /// "unreleased" reading only applies while `is_preorder`). `release_id` and `checked_at` are
    /// filled in when stored.
    pub fn availability(&self) -> bc_types::library::ReleaseAvailability {
        bc_types::library::ReleaseAvailability {
            release_id: 0,
            checked_at: String::new(),
            is_preorder: self.is_preorder,
            release_date: self.release_date.clone(),
            tracks: self
                .tracks
                .iter()
                .map(|t| bc_types::library::TrackAvailability {
                    track_num: t.track_num,
                    title: t.title.clone(),
                    // Bandcamp lists 0 for a track that is not out yet.
                    duration_sec: t.duration_sec.filter(|d| *d > 0.0),
                    available: t.stream_url.is_some(),
                })
                .collect(),
            fetched: false,
        }
    }
}

/// Port of `HarvestedRelease`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HarvestedRelease {
    pub url: String,
    pub item_type: String,
    pub title: String,
    pub artist_name: String,
    pub bc_item_id: Option<i64>,
    pub band_id: Option<i64>,
    pub label_name: Option<String>,
    pub art_url: Option<String>,
    pub art_id: Option<i64>,
    pub release_date: Option<String>,
    pub about: Option<String>,
    pub credits: Option<String>,
    pub tags: Vec<String>,
    pub tracks: Vec<HarvestedTrack>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub is_free_download: bool,
    pub is_purchasable: bool,
    pub is_preorder: bool,
    pub is_private: bool,
    pub tier: Tier,
    /// Built from list data (a grid or API entry) rather than the release's own page: its
    /// fields are hints to fill blanks with, not facts to overwrite a full parse.
    pub shallow: bool,
    pub missing: Vec<String>,
}

impl Default for HarvestedRelease {
    fn default() -> Self {
        Self {
            url: String::new(),
            item_type: "album".into(),
            title: String::new(),
            artist_name: String::new(),
            bc_item_id: None,
            band_id: None,
            label_name: None,
            art_url: None,
            art_id: None,
            release_date: None,
            about: None,
            credits: None,
            tags: Vec::new(),
            tracks: Vec::new(),
            price: None,
            currency: None,
            is_free_download: false,
            is_purchasable: true,
            is_preorder: false,
            is_private: false,
            tier: Tier::Blob,
            shallow: false,
            missing: Vec::new(),
        }
    }
}

impl HarvestedRelease {
    /// Python `track_count` property.
    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }
}

/// Port of `GridItem`: one entry from an artist/label `/music` page.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GridItem {
    pub page_url: String,
    pub title: String,
    pub artist: String,
    pub item_type: String,
    pub bc_item_id: Option<i64>,
    pub band_id: Option<i64>,
    /// Bandcamp's art id, from which a URL at any size can be built.
    pub art_id: Option<i64>,
    /// A scraped cover URL, for the pages that expose no art id at all.
    pub art_url: Option<String>,
}

/// Port of `BandProfile`: the identity half of an artist or label page.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BandProfile {
    pub url: String,
    pub name: String,
    pub band_id: Option<i64>,
    pub location: Option<String>,
    pub bio: Option<String>,
    pub image_url: Option<String>,
    pub is_label: bool,
    /// `{"label": text, "url": href}` pairs.
    pub links: Vec<BTreeMap<String, String>>,
}

/// Port of `RosterArtist`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RosterArtist {
    pub name: String,
    pub url: String,
    pub band_id: Option<i64>,
    pub location: Option<String>,
}

/// Port of `FanPage`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FanPage {
    pub fan_id: i64,
    pub username: String,
    pub display_name: String,
    pub collection_count: i64,
    pub wishlist_count: i64,
    pub hidden_count: i64,
    /// Keyed by tab -- `collection` / `wishlist` / `hidden` -- each mapping an item key like
    /// `a3106559041` to its record (insertion order preserved: newest first). Not a flat list.
    pub item_cache: serde_json::Map<String, Value>,
    /// Per tab, the cursor for the item after the embedded batch. It is the *oldest* of the
    /// batch, so on a list short enough to fit in the page it is the end of the list.
    pub last_tokens: BTreeMap<String, String>,
}

impl FanPage {
    /// Port of `FanPage.cached_items`: the records the fan page already embedded for a tab
    /// (free with the page fetch). Absent / non-object tabs give an empty list.
    pub fn cached_items(&self, which: &str) -> Vec<Value> {
        match self.item_cache.get(which) {
            Some(Value::Object(tab)) => tab.values().cloned().collect(),
            _ => Vec::new(),
        }
    }
}

/// Port of `Collector`: one fan who bought a record.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Collector {
    pub username: String,
    pub name: String,
    pub fan_id: Option<i64>,
    pub image_id: Option<i64>,
    /// Bandcamp's paging cursor; the last one on a page asks for the next.
    pub token: Option<String>,
    pub why: Option<String>,
    pub fav_track: Option<String>,
}

impl Collector {
    /// Python `url` property.
    pub fn url(&self) -> String {
        format!("https://bandcamp.com/{}", self.username)
    }
}

/// Port of `Collectors`: the "supported by" section of a release page.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Collectors {
    pub thumbs: Vec<Collector>,
    pub reviews: Vec<Collector>,
    pub more_thumbs: bool,
    pub more_reviews: bool,
}
