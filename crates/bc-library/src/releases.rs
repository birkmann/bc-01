//! `/releases`: listing, ids, next, one, related shelves.

use std::collections::HashSet;

use bc_db::rusqlite::{Connection, params_from_iter, types::Value};
use bc_db::util::parse_bound;
use bc_libcore::{ApiError, ApiResult, Scope, hydrate};
use bc_types::library::{RelatedGroup, ReleaseOut, ReleaseQuery, ReleaseSort, ReleaseStub, SortDir};
use bc_types::Page;

use crate::sqlb::Sql;
use crate::tracks::{resolve_tag_ids, shuffle_expr};

/// How many of a release's tags blend into the "more like this" match, how many get a shelf of
/// their own, and how common a tag has to be library-wide before a shelf of it is worth showing.
pub const PROFILE_TAGS: usize = 5;
pub const TAG_SHELVES: usize = 5;
pub const MIN_TAG_SHELF: i64 = 10;

pub struct RelFilter {
    pub sql: Sql,
    pub join_artist: bool,
    pub join_have: bool,
    pub empty: bool,
}

fn bound(label: &str, s: &Option<String>) -> ApiResult<Option<String>> {
    match s.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) => parse_bound(v).map(Some).ok_or_else(|| ApiError::bad(format!("{label}: not a date: {v}"))),
    }
}

pub fn build_filter(c: &Connection, q: &ReleaseQuery, scope: &Scope, sort: ReleaseSort) -> ApiResult<RelFilter> {
    let mut s = Sql::new();
    let mut join_artist = sort == ReleaseSort::Artist;
    let mut empty = false;
    if let Some(p) = scope.release_pred("r") {
        s.cond(p);
    }
    if let Some(a) = q.artist_id {
        let p = s.bind(a);
        s.cond(format!("r.artist_id = {p}"));
    }
    if let Some(l) = q.label_id {
        let p = s.bind(l);
        s.cond(format!("r.label_id = {p}"));
    }
    if let Some(text) = q.q.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        match bc_db::fts::trigram_phrase(text) {
            Some(phrase) => {
                let p = s.bind(format!("{{title artist}} : {phrase}"));
                s.cond(format!("r.id IN (SELECT rowid FROM release_search WHERE release_search MATCH {p})"));
            }
            None => {
                join_artist = true;
                let p = s.bind(format!("%{}%", text.to_lowercase()));
                s.cond(format!("(lower(r.title) LIKE {p} OR lower(ra.name) LIKE {p})"));
            }
        }
    }
    if !q.tags.is_empty() {
        match resolve_tag_ids(c, &q.tags)? {
            None => empty = true,
            Some(ids) => {
                for id in ids {
                    s.cond(format!(
                        "r.id IN (SELECT t.release_id FROM track_tags tt JOIN tracks t ON t.id = tt.track_id WHERE tt.tag_id = {id})"
                    ));
                }
            }
        }
    }
    if let Some(l) = q.loved {
        s.cond(format!("r.id IN (SELECT release_id FROM tracks WHERE loved = {} AND release_id IS NOT NULL)", l as i32));
    }
    if let Some(d) = bound("added_after", &q.added_after)? {
        let p = s.bind(d);
        s.cond(format!("r.added_at >= {p}"));
    }
    if let Some(d) = bound("added_before", &q.added_before)? {
        let p = s.bind(d);
        s.cond(format!("r.added_at < {p}"));
    }
    let join_have = q.missing == Some(true);
    if join_have {
        s.cond(hydrate::MISSING_COND);
    }
    Ok(RelFilter { sql: s, join_artist, join_have, empty })
}

fn from_clause(f: &RelFilter) -> String {
    let mut from = String::from("releases r");
    if f.join_artist {
        from.push_str(" LEFT JOIN artists ra ON ra.id = r.artist_id");
    }
    if f.join_have {
        from.push_str(
            " LEFT JOIN (SELECT release_id, COUNT(*) AS n, COALESCE(MAX(track_no), 0) AS top FROM tracks GROUP BY release_id) have ON have.release_id = r.id",
        );
    }
    from
}

