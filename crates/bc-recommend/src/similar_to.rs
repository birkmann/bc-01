//! Another playlist like this one: the same size, the same kind of music (port of
//! `playlists/similar_to.py`).
//!
//! The source playlist is boiled down to a taste profile (the loved shelf's profile builder) and
//! the library is scored against it, minus everything the source already holds. Same hot-path
//! SQL rules as `similar` (PLAN 9l): correlated `EXISTS`, per-table statements instead of `OR`.

use std::collections::{BTreeSet, HashMap, HashSet};

use bc_db::rusqlite::Connection;

use crate::error::Result;
use crate::nextup::idf;
use crate::pooling::{self, IN_CHUNK_ROWS};
use crate::scope::{Scope, ScopeExt};
use crate::sqlutil::{PRESENT, in_list};
use crate::taste::{self, Candidate, LovedRow, Profile};

pub const POOL_BY_TAGS: usize = 2000;
pub const POOL_BY_ARTIST: usize = 600;
pub const POOL_BY_LABEL: usize = 800;
pub const POOL_PER_ROW: usize = 10;
pub const POOL_MAX: usize = 30_000;
/// How many of the source's artists and labels the pool is drawn from, most frequent first.
pub const MAX_PEOPLE: usize = 40;
pub const POOL_TAGS: usize = 30;

type Row = (i64, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<f64>, Option<f64>, String);

const ROW_COLS: &str = "t.id, t.artist_id, t.release_id, r.artist_id, r.label_id, t.play_count, a.bpm, a.energy, t.title_key";

fn map_row(r: &bc_db::rusqlite::Row<'_>) -> bc_db::rusqlite::Result<Row> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get::<_, Option<String>>(8)?.unwrap_or_default()))
}

fn rows_of(conn: &Connection, ids: &[i64]) -> Result<Vec<Row>> {
    let mut out = vec![];
    for chunk in ids.chunks(IN_CHUNK_ROWS) {
        let sql = format!(
            "SELECT {ROW_COLS} FROM tracks t LEFT JOIN releases r ON r.id = t.release_id LEFT JOIN analysis a ON a.track_id = t.id WHERE t.id IN ({})",
            in_list(chunk)
        );
        let mut st = conn.prepare(&sql)?;
        for row in st.query_map([], map_row)? {
            out.push(row?);
        }
    }
    Ok(out)
}

/// Summarise a playlist: tags, artists, labels, tempo, energy.
pub fn profile_of(conn: &Connection, track_ids: &[i64]) -> Result<Profile> {
    if track_ids.is_empty() {
        return Ok(taste::build_profile(&[], &HashMap::new()));
    }
    let rows = rows_of(conn, track_ids)?;
    let (tags, counts) = pooling::tags_by_track(conn, track_ids)?;
    let total = pooling::total_tracks(conn)?;
    let idf_of: HashMap<String, f64> = counts.iter().map(|(k, n)| (k.clone(), idf(*n, total))).collect();
    let loved: Vec<LovedRow> = rows
        .iter()
        .map(|(tid, t_artist, _rel, r_artist, label, _plays, bpm, energy, _title)| LovedRow {
            track_id: *tid,
            tags: tags.get(tid).cloned().unwrap_or_default(),
            artist_id: t_artist.or(*r_artist),
            label_id: *label,
            bpm: *bpm,
            energy: *energy,
        })
        .collect();
    Ok(taste::build_profile(&loved, &idf_of))
}

fn pool_tag_ids(conn: &Connection, profile: &Profile) -> Result<Vec<i64>> {
    let mut wanted: Vec<(&String, &f64)> = profile.tag_weight.iter().collect();
    wanted.sort_by(|a, b| b.1.total_cmp(a.1).then(a.0.cmp(b.0)));
    let names: Vec<String> = wanted.into_iter().take(POOL_TAGS).map(|(k, _)| k.clone()).collect();
    if names.is_empty() {
        return Ok(vec![]);
    }
    let rows = pooling::tag_rows(conn, names.iter())?;
    Ok(pooling::drawing_tags(&rows, pooling::TAG_BUDGET).into_iter().map(|r| r.0).collect())
}

