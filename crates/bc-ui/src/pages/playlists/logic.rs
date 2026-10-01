//! Pure logic of the playlists page: the scratch pool (a playlist that is not one yet),
//! free-name picking, a seeded shuffle and cover selection. Ported from
//! `lib/scratchPool.ts` and `Playlists.tsx`; tested natively.
use std::collections::{HashMap, HashSet};

use bc_types::library::TrackOut;
use serde::{Deserialize, Serialize};

/// Where loose tracks land, so they are one removable chip like the rest.
pub const PICKED: &str = "picked";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScratchSource {
    /// `playlist:12`, or `picked` for tracks dragged in one at a time.
    pub key: String,
    pub label: String,
    pub track_ids: Vec<i64>,
}

/// Sources plus a track index (so removing a playlist keeps the tracks it shares with the others).
/// Persisted server-side under ui_state `scratch-pool`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ScratchPool {
    pub sources: Vec<ScratchSource>,
    pub tracks: HashMap<String, TrackOut>,
}

impl ScratchPool {
    fn prune(&mut self) {
        let keep: HashSet<String> = self.sources.iter().flat_map(|s| s.track_ids.iter().map(|i| i.to_string())).collect();
        self.tracks.retain(|k, _| keep.contains(k));
    }

    /// Add (or replace) a whole source: dropping the same playlist twice is the same pool.
    pub fn add_source(&mut self, key: &str, label: &str, tracks: &[TrackOut]) {
        let src = ScratchSource { key: key.into(), label: label.into(), track_ids: tracks.iter().map(|t| t.id).collect() };
        match self.sources.iter().position(|s| s.key == key) {
            Some(i) => self.sources[i] = src,
            None => self.sources.push(src),
        }
        for t in tracks {
            self.tracks.insert(t.id.to_string(), t.clone());
        }
        self.prune();
    }

    /// Append loose tracks to the picked chip (deduped).
    pub fn add_tracks(&mut self, tracks: &[TrackOut]) {
        let pos = self.sources.iter().position(|s| s.key == PICKED);
        let mut ids: Vec<i64> = pos.map(|i| self.sources[i].track_ids.clone()).unwrap_or_default();
        let mut seen: HashSet<i64> = ids.iter().copied().collect();
        for t in tracks {
            if seen.insert(t.id) {
                ids.push(t.id);
            }
            self.tracks.insert(t.id.to_string(), t.clone());
        }
        let src = ScratchSource { key: PICKED.into(), label: "Picked tracks".into(), track_ids: ids };
        match pos {
            Some(i) => self.sources[i] = src,
            None => self.sources.push(src),
        }
    }

    pub fn remove_source(&mut self, key: &str) {
        self.sources.retain(|s| s.key != key);
        self.prune();
    }

    pub fn clear(&mut self) {
        self.sources.clear();
        self.tracks.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Every source's tracks, deduped, in the order they first arrived.
    pub fn tracks_in_order(&self) -> Vec<TrackOut> {
        let mut seen = HashSet::new();
        let mut out = vec![];
        for s in &self.sources {
            for id in &s.track_ids {
                if !seen.insert(*id) {
                    continue;
                }
                if let Some(t) = self.tracks.get(&id.to_string()) {
                    out.push(t.clone());
                }
            }
        }
        out
    }

    /// Cheap change fingerprint (to clear a stale "saved as X" link).
    pub fn shape(&self) -> String {
        self.sources.iter().map(|s| format!("{}/{}", s.key, s.track_ids.len())).collect::<Vec<_>>().join("|")
    }
}

/// `base`, or `base 2`, `base 3`... whichever no playlist is called yet.
pub fn free_name(taken: &[String], base: &str) -> String {
    let names: HashSet<&str> = taken.iter().map(|s| s.as_str()).collect();
    if !names.contains(base) {
        return base.to_string();
    }
    let mut n = 2;
    while names.contains(format!("{base} {n}").as_str()) {
        n += 1;
    }
    format!("{base} {n}")
}

/// xorshift Fisher-Yates; deterministic for a seed.
pub fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    for i in (1..items.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

/// One cover per release, so a playlist of one album is one cover rather than the same square four times.
pub fn collage_urls(tracks: &[TrackOut], max: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = vec![];
    for t in tracks {
        let Some(url) = t.art_url.clone().filter(|u| !u.is_empty()) else { continue };
        let key = t.release.as_ref().map(|r| r.id).unwrap_or(t.id);
        if seen.insert(key) {
            out.push(url);
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: i64) -> TrackOut {
        TrackOut { id, title: format!("t{id}"), ..Default::default() }
    }

    #[test]
    fn sources_dedupe_and_keep_order() {
        let mut p = ScratchPool::default();
        p.add_source("playlist:1", "A", &[t(1), t(2), t(3)]);
        p.add_source("playlist:2", "B", &[t(3), t(4)]);
        let ids: Vec<i64> = p.tracks_in_order().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }

    #[test]
    fn same_playlist_twice_replaces() {
        let mut p = ScratchPool::default();
        p.add_source("playlist:1", "A", &[t(1), t(2)]);
        p.add_source("playlist:1", "A", &[t(1), t(2), t(5)]);
        assert_eq!(p.sources.len(), 1);
        assert_eq!(p.tracks_in_order().len(), 3);
    }

    #[test]
    fn removing_a_source_keeps_shared_tracks() {
        let mut p = ScratchPool::default();
        p.add_source("playlist:1", "A", &[t(1), t(2)]);
        p.add_source("playlist:2", "B", &[t(2), t(3)]);
        p.remove_source("playlist:1");
        let ids: Vec<i64> = p.tracks_in_order().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![2, 3]);
        assert!(!p.tracks.contains_key("1"));
    }

    #[test]
    fn picked_tracks_merge_without_duplicates() {
        let mut p = ScratchPool::default();
        p.add_tracks(&[t(1), t(2)]);
        p.add_tracks(&[t(2), t(3)]);
        assert_eq!(p.sources.len(), 1);
        assert_eq!(p.sources[0].track_ids, vec![1, 2, 3]);
        assert_eq!(p.sources[0].label, "Picked tracks");
    }

    #[test]
    fn free_names() {
        let taken = vec!["Pool".to_string(), "Pool 2".to_string()];
        assert_eq!(free_name(&taken, "Pool"), "Pool 3");
        assert_eq!(free_name(&[], "Pool"), "Pool");
        assert_eq!(free_name(&taken, "Mix"), "Mix");
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut v: Vec<i32> = (0..50).collect();
        shuffle(&mut v, 7);
        let mut s = v.clone();
        s.sort();
        assert_eq!(s, (0..50).collect::<Vec<_>>());
        assert_ne!(v, (0..50).collect::<Vec<_>>());
    }

    #[test]
    fn one_cover_per_release() {
        use bc_types::library::ReleaseRef;
        let mk = |id, rel, url: &str| TrackOut { id, art_url: Some(url.into()), release: Some(ReleaseRef { id: rel, ..Default::default() }), ..Default::default() };
        let tr = vec![mk(1, 10, "a"), mk(2, 10, "a"), mk(3, 11, "b")];
        assert_eq!(collage_urls(&tr, 4), vec!["a".to_string(), "b".to_string()]);
    }
}
