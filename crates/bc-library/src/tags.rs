//! `/tags` (cloud, optionally narrowed by a track filter) and `/facets`.

use bc_db::rusqlite::{Connection, params_from_iter};
use bc_libcore::{ApiResult, Scope};
use bc_types::library::*;

use crate::tracks;

pub fn list_tags(c: &Connection, q: &TagsQuery, scope: &Scope) -> ApiResult<Vec<TagOut>> {
    let min_count = q.min_count.unwrap_or(1).max(0);
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let narrowed = q.q.as_deref().is_some_and(|s| !s.trim().is_empty()) || !q.tags.is_empty() || q.loved.is_some() || q.added_after.is_some() || q.added_before.is_some() || scope.filtered();
    if !narrowed {
        let mut st = c.prepare("SELECT id, name, track_count FROM tags WHERE track_count >= ?1 ORDER BY track_count DESC, name_key LIMIT ?2")?;
        let rows = st
            .query_map([min_count, limit], |r| Ok(TagOut { id: r.get(0)?, name: r.get(1)?, track_count: r.get(2)? }))?
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(rows);
    }
    // Only tags carried by a matching track, each counted over those tracks alone: what a "narrow this filter
    // further" picker needs (adding a tag that shares no track would only produce an empty page).
    let tq = TrackQuery {
        q: q.q.clone(),
        tags: q.tags.clone(),
        loved: q.loved,
        added_after: q.added_after.clone(),
        added_before: q.added_before.clone(),
        missing: Some(false),
        ..Default::default()
    };
    let filter = tracks::build_filter(c, &tq, scope)?;
    if filter.empty {
        return Ok(vec![]);
    }
    let mut params = filter.sql.params.clone();
    let mut conds = filter.sql.conds.clone();
    if let Some(expr) = &filter.fts {
        params.push(bc_db::rusqlite::types::Value::Text(expr.clone()));
        conds.push(format!("t.id IN (SELECT track_id FROM search_index WHERE search_index MATCH ?{})", params.len()));
    }
    let wh = if conds.is_empty() { "1=1".to_string() } else { conds.join(" AND ") };
    let sql = format!(
        "SELECT g.id, g.name, COUNT(DISTINCT tt.track_id) AS n FROM tracks t JOIN track_tags tt ON tt.track_id = t.id JOIN tags g ON g.id = tt.tag_id
          WHERE {wh} GROUP BY g.id HAVING n >= {min_count} ORDER BY n DESC, g.name_key LIMIT {limit}"
    );
    let mut st = c.prepare(&sql)?;
    let rows = st
        .query_map(params_from_iter(params.iter()), |r| Ok(TagOut { id: r.get(0)?, name: r.get(1)?, track_count: r.get(2)? }))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn facets(c: &Connection, limit: i64, scope: &Scope) -> ApiResult<Facets> {
    let limit = limit.clamp(1, 200);
    let tags: Vec<TagOut> = if scope.filtered() {
        let sql = format!(
            "SELECT g.id, g.name, COUNT(DISTINCT tt.track_id) AS n FROM tags g JOIN track_tags tt ON tt.tag_id = g.id JOIN tracks t ON t.id = tt.track_id
              WHERE 1=1{} GROUP BY g.id ORDER BY n DESC LIMIT {limit}",
            scope.and_track("t")
        );
        let mut st = c.prepare(&sql)?;
        st.query_map([], |r| Ok(TagOut { id: r.get(0)?, name: r.get(1)?, track_count: r.get(2)? }))?.collect::<Result<_, _>>()?
    } else {
        let mut st = c.prepare(&format!("SELECT id, name, track_count FROM tags WHERE track_count > 0 ORDER BY track_count DESC LIMIT {limit}"))?;
        st.query_map([], |r| Ok(TagOut { id: r.get(0)?, name: r.get(1)?, track_count: r.get(2)? }))?.collect::<Result<_, _>>()?
    };
    let artists: Vec<ArtistOut> = {
        let mut st = c.prepare(&format!(
            "SELECT a.id, a.name, COUNT(t.id) AS n FROM artists a JOIN releases r ON r.artist_id = a.id JOIN tracks t ON t.release_id = r.id
              WHERE 1=1{} GROUP BY a.id ORDER BY n DESC LIMIT {limit}",
            scope.and_release("r")
        ))?;
        st.query_map([], |r| Ok(ArtistOut { id: r.get(0)?, name: r.get(1)?, track_count: r.get(2)?, ..Default::default() }))?.collect::<Result<_, _>>()?
    };
    let years: Vec<YearFacet> = {
        let mut st = c.prepare(&format!(
            "SELECT r.year, COUNT(t.id) FROM releases r JOIN tracks t ON t.release_id = r.id WHERE r.year IS NOT NULL{} GROUP BY r.year ORDER BY r.year DESC",
            scope.and_release("r")
        ))?;
        st.query_map([], |r| Ok(YearFacet { year: r.get(0)?, count: r.get(1)? }))?.collect::<Result<_, _>>()?
    };
    let total: i64 = c.query_row(&format!("SELECT COUNT(*) FROM tracks t WHERE 1=1{}", scope.and_track("t")), [], |r| r.get(0))?;
    Ok(Facets { tags, artists, years, total })
}
