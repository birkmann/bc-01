//! The Feed page's grouping: a page of inbox items folded into label groups, each split by
//! artist where it spans several.
//!
//! Ports `feedGroups.ts`: [`build_feed_segments`] = `buildFeedSegments`, constants
//! [`GROUP_MIN`] / [`ARTIST_MIN`]. The Feed page calls `build_feed_segments(&page_items)` per
//! rendered page. Pure and per page. Items are cloned into the groups (page order preserved).

use std::collections::HashMap;

use crate::bandcamp::HarvestItemOut;

/// A name needs this many items on the page before it becomes a group.
pub const GROUP_MIN: usize = 5;
/// Inside a label, an artist needs this many releases to earn its own row.
pub const ARTIST_MIN: usize = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct ArtistGroup {
    /// `label:<label>/artist:<artist>`.
    pub key: String,
    pub name: String,
    pub items: Vec<HarvestItemOut>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LabelGroup {
    /// `label:<casefolded name>`
    pub key: String,
    /// First-seen spelling of the label, the follow's name, or the artist.
    pub name: String,
    /// Every item of the label in page order.
    pub items: Vec<HarvestItemOut>,
    /// Artists with at least [`ARTIST_MIN`] releases, in order of first appearance. Empty when the
    /// label reads as one artist (or names no artist at all).
    pub artists: Vec<ArtistGroup>,
    /// One-offs and unnamed rows, page order.
    pub rest: Vec<HarvestItemOut>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FeedSegment {
    Loose(Vec<HarvestItemOut>),
    Label(LabelGroup),
}

impl FeedSegment {
    pub fn is_label(&self) -> bool {
        matches!(self, FeedSegment::Label(_))
    }
    pub fn items(&self) -> &[HarvestItemOut] {
        match self {
            FeedSegment::Loose(i) => i,
            FeedSegment::Label(g) => &g.items,
        }
    }
}

/// The name a card's label line repeats: label, else the follow's name, else the artist.
fn top_name(item: &HarvestItemOut) -> &str {
    [
        item.label_name.as_deref(),
        item.source_label.as_deref(),
        Some(item.artist_name.as_str()),
    ]
    .into_iter()
    .flatten()
    .find(|s| !s.is_empty())
    .unwrap_or("")
}

fn fold(name: &str) -> String {
    name.trim().to_lowercase()
}

fn split_by_artist(group: &mut LabelGroup) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in &group.items {
        let key = fold(&item.artist_name);
        if !key.is_empty() {
            *counts.entry(key).or_default() += 1;
        }
    }

    let mut open: HashMap<String, usize> = HashMap::new();
    for item in &group.items {
        let key = fold(&item.artist_name);
        if !key.is_empty() && counts.get(&key).copied().unwrap_or(0) >= ARTIST_MIN {
            let idx = *open.entry(key.clone()).or_insert_with(|| {
                group.artists.push(ArtistGroup {
                    key: format!("{}/artist:{}", group.key, key),
                    name: item.artist_name.clone(),
                    items: Vec::new(),
                });
                group.artists.len() - 1
            });
            group.artists[idx].items.push(item.clone());
        } else {
            group.rest.push(item.clone());
        }
    }

    // One artist and nothing else is not a split.
    if group.artists.len() == 1 && group.rest.is_empty() {
        if let Some(only) = group.artists.pop() {
            group.rest = only.items;
        }
    }
}

/// Fold a page into label groups and loose runs. Grouping is by (case-folded) name across the
/// whole page, each group sits where its first item appeared; below [`GROUP_MIN`] a name stays
/// loose.
pub fn build_feed_segments(list: &[HarvestItemOut]) -> Vec<FeedSegment> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in list {
        let key = fold(top_name(item));
        if !key.is_empty() {
            *counts.entry(key).or_default() += 1;
        }
    }

    let mut segments: Vec<FeedSegment> = Vec::new();
    let mut open: HashMap<String, usize> = HashMap::new();
    for item in list {
        let key = fold(top_name(item));
        if !key.is_empty() && counts.get(&key).copied().unwrap_or(0) >= GROUP_MIN {
            let idx = *open.entry(key.clone()).or_insert_with(|| {
                segments.push(FeedSegment::Label(LabelGroup {
                    key: format!("label:{key}"),
                    name: top_name(item).to_string(),
                    items: Vec::new(),
                    artists: Vec::new(),
                    rest: Vec::new(),
                }));
                segments.len() - 1
            });
            if let FeedSegment::Label(g) = &mut segments[idx] {
                g.items.push(item.clone());
            }
            continue;
        }
        match segments.last_mut() {
            Some(FeedSegment::Loose(items)) => items.push(item.clone()),
            _ => segments.push(FeedSegment::Loose(vec![item.clone()])),
        }
    }

    for seg in &mut segments {
        if let FeedSegment::Label(g) = seg {
            split_by_artist(g);
        }
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! { static SEQ: Cell<i64> = const { Cell::new(0) }; }

    fn item(label: Option<&str>, artist: &str, source_label: Option<&str>) -> HarvestItemOut {
        let id = SEQ.with(|s| {
            s.set(s.get() + 1);
            s.get()
        });
        HarvestItemOut {
            id,
            url_kind: "album".into(),
            state: "new".into(),
            artist_name: artist.into(),
            label_name: label.map(str::to_string),
            source_kind: Some("follow".into()),
            source_label: source_label.map(str::to_string),
            ..Default::default()
        }
    }
    fn plain(artist: &str) -> HarvestItemOut {
        item(None, artist, None)
    }

    fn ids(xs: &[HarvestItemOut]) -> Vec<i64> {
        xs.iter().map(|i| i.id).collect()
    }
    fn on_label(label: &str, artists: &[&str]) -> Vec<HarvestItemOut> {
        artists.iter().map(|a| item(Some(label), a, None)).collect()
    }
    fn label(seg: &FeedSegment) -> &LabelGroup {
        match seg {
            FeedSegment::Label(g) => g,
            FeedSegment::Loose(_) => panic!("expected a label group, got loose"),
        }
    }
    fn artist_names(g: &LabelGroup) -> Vec<&str> {
        g.artists.iter().map(|a| a.name.as_str()).collect()
    }
    fn seg_names(segs: &[FeedSegment]) -> Vec<String> {
        segs.iter()
            .map(|s| match s {
                FeedSegment::Label(g) => g.name.clone(),
                FeedSegment::Loose(_) => "loose".into(),
            })
            .collect()
    }

    #[test]
    fn nests_the_artists_who_repeat_under_their_label_one_offs_and_unnamed_trailing() {
        let list = on_label("Vibraphone", &["A", "A", "B", "C", "A", "", "B", "D"]);
        let segments = build_feed_segments(&list);
        assert_eq!(segments.len(), 1);
        let g = label(&segments[0]);
        assert_eq!(g.key, "label:vibraphone");
        assert_eq!(g.name, "Vibraphone");
        assert_eq!(ids(&g.items), ids(&list));
        assert_eq!(artist_names(g), ["A", "B"]);
        let keys: Vec<_> = g.artists.iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, ["label:vibraphone/artist:a", "label:vibraphone/artist:b"]);
        assert_eq!(ids(&g.artists[0].items), [list[0].id, list[1].id, list[4].id]);
        assert_eq!(ids(&g.artists[1].items), [list[2].id, list[6].id]);
        assert_eq!(ids(&g.rest), [list[3].id, list[5].id, list[7].id]);
    }

    #[test]
    fn shows_no_sub_groups_for_a_single_artist_label() {
        let list = on_label("L", &["A", "A", "A", "A", "A"]);
        let segs = build_feed_segments(&list);
        let g = label(&segs[0]);
        assert!(g.artists.is_empty());
        assert_eq!(ids(&g.rest), ids(&list));
    }

    #[test]
    fn shows_no_sub_groups_for_a_label_whose_rows_name_no_artist() {
        let list = on_label("L", &["", "", "", "", ""]);
        let segs = build_feed_segments(&list);
        let g = label(&segs[0]);
        assert!(g.artists.is_empty());
        assert_eq!(ids(&g.rest), ids(&list));
    }

    #[test]
    fn keeps_one_repeating_artist_as_a_sub_group_when_one_offs_sit_beside_it() {
        let list = on_label("L", &["A", "A", "B", "C", ""]);
        let segs = build_feed_segments(&list);
        let g = label(&segs[0]);
        assert_eq!(artist_names(g), ["A"]);
        assert_eq!(ids(&g.rest), [list[2].id, list[3].id, list[4].id]);
    }

    #[test]
    fn earns_an_artist_row_at_artist_min_releases_and_not_below() {
        assert_eq!(ARTIST_MIN, 2);
        let list = on_label("L", &["A", "A", "B", "C", "D", "E"]);
        let segs = build_feed_segments(&list);
        let g = label(&segs[0]);
        assert_eq!(artist_names(g), ["A"]);
        let rest: Vec<_> = g.rest.iter().map(|i| i.artist_name.as_str()).collect();
        assert_eq!(rest, ["B", "C", "D", "E"]);
    }

    #[test]
    fn falls_back_to_the_follow_name_then_the_artist_when_there_is_no_label() {
        let mut list: Vec<_> = (0..5)
            .map(|i| item(None, &format!("Q{i}"), Some("deep techno")))
            .collect();
        list.extend((0..5).map(|_| plain("Solo")));
        let segs = build_feed_segments(&list);
        assert_eq!(seg_names(&segs), ["deep techno", "Solo"]);
        // The query group has five different artists: all one-offs, no rows.
        assert!(label(&segs[0]).artists.is_empty());
        assert!(label(&segs[1]).artists.is_empty());
    }

    #[test]
    fn leaves_items_with_no_name_at_all_loose() {
        let list: Vec<_> = (0..6).map(|_| plain("")).collect();
        let segs = build_feed_segments(&list);
        assert_eq!(segs.len(), 1);
        assert!(!segs[0].is_label());
    }

    #[test]
    fn names_a_self_published_artist_once() {
        let list = on_label("Solo", &["Solo", "Solo", "Solo", "Solo", "Solo"]);
        let segs = build_feed_segments(&list);
        let g = label(&segs[0]);
        assert_eq!(g.name, "Solo");
        assert!(g.artists.is_empty());
    }

    #[test]
    fn keeps_a_label_under_group_min_items_loose_repeated_artist_or_not() {
        assert_eq!(GROUP_MIN, 5);
        let list = on_label("L", &["A", "A", "A", "B"]);
        let segs = build_feed_segments(&list);
        assert_eq!(segs.len(), 1);
        assert!(!segs[0].is_label());
    }

    #[test]
    fn orders_labels_and_artists_by_first_appearance() {
        let mut list = on_label("L", &["B"]);
        list.extend(on_label("M", &["X"]));
        list.extend(on_label("L", &["A", "B", "A", "A"]));
        list.extend(on_label("M", &["Y", "X", "Y", "X"]));
        let segs = build_feed_segments(&list);
        assert_eq!(seg_names(&segs), ["L", "M"]);
        assert_eq!(artist_names(label(&segs[0])), ["B", "A"]);
        assert_eq!(artist_names(label(&segs[1])), ["X", "Y"]);
    }

    #[test]
    fn sits_a_group_where_its_first_item_appeared_loose_runs_around_it() {
        let x = item(Some("X"), "x", None);
        let wall = on_label("L", &["A", "A", "B", "B", "C"]);
        let y = item(Some("Y"), "", None);
        let z = plain("z");
        let mut list = vec![x.clone()];
        list.extend(wall);
        list.push(y.clone());
        list.push(z.clone());
        let segs = build_feed_segments(&list);
        let kinds: Vec<_> = segs.iter().map(|s| s.is_label()).collect();
        assert_eq!(kinds, [false, true, false]);
        assert_eq!(ids(segs[0].items()), [x.id]);
        assert_eq!(ids(segs[2].items()), [y.id, z.id]);
    }

    #[test]
    fn partitions_a_label_its_rows_and_rest_are_exactly_its_items_in_page_order() {
        let list = on_label("L", &["A", "", "B", "A", "C", "B", "D", "A", "", "E"]);
        let segs = build_feed_segments(&list);
        let g = label(&segs[0]);
        assert_eq!(ids(&g.items), ids(&list));
        let mut partitioned: Vec<i64> = g
            .artists
            .iter()
            .flat_map(|a| ids(&a.items))
            .chain(ids(&g.rest))
            .collect();
        partitioned.sort();
        assert_eq!(partitioned, ids(&list));
        let sorted = |v: Vec<i64>| {
            let mut s = v.clone();
            s.sort();
            assert_eq!(v, s);
        };
        for a in &g.artists {
            sorted(ids(&a.items));
        }
        sorted(ids(&g.rest));
    }

    #[test]
    fn merges_spellings_by_case_and_keeps_the_first_one_seen() {
        let mut list = on_label("Vibronics", &["dub", "DUB"]);
        list.extend(on_label("VIBRONICS", &["Dub", "x", "y"]));
        let segs = build_feed_segments(&list);
        assert_eq!(segs.len(), 1);
        let g = label(&segs[0]);
        assert_eq!(g.name, "Vibronics");
        assert_eq!(artist_names(g), ["dub"]);
        assert_eq!(g.artists[0].items.len(), 3);
    }
}
