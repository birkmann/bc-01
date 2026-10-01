//! Row hydration shared by every router: `TrackOut` / `ReleaseOut` for a list of ids, in the
//! order given, with a fixed number of queries per page (never per row).

use std::collections::HashMap;

use bc_db::rusqlite::{Connection, params};
use bc_db::util::iso_opt;
use bc_types::library::{ArtistRef, FileRef, ReleaseOut, ReleaseRef, TrackOut};

use crate::error::ApiResult;

/// Tags carried on a listed release: enough for a card line, not a tag cloud.
pub const CARD_TAGS: usize = 6;

const KEY_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

pub fn key_name(root: Option<i64>, mode: Option<&str>) -> Option<String> {
    let (r, m) = (root?, mode?);
    let name = KEY_NAMES.get(usize::try_from(r).ok()?)?;
    Some(format!("{name}{}", if m == "minor" { "m" } else { "" }))
}

/// `/api/art/release/{id}?size=thumb|full&v=<version>` (immutable caching via `v`).
pub fn art_url(release_id: i64, version: Option<&str>, thumb: bool) -> String {
    format!(
        "/api/art/release/{release_id}?size={}&v={}",
        if thumb { "thumb" } else { "full" },
        version.unwrap_or("0")
    )
}

pub fn stream_url(track_id: i64) -> String {
    format!("/api/stream/{track_id}")
}

fn ids_json(ids: &[i64]) -> String {
    serde_json::to_string(ids).unwrap_or_else(|_| "[]".into())
}