fn most_common(counts: &std::collections::BTreeMap<i64, i64>) -> Vec<i64> {
    let mut v: Vec<(i64, i64)> = counts.iter().map(|(k, n)| (*k, *n)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v.into_iter().take(MAX_PEOPLE).map(|(k, _)| k).collect()
}

/// The draw statements, exposed for `EXPLAIN QUERY PLAN` tests.
#[doc(hidden)]
pub fn draw_statements(profile: &Profile, tag_ids: &[i64], scope: &Scope, limit: usize, shuffle_seed: i64) -> Vec<(&'static str, String)> {
    let cap = |floor: usize| POOL_MAX.min(floor.max(limit * POOL_PER_ROW));
    let base = format!(
        "SELECT {ROW_COLS} FROM tracks t LEFT JOIN releases r ON r.id = t.release_id LEFT JOIN analysis a ON a.track_id = t.id \
         WHERE {PRESENT} AND {}",
        scope.tp()
    );
    let order = format!(" ORDER BY {}", pooling::shuffled(shuffle_seed));
    let mut out = vec![];
    if !tag_ids.is_empty() {
        out.push((
            "tags",
            format!("{base} AND t.id IN (SELECT tt.track_id FROM track_tags tt WHERE tt.tag_id IN ({})){order} LIMIT {}", in_list(tag_ids), cap(POOL_BY_TAGS)),
        ));
    }
    let artists = most_common(&profile.artist_count);
    if !artists.is_empty() {
        let l = in_list(&artists);
        out.push(("artist/tracks", format!("{base} AND t.artist_id IN ({l}){order} LIMIT {}", cap(POOL_BY_ARTIST))));
        out.push((
            "artist/releases",
            format!("{base} AND t.release_id IN (SELECT sr.id FROM releases sr WHERE sr.artist_id IN ({l})){order} LIMIT {}", cap(POOL_BY_ARTIST)),
        ));
    }
    let labels = most_common(&profile.label_count);
    if !labels.is_empty() {
        out.push((
            "label",
            format!("{base} AND t.release_id IN (SELECT sr.id FROM releases sr WHERE sr.label_id IN ({})){order} LIMIT {}", in_list(&labels), cap(POOL_BY_LABEL)),
        ));
    }
    out
}

/// Track ids for a playlist like the one `profile` describes, best first. `exclude` is filtered
/// in Rust (the source can be longer than any bind limit and the pool is a few thousand rows).
pub fn draw(conn: &Connection, scope: &Scope, profile: &Profile, exclude: &HashSet<i64>, limit: usize, shuffle_seed: i64) -> Result<Vec<i64>> {
    if profile.count == 0 || limit == 0 {
        return Ok(vec![]);
    }
    // Artist + normalised title: one recording filed twice has two ids, so "none of these ids"
    // is not "none of these tracks".
    let ex: Vec<i64> = exclude.iter().copied().collect();
    let mut seen: HashSet<(Option<i64>, String)> =
        rows_of(conn, &ex)?.into_iter().map(|(_, ta, _, ra, _, _, _, _, title)| (ta.or(ra), title)).collect();

    let tag_ids = pool_tag_ids(conn, profile)?;
    let mut pool: Vec<Row> = vec![];
    let mut in_pool: HashSet<i64> = HashSet::new();
    for (_, sql) in draw_statements(profile, &tag_ids, scope, limit, shuffle_seed.max(0)) {
        let mut st = conn.prepare(&sql)?;
        for row in st.query_map([], map_row)? {
            let row = row?;
            if exclude.contains(&row.0) {
                continue;
            }
            let sig = (row.1.or(row.3), row.8.clone());
            if seen.contains(&sig) {
                continue;
            }
            seen.insert(sig);
            if in_pool.insert(row.0) {
                pool.push(row);
            }
        }
    }
    if pool.is_empty() {
        return Ok(vec![]);
    }

    let ids: Vec<i64> = pool.iter().map(|r| r.0).collect();
    let (cand_tags, _) = pooling::tags_by_track(conn, &ids)?;
    let cands: Vec<Candidate> = pool
        .iter()
        .map(|(tid, t_artist, release_id, r_artist, label, plays, bpm, energy, _)| Candidate {
            track_id: *tid,
            tags: cand_tags.get(tid).cloned().unwrap_or_default(),
            artist_id: t_artist.or(*r_artist),
            release_id: *release_id,
            label_id: *label,
            bpm: *bpm,
            energy: *energy,
            play_count: plays.unwrap_or(0),
        })
        .collect();
    Ok(taste::rank(cands.iter(), profile, limit).into_iter().map(|s| s.candidate.track_id).collect())
}

#[allow(dead_code)]
fn _unused(_: BTreeSet<i64>) {}
