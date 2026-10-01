//! Bandcamp search hits read into the shapes they are scanned in.
//!
//! Ports `bandcampSearch.ts`:
//! - [`group_hits`] = `groupHits` (Explore screen and the library search's Bandcamp fallback),
//! - [`hit_to_card`] = `hitToCard` (album hit as a grid card),
//! - [`explore_path`] = `explorePath` (link to `/explore?q=...`).

use crate::bandcamp::{ReleaseCardOut, SearchHitOut};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupedHits {
    /// Artists and labels: pages, not music.
    pub bands: Vec<SearchHitOut>,
    pub albums: Vec<SearchHitOut>,
    pub tracks: Vec<SearchHitOut>,
}

/// Sort one ranked list into bands/albums/tracks. Fan profiles are dropped (the band view cannot
/// open them) and a second hit for the same URL is noise.
pub fn group_hits(hits: &[SearchHitOut]) -> GroupedHits {
    let mut seen = std::collections::HashSet::new();
    let mut out = GroupedHits::default();
    for hit in hits {
        if hit.kind == "fan" {
            continue;
        }
        if !seen.insert(hit.url.as_str()) {
            continue;
        }
        match hit.kind.as_str() {
            "album" => out.albums.push(hit.clone()),
            "track" => out.tracks.push(hit.clone()),
            _ => out.bands.push(hit.clone()),
        }
    }
    out
}

/// An album hit as a grid card. `is_free_download` is `false` = unknown (autocomplete does not
/// say), so the card shows no badge rather than the wrong one.
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

/// JS `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*'
            | b'\'' | b'(' | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Where the same words go on the Explore screen.
pub fn explore_path(q: &str) -> String {
    format!("/explore?q={}", encode_uri_component(q))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: &str, name: &str, url: &str) -> SearchHitOut {
        SearchHitOut {
            kind: kind.into(),
            name: name.into(),
            url: url.into(),
            ..Default::default()
        }
    }

    fn names(v: &[SearchHitOut]) -> Vec<&str> {
        v.iter().map(|h| h.name.as_str()).collect()
    }

    #[test]
    fn sorts_one_ranked_list_into_bands_albums_and_tracks() {
        let g = group_hits(&[
            hit("track", "Her", "https://c.bandcamp.com/track/her"),
            hit("album", "Volume One", "https://c.bandcamp.com/album/v1"),
            hit("artist", "Guy J", "https://guyj.bandcamp.com"),
            hit("label", "Cocoon", "https://c.bandcamp.com"),
        ]);
        assert_eq!(names(&g.bands), ["Guy J", "Cocoon"]);
        assert_eq!(names(&g.albums), ["Volume One"]);
        assert_eq!(names(&g.tracks), ["Her"]);
    }

    #[test]
    fn keeps_the_first_of_two_hits_for_the_same_page() {
        let g = group_hits(&[
            hit("album", "Same", "https://c.bandcamp.com/album/same"),
            hit("track", "Same", "https://c.bandcamp.com/album/same"),
        ]);
        assert_eq!(g.albums.len(), 1);
        assert_eq!(g.tracks.len(), 0);
    }

    #[test]
    fn drops_fan_profiles_which_the_app_cannot_open() {
        let g = group_hits(&[
            hit("fan", "numajohn", "https://bandcamp.com/numajohn"),
            hit("artist", "Unca John", "https://uncajohn.bandcamp.com"),
        ]);
        assert_eq!(names(&g.bands), ["Unca John"]);
    }

    #[test]
    fn hit_to_card_carries_the_hit_over_and_leaves_the_unknowns_unclaimed() {
        let h = SearchHitOut {
            subtitle: "Various Artists".into(),
            art_url: Some("https://f4.bcbits.com/img/a.jpg".into()),
            in_library: true,
            ..hit("album", "Volume One", "https://c.bandcamp.com/album/v1")
        };
        let card = hit_to_card(&h);
        assert_eq!(card.url, "https://c.bandcamp.com/album/v1");
        assert_eq!(card.title, "Volume One");
        assert_eq!(card.artist_name, "Various Artists");
        assert_eq!(card.item_type, "album");
        assert_eq!(card.art_url.as_deref(), Some("https://f4.bcbits.com/img/a.jpg"));
        assert!(card.in_library);
        assert!(!card.blacklisted);
        // Autocomplete does not say whether a release is free; a badge would be a guess.
        assert!(!card.is_free_download);
        assert_eq!(card.release_date, None);
    }

    #[test]
    fn explore_path_sends_the_same_words_to_the_explore_screen_escaped() {
        assert_eq!(
            explore_path("25 Years Cocoon Recordings - Volume One"),
            "/explore?q=25%20Years%20Cocoon%20Recordings%20-%20Volume%20One"
        );
        assert_eq!(explore_path("a&b/é"), "/explore?q=a%26b%2F%C3%A9");
    }
}
