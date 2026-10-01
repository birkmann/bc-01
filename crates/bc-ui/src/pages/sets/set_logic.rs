//! Pure logic of the DJ-set pages (ported from the legacy vitest cases).
use bc_types::sets::{MAX_EXPLICIT_TRACKS, PoolSource, PoolSourceKind};

/// Merge picked ids into the set's single `tracks` source (deduped, capped at 1000).
pub fn with_picked_tracks(sources: &[PoolSource], ids: &[i64]) -> Vec<PoolSource> {
    let mut rest: Vec<PoolSource> = sources.iter().filter(|s| s.kind != PoolSourceKind::Tracks).cloned().collect();
    let mut merged: Vec<i64> = vec![];
    for s in sources.iter().filter(|s| s.kind == PoolSourceKind::Tracks) {
        for id in &s.track_ids {
            if !merged.contains(id) {
                merged.push(*id);
            }
        }
    }
    for id in ids {
        if !merged.contains(id) {
            merged.push(*id);
        }
    }
    merged.truncate(MAX_EXPLICIT_TRACKS);
    rest.push(PoolSource { kind: PoolSourceKind::Tracks, tag: None, playlist_id: None, label_id: None, artist_id: None, track_ids: merged, name: None });
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picked_tracks_merge_into_one_source() {
        let tag = PoolSource { kind: PoolSourceKind::Tag, tag: Some("house".into()), playlist_id: None, label_id: None, artist_id: None, track_ids: vec![], name: None };
        let existing = PoolSource { kind: PoolSourceKind::Tracks, tag: None, playlist_id: None, label_id: None, artist_id: None, track_ids: vec![1, 2], name: None };
        let out = with_picked_tracks(&[existing, tag.clone()], &[2, 3]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], tag);
        assert_eq!(out[1].track_ids, vec![1, 2, 3]);
    }

    #[test]
    fn picked_tracks_capped() {
        let ids: Vec<i64> = (0..1500).collect();
        let out = with_picked_tracks(&[], &ids);
        assert_eq!(out[0].track_ids.len(), MAX_EXPLICIT_TRACKS);
    }
}
