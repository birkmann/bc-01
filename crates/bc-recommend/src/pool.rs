//! The working pool a DJ set plans from (port of `playlists/pool.py`).
//!
//! A pool is a JSON list of *sources* stored on the set (`dj_sets.pool_sources`): tags, the
//! loved shelf, playlists, labels, artists, or an explicit pick list. It resolves lazily to ONE
//! track-id `UNION` of per-source selects; a deleted playlist or label resolves to nothing.
//! The artist source is two arms (tracks by artist, tracks on the artist's releases) so no arm
//! is an `OR` across tables.

use bc_db::rusqlite::Connection;
use bc_types::sets::{MAX_EXPLICIT_TRACKS, PoolSource, PoolSourceCountOut, PoolSourceKind};

use crate::error::Result;
use crate::scope::{Scope, ScopeExt};
use crate::sqlutil::{PRESENT, in_list, lit, name_key};

/// Does the source carry the reference its kind needs (legacy `_ref_present`)?
fn valid(s: &PoolSource) -> bool {
    match s.kind {
        PoolSourceKind::Tag => s.tag.as_deref().is_some_and(|t| !t.is_empty()),
        PoolSourceKind::Playlist => s.playlist_id.is_some_and(|i| i != 0),
        PoolSourceKind::Label => s.label_id.is_some_and(|i| i != 0),
        PoolSourceKind::Artist => s.artist_id.is_some_and(|i| i != 0),
        PoolSourceKind::Tracks => !s.track_ids.is_empty() && s.track_ids.len() <= MAX_EXPLICIT_TRACKS,
        PoolSourceKind::Loved => s.track_ids.len() <= MAX_EXPLICIT_TRACKS,
    }
}

/// Tolerant read of the stored column: bad JSON or bad entries are skipped, never an error.
pub fn parse_sources(raw: Option<&str>) -> Vec<PoolSource> {
    let Some(raw) = raw.filter(|r| !r.is_empty()) else { return vec![] };
    let Ok(serde_json::Value::Array(entries)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return vec![];
    };
    entries
        .into_iter()
        .filter_map(|e| serde_json::from_value::<PoolSource>(e).ok())
        .filter(valid)
        .collect()
}

/// SQL `SELECT` of the track ids one source resolves to. Unknown refs resolve to nothing.
pub fn source_track_ids(s: &PoolSource) -> String {
    match s.kind {
        PoolSourceKind::Tag => format!(
            "SELECT tt.track_id FROM track_tags tt JOIN tags g ON g.id = tt.tag_id WHERE g.name_key = {}",
            lit(&name_key(s.tag.as_deref().unwrap_or("")))
        ),
        PoolSourceKind::Loved => "SELECT id FROM tracks WHERE loved = 1".into(),
        PoolSourceKind::Playlist => {
            format!("SELECT track_id FROM playlist_items WHERE playlist_id = {}", s.playlist_id.unwrap_or(-1))
        }
        PoolSourceKind::Label => format!(
            "SELECT id FROM tracks WHERE release_id IN (SELECT id FROM releases WHERE label_id = {})",
            s.label_id.unwrap_or(-1)
        ),
        PoolSourceKind::Artist => {
            let a = s.artist_id.unwrap_or(-1);
            format!(
                "SELECT id FROM tracks WHERE artist_id = {a} \
                 UNION SELECT id FROM tracks WHERE release_id IN (SELECT id FROM releases WHERE artist_id = {a})"
            )
        }
        PoolSourceKind::Tracks => format!("SELECT id FROM tracks WHERE id IN ({})", in_list(&s.track_ids)),
    }
}

/// The pool as one select of track ids -- `UNION` dedups for free.
pub fn pool_track_ids(sources: &[PoolSource]) -> String {
    if sources.is_empty() {
        return "SELECT id FROM tracks WHERE 0".into();
    }
    sources.iter().map(source_track_ids).collect::<Vec<_>>().join(" UNION ")
}