/// Tracks for `ids`, in the order of `ids` (unknown ids are skipped).
pub fn tracks_out(c: &Connection, ids: &[i64]) -> ApiResult<Vec<TrackOut>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let json = ids_json(ids);
    let mut by_id: HashMap<i64, TrackOut> = HashMap::with_capacity(ids.len());
    {
        let mut st = c.prepare_cached(
            "SELECT t.id, t.title,
                    COALESCE(ta.id, ra.id), COALESCE(ta.name, ra.name),
                    t.release_id, r.title, r.year, r.cover_path IS NOT NULL, aw.version, aw.blurhash, aw.color,
                    r.label_id, r.source_fan_id, r.snippet_only,
                    t.track_no, t.disc_no, t.duration_ms, t.rating, t.loved, t.play_count, t.last_played_at, t.added_at,
                    an.bpm, an.camelot, an.key_root, an.key_mode, an.energy,
                    f.id, f.ext, f.codec, f.bitrate, f.size_bytes, f.missing_since,
                    t.is_snippet
               FROM tracks t
               LEFT JOIN artists ta ON ta.id = t.artist_id
               LEFT JOIN releases r ON r.id = t.release_id
               LEFT JOIN artists ra ON ra.id = r.artist_id
               LEFT JOIN artwork aw ON aw.release_id = r.id
               LEFT JOIN analysis an ON an.track_id = t.id
               LEFT JOIN files f ON f.id = (SELECT id FROM files WHERE track_id = t.id ORDER BY (missing_since IS NOT NULL), id LIMIT 1)
              WHERE t.id IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = st.query_map([&json], |r| {
            let id: i64 = r.get(0)?;
            let release_id: Option<i64> = r.get(4)?;
            let has_cover: bool = r.get::<_, Option<bool>>(7)?.unwrap_or(false);
            let version: Option<String> = r.get(8)?;
            let art = release_id.filter(|_| has_cover).map(|rid| art_url(rid, version.as_deref(), true));
            let artist = r.get::<_, Option<i64>>(2)?.map(|aid| ArtistRef { id: aid, name: r.get::<_, String>(3).unwrap_or_default() });
            let release = match release_id {
                Some(rid) => Some(ReleaseRef {
                    id: rid,
                    title: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    year: r.get(6)?,
                    art_url: art.clone(),
                    art_blurhash: r.get(9)?,
                    art_color: r.get(10)?,
                    label_id: r.get(11)?,
                    source_fan_id: r.get(12)?,
                    snippet_only: r.get::<_, Option<bool>>(13)?.unwrap_or(false),
                }),
                None => None,
            };
            let file = match r.get::<_, Option<i64>>(27)? {
                Some(fid) => Some(FileRef {
                    id: fid,
                    ext: r.get(28)?,
                    codec: r.get(29)?,
                    bitrate: r.get(30)?,
                    size_bytes: r.get(31)?,
                    missing: r.get::<_, Option<String>>(32)?.is_some(),
                }),
                None => None,
            };
            let root: Option<i64> = r.get(24)?;
            let mode: Option<String> = r.get(25)?;
            Ok(TrackOut {
                id,
                title: r.get(1)?,
                artist,
                release,
                track_no: r.get(14)?,
                disc_no: r.get(15)?,
                duration_ms: r.get(16)?,
                tags: vec![],
                rating: r.get(17)?,
                loved: r.get(18)?,
                play_count: r.get(19)?,
                last_played_at: iso_opt(r.get(20)?),
                added_at: iso_opt(r.get(21)?),
                bpm: r.get(22)?,
                camelot: r.get(23)?,
                key: key_name(root, mode.as_deref()),
                energy: r.get(26)?,
                file,
                stream_url: stream_url(id),
                art_url: art,
                is_snippet: r.get(33)?,
                item_id: None,
            })
        })?;
        for t in rows {
            let t = t?;
            by_id.insert(t.id, t);
        }
    }
    {
        let mut st = c.prepare_cached(
            "SELECT tt.track_id, g.name FROM track_tags tt JOIN tags g ON g.id = tt.tag_id
              WHERE tt.track_id IN (SELECT value FROM json_each(?1)) ORDER BY tt.track_id, g.id",
        )?;
        let mut rows = st.query([&json])?;
        while let Some(r) = rows.next()? {
            let (tid, name): (i64, String) = (r.get(0)?, r.get(1)?);
            if let Some(t) = by_id.get_mut(&tid)
                && !t.tags.contains(&name)
            {
                t.tags.push(name);
            }
        }
    }
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

pub fn track_out(c: &Connection, id: i64) -> ApiResult<Option<TrackOut>> {
    Ok(tracks_out(c, &[id])?.pop())
}

// ---------------------------------------------------------------------------------------
// Releases
// ---------------------------------------------------------------------------------------

/// SQL boolean over `r` (releases) and `have` (the per-release `n`/`top` aggregate): the record
/// is short of tracks *and* a fill could repair it (port of `completeness.only_missing`). A
/// pre-order whose every missing track is still unreleased (cached `release_availability`, date
/// ahead or unknown) is not short: nothing can be filled until the date passes.
pub const MISSING_COND: &str = "(CASE WHEN r.expected_track_count IS NOT NULL
             THEN r.expected_track_count > COALESCE(have.n, 0)
             ELSE (COALESCE(have.top, 0) > COALESCE(have.n, 0)
                   AND NOT (COALESCE(have.n, 0) = 1 AND r.kind IS NOT 'single'
                            AND COALESCE(r.bandcamp_url, '') NOT LIKE '%/album/%'
                            AND (COALESCE(r.bandcamp_url, '') LIKE '%/track/%' OR COALESCE(have.top, 0) > 1))) END)
        AND (r.bandcamp_url IS NOT NULL
             OR EXISTS (SELECT 1 FROM harvest_items h WHERE h.release_id = r.id)
             OR EXISTS (SELECT 1 FROM tracks tb WHERE tb.release_id = r.id AND tb.bandcamp_url IS NOT NULL))
        AND NOT EXISTS (SELECT 1 FROM release_availability av
                         WHERE av.release_id = r.id AND av.is_preorder = 1 AND av.unreleased_count > 0
                           AND (av.release_date IS NULL OR av.release_date > date('now'))
                           AND COALESCE(r.expected_track_count, have.top, 0) - COALESCE(have.n, 0) <= av.unreleased_count)";

