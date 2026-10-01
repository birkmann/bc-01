//! `/artists`: listing, detail, related, patch.

use std::collections::HashMap;

use bc_db::rusqlite::{Connection, OptionalExtension, params_from_iter, types::Value};
use bc_libcore::{ApiError, ApiResult, Scope, hydrate};
use bc_types::Page;
use bc_types::library::*;

use crate::releases::{ShelfBuilder, shelves_by_profile};

pub const SIMILAR_ARTISTS: i64 = 12;
pub const ARTIST_LABEL_SHELVES: i64 = 2;

fn json_ids(ids: &[i64]) -> String {
    serde_json::to_string(ids).unwrap_or_else(|_| "[]".into())
}

/// A cover to stand in for each artist's face: their own earliest cover, else the cover of a release
/// one of their tracks appears on. Returns (art_url, blurhash).
pub fn artist_art(c: &Connection, ids: &[i64]) -> ApiResult<HashMap<i64, (String, Option<String>)>> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let json = json_ids(ids);
    let mut cover_of: HashMap<i64, i64> = HashMap::new();
    {
        let mut st = c.prepare(
            "SELECT artist_id, MIN(id) FROM releases WHERE artist_id IN (SELECT value FROM json_each(?1)) AND cover_path IS NOT NULL GROUP BY artist_id",
        )?;
        let mut rows = st.query([&json])?;
        while let Some(r) = rows.next()? {
            cover_of.insert(r.get(0)?, r.get(1)?);
        }
    }
    let missing: Vec<i64> = ids.iter().copied().filter(|i| !cover_of.contains_key(i)).collect();
    if !missing.is_empty() {
        let mut st = c.prepare(
            "SELECT t.artist_id, MIN(r.id) FROM tracks t JOIN releases r ON r.id = t.release_id
              WHERE t.artist_id IN (SELECT value FROM json_each(?1)) AND r.cover_path IS NOT NULL GROUP BY t.artist_id",
        )?;
        let mut rows = st.query([json_ids(&missing)])?;
        while let Some(r) = rows.next()? {
            cover_of.insert(r.get(0)?, r.get(1)?);
        }
    }
    let rels: Vec<i64> = cover_of.values().copied().collect();
    let mut art: HashMap<i64, (Option<String>, Option<String>)> = HashMap::new();
    {
        let mut st = c.prepare("SELECT release_id, version, blurhash FROM artwork WHERE release_id IN (SELECT value FROM json_each(?1))")?;
        let mut rows = st.query([json_ids(&rels)])?;
        while let Some(r) = rows.next()? {
            art.insert(r.get(0)?, (r.get(1)?, r.get(2)?));
        }
    }
    for (aid, rid) in cover_of {
        let (v, bh) = art.get(&rid).cloned().unwrap_or((None, None));
        out.insert(aid, (hydrate::art_url(rid, v.as_deref(), false), bh));
    }
    Ok(out)
}

/// Cards for `ids`, in the order given: counts and a face for each, batched.
pub fn artists_out(c: &Connection, ids: &[i64], scope: &Scope, play_sums: &HashMap<i64, i64>) -> ApiResult<Vec<ArtistOut>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let json = json_ids(ids);
    let mut names: HashMap<i64, String> = HashMap::new();
    {
        let mut st = c.prepare("SELECT id, name FROM artists WHERE id IN (SELECT value FROM json_each(?1))")?;
        let mut rows = st.query([&json])?;
        while let Some(r) = rows.next()? {
            names.insert(r.get(0)?, r.get(1)?);
        }
    }
    let mut releases: HashMap<i64, i64> = HashMap::new();
    {
        let sql = format!(
            "SELECT r.artist_id, COUNT(*) FROM releases r WHERE r.artist_id IN (SELECT value FROM json_each(?1)){} GROUP BY r.artist_id",
            scope.and_release("r")
        );
        let mut st = c.prepare(&sql)?;
        let mut rows = st.query([&json])?;
        while let Some(r) = rows.next()? {
            releases.insert(r.get(0)?, r.get(1)?);
        }
    }
    // Credits plus ownership: the same union /tracks?artist_id= filters by, so a card never says
    // "0 tracks" for an artist whose page lists plenty.
    let mut tracks: HashMap<i64, i64> = HashMap::new();
    {
        let sql = format!(
            "SELECT aid, COUNT(*) FROM (
                SELECT t.artist_id AS aid, t.id AS tid FROM tracks t WHERE t.artist_id IN (SELECT value FROM json_each(?1)){sc}
                UNION
                SELECT r.artist_id, t.id FROM releases r JOIN tracks t ON t.release_id = r.id
                 WHERE r.artist_id IN (SELECT value FROM json_each(?1)){sc}
             ) GROUP BY aid",
            sc = scope.and_track("t")
        );
        let mut st = c.prepare(&sql)?;
        let mut rows = st.query([&json])?;
        while let Some(r) = rows.next()? {
            tracks.insert(r.get(0)?, r.get(1)?);
        }
    }
    let art = artist_art(c, ids)?;
    Ok(ids
        .iter()
        .filter_map(|id| {
            let name = names.get(id)?.clone();
            let (art_url, art_blurhash) = match art.get(id) {
                Some((u, b)) => (Some(u.clone()), b.clone()),
                None => (None, None),
            };
            Some(ArtistOut {
                id: *id,
                name,
                release_count: releases.get(id).copied().unwrap_or(0),
                track_count: tracks.get(id).copied().unwrap_or(0),
                play_count: play_sums.get(id).copied().unwrap_or(0),
                art_url,
                art_blurhash,
            })
        })
        .collect())
}

