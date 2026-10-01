//! Bookkeeping of the keyed resource cache: which keys exist, which are stale and
//! how invalidation reaches them. Pure so the rules are testable.
//!
//! Rules (port of the legacy `homeQuery.test.ts` plus targeted invalidation):
//! * `invalidate_entity("track", ids)` marks entries tagged `track` (and, when
//!   `ids` is non-empty, entries tagged `track:<id>`; list entries tagged only
//!   `track` are always hit, since any id may appear in them).
//! * A blanket `invalidate_all()` or entity invalidation leaves *sparing* keys
//!   (the Home snapshot shelves, prefix `home`) alone; only naming the sparing
//!   prefix reaches them.
use std::collections::HashMap;

pub const HOME_PREFIX: &str = "home";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    pub tags: Vec<String>,
    pub stale: bool,
    pub sparing: bool,
}

#[derive(Debug, Default)]
pub struct KeyIndex {
    entries: HashMap<String, Meta>,
}

pub fn is_sparing(key: &str) -> bool {
    key == HOME_PREFIX || key.starts_with("home:")
}

impl KeyIndex {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn insert(&mut self, key: &str, tags: Vec<String>) {
        self.entries.insert(key.to_string(), Meta { tags, stale: false, sparing: is_sparing(key) });
    }
    pub fn mark_fresh(&mut self, key: &str) {
        if let Some(m) = self.entries.get_mut(key) {
            m.stale = false;
        }
    }
    pub fn is_stale(&self, key: &str) -> bool {
        self.entries.get(key).map(|m| m.stale).unwrap_or(true)
    }
    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }
    pub fn remove(&mut self, key: &str) {
        self.entries.remove(key);
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn mark(&mut self, mut pred: impl FnMut(&str, &Meta) -> bool) -> Vec<String> {
        let mut hit = vec![];
        for (k, m) in self.entries.iter_mut() {
            if pred(k, m) {
                m.stale = true;
                hit.push(k.clone());
            }
        }
        hit.sort();
        hit
    }

    /// Everything except the sparing keys.
    pub fn invalidate_all(&mut self) -> Vec<String> {
        self.mark(|_, m| !m.sparing)
    }

    /// Targeted: entries tagged with the entity (and the specific ids when given).
    pub fn invalidate_entity(&mut self, entity: &str, ids: &[i64]) -> Vec<String> {
        let id_tags: Vec<String> = ids.iter().map(|i| format!("{entity}:{i}")).collect();
        self.mark(|_, m| {
            !m.sparing
                && m.tags.iter().any(|t| t == entity || (!id_tags.is_empty() && id_tags.contains(t)))
        })
    }

    /// Entries whose key starts with the prefix; naming the Home prefix reaches Home.
    pub fn invalidate_prefix(&mut self, prefix: &str) -> Vec<String> {
        self.mark(|k, _| k.starts_with(prefix))
    }

    /// Drop entries nobody observes (`observed` says which keys are in use), oldest-agnostic;
    /// keeps memory flat.
    pub fn evict_unobserved(&mut self, observed: impl Fn(&str) -> bool, keep: usize) -> Vec<String> {
        if self.entries.len() <= keep {
            return vec![];
        }
        let mut victims: Vec<String> = self.entries.keys().filter(|k| !observed(k)).cloned().collect();
        victims.sort();
        let excess = self.entries.len() - keep;
        victims.truncate(excess);
        for v in &victims {
            self.entries.remove(v);
        }
        victims
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> KeyIndex {
        let mut i = KeyIndex::new();
        i.insert("home:releases-added", vec![]);
        i.insert("home:dust-off:0", vec![]);
        i.insert("tracks:q=a", vec!["track".into()]);
        i.insert("track:7", vec!["track:7".into()]);
        i.insert("stats", vec!["stats".into()]);
        i.insert("albums:q=x", vec!["release".into()]);
        i
    }

    #[test]
    fn blanket_invalidation_leaves_the_home_shelves_alone() {
        let mut i = idx();
        i.invalidate_all();
        assert!(!i.is_stale("home:releases-added"));
        assert!(!i.is_stale("home:dust-off:0"));
        assert!(i.is_stale("tracks:q=a"));
        assert!(i.is_stale("stats"));
    }

    #[test]
    fn targeted_invalidation_hits_only_tagged_entries() {
        let mut i = idx();
        let hit = i.invalidate_entity("track", &[7]);
        assert_eq!(hit, ["track:7", "tracks:q=a"]);
        assert!(!i.is_stale("stats"));
        assert!(!i.is_stale("albums:q=x"));
        assert!(!i.is_stale("home:releases-added"));
    }

    #[test]
    fn other_ids_do_not_hit_an_id_entry_but_do_hit_lists() {
        let mut i = idx();
        let hit = i.invalidate_entity("track", &[99]);
        assert_eq!(hit, ["tracks:q=a"]);
    }

    #[test]
    fn empty_ids_means_every_entity_of_the_kind() {
        let mut i = idx();
        assert!(i.invalidate_entity("release", &[]).contains(&"albums:q=x".to_string()));
    }

    #[test]
    fn naming_the_home_prefix_reaches_the_shelves() {
        let mut i = idx();
        i.invalidate_prefix("home");
        assert!(i.is_stale("home:releases-added"));
        assert!(i.is_stale("home:dust-off:0"));
        assert!(!i.is_stale("tracks:q=a"));
    }

    #[test]
    fn eviction_spares_observed_keys() {
        let mut i = idx();
        let gone = i.evict_unobserved(|k| k == "stats", 2);
        assert_eq!(gone.len(), 4);
        assert!(i.contains("stats"));
    }
}
