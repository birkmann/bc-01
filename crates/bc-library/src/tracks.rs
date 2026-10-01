//! `/tracks`: server-side filters and sorts, unlimited offset paging, bm25 relevance order,
//! seeded-hash shuffle, tag filter on `name_key` (PLAN §3.4, §9).
//!
//! Shape of every listing: an id-only query (index walks; the deep OFFSET never touches the table
//! thanks to the `ix_tracks_s_*` covering indexes) followed by one hydration of the returned page.

use bc_db::rusqlite::{Connection, params_from_iter, types::Value};
use bc_db::util::{SHUFFLE_MOD, SHUFFLE_MULT, name_key, parse_bound};
use bc_libcore::{ApiError, ApiResult, Scope, hydrate};
use bc_types::library::{SortDir, TrackOut, TrackPage, TrackQuery, TrackSort};
use bc_types::Page;

use crate::sqlb::Sql;

pub const DEFAULT_LIMIT: i64 = 100;
pub const MAX_LIMIT: i64 = 5000;
/// Relevance results are capped at the best N matches (bm25 over a huge prefix match is costly).
pub const RELEVANCE_CAP: i64 = 5_000;

/// A compiled filter: conditions over alias `t` plus an optional FTS expression.
pub struct TrackFilter {
    pub sql: Sql,
    pub fts: Option<String>,
    /// Provably empty (e.g. an unknown tag): skip the query.
    pub empty: bool,
}

fn bound(label: &str, s: &Option<String>) -> ApiResult<Option<String>> {
    match s {
        None => Ok(None),
        Some(v) if v.trim().is_empty() => Ok(None),
        Some(v) => parse_bound(v).map(Some).ok_or_else(|| ApiError::bad(format!("{label}: not a date: {v}"))),
    }
}