pub fn list_artists(c: &Connection, q: &ArtistQuery, scope: &Scope) -> ApiResult<Page<ArtistOut>> {
    let sort = q.sort.unwrap_or_default();
    let order = q.order.unwrap_or(if sort == ArtistSort::Name { SortDir::Asc } else { SortDir::Desc });
    let d = if order == SortDir::Desc { "DESC" } else { "ASC" };
    let offset = q.offset.unwrap_or(0).max(0);
    let limit = q.limit.unwrap_or(100).clamp(1, 500);

    let mut conds: Vec<String> = vec![];
    let mut params: Vec<Value> = vec![];
    if let Some(p) = scope.artist_pred("a") {
        conds.push(p);
    }
    if let Some(text) = q.q.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        match bc_db::fts::trigram_phrase(text) {
            Some(phrase) => {
                params.push(Value::Text(phrase));
                conds.push(format!("a.id IN (SELECT rowid FROM artist_search WHERE artist_search MATCH ?{})", params.len()));
            }
            None => {
                params.push(Value::Text(format!("%{}%", text.to_lowercase())));
                conds.push(format!("lower(a.name) LIKE ?{}", params.len()));
            }
        }
    }
    let mut joins = String::new();
    let order_sql = match sort {
        ArtistSort::Plays => {
            joins.push_str(&format!(
                " JOIN (SELECT t.artist_id AS aid, SUM(t.play_count) AS plays FROM tracks t WHERE t.artist_id IS NOT NULL{} GROUP BY t.artist_id HAVING plays > 0) pl ON pl.aid = a.id",
                scope.and_track("t")
            ));
            format!("pl.plays {d}, a.id")
        }
        ArtistSort::Releases | ArtistSort::Added => {
            joins.push_str(&format!(
                " LEFT JOIN (SELECT r.artist_id AS aid, COUNT(*) AS rc, MAX(r.added_at) AS la FROM releases r WHERE r.artist_id IS NOT NULL{} GROUP BY r.artist_id) rl ON rl.aid = a.id",
                scope.and_release("r")
            ));
            if sort == ArtistSort::Releases { format!("COALESCE(rl.rc, 0) {d}, a.id") } else { format!("rl.la {d}, a.id") }
        }
        ArtistSort::Tracks => {
            joins.push_str(&format!(
                " LEFT JOIN (SELECT aid, COUNT(*) AS tc FROM (
                      SELECT t.artist_id AS aid, t.id AS tid FROM tracks t WHERE t.artist_id IS NOT NULL{sc}
                      UNION SELECT r.artist_id, t.id FROM releases r JOIN tracks t ON t.release_id = r.id WHERE r.artist_id IS NOT NULL{sc}
                   ) GROUP BY aid) tk ON tk.aid = a.id",
                sc = scope.and_track("t")
            ));
            format!("COALESCE(tk.tc, 0) {d}, a.id")
        }
        ArtistSort::Name => format!("a.name_key {d}, a.id"),
    };
    let wh = if conds.is_empty() { "1=1".into() } else { conds.join(" AND ") };
    let total: i64 = c.query_row(&format!("SELECT COUNT(*) FROM artists a{joins} WHERE {wh}"), params_from_iter(params.iter()), |r| r.get(0))?;
    let mut st = c.prepare(&format!("SELECT a.id FROM artists a{joins} WHERE {wh} ORDER BY {order_sql} LIMIT {limit} OFFSET {offset}"))?;
    let ids: Vec<i64> = st.query_map(params_from_iter(params.iter()), |r| r.get(0))?.collect::<Result<_, _>>()?;

    let mut plays = HashMap::new();
    if sort == ArtistSort::Plays && !ids.is_empty() {
        let sql = format!(
            "SELECT t.artist_id, SUM(t.play_count) FROM tracks t WHERE t.artist_id IN (SELECT value FROM json_each(?1)){} GROUP BY t.artist_id",
            scope.and_track("t")
        );
        let mut st = c.prepare(&sql)?;
        let mut rows = st.query([json_ids(&ids)])?;
        while let Some(r) = rows.next()? {
            plays.insert(r.get::<_, i64>(0)?, r.get::<_, i64>(1)?);
        }
    }
    Ok(Page { items: artists_out(c, &ids, scope, &plays)?, total, offset, limit })
}

