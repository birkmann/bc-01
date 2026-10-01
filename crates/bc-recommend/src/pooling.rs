//! Shared candidate-pool plumbing (port of `recommend/pooling.py`).
//!
//! * A stable pseudo-random order: `(id * 2654435761 + seed) % 2^32` (see
//!   [`bc_music::shuffle`]) so a reroll is free and a page is stable.
//! * A tag budget: walking a seed's tags rarest-first and stopping at a cumulative track count
//!   keeps the pool an index seek instead of a table scan.
//! * One chunked read of everyone's tags.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bc_db::rusqlite::Connection;

use crate::error::Result;
use crate::sqlutil::{IN_CHUNK, in_list, lit, name_key};

/// Cumulative `track_count` budget for the tags a pool is drawn from.
pub const TAG_BUDGET: i64 = 25_000;

/// Id-chunk size for row reads.
pub const IN_CHUNK_ROWS: usize = IN_CHUNK;

/// `ORDER BY` fragment giving a stable per-seed order over `t.id`.
pub fn shuffled(seed: i64) -> String {
    bc_music::shuffle::sql_order("t.id", seed)
}

/// Tag names per track, plus each tag's library count (casefolded name -> count) for IDF.
/// Tag names per track and library count per casefolded tag name.
pub type TagsByTrack = (HashMap<i64, BTreeSet<String>>, HashMap<String, i64>);

pub fn tags_by_track(conn: &Connection, ids: &[i64]) -> Result<TagsByTrack> {
    let mut tags: HashMap<i64, BTreeSet<String>> = HashMap::new();
    let mut counts: HashMap<String, i64> = HashMap::new();
    for chunk in ids.chunks(IN_CHUNK) {
        let sql = format!(
            "SELECT tt.track_id, g.name, g.track_count FROM track_tags tt JOIN tags g ON g.id = tt.tag_id WHERE tt.track_id IN ({})",
            in_list(chunk)
        );
        let mut st = conn.prepare(&sql)?;
        let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?)))?;
        for row in rows {
            let (tid, name, count) = row?;
            counts.entry(name.to_lowercase()).or_insert(count.unwrap_or(0));
            tags.entry(tid).or_default().insert(name);
        }
    }
    Ok((tags, counts))
}

/// Total number of tracks (for IDF).
pub fn total_tracks(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT count(*) FROM tracks", [], |r| r.get(0))?)
}

/// `(id, name, track_count)` of a tag row.
pub type TagRow = (i64, String, i64);

/// The named tags as `(id, name, track_count)`, rarest first.
pub fn tag_rows<'a>(conn: &Connection, tags: impl IntoIterator<Item = &'a String>) -> Result<Vec<TagRow>> {
    let keys: BTreeSet<String> = tags.into_iter().filter(|t| !t.trim().is_empty()).map(|t| name_key(t)).collect();
    if keys.is_empty() {
        return Ok(vec![]);
    }
    let list = keys.iter().map(|k| lit(k)).collect::<Vec<_>>().join(",");
    let sql = format!("SELECT id, name, track_count FROM tags WHERE name_key IN ({list}) ORDER BY track_count, id");
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0))))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Tag rows by id, rarest first (used by the timing example).
pub fn tag_rows_by_ids(conn: &Connection, ids: &[i64]) -> Result<Vec<TagRow>> {
    let sql = format!("SELECT id, name, track_count FROM tags WHERE id IN ({}) ORDER BY track_count, id", in_list(ids));
    let mut st = conn.prepare(&sql)?;
    let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?.unwrap_or(0))))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// The rarest of `rows`, up to a cumulative `budget` of tracks. The rarest is always taken even
/// when it alone blows the budget: a track whose only tag is `Electronic` must still get a pool.
pub fn drawing_tags(rows: &[TagRow], budget: i64) -> Vec<TagRow> {
    let mut out: Vec<TagRow> = vec![];
    let mut total = 0;
    for row in rows {
        if !out.is_empty() && total + row.2 > budget {
            break;
        }
        out.push(row.clone());
        total += row.2;
    }
    out
}

/// Group helper used by tests.
#[allow(dead_code)]
pub(crate) fn sorted_tags(m: &HashMap<i64, BTreeSet<String>>) -> BTreeMap<i64, Vec<String>> {
    m.iter().map(|(k, v)| (*k, v.iter().cloned().collect())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tag_budget_leaves_out_the_tag_that_says_nothing() {
        // At real-library scale: `Electronic` sits on 136,524 of 169,684 tracks, so asking the
        // pool query for it would match most of the table. The budget walks rarest-first and
        // stops before it.
        let mut seed_tags: Vec<TagRow> = vec![
            (1, "Electronic".into(), 136_524),
            (2, "Techno".into(), 115_585),
            (3, "Berlin".into(), 21_643),
            (4, "hypnotic techno".into(), 11_748),
            (5, "dub techno".into(), 11_972),
            (6, "Tools".into(), 175),
        ];
        seed_tags.sort_by_key(|r| r.2);
        let drawn: Vec<String> = drawing_tags(&seed_tags, TAG_BUDGET).into_iter().map(|r| r.1).collect();
        assert_eq!(drawn, ["Tools", "hypnotic techno", "dub techno"]);
    }

    #[test]
    fn the_rarest_tag_is_drawn_even_when_it_alone_blows_the_budget() {
        let drawn = drawing_tags(&[(1, "Electronic".into(), 136_524)], TAG_BUDGET);
        assert_eq!(drawn.into_iter().map(|r| r.1).collect::<Vec<_>>(), ["Electronic"]);
    }

    #[test]
    fn shuffled_matches_the_hash() {
        assert_eq!(shuffled(3), "((t.id * 2654435761 + 3) % 4294967296)");
    }
}
