//! Pure bits of the Fans page: the list/state filters, the player cursor the server continues from
//! (the legacy `fanQueue.ts` cursor logic lives in the server's `fan_next`), counts per tab.
use std::collections::BTreeMap;

use bc_types::bandcamp::FanOut;
use bc_types::player::{FanCursor, FanOrder, FanTab};

/// Their wishlist, their collection, or both as one list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListTab {
    Wishlist,
    Collection,
    All,
}

impl ListTab {
    pub const ALL: [ListTab; 3] = [ListTab::Wishlist, ListTab::Collection, ListTab::All];
    pub fn label(self) -> &'static str {
        match self {
            ListTab::Wishlist => "Wishlist",
            ListTab::Collection => "Collection",
            ListTab::All => "Both",
        }
    }
    /// The `tab` value of the REST routes.
    pub fn param(self) -> &'static str {
        match self {
            ListTab::Wishlist => "wishlist",
            ListTab::Collection => "collection",
            ListTab::All => "all",
        }
    }
    pub fn tab(self) -> Option<FanTab> {
        match self {
            ListTab::Wishlist => Some(FanTab::Wishlist),
            ListTab::Collection => Some(FanTab::Collection),
            ListTab::All => None,
        }
    }
    pub fn from_param(s: &str) -> Option<ListTab> {
        Some(match s {
            "wishlist" => ListTab::Wishlist,
            "collection" => ListTab::Collection,
            "all" => ListTab::All,
            _ => return None,
        })
    }
}

pub const STATE_TABS: [(&str, &str); 5] = [("new", "New"), ("queued", "Queued"), ("in_library", "In library"), ("ignored", "Ignored"), ("all", "All")];

/// My own fan opens on the wishlist (the backfill it always was); someone else's on both lists.
pub fn default_list(fan: &FanOut) -> ListTab {
    if fan.is_self { ListTab::Wishlist } else { ListTab::All }
}
pub fn default_state(fan: &FanOut) -> &'static str {
    if fan.is_self { "new" } else { "all" }
}

pub fn fan_name(fan: &FanOut) -> String {
    fan.display_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| fan.username.clone())
}

/// Item counts by inbox state for the chosen list.
pub fn counts_for(fan: &FanOut, list: ListTab) -> BTreeMap<String, i64> {
    match list {
        ListTab::All => fan.counts.clone(),
        l => fan.tabs.get(l.param()).map(|t| t.counts.clone()).unwrap_or_default(),
    }
}

pub fn count_for_state(counts: &BTreeMap<String, i64>, state: &str) -> i64 {
    if state == "all" { counts.values().sum() } else { counts.get(state).copied().unwrap_or(0) }
}

pub fn list_items(fan: &FanOut, list: ListTab) -> i64 {
    match list {
        ListTab::All => fan.items,
        l => fan.tabs.get(l.param()).map(|t| t.items).unwrap_or(0),
    }
}

/// States the listing is filtered to (`all` = no filter on the cursor).
pub fn cursor_states(state: &str) -> Vec<String> {
    if state == "all" { vec![] } else { vec![state.to_string()] }
}

/// The cursor a `StartSource` continues from. `after` is the item played last (`None` = from the start).
pub fn cursor(fan: &FanOut, list: ListTab, state: &str, order: FanOrder, seed: i64, after: Option<i64>) -> FanCursor {
    FanCursor {
        fan_id: fan.id,
        item_id: after,
        order,
        seed: if order == FanOrder::Shuffle { seed } else { 0 },
        states: cursor_states(state),
        tab: list.tab(),
        fan_name: fan_name(fan),
        shelf: fan.shelf.clone(),
    }
}

/// A fresh shuffle order: the seed the server's fixed order is keyed by.
pub fn new_seed(entropy: u64) -> i64 {
    (entropy % (1 << 31)) as i64
}

/// What a walk of the list on screen asks for: the list on screen, or both.
pub fn walk_tabs(list: ListTab) -> Option<Vec<String>> {
    match list {
        ListTab::All => None,
        l => Some(vec![l.param().to_string()]),
    }
}

/// Where the first card of a play-from-here starts: the item before it in list order.
pub fn play_from_offset(index: usize) -> Option<usize> {
    index.checked_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_types::bandcamp::FanTabOut;

    fn fan(is_self: bool) -> FanOut {
        let mut f = FanOut { id: 7, username: "alice".into(), shelf: "fans/alice".into(), is_self, items: 10, ..Default::default() };
        f.counts.insert("new".into(), 6);
        f.counts.insert("queued".into(), 4);
        f.tabs.insert("wishlist".into(), FanTabOut { items: 8, counts: [("new".to_string(), 5)].into(), reported: None });
        f
    }

    #[test]
    fn defaults_follow_ownership() {
        assert_eq!((default_list(&fan(true)), default_state(&fan(true))), (ListTab::Wishlist, "new"));
        assert_eq!((default_list(&fan(false)), default_state(&fan(false))), (ListTab::All, "all"));
        let mut f = fan(false);
        assert_eq!(fan_name(&f), "alice");
        f.display_name = Some("Alice B".into());
        assert_eq!(fan_name(&f), "Alice B");
    }

    #[test]
    fn counts_per_list_and_state() {
        let f = fan(false);
        assert_eq!(count_for_state(&counts_for(&f, ListTab::All), "all"), 10);
        assert_eq!(count_for_state(&counts_for(&f, ListTab::All), "queued"), 4);
        assert_eq!(count_for_state(&counts_for(&f, ListTab::Wishlist), "new"), 5);
        assert_eq!(count_for_state(&counts_for(&f, ListTab::Collection), "new"), 0);
        assert_eq!(list_items(&f, ListTab::Wishlist), 8);
        assert_eq!(list_items(&f, ListTab::All), 10);
    }

    #[test]
    fn cursor_matches_the_legacy_shape() {
        let f = fan(false);
        let c = cursor(&f, ListTab::Wishlist, "new", FanOrder::Shuffle, 42, None);
        assert_eq!((c.fan_id, c.item_id, c.seed, c.tab), (7, None, 42, Some(FanTab::Wishlist)));
        assert_eq!(c.states, vec!["new".to_string()]);
        let c = cursor(&f, ListTab::All, "all", FanOrder::Seq, 42, Some(3));
        assert_eq!((c.seed, c.item_id, c.tab), (0, Some(3), None));
        assert!(c.states.is_empty());
        assert_eq!(c.shelf, "fans/alice");
    }

    #[test]
    fn list_tab_round_trip() {
        for t in ListTab::ALL {
            assert_eq!(ListTab::from_param(t.param()), Some(t));
        }
        assert_eq!(walk_tabs(ListTab::All), None);
        assert_eq!(walk_tabs(ListTab::Collection), Some(vec!["collection".to_string()]));
        assert_eq!(play_from_offset(0), None);
        assert_eq!(play_from_offset(5), Some(4));
        assert!(new_seed(u64::MAX) < (1 << 31));
    }
}