const ARTIST_TRACKS: &str = "t.id IN (SELECT id FROM tracks WHERE artist_id = ?1 UNION SELECT id FROM tracks WHERE release_id IN (SELECT id FROM releases WHERE artist_id = ?1))";

pub fn get_artist(c: &Connection, id: i64, scope: &Scope) -> ApiResult<ArtistDetailOut> {
    let (name, url, location): (String, Option<String>, Option<String>) = c
        .query_row("SELECT name, bandcamp_url, location FROM artists WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?
        .ok_or_else(|| ApiError::not_found(format!("artist {id} not found")))?;
    let release_count: i64 = c.query_row(
        &format!("SELECT COUNT(*) FROM releases r WHERE r.artist_id = ?1{}", scope.and_release("r")),
        [id],
        |r| r.get(0),
    )?;
    let track_count: i64 = c.query_row(
        &format!("SELECT COUNT(*) FROM tracks t WHERE {ARTIST_TRACKS}{}", scope.and_track("t")),
        [id],
        |r| r.get(0),
    )?;
    // Own credits only, matching the `sort=plays` attribution in the listing.
    let play_count: i64 = c.query_row(
        &format!("SELECT COALESCE(SUM(t.play_count), 0) FROM tracks t WHERE t.artist_id = ?1{}", scope.and_track("t")),
        [id],
        |r| r.get(0),
    )?;
    let (ymin, ymax): (Option<i64>, Option<i64>) = c.query_row(
        &format!("SELECT MIN(r.year), MAX(r.year) FROM releases r WHERE r.artist_id = ?1 AND r.year IS NOT NULL{}", scope.and_release("r")),
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let labels: Vec<ArtistLabelOut> = {
        let mut st = c.prepare(&format!(
            "SELECT l.id, l.name, COUNT(*) FROM labels l JOIN releases r ON r.label_id = l.id WHERE r.artist_id = ?1{} GROUP BY l.id ORDER BY COUNT(*) DESC, l.name_key",
            scope.and_release("r")
        ))?;
        st.query_map([id], |r| Ok(ArtistLabelOut { id: r.get(0)?, name: r.get(1)?, release_count: r.get(2)? }))?.collect::<Result<_, _>>()?
    };
    let tags: Vec<ArtistTagOut> = {
        let mut st = c.prepare(&format!(
            "SELECT g.id, g.name, COUNT(*) FROM tags g JOIN track_tags tt ON tt.tag_id = g.id JOIN tracks t ON t.id = tt.track_id
              WHERE {ARTIST_TRACKS}{} GROUP BY g.id ORDER BY COUNT(*) DESC, g.track_count ASC, g.name_key LIMIT 12",
            scope.and_track("t")
        ))?;
        st.query_map([id], |r| Ok(ArtistTagOut { id: r.get(0)?, name: r.get(1)?, count: r.get(2)? }))?.collect::<Result<_, _>>()?
    };
    let art = artist_art(c, &[id])?.remove(&id).map(|a| a.0);
    Ok(ArtistDetailOut {
        id,
        name,
        bandcamp_url: url,
        location,
        release_count,
        track_count,
        play_count,
        year_min: ymin,
        year_max: ymax,
        art_url: art,
        labels,
        tags,
    })
}

pub fn artist_related(c: &Connection, id: i64, limit: i64, scope: &Scope) -> ApiResult<ArtistRelatedOut> {
    if !hydrate::exists(c, "artists", id)? {
        return Err(ApiError::not_found(format!("artist {id} not found")));
    }
    let profile: Vec<(i64, String, i64)> = {
        let mut st = c.prepare(&format!(
            "SELECT g.id, g.name, g.track_count FROM tags g JOIN track_tags tt ON tt.tag_id = g.id JOIN tracks t ON t.id = tt.track_id
              WHERE {ARTIST_TRACKS} GROUP BY g.id ORDER BY COUNT(*) DESC, g.track_count ASC, g.name_key LIMIT {}",
            crate::releases::PROFILE_TAGS
        ))?;
        st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?
    };
    let profile = crate::releases::trim_profile(c, profile);
    let profile_ids: Vec<i64> = profile.iter().map(|t| t.0).collect();
    let mut similar = vec![];
    if !profile_ids.is_empty() {
        let ids_csv = profile_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
        let rows: Vec<i64> = {
            let mut st = c.prepare(&format!(
                "SELECT t.artist_id FROM tracks t JOIN track_tags tt ON tt.track_id = t.id
                  WHERE tt.tag_id IN ({ids_csv}) AND t.artist_id IS NOT NULL AND t.artist_id != ?1
                  GROUP BY t.artist_id ORDER BY COUNT(DISTINCT tt.tag_id) DESC, COUNT(*) DESC LIMIT {SIMILAR_ARTISTS}"
            ))?;
            st.query_map([id], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        if !rows.is_empty() {
            let json = json_ids(&rows);
            let mut names = HashMap::new();
            let mut st = c.prepare("SELECT id, name FROM artists WHERE id IN (SELECT value FROM json_each(?1))")?;
            let mut it = st.query([&json])?;
            while let Some(r) = it.next()? {
                names.insert(r.get::<_, i64>(0)?, r.get::<_, String>(1)?);
            }
            let mut rc = HashMap::new();
            let mut st = c.prepare("SELECT artist_id, COUNT(*) FROM releases WHERE artist_id IN (SELECT value FROM json_each(?1)) GROUP BY artist_id")?;
            let mut it = st.query([&json])?;
            while let Some(r) = it.next()? {
                rc.insert(r.get::<_, i64>(0)?, r.get::<_, i64>(1)?);
            }
            let mut shared: HashMap<i64, Vec<String>> = HashMap::new();
            let mut st = c.prepare(&format!(
                "SELECT t.artist_id, g.name FROM tracks t JOIN track_tags tt ON tt.track_id = t.id JOIN tags g ON g.id = tt.tag_id
                  WHERE tt.tag_id IN ({ids_csv}) AND t.artist_id IN (SELECT value FROM json_each(?1))
                  GROUP BY t.artist_id, g.id ORDER BY t.artist_id, COUNT(*) DESC"
            ))?;
            let mut it = st.query([&json])?;
            while let Some(r) = it.next()? {
                shared.entry(r.get(0)?).or_default().push(r.get(1)?);
            }
            let art = artist_art(c, &rows)?;
            for aid in rows {
                if let Some(name) = names.get(&aid) {
                    similar.push(SimilarArtistOut {
                        id: aid,
                        name: name.clone(),
                        art_url: art.get(&aid).map(|a| a.0.clone()),
                        release_count: rc.get(&aid).copied().unwrap_or(0),
                        shared_tags: shared.remove(&aid).unwrap_or_default(),
                    });
                }
            }
        }
    }

    let own: std::collections::HashSet<i64> = {
        let mut st = c.prepare("SELECT id FROM releases WHERE artist_id = ?1")?;
        st.query_map([id], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    let mut b = ShelfBuilder { c, scope, limit, seen: own, groups: vec![] };
    if !profile.is_empty() {
        // suggested albums (profile-ranked, never the artist's own), then label shelves, then per-tag shelves.
        let first_len = b.groups.len();
        let tmp_profile = profile.clone();
        shelves_by_profile_head(&mut b, &tmp_profile, id)?;
        let _ = first_len;
    }
    let labels: Vec<(i64, String)> = {
        let mut st = c.prepare(&format!(
            "SELECT l.id, l.name FROM labels l JOIN releases r ON r.label_id = l.id WHERE r.artist_id = ?1 GROUP BY l.id
              ORDER BY COUNT(*) DESC, l.name_key LIMIT {ARTIST_LABEL_SHELVES}"
        ))?;
        st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
    };
    for (lid, lname) in labels {
        b.shelf(
            "label",
            &lname,
            &format!("More on {lname}"),
            Some(lid),
            "SELECT r.id FROM releases r WHERE r.label_id = ?1 AND (r.artist_id != ?2 OR r.artist_id IS NULL){SCOPE} ORDER BY r.year DESC, r.added_at DESC",
            vec![lid.into(), id.into()],
            vec![],
        )?;
    }
    if !profile.is_empty() {
        shelves_by_profile_tags(&mut b, &profile, id)?;
    }
    Ok(ArtistRelatedOut { similar_artists: similar, shelves: b.groups })
}

fn shelves_by_profile_head(b: &mut ShelfBuilder<'_>, profile: &[(i64, String, i64)], artist: i64) -> ApiResult<()> {
    let ids = profile.iter().map(|t| t.0.to_string()).collect::<Vec<_>>().join(",");
    let names: Vec<String> = profile.iter().map(|t| t.1.clone()).collect();
    let sql = format!(
        "SELECT r.id FROM releases r JOIN tracks t ON t.release_id = r.id JOIN track_tags tt ON tt.track_id = t.id \
          WHERE tt.tag_id IN ({ids}) AND (r.artist_id IS NULL OR r.artist_id != {artist}){{SCOPE}} GROUP BY r.id ORDER BY COUNT(DISTINCT tt.tag_id) DESC, r.added_at DESC"
    );
    b.shelf("tag", &names.join(", "), "Suggested albums", None, &sql, vec![], names)
}

fn shelves_by_profile_tags(b: &mut ShelfBuilder<'_>, profile: &[(i64, String, i64)], artist: i64) -> ApiResult<()> {
    let ids = profile.iter().map(|t| t.0.to_string()).collect::<Vec<_>>().join(",");
    let mut own: Vec<&(i64, String, i64)> = profile.iter().filter(|t| t.2 >= crate::releases::MIN_TAG_SHELF).collect();
    own.sort_by_key(|t| t.2);
    for t in own.into_iter().take(crate::releases::TAG_SHELVES) {
        let sql = format!(
            "SELECT r.id FROM releases r JOIN tracks t ON t.release_id = r.id JOIN track_tags tt ON tt.track_id = t.id \
              WHERE tt.tag_id IN ({ids}) AND (r.artist_id IS NULL OR r.artist_id != {artist}) \
                AND r.id IN (SELECT t2.release_id FROM track_tags x JOIN tracks t2 ON t2.id = x.track_id WHERE x.tag_id = {}){{SCOPE}} \
              GROUP BY r.id ORDER BY COUNT(DISTINCT tt.tag_id) DESC, r.added_at DESC",
            t.0
        );
        b.shelf("tag", &t.1, &format!("More {}", t.1), Some(t.0), &sql, vec![], vec![])?;
    }
    Ok(())
}

pub fn patch_artist(ctx: &bc_libcore::Ctx, id: i64, patch: ArtistPatch) -> ApiResult<()> {
    ctx.write(move |t| {
        let exists: bool = t.query_row("SELECT 1 FROM artists WHERE id = ?1", [id], |_| Ok(())).is_ok();
        if !exists {
            return Err(ApiError::not_found(format!("artist {id} not found")));
        }
        if let Some(name) = patch.name.as_deref() {
            let name = name.trim();
            if name.is_empty() {
                return Err(ApiError::bad("an artist needs a name"));
            }
            let key = bc_db::util::name_key(name);
            if key.is_empty() {
                return Err(ApiError::bad("an artist needs a name"));
            }
            let other: Option<String> = t
                .query_row("SELECT name FROM artists WHERE name_key = ?1 AND id != ?2", bc_db::rusqlite::params![key, id], |r| r.get(0))
                .optional()?;
            if let Some(o) = other {
                return Err(ApiError::conflict(format!("\u{201c}{o}\u{201d} already exists")));
            }
            t.execute("UPDATE artists SET name = ?1, name_key = ?2 WHERE id = ?3", bc_db::rusqlite::params![name, key, id])?;
        }
        if let Some(raw) = patch.bandcamp_url {
            let raw = raw.trim();
            if raw.is_empty() {
                t.execute("UPDATE artists SET bandcamp_url = NULL WHERE id = ?1", [id])?;
            } else {
                let canonical = crate::urls::normalise(raw);
                let other: Option<String> = t
                    .query_row("SELECT name FROM artists WHERE bandcamp_url = ?1 AND id != ?2", bc_db::rusqlite::params![canonical, id], |r| r.get(0))
                    .optional()?;
                if let Some(o) = other {
                    return Err(ApiError::bad(format!("\u{201c}{o}\u{201d} already uses that URL")));
                }
                t.execute("UPDATE artists SET bandcamp_url = ?1 WHERE id = ?2", bc_db::rusqlite::params![canonical, id])?;
            }
        }
        Ok(())
    })
}

// keep `shelves_by_profile` referenced for the release-side shelves
#[allow(dead_code)]
fn _use() {
    let _ = shelves_by_profile;
}
