//! `/labels`: listing (counts + cover stack), shuffle, random, next, detail, patch.

use std::collections::HashMap;

use bc_db::rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};
use bc_db::util::{iso_opt, name_key};
use bc_libcore::{ApiError, ApiResult, Ctx, Scope, hydrate};
use bc_types::Page;
use bc_types::library::*;

/// Covers stacked on one label's folder.
pub const LABEL_COVERS: usize = 4;

fn json_ids(ids: &[i64]) -> String {
    serde_json::to_string(ids).unwrap_or_else(|_| "[]".into())
}

struct Listing {
    sql_from: String,
    wheres: Vec<String>,
    params: Vec<Value>,
    order: String,
}

/// The labels shelf as a query: only labels with something on them (a folder that opens onto an
/// empty grid is worse than no folder), the counts computed in the query the sort runs over.
fn listing(q: &LabelQuery, scope: &Scope, sort: LabelSort, order: SortDir) -> Listing {
    let d = if order == SortDir::Desc { "DESC" } else { "ASC" };
    // Release counts come from `releases` alone; the (costlier) track join is only paid when the sort or
    // the random draw needs track counts.
    let need_tracks = sort == LabelSort::Tracks || q.exclude_id.is_some();
    let tot = if need_tracks {
        format!(
            "(SELECT r.label_id AS lid, COUNT(DISTINCT r.id) AS rc, COUNT(t.id) AS tc, MAX(r.added_at) AS la
                FROM releases r LEFT JOIN tracks t ON t.release_id = r.id
               WHERE r.label_id IS NOT NULL{} GROUP BY r.label_id) tot",
            scope.and_release("r")
        )
    } else {
        format!(
            "(SELECT r.label_id AS lid, COUNT(*) AS rc, 0 AS tc, MAX(r.added_at) AS la
                FROM releases r WHERE r.label_id IS NOT NULL{} GROUP BY r.label_id) tot",
            scope.and_release("r")
        )
    };
    let mut params = vec![];
    let mut wheres = vec![];
    if let Some(text) = q.q.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        match bc_db::fts::trigram_phrase(text) {
            Some(phrase) => {
                params.push(Value::Text(phrase));
                wheres.push(format!("l.id IN (SELECT rowid FROM label_search WHERE label_search MATCH ?{})", params.len()));
            }
            None => {
                params.push(Value::Text(format!("%{}%", text.to_lowercase())));
                wheres.push(format!("lower(l.name) LIKE ?{}", params.len()));
            }
        }
    }
    let order_sql = match sort {
        LabelSort::Name => format!("l.name_key {d}, l.id ASC"),
        LabelSort::Tracks => format!("tot.tc {d}, l.id ASC"),
        LabelSort::Added => format!("tot.la {d}, l.id ASC"),
        LabelSort::Releases => format!("tot.rc {d}, l.id ASC"),
    };
    Listing { sql_from: format!("labels l JOIN {tot} ON tot.lid = l.id"), wheres, params, order: order_sql }
}