fn order_sql(q: &ReleaseQuery, sort: ReleaseSort) -> String {
    let d = if q.order.unwrap_or(SortDir::Desc) == SortDir::Desc { "DESC" } else { "ASC" };
    // The id breaks ties so the order is total (stable paging and "what comes after this album").
    match sort {
        ReleaseSort::Added => format!("r.added_at {d}, r.id ASC"),
        ReleaseSort::Title => format!("r.title_key {d}, r.id ASC"),
        ReleaseSort::Year => format!("r.year {d}, r.id ASC"),
        ReleaseSort::Artist => format!("ra.name_key {d}, r.id ASC"),
        ReleaseSort::Random => {
            let seed = q.seed.filter(|s| *s >= 0).unwrap_or_else(|| {
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| (d.subsec_nanos() % 1_000_000) as i64 + 1).unwrap_or(1)
            });
            format!("{}, r.id", shuffle_expr("r.id", seed))
        }
    }
}

pub fn list_ids(c: &Connection, q: &ReleaseQuery, scope: &Scope, paged: bool) -> ApiResult<(Vec<i64>, i64)> {
    let sort = q.sort.unwrap_or_default();
    let f = build_filter(c, q, scope, sort)?;
    if f.empty {
        return Ok((vec![], 0));
    }
    let from = from_clause(&f);
    let wh = f.sql.where_clause();
    let total: i64 = c.query_row(&format!("SELECT COUNT(*) FROM {from} WHERE {wh}"), params_from_iter(f.sql.params.iter()), |r| r.get(0))?;
    let tail = if paged {
        let offset = q.offset.unwrap_or(0).max(0);
        let limit = q.limit.unwrap_or(60).clamp(1, 1000);
        format!("LIMIT {limit} OFFSET {offset}")
    } else {
        String::new()
    };
    let sql = format!("SELECT r.id FROM {from} WHERE {wh} ORDER BY {} {tail}", order_sql(q, sort));
    let mut st = c.prepare(&sql)?;
    let ids = st.query_map(params_from_iter(f.sql.params.iter()), |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok((ids, total))
}

pub fn list_releases(c: &Connection, q: &ReleaseQuery, scope: &Scope) -> ApiResult<Page<ReleaseOut>> {
    let (ids, total) = list_ids(c, q, scope, true)?;
    let items = hydrate::releases_out(c, &ids)?;
    Ok(Page { items, total, offset: q.offset.unwrap_or(0).max(0), limit: q.limit.unwrap_or(60).clamp(1, 1000) })
}

/// Every release of a listing as id + track count (for "select all").
pub fn release_stubs(c: &Connection, q: &ReleaseQuery, scope: &Scope) -> ApiResult<Vec<ReleaseStub>> {
    let mut q2 = q.clone();
    if q2.sort == Some(ReleaseSort::Random) {
        q2.sort = None;
    }
    let (ids, _) = list_ids(c, &q2, scope, false)?;
    let json = serde_json::to_string(&ids).unwrap_or_default();
    let mut counts = std::collections::HashMap::new();
    let mut st = c.prepare("SELECT release_id, COUNT(*) FROM tracks WHERE release_id IN (SELECT value FROM json_each(?1)) GROUP BY release_id")?;
    let mut rows = st.query([json])?;
    while let Some(r) = rows.next()? {
        counts.insert(r.get::<_, i64>(0)?, r.get::<_, i64>(1)?);
    }
    Ok(ids.into_iter().map(|id| ReleaseStub { id, track_count: counts.get(&id).copied().unwrap_or(0) }).collect())
}

/// The release after `release_id` in the listing described by `q` (random order is not meaningful here).
pub fn next_release(c: &Connection, release_id: i64, q: &ReleaseQuery, scope: &Scope) -> ApiResult<Option<ReleaseOut>> {
    let mut q2 = q.clone();
    if q2.sort == Some(ReleaseSort::Random) {
        q2.sort = None;
    }
    let (ids, _) = list_ids(c, &q2, scope, false)?;
    let Some(pos) = ids.iter().position(|i| *i == release_id) else { return Ok(None) };
    let Some(next) = ids.get(pos + 1) else { return Ok(None) };
    Ok(hydrate::releases_out_n(c, &[*next], 12)?.pop())
}

pub fn get_release(c: &Connection, id: i64) -> ApiResult<ReleaseOut> {
    hydrate::releases_out_n(c, &[id], 12)?.pop().ok_or_else(|| ApiError::not_found(format!("release {id} not found")))
}

// ---------------------------------------------------------------------------------------
// Related shelves
// ---------------------------------------------------------------------------------------

pub(crate) struct ShelfBuilder<'a> {
    pub c: &'a Connection,
    pub scope: &'a Scope,
    pub limit: i64,
    pub seen: HashSet<i64>,
    pub groups: Vec<RelatedGroup>,
}