/// Releases short of tracks (all, or among `ids`).
pub fn missing_release_ids(c: &Connection, ids: Option<&[i64]>) -> ApiResult<Vec<i64>> {
    let (agg_where, outer_where, p): (String, String, Option<String>) = match ids {
        Some([]) => return Ok(vec![]),
        Some(ids) => (
            "WHERE release_id IN (SELECT value FROM json_each(?1))".into(),
            "AND r.id IN (SELECT value FROM json_each(?1))".into(),
            Some(ids_json(ids)),
        ),
        None => (String::new(), String::new(), None),
    };
    let sql = format!(
        "SELECT r.id FROM releases r
           LEFT JOIN (SELECT release_id, COUNT(*) AS n, COALESCE(MAX(track_no), 0) AS top FROM tracks {agg_where} GROUP BY release_id) have
                  ON have.release_id = r.id
          WHERE {MISSING_COND} {outer_where} ORDER BY r.id"
    );
    let mut st = c.prepare(&sql)?;
    let rows = match p {
        Some(j) => st.query_map([j], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?,
        None => st.query_map([], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?,
    };
    Ok(rows)
}

/// Release cards for `ids` in the given order. `detailed_tags` > CARD_TAGS lets the single-release
/// page show up to 12 tags.
pub fn releases_out(c: &Connection, ids: &[i64]) -> ApiResult<Vec<ReleaseOut>> {
    releases_out_n(c, ids, CARD_TAGS)
}

pub fn releases_out_n(c: &Connection, ids: &[i64], max_tags: usize) -> ApiResult<Vec<ReleaseOut>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let json = ids_json(ids);
    let short: std::collections::HashSet<i64> = missing_release_ids(c, Some(ids))?.into_iter().collect();
    let mut counts: HashMap<i64, (i64, i64, i64)> = HashMap::new(); // n, top_no, duration
    {
        let mut st = c.prepare_cached(
            "SELECT release_id, COUNT(*), COALESCE(MAX(track_no), 0), COALESCE(SUM(duration_ms), 0)
               FROM tracks WHERE release_id IN (SELECT value FROM json_each(?1)) GROUP BY release_id",
        )?;
        let mut rows = st.query([&json])?;
        while let Some(r) = rows.next()? {
            counts.insert(r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?));
        }
    }
    // Most-used first so a card with room for two or three shows what describes the release.
    let mut tags: HashMap<i64, Vec<String>> = HashMap::new();
    {
        let mut st = c.prepare_cached(
            "SELECT t.release_id, g.name FROM tracks t
               JOIN track_tags tt ON tt.track_id = t.id JOIN tags g ON g.id = tt.tag_id
              WHERE t.release_id IN (SELECT value FROM json_each(?1))
              GROUP BY t.release_id, g.id ORDER BY t.release_id, COUNT(*) DESC, g.id",
        )?;
        let mut rows = st.query([&json])?;
        while let Some(r) = rows.next()? {
            let v = tags.entry(r.get(0)?).or_default();
            if v.len() < max_tags {
                v.push(r.get(1)?);
            }
        }
    }
    // Cached pre-order knowledge only (no network per card). Counted while the date is ahead or unknown.
    let mut waiting: HashMap<i64, (Option<String>, Vec<bc_types::library::TrackAvailability>)> = HashMap::new();
    {
        let mut st = c.prepare_cached(
            "SELECT release_id, release_date, tracks FROM release_availability
              WHERE release_id IN (SELECT value FROM json_each(?1)) AND is_preorder = 1
                AND (release_date IS NULL OR release_date > date('now'))",
        )?;
        let mut rows = st.query([&json])?;
        while let Some(r) = rows.next()? {
            let tracks: Vec<bc_types::library::TrackAvailability> = serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default();
            waiting.insert(r.get(0)?, (r.get(1)?, tracks));
        }
    }
    let mut by_id: HashMap<i64, ReleaseOut> = HashMap::with_capacity(ids.len());
    {
        let mut st = c.prepare_cached(
            "SELECT r.id, r.title, r.artist_id, a.name, l.name, r.label_id, r.year, r.kind, r.expected_track_count,
                    r.cover_path IS NOT NULL, aw.version, aw.blurhash, aw.color, r.added_at, r.bandcamp_url, r.source_fan_id, r.snippet_only
               FROM releases r LEFT JOIN artists a ON a.id = r.artist_id LEFT JOIN labels l ON l.id = r.label_id
               LEFT JOIN artwork aw ON aw.release_id = r.id
              WHERE r.id IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = st.query_map([&json], |r| {
            let id: i64 = r.get(0)?;
            let (n, top, dur) = counts.get(&id).copied().unwrap_or((0, 0, 0));
            let expected: Option<i64> = r.get(8)?;
            let is_short = short.contains(&id);
            // The stored count is the authority; the numbering only stands in for releases nothing measured.
            let expected_out = if !is_short { expected } else { expected.or(if top > 0 { Some(top) } else { None }) };
            let has_cover: bool = r.get(9)?;
            let version: Option<String> = r.get(10)?;
            let gap = expected_out.map(|e| (e - n).max(0)).unwrap_or(0);
            let (pre_date, mut unreleased_tracks) = match waiting.get(&id) {
                Some((d, t)) => (d.clone(), t.iter().filter(|t| !t.available).cloned().collect::<Vec<_>>()),
                None => (None, vec![]),
            };
            let is_preorder = waiting.contains_key(&id);
            unreleased_tracks.sort_by_key(|t| t.track_num.unwrap_or(i64::MAX));
            // Never claim more unreleased tracks than the library lacks.
            let unreleased_count = if expected_out.is_some() { (unreleased_tracks.len() as i64).min(gap) } else { unreleased_tracks.len() as i64 };
            let fillable_missing = if is_short { (gap - unreleased_count).max(0) } else { 0 };
            Ok(ReleaseOut {
                id,
                title: r.get(1)?,
                artist: r.get::<_, Option<i64>>(2)?.map(|aid| ArtistRef { id: aid, name: r.get::<_, String>(3).unwrap_or_default() }),
                label: r.get(4)?,
                label_id: r.get(5)?,
                year: r.get(6)?,
                kind: r.get(7)?,
                track_count: n,
                expected_track_count: expected_out,
                duration_ms: dur,
                art_url: has_cover.then(|| art_url(id, version.as_deref(), false)),
                art_blurhash: r.get(11)?,
                art_color: r.get(12)?,
                tags: tags.remove(&id).unwrap_or_default(),
                added_at: iso_opt(r.get(13)?),
                bandcamp_url: r.get(14)?,
                source_fan_id: r.get(15)?,
                snippet_only: r.get(16)?,
                is_preorder,
                release_date: pre_date,
                unreleased_count,
                fillable_missing,
                unreleased_tracks,
            })
        })?;
        for r in rows {
            let r = r?;
            by_id.insert(r.id, r);
        }
    }
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// Existence probe used by routes before heavy work.
pub fn exists(c: &Connection, table: &str, id: i64) -> ApiResult<bool> {
    // table is an internal constant at every call site
    let sql = format!("SELECT 1 FROM {table} WHERE id = ?1");
    Ok(c.query_row(&sql, params![id], |_| Ok(())).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_db::Db;

    #[test]
    fn key_names() {
        assert_eq!(key_name(Some(9), Some("minor")).as_deref(), Some("Am"));
        assert_eq!(key_name(Some(1), Some("major")).as_deref(), Some("C#"));
        assert_eq!(key_name(None, Some("major")), None);
    }

    #[test]
    fn hydrates_tracks_and_releases_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        db.write(|t| {
            t.execute_batch(
                "INSERT INTO artists(id,name,name_key,created_at) VALUES (1,'Art','art','x');
                 INSERT INTO releases(id,title,title_key,artist_id,kind,year,added_at,cover_path,expected_track_count,bandcamp_url)
                      VALUES (1,'Rel','rel',1,'album',2020,'2020-01-02 03:04:05.123456','/c.jpg',3,'https://a.bandcamp.com/album/rel');
                 INSERT INTO artwork(release_id,version,blurhash,color) VALUES (1,'abc','LEHV6n','#112233');
                 INSERT INTO tags(id,name,name_key,kind,track_count) VALUES (1,'Techno','techno','genre',2),(2,'Dub','dub','genre',1);
                 INSERT INTO tracks(id,release_id,title,title_key,track_no,loved,play_count,skip_count,added_at,is_snippet) VALUES
                     (1,1,'One','one',1,0,0,0,'x',0),(2,1,'Two','two',2,1,3,0,'x',0);
                 INSERT INTO track_tags(track_id,tag_id,source,weight) VALUES (1,1,'file',1),(1,2,'file',1),(2,1,'file',1);
                 INSERT INTO library_roots(id,path,kind,watch,enabled) VALUES (1,'/r','library',0,1);
                 INSERT INTO files(id,track_id,root_id,path,rel_path,ext,size_bytes,mtime_ns,first_seen_at,last_seen_at) VALUES (1,1,1,'/r/1.mp3','1.mp3','mp3',10,1,'x','x'),(2,2,1,'/r/2.mp3','2.mp3','mp3',10,1,'x','x');
                 INSERT INTO analysis(track_id,analyzer_version,backend,status,analyzed_at,bpm,camelot,key_root,key_mode,energy) VALUES (1,1,'e','ok','x',128.0,'8A',9,'minor',0.5);",
            )?;
            Ok(())
        })
        .unwrap();
        db.read(|c| {
            let v = tracks_out(c, &[2, 1, 99]).unwrap();
            assert_eq!(v.iter().map(|t| t.id).collect::<Vec<_>>(), vec![2, 1]);
            let t1 = &v[1];
            assert_eq!(t1.tags, vec!["Techno", "Dub"]);
            assert_eq!(t1.key.as_deref(), Some("Am"));
            assert_eq!(t1.artist.as_ref().unwrap().name, "Art");
            assert_eq!(t1.art_url.as_deref(), Some("/api/art/release/1?size=thumb&v=abc"));
            assert_eq!(t1.release.as_ref().unwrap().art_blurhash.as_deref(), Some("LEHV6n"));
            assert_eq!(t1.added_at.as_deref(), Some("xZ"));
            assert!(v[0].loved);
            let r = releases_out(c, &[1]).unwrap();
            assert_eq!(r[0].track_count, 2);
            assert_eq!(r[0].expected_track_count, Some(3), "expected 3, have 2 -> short");
            assert_eq!(r[0].tags[0], "Techno");
            assert_eq!(r[0].added_at.as_deref(), Some("2020-01-02T03:04:05.123456Z"));
            assert_eq!(missing_release_ids(c, None).unwrap(), vec![1]);
            Ok(())
        })
        .unwrap();
    }
}