/// Human name for a chip: the frozen name when there is one, the raw ref as a last resort.
pub fn source_label(s: &PoolSource) -> String {
    match s.kind {
        PoolSourceKind::Tag => format!("tag: {}", s.tag.as_deref().unwrap_or("")),
        PoolSourceKind::Loved => "loved".into(),
        PoolSourceKind::Tracks => format!("{} picked tracks", s.track_ids.len()),
        kind => {
            let (k, id) = match kind {
                PoolSourceKind::Playlist => ("playlist", s.playlist_id),
                PoolSourceKind::Label => ("label", s.label_id),
                _ => ("artist", s.artist_id),
            };
            match s.name.as_deref().filter(|n| !n.is_empty()) {
                Some(n) => format!("{k}: {n}"),
                None => format!("{k}: #{}", id.unwrap_or(0)),
            }
        }
    }
}

pub fn kind_str(k: PoolSourceKind) -> &'static str {
    match k {
        PoolSourceKind::Tag => "tag",
        PoolSourceKind::Loved => "loved",
        PoolSourceKind::Playlist => "playlist",
        PoolSourceKind::Label => "label",
        PoolSourceKind::Artist => "artist",
        PoolSourceKind::Tracks => "tracks",
    }
}

/// Track count per source (pre-union, pre-exclusion), for the UI chips.
pub fn source_counts(conn: &Connection, sources: &[PoolSource], scope: &Scope) -> Result<Vec<i64>> {
    let mut counts = vec![];
    for s in sources {
        let sql = format!(
            "SELECT count(DISTINCT t.id) FROM tracks t WHERE t.id IN ({}) AND {PRESENT} AND {}",
            source_track_ids(s),
            scope.tp()
        );
        counts.push(conn.query_row(&sql, [], |r| r.get::<_, i64>(0))?);
    }
    Ok(counts)
}

/// Chips with their counts.
pub fn source_chips(conn: &Connection, sources: &[PoolSource], scope: &Scope) -> Result<Vec<PoolSourceCountOut>> {
    let counts = source_counts(conn, sources, scope)?;
    Ok(sources
        .iter()
        .zip(counts)
        .enumerate()
        .map(|(index, (s, track_count))| PoolSourceCountOut { index, kind: kind_str(s.kind).into(), label: source_label(s), track_count })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_is_tolerant() {
        assert!(parse_sources(None).is_empty());
        assert!(parse_sources(Some("")).is_empty());
        assert!(parse_sources(Some("not json")).is_empty());
        assert!(parse_sources(Some(r#"{"kind":"loved"}"#)).is_empty());
        let got = parse_sources(Some(
            r#"[{"kind":"loved"},{"kind":"tag"},{"kind":"nope"},{"kind":"tag","tag":"techno"},{"kind":"tracks","track_ids":[]},{"kind":"playlist","playlist_id":0},7]"#,
        ));
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].tag.as_deref(), Some("techno"));
    }

    #[test]
    fn too_many_explicit_tracks_is_skipped() {
        let ids: Vec<String> = (1..=1001).map(|i| i.to_string()).collect();
        let raw = format!(r#"[{{"kind":"tracks","track_ids":[{}]}}]"#, ids.join(","));
        assert!(parse_sources(Some(&raw)).is_empty());
    }

    #[test]
    fn labels() {
        let s = |json: &str| serde_json::from_str::<PoolSource>(json).unwrap();
        assert_eq!(source_label(&s(r#"{"kind":"tag","tag":"techno"}"#)), "tag: techno");
        assert_eq!(source_label(&s(r#"{"kind":"loved"}"#)), "loved");
        assert_eq!(source_label(&s(r#"{"kind":"tracks","track_ids":[1,2]}"#)), "2 picked tracks");
        assert_eq!(source_label(&s(r#"{"kind":"playlist","playlist_id":3,"name":"Warm"}"#)), "playlist: Warm");
        assert_eq!(source_label(&s(r#"{"kind":"label","label_id":4}"#)), "label: #4");
    }

    #[test]
    fn the_pool_is_one_union() {
        let a: PoolSource = serde_json::from_str(r#"{"kind":"loved"}"#).unwrap();
        let b: PoolSource = serde_json::from_str(r#"{"kind":"artist","artist_id":5}"#).unwrap();
        let sql = pool_track_ids(&[a, b]);
        assert_eq!(sql.matches("UNION").count(), 2);
        assert!(!sql.contains(" OR "));
        assert_eq!(pool_track_ids(&[]), "SELECT id FROM tracks WHERE 0");
    }
}
