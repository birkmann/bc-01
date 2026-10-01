//! FTS5 maintenance for the track search index (`search_index`, app-maintained as in the
//! legacy app). Entity indexes (artist/label/release) are trigger-maintained in SQL.
//!
//! `reindex_tracks` is called from ingest, metadata write and tag mutation.

use rusqlite::{Connection, params_from_iter};

use crate::Result;

const CHUNK: usize = 400;

/// Free text -> safe FTS5 MATCH expression. Every term is quoted (internal quotes
/// doubled) and the last term gets a prefix `*` so search feels live as you type.
pub fn fts_escape(query: &str) -> String {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| t.replace('"', "\"\""))
        .filter(|t| !t.is_empty())
        .collect();
    if terms.is_empty() {
        return String::new();
    }
    let n = terms.len();
    terms
        .iter()
        .enumerate()
        .map(|(i, t)| if i + 1 == n { format!("\"{t}\"*") } else { format!("\"{t}\"") })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Trigram MATCH expression for an entity name search: the whole query as one
/// quoted phrase (substring semantics). `None` when shorter than 3 chars, where the
/// caller should fall back to `LIKE`.
pub fn trigram_phrase(query: &str) -> Option<String> {
    let q = query.trim();
    if q.chars().count() < 3 {
        return None;
    }
    Some(format!("\"{}\"", q.replace('"', "\"\"")))
}

/// Refresh FTS rows for `track_ids`. Safe to call repeatedly. Runs inside the caller's transaction.
pub fn reindex_tracks(c: &Connection, track_ids: &[i64]) -> Result<()> {
    for chunk in track_ids.chunks(CHUNK) {
        let ph = vec!["?"; chunk.len()].join(",");
        c.execute(
            &format!("DELETE FROM search_index WHERE track_id IN ({ph})"),
            params_from_iter(chunk.iter()),
        )?;
        c.execute(
            &format!(
                "INSERT INTO search_index (track_id, title, artist, album, label, tags)
                 SELECT t.id, t.title, COALESCE(ta.name, ra.name, ''), COALESCE(r.title, ''), COALESCE(l.name, ''),
                        COALESCE((SELECT group_concat(g.name, ' ') FROM track_tags tt JOIN tags g ON g.id = tt.tag_id WHERE tt.track_id = t.id), '')
                   FROM tracks t
                   LEFT JOIN artists  ta ON ta.id = t.artist_id
                   LEFT JOIN releases r  ON r.id  = t.release_id
                   LEFT JOIN artists  ra ON ra.id = r.artist_id
                   LEFT JOIN labels   l  ON l.id  = r.label_id
                  WHERE t.id IN ({ph})"
            ),
            params_from_iter(chunk.iter()),
        )?;
    }
    Ok(())
}

/// Drop FTS rows for deleted tracks (FTS5 has no foreign keys, so nothing cascades).
pub fn remove_tracks(c: &Connection, track_ids: &[i64]) -> Result<()> {
    for chunk in track_ids.chunks(CHUNK) {
        let ph = vec!["?"; chunk.len()].join(",");
        c.execute(
            &format!("DELETE FROM search_index WHERE track_id IN ({ph})"),
            params_from_iter(chunk.iter()),
        )?;
    }
    Ok(())
}

/// Rebuild the whole track index; returns the number of tracks indexed.
pub fn rebuild_all(c: &Connection) -> Result<usize> {
    c.execute("DELETE FROM search_index", [])?;
    let ids: Vec<i64> = {
        let mut st = c.prepare("SELECT id FROM tracks")?;
        st.query_map([], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?
    };
    reindex_tracks(c, &ids)?;
    Ok(ids.len())
}

/// (Re)build the opt-in trigram table over tracks.
pub fn rebuild_trigram(c: &Connection) -> Result<usize> {
    c.execute("DELETE FROM search_trigram", [])?;
    let n = c.execute(
        "INSERT INTO search_trigram(rowid, title, artist, album, label)
         SELECT t.id, t.title, COALESCE(ta.name, ra.name, ''), COALESCE(r.title, ''), COALESCE(l.name, '')
           FROM tracks t
           LEFT JOIN artists ta ON ta.id = t.artist_id
           LEFT JOIN releases r ON r.id = t.release_id
           LEFT JOIN artists ra ON ra.id = r.artist_id
           LEFT JOIN labels l ON l.id = r.label_id",
        [],
    )?;
    Ok(n)
}

/// bm25 column weights used for track search relevance (title, artist, album, label, tags).
pub const BM25_WEIGHTS: &str = "10.0, 6.0, 4.0, 1.0, 2.0";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape() {
        assert_eq!(fts_escape("dub  techno"), "\"dub\" \"techno\"*");
        assert_eq!(fts_escape("a\"b"), "\"a\"\"b\"*");
        assert_eq!(fts_escape("  "), "");
        assert_eq!(trigram_phrase("ab"), None);
        assert_eq!(trigram_phrase(" abc ").unwrap(), "\"abc\"");
    }
}
