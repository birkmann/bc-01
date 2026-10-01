//! `POST /playlists/{id}/similar`: a new playlist of the same length and the same kind of music
//! (port of `similar_playlist`, `_build_playlist` and `_unique_name`). Playlist CRUD is WS1's;
//! this module writes exactly one new playlist, in one transaction.

use std::collections::HashSet;

use bc_db::rusqlite::{OptionalExtension, Transaction, params};
use bc_music::ordering;
use bc_types::suggest::{SimilarPlaylistOut, SimilarPlaylistRequest};

use crate::error::{self, RecommendError, Result};
use crate::scope::Scope;
use crate::similar_to;
use crate::sqlutil::iso;

/// `base`, or `base 2`, `base 3`... whichever is free. The `LIKE` may match more names than it
/// means to (a name carrying `_` or `%`), which only ever widens the set of names avoided.
pub fn unique_name(t: &Transaction<'_>, base: &str) -> Result<String> {
    let mut st = t.prepare("SELECT name FROM playlists WHERE name LIKE ?1")?;
    let taken: HashSet<String> = st
        .query_map([format!("{base}%")], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<_, _>>()?;
    if !taken.contains(base) {
        return Ok(base.to_string());
    }
    let mut n = 2;
    while taken.contains(&format!("{base} {n}")) {
        n += 1;
    }
    Ok(format!("{base} {n}"))
}

/// A playlist and its items in one transaction, in the order given (legacy `_build_playlist`).
pub fn build_playlist(t: &Transaction<'_>, name: &str, description: Option<&str>, track_ids: &[i64]) -> Result<i64> {
    t.execute(
        "INSERT INTO playlists(name, description, kind, rules, created_at, updated_at) \
         VALUES (?1, ?2, 'manual', NULL, strftime('%Y-%m-%d %H:%M:%f','now'), strftime('%Y-%m-%d %H:%M:%f','now'))",
        params![name, description],
    )?;
    let id = t.last_insert_rowid();
    let mut ins = t.prepare(
        "INSERT INTO playlist_items(playlist_id, track_id, position, added_at) VALUES (?1, ?2, ?3, strftime('%Y-%m-%d %H:%M:%f','now'))",
    )?;
    for (i, tid) in track_ids.iter().enumerate() {
        ins.execute(params![id, tid, ordering::STEP * (i as f64 + 1.0)])?;
    }
    Ok(id)
}

fn playlist_out(t: &Transaction<'_>, id: i64) -> Result<SimilarPlaylistOut> {
    let (count, duration): (i64, i64) = t.query_row(
        "SELECT count(pi.id), COALESCE(sum(tr.duration_ms), 0) FROM playlist_items pi LEFT JOIN tracks tr ON tr.id = pi.track_id WHERE pi.playlist_id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
    )?;
    let (name, description, kind, created): (String, Option<String>, String, Option<String>) = t.query_row(
        "SELECT name, description, kind, created_at FROM playlists WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    Ok(SimilarPlaylistOut { id, name, description, kind, track_count: count, duration_ms: duration, art_url: None, created_at: iso(created) })
}

/// Another playlist of the same length and the same kind of music: not a copy and not a
/// reordering; the source describes a taste and the library is scored against it, minus
/// everything the source already holds. A fresh `shuffle_seed` draws a different set of the same
/// character. 404 for an unknown playlist; 400 when it is empty or nothing resembles it.
pub fn similar_playlist(db: &bc_db::Db, scope: &Scope, playlist_id: i64, req: &SimilarPlaylistRequest) -> Result<SimilarPlaylistOut> {
    let (source_name, picks) = error::read(db, |c| {
        let name: String = c
            .query_row("SELECT name FROM playlists WHERE id = ?1", [playlist_id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| RecommendError::not_found(format!("playlist {playlist_id} not found")))?;
        let mut st = c.prepare("SELECT track_id FROM playlist_items WHERE playlist_id = ?1 ORDER BY position")?;
        let source_ids: Vec<i64> = st.query_map([playlist_id], |r| r.get(0))?.collect::<std::result::Result<_, _>>()?;
        if source_ids.is_empty() {
            return Err(RecommendError::bad_request("an empty playlist says nothing to go on"));
        }
        let profile = similar_to::profile_of(c, &source_ids)?;
        let limit = match req.limit {
            Some(l) if l > 0 => l as usize,
            _ => source_ids.len(),
        };
        let exclude: HashSet<i64> = source_ids.iter().copied().collect();
        let picks = similar_to::draw(c, scope, &profile, &exclude, limit, req.shuffle_seed)?;
        if picks.is_empty() {
            return Err(RecommendError::bad_request("nothing else in the library resembles this playlist"));
        }
        Ok((name, picks))
    })?;

    let wanted = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(String::from).unwrap_or_else(|| format!("{source_name} (similar)"));
    let description = format!("Drawn from the library to sound like {source_name}");
    error::write(db, move |t| {
        let name = unique_name(t, &wanted)?;
        let id = build_playlist(t, &name, Some(&description), &picks)?;
        playlist_out(t, id)
    })
}