/// Resolve tag names to ids (indexed `name_key`). `None` = one of them does not exist.
pub fn resolve_tag_ids(c: &Connection, names: &[String]) -> ApiResult<Option<Vec<i64>>> {
    let mut ids = Vec::with_capacity(names.len());
    let mut st = c.prepare_cached("SELECT id FROM tags WHERE name_key = ?1")?;
    for n in names {
        let key = name_key(n);
        match st.query_row([&key], |r| r.get::<_, i64>(0)) {
            Ok(id) => ids.push(id),
            Err(bc_db::rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(Some(ids))
}

pub fn build_filter(c: &Connection, q: &TrackQuery, scope: &Scope) -> ApiResult<TrackFilter> {
    let mut s = Sql::new();
    let mut fts = None;
    let mut empty = false;

    // A listing shows the scope's records, but an address (one release's tracks) is always answered.
    if scope.filtered()
        && q.release_id.is_none()
        && q.release_ids.is_empty()
        && let Some(p) = scope.track_pred("t")
    {
        s.cond(p);
    }

    if let Some(text) = q.q.as_deref() {
        let key = name_key(text);
        if key.chars().count() == 1 {
            // A single character: an FTS prefix scan over every term is ~100 ms and ranks nothing useful, so it
            // means "title starts with" (an index range on `title_key`), in the default sort.
            let lo = s.bind(key.clone());
            let hi = s.bind(format!("{key}\u{10FFFF}"));
            s.cond(format!("t.title_key >= {lo} AND t.title_key < {hi}"));
        } else {
            let expr = bc_db::fts::fts_escape(text);
            if !expr.is_empty() {
                fts = Some(expr);
            }
        }
    }

    if let Some(a) = q.artist_id {
        let p = s.bind(a);
        s.cond(format!(
            "t.id IN (SELECT id FROM tracks WHERE artist_id = {p} \
              UNION SELECT id FROM tracks WHERE release_id IN (SELECT id FROM releases WHERE artist_id = {p}))"
        ));
    }
    if let Some(r) = q.release_id {
        let p = s.bind(r);
        s.cond(format!("t.release_id = {p}"));
    }
    if !q.release_ids.is_empty() {
        let p = s.bind(serde_json::to_string(&q.release_ids).unwrap_or_default());
        s.cond(format!("t.release_id IN (SELECT value FROM json_each({p}))"));
    }
    if let Some(l) = q.label_id {
        let p = s.bind(l);
        s.cond(format!("t.release_id IN (SELECT id FROM releases WHERE label_id = {p})"));
    }
    if !q.tags.is_empty() {
        match resolve_tag_ids(c, &q.tags)? {
            None => empty = true,
            Some(ids) => {
                for id in ids {
                    s.cond(format!("t.id IN (SELECT track_id FROM track_tags WHERE tag_id = {id})"));
                }
            }
        }
    }
    if q.favorites == Some(true) {
        s.cond(
            "t.id IN (SELECT id FROM tracks WHERE artist_id IN (SELECT artist_id FROM favorites WHERE artist_id IS NOT NULL) \
               UNION SELECT id FROM tracks WHERE release_id IN (SELECT id FROM releases WHERE artist_id IN (SELECT artist_id FROM favorites WHERE artist_id IS NOT NULL)) \
               UNION SELECT id FROM tracks WHERE release_id IN (SELECT id FROM releases WHERE label_id IN (SELECT label_id FROM favorites WHERE label_id IS NOT NULL)) \
               UNION SELECT track_id FROM track_tags WHERE tag_id IN (SELECT tag_id FROM favorites WHERE tag_id IS NOT NULL))",
        );
    }
    if let Some(y) = q.year_min {
        let p = s.bind(y);
        s.cond(format!("t.year >= {p}"));
    }
    if let Some(y) = q.year_max {
        let p = s.bind(y);
        s.cond(format!("t.year <= {p}"));
    }
    if let Some(l) = q.loved {
        s.cond(format!("t.loved = {}", l as i32));
    }
    if let Some(d) = bound("added_after", &q.added_after)? {
        let p = s.bind(d);
        s.cond(format!("t.added_at >= {p}"));
    }
    if let Some(d) = bound("added_before", &q.added_before)? {
        let p = s.bind(d);
        s.cond(format!("t.added_at < {p}"));
    }
    match q.played {
        Some(true) => s.cond("t.play_count > 0"),
        Some(false) => s.cond("t.play_count = 0"),
        None => {}
    }
    if let Some(d) = bound("last_played_before", &q.last_played_before)? {
        let p = s.bind(d);
        s.cond(format!("t.last_played_at < {p}"));
    }
    // `missing` defaults to false in the legacy API: hide tracks with no live file.
    match q.missing {
        Some(false) | None => s.cond("t.available = 1"),
        Some(true) => s.cond("EXISTS (SELECT 1 FROM files f WHERE f.track_id = t.id AND f.missing_since IS NOT NULL)"),
    }
    if let Some(b) = q.bpm_min {
        let p = s.bind(b);
        s.cond(format!("t.bpm >= {p}"));
    }
    if let Some(b) = q.bpm_max {
        let p = s.bind(b);
        s.cond(format!("t.bpm <= {p}"));
    }
    if let Some(k) = q.camelot.as_deref().filter(|k| !k.is_empty()) {
        let p = s.bind(k.to_uppercase());
        s.cond(format!("EXISTS (SELECT 1 FROM analysis an WHERE an.track_id = t.id AND an.camelot = {p})"));
    }
    Ok(TrackFilter { sql: s, fts, empty })
}

/// A fixed-seed shuffle key: `((id + seed) * 2654435761) % 2^32` (Knuth multiplicative hash;
/// the odd multiplier makes it a bijection so no two ids tie).
pub fn shuffle_expr(col: &str, seed: i64) -> String {
    format!("(({col} + {seed}) * {SHUFFLE_MULT} % {SHUFFLE_MOD})")
}

fn norm_seed(seed: Option<i64>) -> i64 {
    seed.filter(|s| *s >= 0).unwrap_or_else(|| {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(1);
        (n % 1_000_000) as i64 + 1
    })
}

struct Order {
    sql: String,
    join_analysis: bool,
    relevance: bool,
}

fn order_for(q: &TrackQuery, has_fts: bool) -> Order {
    let sort = q.sort.unwrap_or(if has_fts { TrackSort::Relevance } else { TrackSort::Added });
    let sort = if sort == TrackSort::Relevance && !has_fts { TrackSort::Added } else { sort };
    let dir = q.order.unwrap_or(SortDir::Desc);
    let d = if dir == SortDir::Desc { "DESC" } else { "ASC" };
    let mut join_analysis = false;
    let mut relevance = false;
    let sql = match sort {
        TrackSort::Added => format!("t.added_at {d}, t.id {d}"),
        TrackSort::Title => format!("t.title_key {d}, t.id {d}"),
        TrackSort::Artist => format!("t.artist_key {d}, t.id {d}"),
        // Within an album the order is always disc, track (a descending album sort still plays side A first).
        TrackSort::Album => format!("t.album_key {d}, t.disc_no ASC, t.track_no ASC, t.id ASC"),
        TrackSort::Duration => format!("t.duration_ms {d}, t.id {d}"),
        TrackSort::Bpm => format!("t.bpm {d}, t.id {d}"),
        TrackSort::PlayCount => format!("t.play_count {d}, t.id {d}"),
        TrackSort::LastPlayed => format!("t.last_played_at {d}, t.id {d}"),
        TrackSort::Year => format!("t.year {d}, t.id {d}"),
        TrackSort::Rating => format!("t.rating {d}, t.id {d}"),
        TrackSort::Key => {
            join_analysis = true;
            format!("CAST(substr(an2.camelot, 1, length(an2.camelot) - 1) AS INTEGER) {d}, substr(an2.camelot, -1) {d}, t.id {d}")
        }
        TrackSort::Energy => {
            join_analysis = true;
            format!("an2.energy {d}, t.id {d}")
        }
        TrackSort::Random => {
            let seed = norm_seed(q.seed);
            format!("{}, t.id", shuffle_expr("t.id", seed))
        }
        TrackSort::Relevance => {
            relevance = true;
            "m.rk, t.id".to_string()
        }
    };
    Order { sql, join_analysis, relevance }
}

pub fn clamp_paging(q_offset: Option<i64>, q_limit: Option<i64>) -> (i64, i64) {
    (q_offset.unwrap_or(0).max(0), q_limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT))
}

/// `FROM`/`WHERE` for the id query, plus the params. Relevance uses a ranked CTE.
fn id_query(filter: &TrackFilter, order: &Order, select: &str, tail: &str) -> (String, Vec<Value>) {
    let mut params = filter.sql.params.clone();
    let mut sql = String::new();
    let mut from = String::from("tracks t");
    if order.relevance {
        params.push(Value::Text(filter.fts.clone().unwrap_or_default()));
        let n = params.len();
        sql.push_str(&format!(
            "WITH m AS (SELECT track_id, bm25(search_index, {}) AS rk FROM search_index WHERE search_index MATCH ?{n} ORDER BY rk LIMIT {RELEVANCE_CAP}) ",
            bc_db::fts::BM25_WEIGHTS
        ));
        from = "m JOIN tracks t ON t.id = m.track_id".into();
    }
    if order.join_analysis {
        from.push_str(" LEFT JOIN analysis an2 ON an2.track_id = t.id");
    }
    let mut conds = filter.sql.conds.clone();
    if !order.relevance
        && let Some(expr) = &filter.fts
    {
        params.push(Value::Text(expr.clone()));
        conds.push(format!("t.id IN (SELECT track_id FROM search_index WHERE search_index MATCH ?{})", params.len()));
    }
    let where_clause = if conds.is_empty() { "1=1".to_string() } else { conds.join(" AND ") };
    sql.push_str(&format!("SELECT {select} FROM {from} WHERE {where_clause} {tail}"));
    (sql, params)
}

/// The ids of the page (and the unpaged total / playtime when `with_total`).
pub fn page_ids(c: &Connection, q: &TrackQuery, scope: &Scope) -> ApiResult<(Vec<i64>, i64, i64)> {
    let filter = build_filter(c, q, scope)?;
    if filter.empty {
        return Ok((vec![], 0, 0));
    }
    let (offset, limit) = clamp_paging(q.offset, q.limit);
    let order = order_for(q, filter.fts.is_some());
    if order.relevance {
        return relevance_page(c, &filter, offset, limit);
    }

    let (count_sql, count_params) = {
        let o = Order { sql: String::new(), join_analysis: false, relevance: order.relevance };
        id_query(&filter, &o, "COUNT(*), COALESCE(SUM(t.duration_ms), 0)", "")
    };
    let (total, dur): (i64, i64) = c.query_row(&count_sql, params_from_iter(count_params.iter()), |r| Ok((r.get(0)?, r.get(1)?)))?;

    let (sql, params) = id_query(&filter, &order, "t.id", &format!("ORDER BY {} LIMIT {limit} OFFSET {offset}", order.sql));
    let mut st = c.prepare_cached(&sql)?;
    let ids = st.query_map(params_from_iter(params.iter()), |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
    Ok((ids, total, dur))
}

/// Every id of the filter in listing order (exports, "save as playlist", play-all). Unpaged.
pub fn all_ids(c: &Connection, q: &TrackQuery, scope: &Scope, hard_limit: Option<i64>) -> ApiResult<Vec<i64>> {
    let filter = build_filter(c, q, scope)?;
    if filter.empty {
        return Ok(vec![]);
    }
    let order = order_for(q, filter.fts.is_some());
    let tail = match hard_limit {
        Some(n) => format!("ORDER BY {} LIMIT {}", order.sql, n.max(0)),
        None => format!("ORDER BY {}", order.sql),
    };
    let (sql, params) = id_query(&filter, &order, "t.id", &tail);
    let mut st = c.prepare(&sql)?;
    Ok(st.query_map(params_from_iter(params.iter()), |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?)
}

pub fn list_tracks(c: &Connection, q: &TrackQuery, scope: &Scope) -> ApiResult<TrackPage> {
    let (offset, limit) = clamp_paging(q.offset, q.limit);
    let (ids, total, dur) = page_ids(c, q, scope)?;
    let items: Vec<TrackOut> = hydrate::tracks_out(c, &ids)?;
    Ok(TrackPage { page: Page { items, total, offset, limit }, total_duration_ms: dur })
}

/// SQL text of the id query for the plan tests (`EXPLAIN QUERY PLAN`).
pub fn explain_sql(c: &Connection, q: &TrackQuery, scope: &Scope) -> ApiResult<(String, Vec<Value>)> {
    let filter = build_filter(c, q, scope)?;
    let order = order_for(q, filter.fts.is_some());
    let (offset, limit) = clamp_paging(q.offset, q.limit);
    Ok(id_query(&filter, &order, "t.id", &format!("ORDER BY {} LIMIT {limit} OFFSET {offset}", order.sql)))
}

/// bm25-ordered search page. Fast path (nothing but the default availability filter): the LIMIT is pushed into the
/// FTS query (top-N bm25, ~20-40 ms on 190k tracks) and the total is the FTS match count; `total_duration_ms` is
/// only summed for narrow matches (<= RELEVANCE_CAP). With extra filters the best RELEVANCE_CAP matches are
/// ranked once and filtered/counted in the same statement (window aggregates).
fn relevance_page(c: &Connection, filter: &TrackFilter, offset: i64, limit: i64) -> ApiResult<(Vec<i64>, i64, i64)> {
    let expr = filter.fts.clone().unwrap_or_default();
    let fast = filter.sql.conds.iter().all(|x| x == "t.available = 1");
    if fast {
        let want = (offset + limit).min(RELEVANCE_CAP);
        let total: i64 = c.query_row("SELECT COUNT(*) FROM search_index WHERE search_index MATCH ?1", [&expr], |r| r.get(0))?;
        let sql = format!(
            "WITH m AS (SELECT track_id, bm25(search_index, {}) AS rk FROM search_index WHERE search_index MATCH ?1 ORDER BY rk LIMIT {want})
             SELECT t.id FROM m JOIN tracks t ON t.id = m.track_id WHERE t.available = 1 ORDER BY m.rk, t.id LIMIT {limit} OFFSET {offset}",
            bc_db::fts::BM25_WEIGHTS
        );
        let mut st = c.prepare_cached(&sql)?;
        let ids = st.query_map([&expr], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
        let dur: i64 = if total <= RELEVANCE_CAP {
            c.query_row(
                "SELECT COALESCE(SUM(t.duration_ms), 0) FROM search_index s JOIN tracks t ON t.id = s.track_id WHERE search_index MATCH ?1 AND t.available = 1",
                [&expr],
                |r| r.get(0),
            )?
        } else {
            0
        };
        return Ok((ids, total, dur));
    }
    let mut params = filter.sql.params.clone();
    params.push(Value::Text(expr));
    let n = params.len();
    let wh = filter.sql.where_clause();
    let sql = format!(
        "WITH m AS (SELECT track_id, bm25(search_index, {}) AS rk FROM search_index WHERE search_index MATCH ?{n} ORDER BY rk LIMIT {RELEVANCE_CAP}),
              f AS (SELECT t.id AS id, m.rk AS rk, t.duration_ms AS d FROM m JOIN tracks t ON t.id = m.track_id WHERE {wh})
         SELECT id, (SELECT COUNT(*) FROM f), (SELECT COALESCE(SUM(d), 0) FROM f) FROM f ORDER BY rk, id LIMIT {limit} OFFSET {offset}",
        bc_db::fts::BM25_WEIGHTS
    );
    let mut st = c.prepare(&sql)?;
    let mut ids = vec![];
    let (mut total, mut dur) = (0, 0);
    let mut rows = st.query(params_from_iter(params.iter()))?;
    while let Some(r) = rows.next()? {
        ids.push(r.get::<_, i64>(0)?);
        total = r.get(1)?;
        dur = r.get(2)?;
    }
    if ids.is_empty() && offset > 0 {
        // paged past the end: report the real total
        let tq = format!(
            "WITH m AS (SELECT track_id FROM search_index WHERE search_index MATCH ?{n} LIMIT {RELEVANCE_CAP})
             SELECT COUNT(*), COALESCE(SUM(t.duration_ms), 0) FROM m JOIN tracks t ON t.id = m.track_id WHERE {wh}"
        );
        let (a, b): (i64, i64) = c.query_row(&tq, params_from_iter(params.iter()), |r| Ok((r.get(0)?, r.get(1)?)))?;
        total = a;
        dur = b;
    }
    Ok((ids, total, dur))
}