impl<'a> ShelfBuilder<'a> {
    /// `sql` selects release ids (alias `r`, already ordered, scope not yet applied: `{SCOPE}` marks where
    /// the AND-clause goes). Over-fetches by the ids already spent so dropping repeats leaves a full row.
    pub fn shelf(&mut self, kind: &str, key: &str, title: &str, ref_id: Option<i64>, sql: &str, params: Vec<Value>, tags: Vec<String>) -> ApiResult<()> {
        let scoped = sql.replace("{SCOPE}", &self.scope.and_release("r"));
        let q = format!("{scoped} LIMIT {}", self.limit + self.seen.len() as i64);
        let mut st = self.c.prepare(&q)?;
        let cands: Vec<i64> = st
            .query_map(params_from_iter(params.iter()), |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|id| !self.seen.contains(id))
            .take(self.limit as usize)
            .collect();
        if cands.is_empty() {
            return Ok(());
        }
        let items = hydrate::releases_out(self.c, &cands)?;
        self.seen.extend(cands);
        self.groups.push(RelatedGroup { kind: kind.into(), key: key.into(), title: title.into(), id: ref_id, tags, items });
        Ok(())
    }
}

pub fn related_releases(c: &Connection, release_id: i64, limit: i64, scope: &Scope) -> ApiResult<Vec<RelatedGroup>> {
    let (artist, label): (Option<(i64, String)>, Option<(i64, String)>) = {
        let mut st = c.prepare(
            "SELECT r.artist_id, a.name, r.label_id, l.name FROM releases r LEFT JOIN artists a ON a.id = r.artist_id
              LEFT JOIN labels l ON l.id = r.label_id WHERE r.id = ?1",
        )?;
        let row = st
            .query_row([release_id], |r| {
                Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?))
            })
            .map_err(|e| match e {
                bc_db::rusqlite::Error::QueryReturnedNoRows => ApiError::not_found(format!("release {release_id} not found")),
                e => e.into(),
            })?;
        (row.0.zip(row.1), row.2.zip(row.3))
    };
    let mut b = ShelfBuilder { c, scope, limit, seen: HashSet::from([release_id]), groups: vec![] };
    if let Some((aid, name)) = &artist {
        b.shelf(
            "artist",
            name,
            &format!("More by {name}"),
            Some(*aid),
            "SELECT r.id FROM releases r WHERE r.artist_id = ?1{SCOPE} ORDER BY r.year DESC, r.added_at DESC",
            vec![(*aid).into()],
            vec![],
        )?;
    }
    if let Some((lid, name)) = &label {
        b.shelf(
            "label",
            name,
            &format!("More on {name}"),
            Some(*lid),
            "SELECT r.id FROM releases r WHERE r.label_id = ?1{SCOPE} ORDER BY r.year DESC, r.added_at DESC",
            vec![(*lid).into()],
            vec![],
        )?;
    }
    // The release's own tags, most-used first; the rarer tag wins ties ("dub techno" says more than "Electronic").
    let tags: Vec<(i64, String, i64)> = {
        let mut st = c.prepare(
            "SELECT g.id, g.name, g.track_count FROM tags g JOIN track_tags tt ON tt.tag_id = g.id JOIN tracks t ON t.id = tt.track_id
              WHERE t.release_id = ?1 GROUP BY g.id ORDER BY COUNT(*) DESC, g.track_count ASC, g.name_key LIMIT 12",
        )?;
        st.query_map([release_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?
    };
    let tags = trim_profile(c, tags);
    if !tags.is_empty() {
        shelves_by_profile(&mut b, &tags, None)?;
    }
    Ok(b.groups)
}

/// The "profile" shelf (ranked by shared profile tags) and one shelf per narrow tag.
/// `exclude_artist` keeps an artist's own records out of their suggestions.
pub(crate) fn shelves_by_profile(b: &mut ShelfBuilder<'_>, tags: &[(i64, String, i64)], exclude_artist: Option<i64>) -> ApiResult<()> {
    let profile: Vec<&(i64, String, i64)> = tags.iter().take(PROFILE_TAGS).collect();
    let ids = profile.iter().map(|t| t.0.to_string()).collect::<Vec<_>>().join(",");
    let names: Vec<String> = profile.iter().map(|t| t.1.clone()).collect();
    let excl = exclude_artist.map(|a| format!(" AND (r.artist_id IS NULL OR r.artist_id != {a})")).unwrap_or_default();
    let by_profile = format!(
        "SELECT r.id FROM releases r JOIN tracks t ON t.release_id = r.id JOIN track_tags tt ON tt.track_id = t.id \
          WHERE tt.tag_id IN ({ids}){excl}{{SCOPE}} GROUP BY r.id ORDER BY COUNT(DISTINCT tt.tag_id) DESC, r.added_at DESC"
    );
    let (title, key) = if exclude_artist.is_some() { ("Suggested albums", names.join(", ")) } else { ("More like this", names.join(", ")) };
    b.shelf("tag", &key, title, None, &by_profile, vec![], names)?;

    let mut own: Vec<&(i64, String, i64)> = tags.iter().filter(|t| t.2 >= MIN_TAG_SHELF).collect();
    own.sort_by_key(|t| t.2);
    for t in own.into_iter().take(TAG_SHELVES) {
        let sql = format!(
            "SELECT r.id FROM releases r JOIN tracks t ON t.release_id = r.id JOIN track_tags tt ON tt.track_id = t.id \
              WHERE tt.tag_id IN ({ids}){excl} AND r.id IN (SELECT t2.release_id FROM track_tags x JOIN tracks t2 ON t2.id = x.track_id WHERE x.tag_id = {}){{SCOPE}} \
              GROUP BY r.id ORDER BY COUNT(DISTINCT tt.tag_id) DESC, r.added_at DESC",
            t.0
        );
        b.shelf("tag", &t.1, &format!("More {}", t.1), Some(t.0), &sql, vec![], vec![])?;
    }
    Ok(())
}

/// Hyper-common tags ("Electronic") say little about taste and make the profile joins scan a large part of the
/// library: when at least two narrower tags remain, drop the ones carried by > 8% of all tracks.
pub(crate) fn trim_profile(c: &Connection, tags: Vec<(i64, String, i64)>) -> Vec<(i64, String, i64)> {
    let total: i64 = c.query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0)).unwrap_or(0);
    let cap = (total / 12).max(2000);
    let narrow: Vec<_> = tags.iter().filter(|t| t.2 <= cap).cloned().collect();
    if narrow.len() >= 2 { narrow } else { tags }
}
