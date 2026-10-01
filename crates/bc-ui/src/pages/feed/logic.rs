//! Pure port of the legacy `feedGroups.ts` (+ its vitest cases): one page of inbox
//! items folded into label groups, each split by artist where it spans several.
use std::collections::HashMap;

use bc_types::bandcamp::HarvestItemOut;

/// A name needs this many items on the page before it becomes a group.
pub const GROUP_MIN: usize = 5;
/// Inside a label, an artist needs this many releases to earn its own row.
pub const ARTIST_MIN: usize = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct ArtistGroup {
    /// `label:<label>/artist:<artist>`: the same artist on two labels collapses independently.
    pub key: String,
    pub name: String,
    pub items: Vec<HarvestItemOut>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LabelGroup {
    /// `label:<casefolded name>`
    pub key: String,
    pub name: String,
    /// Every item of the label in page order.
    pub items: Vec<HarvestItemOut>,
    pub artists: Vec<ArtistGroup>,
    /// One-offs and unnamed rows, page order.
    pub rest: Vec<HarvestItemOut>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FeedSegment {
    Loose(Vec<HarvestItemOut>),
    Label(LabelGroup),
}

/// What a card's label line repeats: the imprint, else the follow's name, else the artist.
fn top_name(item: &HarvestItemOut) -> String {
    item.label_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(item.source_label.as_deref().filter(|s| !s.is_empty()))
        .unwrap_or(&item.artist_name)
        .to_string()
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
    for item in group.items.clone() {
        let key = fold(&item.artist_name);
        if !key.is_empty() && counts.get(&key).copied().unwrap_or(0) >= ARTIST_MIN {
            let idx = *open.entry(key.clone()).or_insert_with(|| {
                group.artists.push(ArtistGroup { key: format!("{}/artist:{key}", group.key), name: item.artist_name.clone(), items: vec![] });
                group.artists.len() - 1
            });
            group.artists[idx].items.push(item);
        } else {
            group.rest.push(item);
        }
    }
    if group.artists.len() == 1 && group.rest.is_empty() {
        group.rest = group.artists.remove(0).items;
    }
}

/// Fold a page into label groups and loose runs; groups sit where their first item appeared.
pub fn build_feed_segments(list: &[HarvestItemOut]) -> Vec<FeedSegment> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for item in list {
        let key = fold(&top_name(item));
        if !key.is_empty() {
            *counts.entry(key).or_default() += 1;
        }
    }
    let mut segments: Vec<FeedSegment> = vec![];
    let mut open: HashMap<String, usize> = HashMap::new();
    for item in list {
        let key = fold(&top_name(item));
        if !key.is_empty() && counts.get(&key).copied().unwrap_or(0) >= GROUP_MIN {
            let idx = *open.entry(key.clone()).or_insert_with(|| {
                segments.push(FeedSegment::Label(LabelGroup { key: format!("label:{key}"), name: top_name(item), ..Default::default() }));
                segments.len() - 1
            });
            if let FeedSegment::Label(g) = &mut segments[idx] {
                g.items.push(item.clone());
            }
            continue;
        }
        match segments.last_mut() {
            Some(FeedSegment::Loose(v)) => v.push(item.clone()),
            _ => segments.push(FeedSegment::Loose(vec![item.clone()])),
        }
    }
    for seg in segments.iter_mut() {
        if let FeedSegment::Label(g) = seg {
            split_by_artist(g);
        }
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, label: Option<&str>, artist: &str, source: Option<&str>) -> HarvestItemOut {
        HarvestItemOut {
            id,
            url_kind: "album".into(),
            state: "new".into(),
            artist_name: artist.into(),
            label_name: label.map(Into::into),
            source_kind: Some("follow".into()),
            source_label: source.map(Into::into),
            ..Default::default()
        }
    }
    struct Gen(i64);
    impl Gen {
        fn on_label(&mut self, label: &str, artists: &[&str]) -> Vec<HarvestItemOut> {
            artists.iter().map(|a| { self.0 += 1; item(self.0, Some(label), a, None) }).collect()
        }
        fn one(&mut self, label: Option<&str>, artist: &str, source: Option<&str>) -> HarvestItemOut {
            self.0 += 1;
            item(self.0, label, artist, source)
        }
    }
    fn ids(v: &[HarvestItemOut]) -> Vec<i64> {
        v.iter().map(|i| i.id).collect()
    }
    fn label(seg: &FeedSegment) -> &LabelGroup {
        match seg {
            FeedSegment::Label(g) => g,
            _ => panic!("expected a label group"),
        }
    }

    #[test]
    fn nests_repeating_artists_under_their_label() {
        let mut g = Gen(0);
        let list = g.on_label("Vibraphone", &["A", "A", "B", "C", "A", "", "B", "D"]);
        let segs = build_feed_segments(&list);
        assert_eq!(segs.len(), 1);
        let l = label(&segs[0]);
        assert_eq!(l.key, "label:vibraphone");
        assert_eq!(l.name, "Vibraphone");
        assert_eq!(ids(&l.items), ids(&list));
        assert_eq!(l.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["A", "B"]);
        assert_eq!(l.artists[0].key, "label:vibraphone/artist:a");
        assert_eq!(ids(&l.artists[0].items), vec![list[0].id, list[1].id, list[4].id]);
        assert_eq!(ids(&l.artists[1].items), vec![list[2].id, list[6].id]);
        assert_eq!(ids(&l.rest), vec![list[3].id, list[5].id, list[7].id]);
    }

    #[test]
    fn single_artist_label_has_no_sub_groups() {
        let mut g = Gen(0);
        let list = g.on_label("L", &["A", "A", "A", "A", "A"]);
        let segs = build_feed_segments(&list);
        assert!(label(&segs[0]).artists.is_empty());
        assert_eq!(ids(&label(&segs[0]).rest), ids(&list));
        let list = g.on_label("L", &["", "", "", "", ""]);
        let segs = build_feed_segments(&list);
        assert!(label(&segs[0]).artists.is_empty());
    }

    #[test]
    fn repeating_artist_stays_when_one_offs_sit_beside() {
        let mut g = Gen(0);
        let list = g.on_label("L", &["A", "A", "B", "C", ""]);
        let l = label(&build_feed_segments(&list)[0]).clone();
        assert_eq!(l.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["A"]);
        assert_eq!(ids(&l.rest), vec![list[2].id, list[3].id, list[4].id]);
    }

    #[test]
    fn artist_row_needs_two_releases() {
        let mut g = Gen(0);
        let list = g.on_label("L", &["A", "A", "B", "C", "D", "E"]);
        let l = label(&build_feed_segments(&list)[0]).clone();
        assert_eq!(l.artists.len(), 1);
        assert_eq!(l.rest.iter().map(|i| i.artist_name.as_str()).collect::<Vec<_>>(), ["B", "C", "D", "E"]);
    }

    #[test]
    fn falls_back_to_follow_name_then_artist() {
        let mut g = Gen(0);
        let mut list: Vec<_> = (0..5).map(|i| g.one(None, &format!("Q{i}"), Some("deep techno"))).collect();
        list.extend((0..5).map(|_| g.one(None, "Solo", None)));
        let segs = build_feed_segments(&list);
        let names: Vec<_> = segs.iter().map(|s| label(s).name.clone()).collect();
        assert_eq!(names, ["deep techno", "Solo"]);
        assert!(label(&segs[0]).artists.is_empty());
        assert!(label(&segs[1]).artists.is_empty());
    }

    #[test]
    fn nameless_items_stay_loose() {
        let mut g = Gen(0);
        let list: Vec<_> = (0..6).map(|_| g.one(None, "", None)).collect();
        let segs = build_feed_segments(&list);
        assert_eq!(segs.len(), 1);
        assert!(matches!(segs[0], FeedSegment::Loose(_)));
    }

    #[test]
    fn below_group_min_stays_loose() {
        let mut g = Gen(0);
        let list = g.on_label("L", &["A", "A", "A", "B"]);
        let segs = build_feed_segments(&list);
        assert_eq!(segs.len(), 1);
        assert!(matches!(segs[0], FeedSegment::Loose(_)));
    }

    #[test]
    fn orders_by_first_appearance_and_sits_where_first_item_was() {
        let mut g = Gen(0);
        let mut list = g.on_label("L", &["B"]);
        list.extend(g.on_label("M", &["X"]));
        list.extend(g.on_label("L", &["A", "B", "A", "A"]));
        list.extend(g.on_label("M", &["Y", "X", "Y", "X"]));
        let segs = build_feed_segments(&list);
        assert_eq!(segs.iter().map(|s| label(s).name.clone()).collect::<Vec<_>>(), ["L", "M"]);
        assert_eq!(label(&segs[0]).artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["B", "A"]);
        assert_eq!(label(&segs[1]).artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["X", "Y"]);

        let x = g.one(Some("X"), "x", None);
        let wall = g.on_label("L", &["A", "A", "B", "B", "C"]);
        let y = g.one(Some("Y"), "", None);
        let z = g.one(None, "z", None);
        let mut list = vec![x.clone()];
        list.extend(wall);
        list.push(y.clone());
        list.push(z.clone());
        let segs = build_feed_segments(&list);
        assert!(matches!(segs[0], FeedSegment::Loose(_)) && matches!(segs[1], FeedSegment::Label(_)) && matches!(segs[2], FeedSegment::Loose(_)));
        if let FeedSegment::Loose(v) = &segs[2] {
            assert_eq!(ids(v), vec![y.id, z.id]);
        }
    }

    #[test]
    fn partitions_a_label_exactly() {
        let mut g = Gen(0);
        let list = g.on_label("L", &["A", "", "B", "A", "C", "B", "D", "A", "", "E"]);
        let l = label(&build_feed_segments(&list)[0]).clone();
        assert_eq!(ids(&l.items), ids(&list));
        let mut all: Vec<i64> = l.artists.iter().flat_map(|a| ids(&a.items)).chain(ids(&l.rest)).collect();
        all.sort();
        assert_eq!(all, ids(&list));
    }

    #[test]
    fn merges_spellings_by_case() {
        let mut g = Gen(0);
        let mut list = g.on_label("Vibronics", &["dub", "DUB"]);
        list.extend(g.on_label("VIBRONICS", &["Dub", "x", "y"]));
        let segs = build_feed_segments(&list);
        assert_eq!(segs.len(), 1);
        let l = label(&segs[0]);
        assert_eq!(l.name, "Vibronics");
        assert_eq!(l.artists.len(), 1);
        assert_eq!(l.artists[0].name, "dub");
        assert_eq!(l.artists[0].items.len(), 3);
    }
}