/// Batched label cards for `ids` (order preserved): counts, bytes on disk and the cover stack.
pub fn labels_out(c: &Connection, ids: &[i64], scope: &Scope) -> ApiResult<Vec<LabelOut>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let json = json_ids(ids);
    let mut base: HashMap<i64, LabelOut> = HashMap::new();
    {
        let sql = format!(
            "SELECT l.id, l.name, l.bandcamp_url, COUNT(DISTINCT r.id), COUNT(t.id), MAX(r.added_at)
               FROM labels l LEFT JOIN releases r ON r.label_id = l.id{} LEFT JOIN tracks t ON t.release_id = r.id
              WHERE l.id IN (SELECT value FROM json_each(?1)) GROUP BY l.id",
            scope.release_pred("r").map(|p| format!(" AND {p}")).unwrap_or_default()
        );
        let mut st = c.prepare(&sql)?;
        let mut rows = st.query([&json])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            base.insert(
                id,
                LabelOut {
                    id,
                    name: r.get(1)?,
                    bandcamp_url: r.get(2)?,
                    release_count: r.get(3)?,
                    track_count: r.get(4)?,
                    size_bytes: 0,
                    art_urls: vec![],
                    last_added_at: iso_opt(r.get(5)?),
                },
            );
        }
    }
    {
        // Bytes apart from the counts: a track may carry more than one file.
        let sql = format!(
            "SELECT r.label_id, COALESCE(SUM(f.size_bytes), 0) FROM releases r JOIN tracks t ON t.release_id = r.id JOIN files f ON f.track_id = t.id
              WHERE r.label_id IN (SELECT value FROM json_each(?1)){} GROUP BY r.label_id",
            scope.and_release("r")
        );
        let mut st = c.prepare(&sql)?;
        let mut rows = st.query([&json])?;
        while let Some(r) = rows.next()? {
            if let Some(l) = base.get_mut(&r.get::<_, i64>(0)?) {
                l.size_bytes = r.get(1)?;
            }
        }
    }
    {
        // The newest few covers per label, one query for the page.
        let sql = format!(
            "SELECT lid, rid, version FROM (
                SELECT r.label_id AS lid, r.id AS rid, aw.version AS version,
                       ROW_NUMBER() OVER (PARTITION BY r.label_id ORDER BY r.added_at DESC) AS rn
                  FROM releases r LEFT JOIN artwork aw ON aw.release_id = r.id
                 WHERE r.label_id IN (SELECT value FROM json_each(?1)) AND r.cover_path IS NOT NULL{}
             ) WHERE rn <= {LABEL_COVERS} ORDER BY lid, rn",
            scope.and_release("r")
        );
        let mut st = c.prepare(&sql)?;
        let mut rows = st.query([&json])?;
        while let Some(r) = rows.next()? {
            if let Some(l) = base.get_mut(&r.get::<_, i64>(0)?) {
                let rid: i64 = r.get(1)?;
                let v: Option<String> = r.get(2)?;
                l.art_urls.push(hydrate::art_url(rid, v.as_deref(), true));
            }
        }
    }
    Ok(ids.iter().filter_map(|i| base.remove(i)).collect())
}

pub fn list_labels(c: &Connection, q: &LabelQuery, scope: &Scope) -> ApiResult<Page<LabelOut>> {
    let sort = q.sort.unwrap_or_default();
    let order = q.order.unwrap_or(SortDir::Desc);
    let l = listing(q, scope, sort, order);
    let offset = q.offset.unwrap_or(0).max(0);
    let limit = q.limit.unwrap_or(200).clamp(1, 500);
    let mut wheres = l.wheres.clone();
    if let Some(p) = scope.label_pred("l") {
        wheres.push(p);
    }
    let wh = if wheres.is_empty() { "1=1".to_string() } else { wheres.join(" AND ") };
    let total: i64 = c.query_row(&format!("SELECT COUNT(*) FROM {} WHERE {wh}", l.sql_from), params_from_iter(l.params.iter()), |r| r.get(0))?;
    let mut st = c.prepare(&format!("SELECT l.id FROM {} WHERE {wh} ORDER BY {} LIMIT {limit} OFFSET {offset}", l.sql_from, l.order))?;
    let ids: Vec<i64> = st.query_map(params_from_iter(l.params.iter()), |r| r.get(0))?.collect::<Result<_, _>>()?;
    Ok(Page { items: labels_out(c, &ids, scope)?, total, offset, limit })
}

pub fn get_label(c: &Connection, id: i64, scope: &Scope) -> ApiResult<LabelOut> {
    labels_out(c, &[id], scope)?.pop().ok_or_else(|| ApiError::not_found(format!("label {id} not found")))
}

/// The label that follows `label_id` in the listing, or `None` at the end / when it left the listing.
pub fn next_label(c: &Connection, label_id: i64, q: &LabelQuery, scope: &Scope) -> ApiResult<Option<LabelOut>> {
    let l = listing(q, scope, q.sort.unwrap_or_default(), q.order.unwrap_or(SortDir::Desc));
    let mut wheres = l.wheres.clone();
    if let Some(p) = scope.label_pred("l") {
        wheres.push(p);
    }
    let wh = if wheres.is_empty() { "1=1".to_string() } else { wheres.join(" AND ") };
    let mut st = c.prepare(&format!("SELECT l.id FROM {} WHERE {wh} ORDER BY {}", l.sql_from, l.order))?;
    let ids: Vec<i64> = st.query_map(params_from_iter(l.params.iter()), |r| r.get(0))?.collect::<Result<_, _>>()?;
    let Some(pos) = ids.iter().position(|i| *i == label_id) else { return Ok(None) };
    match ids.get(pos + 1) {
        Some(n) => Ok(labels_out(c, &[*n], scope)?.pop()),
        None => Ok(None),
    }
}

/// A random label with something playable: where a shuffle of the whole shelf goes next.
pub fn random_label(c: &Connection, q: &LabelQuery, scope: &Scope) -> ApiResult<Option<LabelOut>> {
    let q2 = LabelQuery { exclude_id: Some(-1), ..q.clone() };
    let l = listing(&q2, scope, LabelSort::Releases, SortDir::Desc);
    let mut wheres = l.wheres.clone();
    wheres.push("tot.tc > 0".into());
    if let Some(p) = scope.label_pred("l") {
        wheres.push(p);
    }
    let mut params = l.params.clone();
    if let Some(ex) = q.exclude_id {
        params.push(Value::Integer(ex));
        wheres.push(format!("l.id != ?{}", params.len()));
    }
    let wh = wheres.join(" AND ");
    let id: Option<i64> =
        c.query_row(&format!("SELECT l.id FROM {} WHERE {wh} ORDER BY random() LIMIT 1", l.sql_from), params_from_iter(params.iter()), |r| r.get(0)).optional()?;
    match id {
        Some(id) => Ok(labels_out(c, &[id], scope)?.pop()),
        None => Ok(None),
    }
}

/// Shuffling *labels*: an equal share of tracks from each folder, the shares shuffled together, so the
/// queue jumps from label to label instead of settling into the big ones.
pub fn shuffle_labels(c: &Connection, q: &str, limit: i64, scope: &Scope) -> ApiResult<Page<bc_types::library::TrackOut>> {
    let limit = limit.clamp(1, 500);
    let mut params: Vec<Value> = vec![];
    let mut name_filter = String::new();
    let q = q.trim();
    if !q.is_empty() {
        params.push(Value::Text(format!("%{}%", q.to_lowercase())));
        name_filter = format!(" AND r.label_id IN (SELECT id FROM labels WHERE lower(name) LIKE ?{})", params.len());
    }
    let base_where = format!(
        "r.label_id IS NOT NULL AND t.available = 1{}{}{}",
        scope.and_release("r"),
        scope.and_track("t"),
        name_filter
    );
    let labels_on_shelf: i64 = c.query_row(
        &format!("SELECT COUNT(DISTINCT r.label_id) FROM releases r JOIN tracks t ON t.release_id = r.id WHERE {base_where}"),
        params_from_iter(params.iter()),
        |r| r.get(0),
    )?;
    if labels_on_shelf == 0 {
        return Ok(Page { items: vec![], total: 0, offset: 0, limit });
    }
    let per_label = ((limit + labels_on_shelf - 1) / labels_on_shelf).max(1);
    let sql = format!(
        "SELECT tid FROM (SELECT t.id AS tid, ROW_NUMBER() OVER (PARTITION BY r.label_id ORDER BY random()) AS rn
                           FROM releases r JOIN tracks t ON t.release_id = r.id WHERE {base_where})
          WHERE rn <= {per_label} ORDER BY random() LIMIT {limit}"
    );
    let mut st = c.prepare(&sql)?;
    let ids: Vec<i64> = st.query_map(params_from_iter(params.iter()), |r| r.get(0))?.collect::<Result<_, _>>()?;
    let items = hydrate::tracks_out(c, &ids)?;
    let n = items.len() as i64;
    Ok(Page { items, total: n, offset: 0, limit })
}

/// `PATCH /labels/{id}`: rename (merging onto an existing label of the same `name_key`) and/or set the
/// Bandcamp page. The inbox's `label_name` evidence is renamed with it: it is what the label backfill files
/// from, and left stale the next import/backfill would quietly recreate the old spelling.
pub fn patch_label(ctx: &Ctx, id: i64, patch: LabelPatch) -> ApiResult<()> {
    ctx.write(move |t| {
        let (cur_name, cur_key, cur_url): (String, String, Option<String>) = t
            .query_row("SELECT name, name_key, bandcamp_url FROM labels WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?
            .ok_or_else(|| ApiError::not_found(format!("label {id} not found")))?;
        let mut url = cur_url;
        let mut label_id = id;
        if let Some(raw) = patch.bandcamp_url.as_deref() {
            let raw = raw.trim();
            if raw.is_empty() {
                t.execute("UPDATE labels SET bandcamp_url = NULL WHERE id = ?1", [id])?;
                url = None;
            } else {
                let canonical = crate::urls::normalise(raw);
                let other: Option<String> = t
                    .query_row("SELECT name FROM labels WHERE bandcamp_url = ?1 AND id != ?2", params![canonical, id], |r| r.get(0))
                    .optional()?;
                if let Some(o) = other {
                    return Err(ApiError::bad(format!("\u{201c}{o}\u{201d} already uses that URL")));
                }
                t.execute("UPDATE labels SET bandcamp_url = ?1 WHERE id = ?2", params![canonical, id])?;
                url = Some(canonical);
            }
        }
        if let Some(name) = patch.name.as_deref() {
            let name = name.trim();
            if name.is_empty() {
                return Err(ApiError::bad("a label needs a name"));
            }
            let key = name_key(name);
            // Inbox spellings that currently resolve to this label.
            let old_names: Vec<String> = {
                let mut st = t.prepare("SELECT DISTINCT label_name FROM harvest_items WHERE label_name IS NOT NULL")?;
                let all: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
                all.into_iter().filter(|s| name_key(s) == cur_key).collect()
            };
            let other: Option<(i64, Option<String>)> =
                t.query_row("SELECT id, bandcamp_url FROM labels WHERE name_key = ?1 AND id != ?2", params![key, id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let final_name;
            if let Some((oid, ourl)) = other {
                t.execute("UPDATE releases SET label_id = ?1 WHERE label_id = ?2", params![oid, id])?;
                let moved = if ourl.is_none() { url.clone() } else { None };
                if moved.is_some() {
                    t.execute("UPDATE labels SET bandcamp_url = NULL WHERE id = ?1", [id])?;
                }
                t.execute("DELETE FROM labels WHERE id = ?1", [id])?;
                if let Some(u) = moved {
                    t.execute("UPDATE labels SET bandcamp_url = ?1 WHERE id = ?2", params![u, oid])?;
                }
                label_id = oid;
                final_name = t.query_row("SELECT name FROM labels WHERE id = ?1", [oid], |r| r.get::<_, String>(0))?;
            } else {
                t.execute("UPDATE labels SET name = ?1, name_key = ?2 WHERE id = ?3", params![name, key, id])?;
                final_name = name.to_string();
            }
            for old in old_names {
                t.execute("UPDATE harvest_items SET label_name = ?1 WHERE label_name = ?2", params![final_name, old])?;
            }
            let _ = cur_name;
        }
        let _ = label_id;
        Ok(())
    })
}

/// The id the label ends up with after a patch (a merge changes it).
pub fn label_id_after_patch(c: &Connection, id: i64, patch_name: Option<&str>) -> ApiResult<i64> {
    if c.query_row("SELECT 1 FROM labels WHERE id = ?1", [id], |_| Ok(())).is_ok() {
        return Ok(id);
    }
    if let Some(n) = patch_name {
        let key = name_key(n);
        if let Some(i) = c.query_row("SELECT id FROM labels WHERE name_key = ?1", [key], |r| r.get::<_, i64>(0)).optional()? {
            return Ok(i);
        }
    }
    Err(ApiError::not_found(format!("label {id} not found")))
}
