//! Single tracks filed as records of their own (port of `services/library/strays.py`).
//!
//! Ask Bandcamp for a `/track/` URL and the file that comes back says ALBUM is the *track's*
//! title; the record it belongs to appears nowhere in the tags, though the track number quietly
//! keeps the position it holds on it. Ingest keys a release on (album artist, album title, year),
//! so every such file becomes a one-track release and the album is never assembled.
//!
//! Repairing one is three moves, the first needing the network ([`crate::lookup::BandcampLookup`]):
//!
//! 1. **Ask the track page which album it is from.** A page with no parent is a real standalone
//!    single and is marked `kind='single'` ([`mark_single`]) so it is never asked about again.
//! 2. **Rewrite the file.** The tags created the stray and the scanner re-reads them, so a repair
//!    that only moved rows would be undone by the next scan: album, album artist and "5/20" are
//!    corrected in place ([`Retagger`]); the file then moves into the album's folder, where a
//!    later fill drops the rest of the record beside it.
//! 3. **Re-point the track and drop the empty release.** The track row carries every play count,
//!    rating and loved flag, so moving it -- rather than re-ingesting -- preserves them.
//!
//! Where the album already exists the track joins it; otherwise the album release is created
//! from its page, holding the one track and knowing how many it should have ("1/12", which the
//! grid already offers to fill). [`StrayMerger`] runs the whole sweep as a background task.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bc_db::rusqlite::{Connection, OptionalExtension, Transaction};
use bc_db::util::{iso_now, name_key, now_db};
use bc_libcore::{ApiError, ApiResult, Ctx};
use bc_types::library::{StrayOut, StraySweepStatus, StraysOut, TOPIC_LIBRARY_STRAYS};
use parking_lot::Mutex;

use crate::dedup::{get_or_create_artist, get_or_create_label};
use crate::lookup::{AlbumInfo, BandcampLookup};
use crate::urls::{UrlKind, classify, normalise};
use crate::util::{canon, ids_json, root_paths, under_a_root};
use crate::{completeness, delete, snippets, tidy};

// ---------------------------------------------------------------------------- where bandcamp-dl would have put it

fn sanitize(text: &str) -> String {
    let mut kept = String::new();
    for c in text.chars() {
        if c.is_alphabetic() || c.is_numeric() || "-_~".contains(c) {
            kept.push(c);
        } else if c.is_whitespace() && !c.is_control() {
            kept.push(' ');
        }
    }
    kept.trim().to_string()
}

/// The directory name bandcamp-dl gives an album, reproduced exactly. A merged track has to land
/// where the *rest* of the record will land when the album is filled, or the album ends up split
/// across two folders. bandcamp-dl slugifies each part with `unicode_slugify`'s defaults: keep
/// letters, numbers and `-_~`, collapse runs of whitespace and hyphens into one hyphen,
/// lower-case, keep non-ASCII letters as they are. (A title like "???" slugifies to nothing.)
pub fn folder_slug(title: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let nfkc: String = title.nfkc().collect();
    let clean = sanitize(&nfkc);
    let mut out = String::new();
    let mut in_run = false;
    for c in clean.chars() {
        if c == '-' || c.is_whitespace() {
            if !in_run {
                out.push('-');
            }
            in_run = true;
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out.to_lowercase()
}

// ---------------------------------------------------------------------------- finding them

/// A one-track release that looks like it belongs to a record.
#[derive(Debug, Clone, PartialEq)]
pub struct Stray {
    pub release_id: i64,
    pub title: String,
    pub artist_name: Option<String>,
    pub track_id: i64,
    pub track_no: Option<i64>,
    /// The `/track/` page to ask, or `None` when nothing ever linked this release to Bandcamp --
    /// then it cannot be resolved and is only reported.
    pub url: Option<String>,
    pub path: Option<String>,
    pub label_id: Option<i64>,
    pub source_fan_id: Option<i64>,
}

impl Stray {
    /// Whether a page can be asked which album this is from. The link is the whole requirement: a
    /// release whose file has gone missing still merges (rows move, the retag is skipped).
    pub fn resolvable(&self) -> bool {
        self.url.as_deref().is_some_and(|u| !u.is_empty())
    }
}

struct Cand {
    release_id: i64,
    title: String,
    artist_name: Option<String>,
    track_id: i64,
    track_no: Option<i64>,
    release_url: Option<String>,
    track_url: Option<String>,
    label_id: Option<i64>,
    source_fan_id: Option<i64>,
}

/// Releases holding exactly one track that is not track one. Two signals, either enough: the
/// release's own URL names a `/track/` page (a track page is not a record), or the file numbers
/// itself past the first position (Bandcamp numbers a standalone single `1`, so a lone "05" is
/// the album it came from speaking through the one tag the download did not clobber). An
/// `/album/` URL is never a candidate (its identity is settled; it is merely incomplete -- what
/// the fill is for), nor is one already resolved to `kind='single'`.
///
/// Naming `ids` skips all of that: those signals exist to *guess* which rows are worth a page
/// fetch; a user pointing at one release has already answered. Holding exactly one track is still
/// required: merging a record would move its first track and orphan the rest.
fn candidates(c: &Connection, label_id: Option<i64>, ids: Option<&[i64]>) -> ApiResult<Vec<Cand>> {
    let (scope, heur) = match ids {
        Some(_) => ("WHERE release_id IN (SELECT value FROM json_each(?1))", String::new()),
        None => (
            "",
            // coalesce, not a bare NOT LIKE: NULL NOT LIKE ... is NULL, which would drop every
            // release that never recorded a URL -- exactly the fan-shelf downloads this finds.
            format!(
                "AND r.kind != 'single' AND COALESCE(r.bandcamp_url,'') NOT LIKE '%/album/%'
                 AND (COALESCE(r.bandcamp_url,'') LIKE '%/track/%' OR have.top > 1){}",
                if label_id.is_some() { " AND r.label_id = ?1" } else { "" }
            ),
        ),
    };
    let sql = format!(
        "SELECT r.id, r.title, a.name, t.id, t.track_no, r.bandcamp_url, t.bandcamp_url, r.label_id, r.source_fan_id
           FROM releases r
           JOIN (SELECT release_id AS rid, COUNT(*) AS n, MAX(track_no) AS top FROM tracks {scope} GROUP BY release_id) have ON have.rid = r.id
           JOIN tracks t ON t.release_id = r.id
           LEFT JOIN artists a ON a.id = r.artist_id
          WHERE have.n = 1 {heur} ORDER BY r.id"
    );
    let mut st = c.prepare(&sql)?;
    let map = |r: &bc_db::rusqlite::Row<'_>| {
        Ok(Cand {
            release_id: r.get(0)?,
            title: r.get(1)?,
            artist_name: r.get(2)?,
            track_id: r.get(3)?,
            track_no: r.get(4)?,
            release_url: r.get(5)?,
            track_url: r.get(6)?,
            label_id: r.get(7)?,
            source_fan_id: r.get(8)?,
        })
    };
    let rows = match (ids, label_id) {
        (Some(ids), _) => st.query_map([ids_json(ids)], map)?.collect::<Result<Vec<_>, _>>()?,
        (None, Some(l)) => st.query_map([l], map)?.collect::<Result<Vec<_>, _>>()?,
        (None, None) => st.query_map([], map)?.collect::<Result<Vec<_>, _>>()?,
    };
    Ok(rows)
}

pub fn count_strays(c: &Connection, label_id: Option<i64>) -> ApiResult<usize> {
    Ok(candidates(c, label_id, None)?.len())
}

/// Every candidate, each with the track page it can be asked about.
pub fn find_strays(c: &Connection, label_id: Option<i64>, ids: Option<&[i64]>, limit: Option<usize>) -> ApiResult<Vec<Stray>> {
    let mut rows = candidates(c, label_id, ids)?;
    if let Some(l) = limit {
        rows.truncate(l);
    }
    if rows.is_empty() {
        return Ok(vec![]);
    }
    let rids: Vec<i64> = rows.iter().map(|r| r.release_id).collect();
    let j = ids_json(&rids);
    let mut paths: HashMap<i64, String> = HashMap::new();
    {
        let mut st = c.prepare(
            "SELECT t.release_id, f.path FROM files f JOIN tracks t ON t.id = f.track_id
              WHERE t.release_id IN (SELECT value FROM json_each(?1)) ORDER BY f.id DESC",
        )?;
        for r in st.query_map([&j], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (rid, p) = r?;
            paths.insert(rid, p);
        }
    }
    // The inbox row that produced each release, for the ones with no URL: a wishlist download
    // files `harvest_items.release_id` but never writes the URL onto the release, so for a
    // fan-shelf stray this is the only surviving link to the track page.
    let mut inbox: HashMap<i64, String> = HashMap::new();
    {
        let mut st = c.prepare(
            "SELECT release_id, url FROM harvest_items WHERE release_id IN (SELECT value FROM json_each(?1)) AND url_kind = 'track' ORDER BY id",
        )?;
        for r in st.query_map([&j], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (rid, u) = r?;
            inbox.entry(rid).or_insert(u);
        }
    }
    Ok(rows
        .into_iter()
        .map(|r| {
            let url = r
                .release_url
                .clone()
                .filter(|u| !u.is_empty())
                .or_else(|| r.track_url.clone().filter(|u| !u.is_empty()))
                .or_else(|| inbox.get(&r.release_id).cloned());
            Stray {
                release_id: r.release_id,
                title: r.title,
                artist_name: r.artist_name,
                track_id: r.track_id,
                track_no: r.track_no,
                url: url.filter(|u| classify(u) == UrlKind::Track),
                path: paths.get(&r.release_id).cloned(),
                label_id: r.label_id,
                source_fan_id: r.source_fan_id,
            }
        })
        .collect())
}

/// The `GET /releases/strays` body: listing is free, only the merge needs Bandcamp.
pub fn strays_out(c: &Connection, label_id: Option<i64>, limit: usize) -> ApiResult<StraysOut> {
    let found = find_strays(c, label_id, None, None)?;
    Ok(StraysOut {
        total: found.len() as i64,
        resolvable: found.iter().filter(|s| s.resolvable()).count() as i64,
        items: found
            .iter()
            .take(limit)
            .map(|s| StrayOut {
                release_id: s.release_id,
                title: s.title.clone(),
                artist: s.artist_name.clone(),
                track_no: s.track_no,
                url: s.url.clone(),
                resolvable: s.resolvable(),
            })
            .collect(),
    })
}

// ---------------------------------------------------------------------------- retagging

/// The identity a stray's file must carry after the merge.
#[derive(Debug, Clone, PartialEq)]
pub struct AlbumIdentity {
    pub album: String,
    pub album_artist: Option<String>,
    pub track_no: Option<i64>,
    pub track_total: Option<i64>,
}

/// Rewrites a file's album tags in place. The default implementation is [`LoftyRetagger`]; the
/// metadata crate may provide its own.
pub trait Retagger: Send + Sync {
    fn write_album_identity(&self, path: &Path, identity: &AlbumIdentity) -> Result<(), String>;
}

/// Album, album artist and `n/total` written with `lofty`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoftyRetagger;

impl Retagger for LoftyRetagger {
    fn write_album_identity(&self, path: &Path, id: &AlbumIdentity) -> Result<(), String> {
        use lofty::config::WriteOptions;
        use lofty::file::{TaggedFile, TaggedFileExt};
        use lofty::tag::{Accessor, Tag, TagExt};

        let mut file: TaggedFile = lofty::read_from_path(path).map_err(|e| e.to_string())?;
        if file.primary_tag().is_none() {
            let tt = file.primary_tag_type();
            file.insert_tag(Tag::new(tt));
        }
        let tag = file.primary_tag_mut().ok_or("no tag")?;
        tag.set_album(id.album.clone());
        match &id.album_artist {
            Some(a) => {
                tag.insert_text(lofty::tag::ItemKey::AlbumArtist, a.clone());
            }
            None => tag.remove_key(lofty::tag::ItemKey::AlbumArtist),
        }
        if let Some(n) = id.track_no {
            tag.set_track(n as u32);
        }
        if let Some(t) = id.track_total {
            tag.set_track_total(t as u32);
        }
        tag.save_to_path(path, WriteOptions::default()).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------- merging one

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Merged,
    Single,
    Album,
    Unresolved,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MergeOutcome {
    pub release_id: i64,
    pub status: Status,
    pub detail: String,
    pub album_release_id: Option<i64>,
    pub album_title: Option<String>,
    pub created_album: bool,
    pub file_moved: bool,
    pub duplicate_removed: bool,
}

impl MergeOutcome {
    fn new(release_id: i64, status: Status, detail: impl Into<String>) -> Self {
        Self {
            release_id,
            status,
            detail: detail.into(),
            album_release_id: None,
            album_title: None,
            created_album: false,
            file_moved: false,
            duplicate_removed: false,
        }
    }
}

/// Record that a track page turned out to stand alone: `kind='single'` (and the URL, if the
/// release never had one) keeps the next sweep from spending another request re-asking.
pub fn mark_single(ctx: &Ctx, release_id: i64, url: &str) -> ApiResult<MergeOutcome> {
    let canonical = normalise(url);
    ctx.write(move |t| {
        let exists = t.query_row("SELECT bandcamp_url FROM releases WHERE id = ?1", [release_id], |r| r.get::<_, Option<String>>(0)).optional()?;
        let Some(cur) = exists else { return Ok(MergeOutcome::new(release_id, Status::Failed, "release is gone")) };
        t.execute("UPDATE releases SET kind = 'single' WHERE id = ?1", [release_id])?;
        if cur.is_none() {
            let taken: Option<i64> = t
                .query_row("SELECT id FROM releases WHERE bandcamp_url = ?1 AND id != ?2", (&canonical, release_id), |r| r.get(0))
                .optional()?;
            if taken.is_none() {
                t.execute("UPDATE releases SET bandcamp_url = ?1 WHERE id = ?2", (&canonical, release_id))?;
            }
        }
        Ok(MergeOutcome::new(release_id, Status::Single, "stands alone on Bandcamp"))
    })
}

fn year_of(album: &AlbumInfo) -> Option<i64> {
    let head: String = album.release_date.clone().unwrap_or_default().chars().take(4).collect();
    if !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()) { head.parse().ok() } else { None }
}

/// Move a stray's track onto the record it belongs to. Everything the album page knows is applied
/// to the target release (URL, length, label) because that page is the authority the stray row
/// never had. The stray release is deleted only once its one track has been re-pointed, so an
/// interruption anywhere leaves the track on a release that exists, never orphaned.
pub fn merge(ctx: &Ctx, retag: &dyn Retagger, stray: &Stray, album: &AlbumInfo) -> ApiResult<MergeOutcome> {
    let rid = stray.release_id;
    // The stray must still be what it was when it was listed.
    let state = ctx.read(|c| {
        let rel: Option<(Option<i64>, Option<String>, Option<i64>)> = c
            .query_row("SELECT artist_id, folder_path, source_fan_id FROM releases WHERE id = ?1", [rid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let trk: Option<(Option<i64>, String, Option<i64>)> = c
            .query_row("SELECT release_id, title, track_no FROM tracks WHERE id = ?1", [stray.track_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        Ok((rel, trk))
    })?;
    let (Some((rel_artist, old_folder_s, fan_id)), trk) = state else { return Ok(MergeOutcome::new(rid, Status::Failed, "release is gone")) };
    let Some((trk_release, track_title, track_no)) = trk.filter(|(r, _, _)| *r == Some(rid)) else {
        return Ok(MergeOutcome::new(rid, Status::Failed, "track moved while resolving"));
    };
    let title = album.title.trim().to_string();
    if title.is_empty() {
        return Ok(MergeOutcome::new(rid, Status::Failed, "the album page states no title"));
    }
    let _ = trk_release;

    // Resolve / create the album artist and the target release.
    let (alb, st, tt) = (album.clone(), stray.clone(), title.clone());
    let (target_id, created) = ctx.write(move |t| {
        let artist_id = get_or_create_artist(t, &alb.artist_name)?.or(rel_artist);
        target_release(t, &st, &alb, &tt, artist_id, fan_id)
    })?;
    let Some(target_id) = target_id else { return Ok(MergeOutcome::new(rid, Status::Failed, "could not identify the album release")) };
    let target_title: String = ctx.read(|c| Ok(c.query_row("SELECT title FROM releases WHERE id = ?1", [target_id], |r| r.get(0))?))?;
    let mk = |status, detail: String| {
        let mut o = MergeOutcome::new(rid, status, detail);
        o.album_release_id = Some(target_id);
        o.album_title = Some(target_title.clone());
        o
    };

    if target_id == rid {
        // The stray's own title is the album's: this row *is* the record, just short of tracks.
        // Stamp what the page said and leave it to the fill.
        let (alb, st) = (album.clone(), stray.clone());
        ctx.write(move |t| stamp_album(t, target_id, &alb, &st, false))?;
        return Ok(mk(Status::Album, "this is the album itself, only partly downloaded".into()));
    }
    if created {
        inherit_artwork(ctx, rid, target_id)?;
    }

    let (number, total) = numbering(album, &track_title, track_no, stray);
    let duplicate: Option<i64> = ctx.read(|c| {
        Ok(c.query_row(
            "SELECT id FROM tracks WHERE release_id = ?1 AND title_key = (SELECT title_key FROM tracks WHERE id = ?2) AND id != ?2 ORDER BY id LIMIT 1",
            (target_id, stray.track_id),
            |r| r.get(0),
        )
        .optional()?)
    })?;

    let (alb, st) = (album.clone(), stray.clone());
    ctx.write(move |t| stamp_album(t, target_id, &alb, &st, true))?;

    if let Some(keeper) = duplicate {
        absorb(ctx, stray.track_id, keeper, rid, old_folder_s.as_deref())?;
        ctx.write(move |t| snippets::refresh_releases(t, Some(&[target_id])).map(|_| ()))?;
        let mut o = mk(Status::Merged, format!("already on \u{201c}{target_title}\u{201d}; play counts kept"));
        o.created_album = created;
        o.duplicate_removed = true;
        return Ok(o);
    }

    let moved = refile(ctx, retag, target_id, stray, album, number, total)?;
    let (tid, st_url) = (stray.track_id, stray.url.clone());
    ctx.write(move |t| {
        // One transaction: the track lands on the target and the empty release goes together.
        t.execute("UPDATE tracks SET release_id = ?1 WHERE id = ?2", (target_id, tid))?;
        if let Some(n) = number {
            t.execute("UPDATE tracks SET track_no = ?1 WHERE id = ?2", (n, tid))?;
        }
        if let Some(u) = st_url {
            t.execute("UPDATE tracks SET bandcamp_url = ?1 WHERE id = ?2 AND bandcamp_url IS NULL", (normalise(&u), tid))?;
        }
        repoint_inbox(t, rid, Some(target_id))?;
        tidy::delete_artwork_rows(t, &[rid])?;
        t.execute("DELETE FROM releases WHERE id = ?1", [rid])?;
        bc_db::fts::reindex_tracks(t, &[tid]).map_err(ApiError::from)?;
        // The target just gained a track it did not have. That can make it snippet-only (a teaser
        // filed onto a record with nothing else yet) or stop it being so.
        snippets::refresh_releases(t, Some(&[target_id]))?;
        Ok(())
    })?;
    dissolve(ctx, rid, old_folder_s.as_deref().map(PathBuf::from))?;
    ctx.bus.invalidate("track", vec![stray.track_id]);
    ctx.bus.invalidate("release", vec![rid, target_id]);
    let mut o = mk(Status::Merged, format!("filed under \u{201c}{target_title}\u{201d}"));
    o.created_album = created;
    o.file_moved = moved;
    Ok(o)
}

/// The release the track belongs on, created if the library lacks it. Looked up by URL first --
/// the one identifier that cannot be two records -- then by the same (album artist, title, year)
/// identity ingest keys on, so a merge lands on the album a previous download already created.
fn target_release(t: &Transaction<'_>, stray: &Stray, album: &AlbumInfo, title: &str, artist_id: Option<i64>, fan_id: Option<i64>) -> ApiResult<(Option<i64>, bool)> {
    let _ = stray;
    let url = normalise(&album.url);
    if let Some(id) = t.query_row("SELECT id FROM releases WHERE bandcamp_url = ?1", [&url], |r| r.get::<_, i64>(0)).optional()? {
        return Ok((Some(id), false));
    }
    let key = name_key(title);
    let year = year_of(album);
    if let Some(id) = t
        .query_row("SELECT id FROM releases WHERE title_key = ?1 AND artist_id IS ?2 AND year IS ?3 ORDER BY id LIMIT 1", (&key, artist_id, year), |r| r.get::<_, i64>(0))
        .optional()?
    {
        return Ok((Some(id), false));
    }
    t.execute(
        "INSERT INTO releases (title, title_key, artist_id, year, release_date, kind, bandcamp_url, added_at, source_fan_id)
         VALUES (?1, ?2, ?3, ?4, ?5, 'album', ?6, ?7, ?8)",
        (title, &key, artist_id, year, &album.release_date, &url, now_db(), fan_id),
    )?;
    // Someone else's shelf keeps its records: a stray downloaded for a fan builds the album on
    // that fan's shelf (`source_fan_id`), not in my library.
    Ok((Some(t.last_insert_rowid()), true))
}

/// Apply what the album page settles, filling blanks only -- except `expected_track_count`, which
/// is raised to what the page counted just now ("1/20" rather than nothing is the point).
fn stamp_album(t: &Transaction<'_>, target: i64, album: &AlbumInfo, stray: &Stray, take_label: bool) -> ApiResult<()> {
    let url = normalise(&album.url);
    #[allow(clippy::type_complexity)]
    let (cur_url, expected, date, year, about, credits, label): (Option<String>, Option<i64>, Option<String>, Option<i64>, Option<String>, Option<String>, Option<i64>) = t
        .query_row(
            "SELECT bandcamp_url, expected_track_count, release_date, year, about, credits, label_id FROM releases WHERE id = ?1",
            [target],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )?;
    if cur_url.is_none() {
        let taken: Option<i64> = t.query_row("SELECT id FROM releases WHERE bandcamp_url = ?1 AND id != ?2", (&url, target), |r| r.get(0)).optional()?;
        if taken.is_none() {
            t.execute("UPDATE releases SET bandcamp_url = ?1 WHERE id = ?2", (&url, target))?;
        }
    }
    if !album.tracks.is_empty() && album.tracks.len() as i64 > expected.unwrap_or(0) {
        t.execute("UPDATE releases SET expected_track_count = ?1 WHERE id = ?2", (album.tracks.len() as i64, target))?;
    }
    if date.is_none() && album.release_date.is_some() {
        t.execute("UPDATE releases SET release_date = ?1 WHERE id = ?2", (&album.release_date, target))?;
    }
    if year.is_none()
        && let Some(y) = year_of(album)
    {
        t.execute("UPDATE releases SET year = ?1 WHERE id = ?2", (y, target))?;
    }
    if about.is_none() && album.about.as_deref().is_some_and(|a| !a.is_empty()) {
        t.execute("UPDATE releases SET about = ?1 WHERE id = ?2", (&album.about, target))?;
    }
    if credits.is_none() && album.credits.as_deref().is_some_and(|a| !a.is_empty()) {
        t.execute("UPDATE releases SET credits = ?1 WHERE id = ?2", (&album.credits, target))?;
    }
    if label.is_none() {
        let l = match &album.label_name {
            Some(n) => get_or_create_label(t, n)?,
            None => None,
        };
        if let Some(l) = l {
            t.execute("UPDATE releases SET label_id = ?1 WHERE id = ?2", (l, target))?;
        } else if take_label && let Some(sl) = stray.label_id {
            t.execute("UPDATE releases SET label_id = ?1 WHERE id = ?2", (sl, target))?;
        }
    }
    Ok(())
}

/// Where the track sits on the record, and how long the record is. The album page's own listing
/// wins over the file's number: they agree in the ordinary case, and where they do not it is the
/// file that is stale.
fn numbering(album: &AlbumInfo, track_title: &str, track_no: Option<i64>, stray: &Stray) -> (Option<i64>, Option<i64>) {
    let total = if album.tracks.is_empty() { None } else { Some(album.tracks.len() as i64) };
    let key = name_key(track_title);
    for entry in &album.tracks {
        let Some(n) = entry.track_num.filter(|n| *n != 0) else { continue };
        // A compilation lists its tracks as "Artist - Title"; the file is tagged with the title
        // alone, so the tail has to count as a match.
        let tail = entry.title.rsplit(" - ").next().unwrap_or("");
        if name_key(&entry.title) == key || name_key(tail) == key {
            return (Some(n), total);
        }
    }
    (track_no.filter(|n| *n != 0).or(stray.track_no.filter(|n| *n != 0)), total)
}

fn root_of(roots: &[PathBuf], path: &Path) -> Option<PathBuf> {
    let p = canon(path);
    roots.iter().find(|r| p.starts_with(r)).cloned()
}

/// Where the album's files live, or would. A folder that already holds the album's tracks is used
/// verbatim -- guessed names must never split a record that is already together. Only for an album
/// with nothing on disk yet is the name derived, next to the artist directory the stray is already
/// filed in, exactly as bandcamp-dl would name it.
fn album_folder(c: &Connection, target: i64, album: &AlbumInfo, source: &Path) -> ApiResult<PathBuf> {
    let sibling: Option<String> = c
        .query_row(
            "SELECT f.path FROM files f JOIN tracks t ON t.id = f.track_id WHERE t.release_id = ?1 AND f.missing_since IS NULL ORDER BY f.id LIMIT 1",
            [target],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(s) = sibling
        && let Some(p) = Path::new(&s).parent()
    {
        return Ok(p.to_path_buf());
    }
    let (folder_path, target_title): (Option<String>, String) =
        c.query_row("SELECT folder_path, title FROM releases WHERE id = ?1", [target], |r| Ok((r.get(0)?, r.get(1)?)))?;
    if let Some(f) = folder_path.filter(|f| Path::new(f).is_dir()) {
        return Ok(PathBuf::from(f));
    }
    let leaf = [folder_slug(&album.title), folder_slug(&target_title)].into_iter().find(|s| !s.is_empty()).unwrap_or_else(|| "album".into());
    let artist_dir = source.parent().and_then(Path::parent).unwrap_or(source);
    // The album artist, not the track's: a compilation lands under "Various Artists", and a stray
    // of it sits in its own artist's directory. The album folder has to be where the *rest of the
    // record* will download to, or filling it splits the album across two folders.
    let wanted = folder_slug(&album.artist_name);
    if !wanted.is_empty() && artist_dir.file_name().map(|n| n.to_string_lossy() != wanted).unwrap_or(true) {
        let moved = artist_dir.parent().unwrap_or(artist_dir).join(&wanted);
        if root_of(&root_paths(c)?, &moved).is_some() {
            return Ok(moved.join(leaf));
        }
    }
    Ok(artist_dir.join(leaf))
}

/// Correct the file's album tags and move it in beside the record. The tag write comes first and
/// is the part that matters: it is what stops the next scan reading ALBUM back off the file and
/// rebuilding the stray release around it. The move is a convenience on top -- if it fails the
/// library is still correct, just untidy, so it is logged rather than raised.
fn refile(ctx: &Ctx, retag: &dyn Retagger, target: i64, stray: &Stray, album: &AlbumInfo, number: Option<i64>, total: Option<i64>) -> ApiResult<bool> {
    let row: Option<(i64, String)> = ctx.read(|c| {
        Ok(c.query_row("SELECT id, path FROM files WHERE track_id = ?1 ORDER BY id LIMIT 1", [stray.track_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    })?;
    let Some((file_id, path)) = row else { return Ok(false) };
    let source = PathBuf::from(&path);
    if !source.is_file() {
        tracing::info!(release = stray.release_id, "stray: file is missing, only the rows are merged");
        return Ok(false);
    }
    let identity = AlbumIdentity {
        album: album.title.trim().to_string(),
        album_artist: Some(album.artist_name.trim().to_string()).filter(|a| !a.is_empty()),
        track_no: number,
        track_total: total,
    };
    if let Err(e) = retag.write_album_identity(&source, &identity) {
        tracing::warn!(release = stray.release_id, file = %source.display(), error = %e, "stray: could not rewrite the file's tags");
        return Ok(false);
    }
    let mut moved = source.clone();
    let folder = ctx.read(|c| album_folder(c, target, album, &source))?;
    if Some(folder.as_path()) != source.parent() {
        let destination = folder.join(source.file_name().unwrap_or_default());
        let res: std::io::Result<()> = (|| {
            std::fs::create_dir_all(&folder)?;
            if destination.exists() {
                // Same name, same record: bandcamp-dl would have written this very path, so what is
                // there is this track. Leave it be.
                tracing::info!(release = stray.release_id, dest = %destination.display(), "stray: destination exists, keeping it");
                Ok(())
            } else {
                crate::relocate::move_file(&source, &destination, true)?;
                moved = destination.clone();
                Ok(())
            }
        })();
        if let Err(e) = res {
            tracing::warn!(release = stray.release_id, error = %e, "stray: could not move the file");
        }
    }
    let moved2 = moved.clone();
    let changed = moved != source;
    ctx.write(move |t| {
        let roots = root_paths(t)?;
        if changed {
            let rel = root_of(&roots, &moved2).and_then(|r| canon(&moved2).strip_prefix(&r).ok().map(|p| p.to_string_lossy().into_owned()));
            t.execute("UPDATE files SET path = ?1 WHERE id = ?2", (moved2.to_string_lossy(), file_id))?;
            if let Some(rel) = rel {
                t.execute("UPDATE files SET rel_path = ?1 WHERE id = ?2", (rel, file_id))?;
            }
        }
        // Re-stat whatever the write left behind, or the scanner reads its own size/mtime change as
        // an outside edit and re-ingests the file.
        if let Ok(m) = std::fs::metadata(&moved2) {
            use std::os::unix::fs::MetadataExt;
            t.execute(
                "UPDATE files SET size_bytes = ?1, mtime_ns = ?2, inode = ?3 WHERE id = ?4",
                (m.len() as i64, m.mtime() * 1_000_000_000 + m.mtime_nsec(), m.ino() as i64, file_id),
            )?;
        }
        t.execute("UPDATE files SET last_seen_at = ?1, missing_since = NULL WHERE id = ?2", (now_db(), file_id))?;
        if let Some(parent) = moved2.parent() {
            t.execute("UPDATE releases SET folder_path = ?1 WHERE id = ?2 AND folder_path IS NULL", (parent.to_string_lossy(), target))?;
        }
        Ok(())
    })?;
    Ok(changed)
}

/// Fold a stray whose track the album already holds into that copy. Only the user's own data
/// survives the fold -- plays, rating, loved, every playlist and set -- because that is the half a
/// re-download cannot reproduce. The audio goes: it is the same track from the same source,
/// already on disk under the album, and leaving it would rebuild the stray on the next scan.
fn absorb(ctx: &Ctx, track_id: i64, keeper: i64, release_id: i64, old_folder: Option<&str>) -> ApiResult<()> {
    let files: Vec<String> = ctx.read(|c| {
        let mut st = c.prepare("SELECT path FROM files WHERE track_id = ?1")?;
        Ok(st.query_map([track_id], |r| r.get(0))?.collect::<Result<_, _>>()?)
    })?;
    ctx.write(move |t| {
        #[allow(clippy::type_complexity)]
        let (pc, sc, loved, rating, comment, last, added): (i64, i64, bool, Option<i64>, Option<String>, Option<String>, String) = t.query_row(
            "SELECT play_count, skip_count, loved, rating, comment, last_played_at, added_at FROM tracks WHERE id = ?1",
            [track_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
        )?;
        t.execute(
            "UPDATE tracks SET play_count = play_count + ?1, skip_count = skip_count + ?2, loved = (loved OR ?3),
                 rating = COALESCE(rating, ?4), comment = COALESCE(comment, ?5),
                 last_played_at = CASE WHEN ?6 IS NOT NULL AND (last_played_at IS NULL OR ?6 > last_played_at) THEN ?6 ELSE last_played_at END,
                 added_at = CASE WHEN ?7 < added_at THEN ?7 ELSE added_at END
              WHERE id = ?8",
            (pc, sc, loved, rating, comment, last, added, keeper),
        )?;
        for table in ["playlist_items", "dj_set_items", "play_history"] {
            t.execute(&format!("UPDATE {table} SET track_id = ?1 WHERE track_id = ?2"), (keeper, track_id))?;
        }
        let keeper_release: Option<i64> = t.query_row("SELECT release_id FROM tracks WHERE id = ?1", [keeper], |r| r.get(0))?;
        repoint_inbox(t, release_id, keeper_release)?;
        delete::release_tag_counts(t, &[track_id])?;
        bc_db::fts::remove_tracks(t, &[track_id]).map_err(ApiError::from)?;
        t.execute("DELETE FROM tracks WHERE id = ?1", [track_id])?;
        tidy::delete_artwork_rows(t, &[release_id])?;
        t.execute("DELETE FROM releases WHERE id = ?1", [release_id])?;
        Ok(())
    })?;
    let roots = ctx.read(root_paths)?;
    let mut removed = 0;
    for f in files {
        let p = canon(Path::new(&f));
        if !under_a_root(&p, &roots) {
            continue;
        }
        if std::fs::remove_file(&p).is_ok() {
            removed += 1;
        }
    }
    dissolve(ctx, release_id, old_folder.map(PathBuf::from))?;
    if removed > 0 {
        tracing::info!(release_id, removed, "stray: removed duplicate file(s)");
    }
    ctx.bus.invalidate("track", vec![track_id, keeper]);
    Ok(())
}

/// Keep the inbox rows pointing at a release that still exists (`ON DELETE SET NULL` would drop
/// the link, and with it the only record of which wishlist entry this music came from).
fn repoint_inbox(t: &Transaction<'_>, release_id: i64, target: Option<i64>) -> ApiResult<()> {
    if let Some(target) = target {
        t.execute("UPDATE harvest_items SET release_id = ?1 WHERE release_id = ?2", (target, release_id))?;
    }
    Ok(())
}

/// Clear up after a release that no longer exists: its `cover.jpg` would keep the emptied folder
/// alive forever, and the cached artwork is keyed by an id that will never be issued again.
fn dissolve(ctx: &Ctx, release_id: i64, folder: Option<PathBuf>) -> ApiResult<()> {
    if let Some(f) = folder {
        let folders = vec![f];
        ctx.read(|c| {
            tidy::sweep_sidecars(c, &folders)?;
            tidy::prune_empty_dirs(c, &folders)
        })?;
    }
    tidy::delete_artwork(&ctx.config.art_dir(), &[release_id]);
    Ok(())
}

/// Give a freshly created album the stray's cover. A track page carries the album's own art, so
/// the art already cached under the stray's id *is* the record's cover. Copied rather than moved:
/// the stray's own art is deleted with it moments later.
fn inherit_artwork(ctx: &Ctx, stray: i64, target: i64) -> ApiResult<()> {
    let dir = ctx.config.art_dir();
    let mut copied_any = false;
    for (from, to) in tidy::art_files(&dir, stray).into_iter().zip(tidy::art_files(&dir, target)) {
        if !from.is_file() {
            continue;
        }
        if let Some(p) = to.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        if std::fs::copy(&from, &to).is_ok() {
            copied_any = true;
        }
    }
    if copied_any {
        ctx.write(move |t| {
            t.execute(
                "INSERT OR IGNORE INTO artwork (release_id, hash, version, blurhash, color, width, height, sizes, source, updated_at)
                 SELECT ?1, hash, version, blurhash, color, width, height, sizes, source, updated_at FROM artwork WHERE release_id = ?2",
                (target, stray),
            )?;
            Ok(())
        })?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------- running the sweep

/// A sweep is in flight; a second would re-fetch the same pages.
#[derive(Debug, thiserror::Error)]
#[error("a stray merge is already running")]
pub struct AlreadyRunning;

/// Resolves strays against Bandcamp and merges them, one at a time.
///
/// Each stray costs one or two rate-limited page fetches, so a library-wide sweep is half an hour
/// of work -- far too long to hold a request open. It is also safe to lose: every merge commits
/// on its own, and a merged release stops being a candidate, so a rerun resumes rather than
/// repeats. Cancellation lands between strays (a merge in flight finishes: the file has been
/// retagged and moved by the time the rows are written, and a stop that interrupted that would
/// leave the pair disagreeing). State changes are published as `library.strays` events.
pub struct StrayMerger {
    ctx: Ctx,
    lookup: Arc<dyn BandcampLookup>,
    retag: Arc<dyn Retagger>,
    state: Arc<Mutex<StraySweepStatus>>,
    cancel: Arc<AtomicBool>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn idle() -> StraySweepStatus {
    StraySweepStatus { phase: "idle".into(), ..Default::default() }
}

impl StrayMerger {
    pub fn new(ctx: Ctx, lookup: Arc<dyn BandcampLookup>, retag: Arc<dyn Retagger>) -> Self {
        Self { ctx, lookup, retag, state: Arc::new(Mutex::new(idle())), cancel: Arc::new(AtomicBool::new(false)), task: Mutex::new(None) }
    }

    pub fn state(&self) -> StraySweepStatus {
        self.state.lock().clone()
    }

    fn publish(&self, f: impl FnOnce(&mut StraySweepStatus)) {
        publish(&self.ctx, &self.state, f);
    }

    /// Start a sweep on the running tokio runtime; returns the initial state immediately.
    pub fn start(&self, label_id: Option<i64>, ids: Option<Vec<i64>>, limit: Option<usize>) -> Result<StraySweepStatus, AlreadyRunning> {
        {
            let mut s = self.state.lock();
            if s.running {
                return Err(AlreadyRunning);
            }
            *s = StraySweepStatus { phase: "running".into(), running: true, started_at: Some(iso_now()), ..Default::default() };
        }
        self.cancel.store(false, Ordering::SeqCst);
        self.ctx.bus.publish(TOPIC_LIBRARY_STRAYS, &self.state());
        let (ctx, lookup, retag, state, cancel) = (self.ctx.clone(), self.lookup.clone(), self.retag.clone(), self.state.clone(), self.cancel.clone());
        let handle = tokio::spawn(async move {
            let res = run(&ctx, &*lookup, &retag, &state, &cancel, label_id, ids, limit).await;
            if let Err(e) = res {
                tracing::warn!(error = %e, "stray merge failed");
                publish(&ctx, &state, |s| {
                    s.phase = "failed".into();
                    s.running = false;
                    s.error = Some(e.chars().take(500).collect());
                    s.current = None;
                    s.finished_at = Some(iso_now());
                });
            }
        });
        *self.task.lock() = Some(handle);
        Ok(self.state())
    }

    /// Stop the sweep (between strays) and report `Cancelled`.
    pub async fn stop(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        let handle = self.task.lock().take();
        if let Some(h) = handle {
            h.abort();
            let _ = h.await;
        }
        if self.state.lock().running {
            // A stop that lands before the task's first tick never reaches the handler inside
            // the task, and the state would claim "running" forever.
            self.publish(|s| {
                s.phase = "failed".into();
                s.running = false;
                s.error = Some("Cancelled".into());
                s.current = None;
                s.finished_at = Some(iso_now());
            });
        }
    }
}

fn publish(ctx: &Ctx, state: &Mutex<StraySweepStatus>, f: impl FnOnce(&mut StraySweepStatus)) {
    let snapshot = {
        let mut s = state.lock();
        f(&mut s);
        s.running = s.phase == "running";
        s.clone()
    };
    ctx.bus.publish(TOPIC_LIBRARY_STRAYS, &snapshot);
}

#[allow(clippy::too_many_arguments)]
async fn run(
    ctx: &Ctx,
    lookup: &dyn BandcampLookup,
    retag: &Arc<dyn Retagger>,
    state: &Mutex<StraySweepStatus>,
    cancel: &AtomicBool,
    label_id: Option<i64>,
    ids: Option<Vec<i64>>,
    limit: Option<usize>,
) -> Result<(), String> {
    let queue = {
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || ctx.read(|c| find_strays(c, label_id, ids.as_deref(), limit)))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?
    };
    publish(ctx, state, |s| s.total = Some(queue.len() as i64));
    // Every album the sweep filed a track onto or built around one: whichever are still short of
    // tracks get their fills queued at the end, so a merged record is complete by default rather
    // than sitting at "1/12" until someone scrolls past it.
    let mut albums: HashSet<i64> = HashSet::new();
    for (i, stray) in queue.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            return Err("Cancelled".into());
        }
        publish(ctx, state, |s| {
            s.seen = i as i64 + 1;
            s.current = Some(stray.title.clone());
        });
        if let Some(a) = one(ctx, lookup, retag, state, stray).await {
            albums.insert(a);
        }
    }
    let fills = queue_fills(ctx, &albums).await.map_err(|e| e.to_string())?;
    let remaining = {
        let ctx = ctx.clone();
        tokio::task::spawn_blocking(move || ctx.read(|c| count_strays(c, label_id))).await.map_err(|e| e.to_string())?.unwrap_or(0)
    };
    let merged = state.lock().merged;
    publish(ctx, state, |s| {
        s.phase = "done".into();
        s.fills_queued = fills;
        s.current = None;
        s.finished_at = Some(iso_now());
    });
    ctx.bus.publish("library.changed", &serde_json::json!({ "strays_merged": merged }));
    tracing::info!(merged, fills, remaining, "stray merge finished");
    Ok(())
}

async fn queue_fills(ctx: &Ctx, albums: &HashSet<i64>) -> ApiResult<i64> {
    if albums.is_empty() {
        return Ok(0);
    }
    let mut ids: Vec<i64> = albums.iter().copied().collect();
    ids.sort_unstable();
    let ctx = ctx.clone();
    tokio::task::spawn_blocking(move || {
        let short = ctx.read(|c| completeness::missing_release_ids(c, Some(&ids)))?;
        Ok(completeness::queue_fills(&ctx, &short)?.queued)
    })
    .await
    .map_err(ApiError::internal)?
}

/// Resolve and settle one stray; the album release it touched, if any.
async fn one(ctx: &Ctx, lookup: &dyn BandcampLookup, retag: &Arc<dyn Retagger>, state: &Mutex<StraySweepStatus>, stray: &Stray) -> Option<i64> {
    let Some(url) = stray.url.clone().filter(|u| !u.is_empty()) else {
        publish(ctx, state, |s| s.unresolved += 1);
        return None;
    };
    let bump_failed = |why: String| {
        // One dead page -- a removed track, a private stream -- must not end a sweep of hundreds.
        tracing::info!(release = stray.release_id, url = %url, "stray: {why}");
        publish(ctx, state, |s| s.failed += 1);
    };
    let album_url = match lookup.resolve_album_url(&url).await {
        Ok(u) => u,
        Err(e) => {
            bump_failed(format!("could not be read: {e}"));
            return None;
        }
    };
    if classify(&album_url) != UrlKind::Album {
        let (c, rid, u) = (ctx.clone(), stray.release_id, url.clone());
        let _ = tokio::task::spawn_blocking(move || mark_single(&c, rid, &u)).await;
        publish(ctx, state, |s| s.singles += 1);
        return None;
    }
    let album = match album_info(ctx, lookup, &album_url).await {
        Ok(a) => a,
        Err(e) => {
            bump_failed(format!("album {album_url} could not be read: {e}"));
            return None;
        }
    };
    let (c, st, rt) = (ctx.clone(), stray.clone(), retag.clone());
    let outcome = tokio::task::spawn_blocking(move || merge(&c, &*rt, &st, &album)).await;
    let outcome = match outcome {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => MergeOutcome::new(stray.release_id, Status::Failed, e.to_string()),
        Err(e) => MergeOutcome::new(stray.release_id, Status::Failed, e.to_string()),
    };
    match outcome.status {
        Status::Merged => publish(ctx, state, |s| {
            s.merged += 1;
            if outcome.created_album {
                s.albums_created += 1;
            }
        }),
        Status::Album => publish(ctx, state, |s| s.albums += 1),
        Status::Single => publish(ctx, state, |s| s.singles += 1),
        _ => {
            tracing::info!(release = stray.release_id, "stray: {}", outcome.detail);
            publish(ctx, state, |s| s.failed += 1);
        }
    }
    outcome.album_release_id
}

/// The album's page -- or the library's own copy of it, for free. A label sweep merges many
/// strays onto records already on the shelf; a release the library holds under that URL, whose
/// length it already knows, answers everything the merge asks without another request.
async fn album_info(ctx: &Ctx, lookup: &dyn BandcampLookup, album_url: &str) -> Result<AlbumInfo, crate::lookup::LookupError> {
    let (c, u) = (ctx.clone(), album_url.to_string());
    let local: Option<AlbumInfo> = tokio::task::spawn_blocking(move || {
        c.read(|c| {
            Ok(c.query_row(
                "SELECT r.title, COALESCE(a.name,''), r.release_date, l.name FROM releases r
                   LEFT JOIN artists a ON a.id = r.artist_id LEFT JOIN labels l ON l.id = r.label_id
                  WHERE r.bandcamp_url = ?1 AND r.expected_track_count IS NOT NULL AND r.expected_track_count != 0",
                [&u],
                |r| {
                    Ok(AlbumInfo { url: u.clone(), title: r.get(0)?, artist_name: r.get(1)?, release_date: r.get(2)?, label_name: r.get(3)?, ..Default::default() })
                },
            )
            .optional()?)
        })
    })
    .await
    .ok()
    .and_then(|r| r.ok())
    .flatten();
    match local {
        Some(l) => Ok(l),
        None => lookup.fetch_album(album_url).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lookup::{AlbumTrack, LookupError};
    use crate::testutil::*;
    use async_trait::async_trait;
    use bc_db::Db;

    const TRACK_URL: &str = "https://mutual-rytm.bandcamp.com/track/greedy-man";
    const ALBUM_URL: &str = "https://mutual-rytm.bandcamp.com/album/optimized-grooves";
    const SINGLE_URL: &str = "https://mutual-rytm.bandcamp.com/track/alone";

    fn have_ffmpeg() -> bool {
        std::process::Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
    }

    fn album(over: impl FnOnce(&mut AlbumInfo)) -> AlbumInfo {
        let mut a = AlbumInfo {
            url: ALBUM_URL.into(),
            title: "Optimized Grooves".into(),
            artist_name: "Chl\u{e4}r".into(),
            release_date: Some("2023-05-01".into()),
            tracks: (1..=4).map(|n| AlbumTrack { title: format!("T{n}"), track_num: Some(n), ..Default::default() }).chain([AlbumTrack { title: "Greedy Man".into(), track_num: Some(5), ..Default::default() }]).collect(),
            ..Default::default()
        };
        over(&mut a);
        a
    }

    fn release(db: &Db, title: &str, artist: &str, extra: &str) -> i64 {
        let rid = seed_release(db, artist, title, None, None);
        if !extra.is_empty() {
            exec(db, &format!("UPDATE releases SET {extra} WHERE id={rid}"));
        }
        rid
    }

    fn list(db: &Db) -> Vec<Stray> {
        db.read(|c| Ok(find_strays(c, None, None, None).unwrap())).unwrap()
    }

    // ---- what bandcamp-dl would have called the folder

    #[test]
    fn folder_slug_matches_bandcamp_dl() {
        assert_eq!(folder_slug("Greedy Man"), "greedy-man");
        assert_eq!(folder_slug("Sounds From The Past III"), "sounds-from-the-past-iii");
        assert_eq!(folder_slug("A. Paul & DJ Dextro"), "a-paul-dj-dextro");
        // Non-ASCII letters survive; only punctuation and runs of space collapse.
        assert_eq!(folder_slug("Chl\u{e4}r  \u{2014}  Dub"), "chl\u{e4}r-dub");
        assert_eq!(folder_slug("???"), "");
    }

    // ---- finding them

    #[test]
    fn a_lone_track_numbered_past_one_is_a_stray() {
        let db = test_db();
        let stray = release(&db, "Greedy Man", "Chl\u{e4}r", &format!("bandcamp_url='{TRACK_URL}'"));
        seed_track(&db, stray, "Greedy Man", Some(5));
        // An album URL settles the identity: it is incomplete, not a stray.
        let partial = release(&db, "Intrinsic Drive", "Chl\u{e4}r", &format!("bandcamp_url='{ALBUM_URL}'"));
        seed_track(&db, partial, "Some Track", Some(3));
        // A whole record, and a single that was already resolved once.
        let whole = release(&db, "Whole", "Chl\u{e4}r", "");
        seed_track(&db, whole, "One", Some(1));
        seed_track(&db, whole, "Two", Some(2));
        let settled = release(&db, "Settled", "Chl\u{e4}r", "kind='single'");
        seed_track(&db, settled, "Settled", Some(4));
        let found = list(&db);
        assert_eq!(found.iter().map(|s| s.release_id).collect::<Vec<_>>(), vec![stray]);
        assert_eq!(found[0].url.as_deref(), Some(TRACK_URL));
        assert_eq!(found[0].track_no, Some(5));
        assert_eq!(db.read(|c| Ok(count_strays(c, None).unwrap())).unwrap(), 1);
    }

    #[test]
    fn a_track_url_is_a_stray_even_at_position_one() {
        // A track page is not a record, whatever the numbering says.
        let db = test_db();
        let stray = release(&db, "Greedy Man", "X", &format!("bandcamp_url='{TRACK_URL}'"));
        seed_track(&db, stray, "Greedy Man", Some(1));
        assert_eq!(list(&db).iter().map(|s| s.release_id).collect::<Vec<_>>(), vec![stray]);
    }

    #[test]
    fn the_inbox_supplies_the_url_a_shelf_download_never_recorded() {
        let db = test_db();
        let stray = release(&db, "Intertwined", "X", "");
        seed_track(&db, stray, "Intertwined", Some(20));
        exec(&db, &format!(
            "INSERT INTO harvest_items(url,url_kind,state,title,artist_name,tags,in_collection,in_wishlist,is_free_download,is_purchasable,is_preorder,discovered_at,release_id)
             VALUES ('{TRACK_URL}','track','new','x','y','[]',0,0,0,0,0,'x',{stray})"));
        let found = list(&db);
        assert_eq!(found.iter().map(|s| s.release_id).collect::<Vec<_>>(), vec![stray]);
        assert_eq!(found[0].url.as_deref(), Some(TRACK_URL));
        assert!(found[0].resolvable(), "a link is all the merge needs");
    }

    #[test]
    fn a_release_nothing_ever_linked_is_reported_not_resolvable() {
        let db = test_db();
        let stray = release(&db, "Orphan", "X", "");
        seed_track(&db, stray, "Orphan", Some(7));
        let found = list(&db);
        assert_eq!(found[0].url, None);
        assert!(!found[0].resolvable());
    }

    #[test]
    fn naming_a_release_skips_the_guesswork() {
        // "Put back on its album" is offered on any one-track record, including ones the sweep
        // would never pick up on its own; a record holding several tracks is still refused.
        let db = test_db();
        let settled = release(&db, "Settled", "X", &format!("kind='single', bandcamp_url='{TRACK_URL}'"));
        seed_track(&db, settled, "Settled", Some(1));
        let owned = release(&db, "Owned", "X", &format!("bandcamp_url='{ALBUM_URL}'"));
        seed_track(&db, owned, "One", Some(1));
        let record = release(&db, "Record", "X", "");
        seed_track(&db, record, "One", Some(1));
        seed_track(&db, record, "Two", Some(2));
        assert!(list(&db).is_empty(), "none is a candidate on its own");
        let picked = db.read(move |c| Ok(find_strays(c, None, Some(&[settled, owned, record]), None).unwrap())).unwrap();
        assert_eq!(picked.iter().map(|s| s.release_id).collect::<Vec<_>>(), vec![settled, owned]);
    }

    #[test]
    fn a_standalone_single_is_marked_and_never_asked_again() {
        let env = test_env();
        let r = release(&env.db, "Greedy Man", "X", "");
        seed_track(&env.db, r, "Greedy Man", Some(5));
        let out = mark_single(&env, r, TRACK_URL).unwrap();
        assert_eq!(out.status, Status::Single);
        assert_eq!(q_str(&env.db, &format!("SELECT kind FROM releases WHERE id={r}")).as_deref(), Some("single"));
        assert_eq!(q_str(&env.db, &format!("SELECT bandcamp_url FROM releases WHERE id={r}")).as_deref(), Some(TRACK_URL));
        assert!(list(&env.db).is_empty());
    }

    // ---- merging (real mp3s synthesised with ffmpeg, like the legacy tests; skipped without it)

    fn mp3(path: &Path, title: &str, artist: &str, album: &str, track: u32) {
        use lofty::config::WriteOptions;
        use lofty::file::{TaggedFileExt, TaggedFile};
        use lofty::tag::{Accessor, Tag, TagExt, TagType};
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let st = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=1", "-c:a", "libmp3lame", "-b:a", "64k"])
            .arg(path)
            .status()
            .unwrap();
        assert!(st.success());
        let mut f: TaggedFile = lofty::read_from_path(path).unwrap();
        f.insert_tag(Tag::new(TagType::Id3v2));
        let tag = f.primary_tag_mut().unwrap();
        tag.set_title(title.into());
        tag.set_artist(artist.into());
        tag.set_album(album.into());
        tag.set_track(track);
        tag.save_to_path(path, WriteOptions::default()).unwrap();
    }

    fn read_tags(path: &Path) -> (Option<String>, Option<String>, Option<u32>) {
        use lofty::file::TaggedFileExt;
        use lofty::tag::{Accessor, ItemKey};
        let f = lofty::read_from_path(path).unwrap();
        let t = f.primary_tag().unwrap();
        (t.album().map(|a| a.to_string()), t.get_string(ItemKey::AlbumArtist).map(str::to_string), t.track_total())
    }

    struct Fx {
        env: TestEnv,
        _tmp: tempfile::TempDir,
        music: PathBuf,
        root: i64,
    }

    fn fx() -> Fx {
        let env = test_env();
        let tmp = tempfile::tempdir().unwrap();
        let music = tmp.path().join("music").canonicalize().unwrap_or_else(|_| {
            std::fs::create_dir_all(tmp.path().join("music")).unwrap();
            tmp.path().join("music").canonicalize().unwrap()
        });
        let root = seed_root(&env.db, music.to_str().unwrap(), "downloads");
        Fx { env, _tmp: tmp, music, root }
    }

    impl Fx {
        /// The exact shape a `/track/` download leaves: album == track title.
        fn stray_on_disk(&self, artist: &str) -> (i64, i64, PathBuf) {
            let path = self.music.join(folder_slug(artist)).join("greedy-man").join("05 - greedy-man.mp3");
            mp3(&path, "Greedy Man", artist, "Greedy Man", 5);
            let r = release(&self.env.db, "Greedy Man", artist, &format!("year=2023, bandcamp_url='{TRACK_URL}', folder_path='{}'", path.parent().unwrap().display()));
            let t = seed_track(&self.env.db, r, "Greedy Man", Some(5));
            seed_file(&self.env.db, t, self.root, path.to_str().unwrap(), path.strip_prefix(&self.music).unwrap().to_str().unwrap(), 1000);
            (r, t, path)
        }
        fn first(&self) -> Stray {
            list(&self.env.db).remove(0)
        }
        fn path_of(&self, track: i64) -> String {
            q_str(&self.env.db, &format!("SELECT path FROM files WHERE track_id={track}")).unwrap()
        }
    }

    #[test]
    fn merge_files_the_track_onto_an_album_already_in_the_library() {
        if !have_ffmpeg() {
            return;
        }
        let f = fx();
        let db = &f.env.db;
        let alb = release(db, "Optimized Grooves", "Chl\u{e4}r", &format!("year=2023, bandcamp_url='{ALBUM_URL}'"));
        let sibling = f.music.join("chl\u{e4}r").join("optimized-grooves").join("01 - t1.mp3");
        mp3(&sibling, "T1", "Chl\u{e4}r", "Optimized Grooves", 1);
        let st = seed_track(db, alb, "T1", Some(1));
        seed_file(db, st, f.root, sibling.to_str().unwrap(), "chl\u{e4}r/optimized-grooves/01 - t1.mp3", 1000);
        let (stray_release, track, path) = f.stray_on_disk("Chl\u{e4}r");
        exec(db, &format!("UPDATE tracks SET play_count=9, loved=1 WHERE id={track}"));

        let outcome = merge(&f.env, &LoftyRetagger, &f.first(), &album(|_| {})).unwrap();
        assert_eq!(outcome.status, Status::Merged);
        assert_eq!(outcome.album_release_id, Some(alb));
        assert!(!outcome.created_album && outcome.file_moved);

        assert_eq!(q_i64(db, &format!("SELECT COUNT(*) FROM releases WHERE id={stray_release}")), 0, "the stray release is gone");
        assert_eq!(q_i64(db, &format!("SELECT release_id FROM tracks WHERE id={track}")), alb);
        assert_eq!(q_i64(db, &format!("SELECT play_count FROM tracks WHERE id={track}")), 9);
        assert_eq!(q_i64(db, &format!("SELECT loved FROM tracks WHERE id={track}")), 1, "user data rides on the track row");
        assert_eq!(q_i64(db, &format!("SELECT expected_track_count FROM releases WHERE id={alb}")), 5);
        // The file followed, beside the album's other track, and now says so.
        let landed = sibling.parent().unwrap().join("05 - greedy-man.mp3");
        assert!(landed.is_file() && !path.exists());
        let (alb_tag, _, total) = read_tags(&landed);
        assert_eq!(alb_tag.as_deref(), Some("Optimized Grooves"));
        assert_eq!(total, Some(5));
        assert_eq!(f.path_of(track), landed.to_str().unwrap());
        assert!(!path.parent().unwrap().exists(), "the emptied stray folder is pruned");
        assert_eq!(q_str(db, &format!("SELECT rel_path FROM files WHERE track_id={track}")).unwrap(), "chl\u{e4}r/optimized-grooves/05 - greedy-man.mp3");
    }

    #[test]
    fn merge_builds_the_album_when_the_library_lacks_it() {
        if !have_ffmpeg() {
            return;
        }
        let f = fx();
        let db = &f.env.db;
        let fan = seed_fan(db, "lukehess");
        let (stray_release, track, _path) = f.stray_on_disk("Chl\u{e4}r");
        exec(db, &format!("UPDATE releases SET source_fan_id={fan} WHERE id={stray_release}"));
        let outcome = merge(&f.env, &LoftyRetagger, &f.first(), &album(|a| a.label_name = Some("Mutual Rytm".into()))).unwrap();
        assert_eq!(outcome.status, Status::Merged);
        assert!(outcome.created_album);
        let a = outcome.album_release_id.unwrap();
        assert_eq!(q_str(db, &format!("SELECT title FROM releases WHERE id={a}")).as_deref(), Some("Optimized Grooves"));
        assert_eq!(q_str(db, &format!("SELECT bandcamp_url FROM releases WHERE id={a}")).as_deref(), Some(ALBUM_URL));
        assert_eq!(q_i64(db, &format!("SELECT expected_track_count FROM releases WHERE id={a}")), 5);
        assert_eq!(q_i64(db, &format!("SELECT year FROM releases WHERE id={a}")), 2023);
        assert_eq!(q_str(db, &format!("SELECT l.name FROM releases r JOIN labels l ON l.id=r.label_id WHERE r.id={a}")).as_deref(), Some("Mutual Rytm"));
        assert_eq!(q_i64(db, &format!("SELECT source_fan_id FROM releases WHERE id={a}")), fan, "a shelf record stays on its shelf");
        assert_eq!(q_i64(db, &format!("SELECT release_id FROM tracks WHERE id={track}")), a);
        // Named as bandcamp-dl would, so filling the record lands beside it.
        assert!(f.music.join("chl\u{e4}r").join("optimized-grooves").join("05 - greedy-man.mp3").is_file());
        assert_eq!(q_i64(db, &format!("SELECT COUNT(*) FROM releases WHERE id={stray_release}")), 0);
    }

    #[test]
    fn a_release_that_is_the_album_itself_is_kept_and_stamped() {
        if !have_ffmpeg() {
            return;
        }
        let f = fx();
        let (stray_release, track, _p) = f.stray_on_disk("Chl\u{e4}r");
        let outcome = merge(&f.env, &LoftyRetagger, &f.first(), &album(|a| a.title = "Greedy Man".into())).unwrap();
        assert_eq!(outcome.status, Status::Album);
        let db = &f.env.db;
        assert_eq!(q_i64(db, &format!("SELECT COUNT(*) FROM releases WHERE id={stray_release}")), 1);
        assert_eq!(q_i64(db, &format!("SELECT expected_track_count FROM releases WHERE id={stray_release}")), 5, "now the grid can offer to fill it");
        assert_eq!(q_str(db, &format!("SELECT bandcamp_url FROM releases WHERE id={stray_release}")).as_deref(), Some(TRACK_URL), "a URL it already had is not overwritten");
        assert_eq!(q_i64(db, &format!("SELECT release_id FROM tracks WHERE id={track}")), stray_release);
    }

    #[test]
    fn a_duplicate_keeps_the_play_counts_and_drops_the_second_copy() {
        if !have_ffmpeg() {
            return;
        }
        let f = fx();
        let db = &f.env.db;
        let alb = release(db, "Optimized Grooves", "Chl\u{e4}r", &format!("year=2023, bandcamp_url='{ALBUM_URL}'"));
        let keeper_path = f.music.join("chl\u{e4}r").join("optimized-grooves").join("05 - greedy-man.mp3");
        mp3(&keeper_path, "Greedy Man", "Chl\u{e4}r", "Optimized Grooves", 5);
        let keeper = seed_track(db, alb, "Greedy Man", Some(5));
        seed_file(db, keeper, f.root, keeper_path.to_str().unwrap(), "chl\u{e4}r/optimized-grooves/05 - greedy-man.mp3", 1000);
        let (stray_release, track, path) = f.stray_on_disk("Chl\u{e4}r");
        exec(db, &format!("UPDATE tracks SET play_count=4, rating=5, loved=1 WHERE id={track}"));
        exec(db, "INSERT INTO playlists(name,kind,created_at,updated_at) VALUES ('crate','manual','x','x')");
        exec(db, &format!("INSERT INTO playlist_items(playlist_id,track_id,position,added_at) VALUES (1,{track},0,'x')"));

        let outcome = merge(&f.env, &LoftyRetagger, &f.first(), &album(|_| {})).unwrap();
        assert_eq!(outcome.status, Status::Merged);
        assert!(outcome.duplicate_removed);
        assert_eq!(q_i64(db, &format!("SELECT COUNT(*) FROM releases WHERE id={stray_release}")), 0);
        assert_eq!(q_i64(db, &format!("SELECT COUNT(*) FROM tracks WHERE id={track}")), 0);
        assert_eq!(q_i64(db, &format!("SELECT play_count FROM tracks WHERE id={keeper}")), 4);
        assert_eq!(q_i64(db, &format!("SELECT rating FROM tracks WHERE id={keeper}")), 5);
        assert_eq!(q_i64(db, &format!("SELECT loved FROM tracks WHERE id={keeper}")), 1);
        assert_eq!(q_i64(db, "SELECT track_id FROM playlist_items"), keeper, "the crate follows the surviving track");
        assert!(keeper_path.is_file());
        assert!(!path.exists(), "the second copy of the same file goes");
    }

    #[test]
    fn a_compilation_track_lands_under_the_album_artist() {
        // The real shape of a label's stray: one track off a 30-track V/A record. The album page
        // lists its tracks as "Artist - Title", so the numbering falls back to the page's tail
        // match; and bandcamp-dl would file the record under "Various Artists".
        if !have_ffmpeg() {
            return;
        }
        let f = fx();
        let (_r, track, path) = f.stray_on_disk("Marcal");
        let comp = album(|a| {
            a.title = "Federation Of Rytm IV".into();
            a.artist_name = "Various Artists".into();
            a.release_date = Some("2025-10-24".into());
            a.tracks = vec![
                AlbumTrack { title: "X&B - Strobocop".into(), track_num: Some(1), ..Default::default() },
                AlbumTrack { title: "Marcal - Greedy Man".into(), track_num: Some(20), ..Default::default() },
            ];
        });
        let outcome = merge(&f.env, &LoftyRetagger, &f.first(), &comp).unwrap();
        assert_eq!(outcome.status, Status::Merged);
        let db = &f.env.db;
        let a = outcome.album_release_id.unwrap();
        assert_eq!(q_str(db, &format!("SELECT ar.name FROM releases r JOIN artists ar ON ar.id=r.artist_id WHERE r.id={a}")).as_deref(), Some("Various Artists"));
        assert_eq!(q_i64(db, &format!("SELECT expected_track_count FROM releases WHERE id={a}")), 2);
        assert_eq!(q_i64(db, &format!("SELECT track_no FROM tracks WHERE id={track}")), 20, "the page's own numbering wins");
        let landed = f.music.join("various-artists").join("federation-of-rytm-iv").join(path.file_name().unwrap());
        assert!(landed.is_file(), "beside where the rest of the record will download");
        let (alb, album_artist, _) = read_tags(&landed);
        assert_eq!(alb.as_deref(), Some("Federation Of Rytm IV"));
        assert_eq!(album_artist.as_deref(), Some("Various Artists"));
    }

    // ---- the sweep

    struct Fake {
        resolve_delay_ms: u64,
        calls: parking_lot::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl BandcampLookup for Fake {
        async fn resolve_album_url(&self, url: &str) -> Result<String, LookupError> {
            if self.resolve_delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.resolve_delay_ms)).await;
            }
            self.calls.lock().push(format!("resolve {url}"));
            Ok(if url == TRACK_URL { ALBUM_URL.to_string() } else { url.to_string() })
        }
        async fn fetch_album(&self, url: &str) -> Result<AlbumInfo, LookupError> {
            self.calls.lock().push(format!("fetch {url}"));
            assert_eq!(url, ALBUM_URL);
            Ok(album(|_| {}))
        }
    }

    async fn wait_done(m: &StrayMerger) {
        for _ in 0..400 {
            if !m.state().running {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("sweep did not finish: {:?}", m.state());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_sweep_resolves_merges_and_settles_singles() {
        if !have_ffmpeg() {
            return;
        }
        let f = fx();
        f.stray_on_disk("Chl\u{e4}r");
        let alone = release(&f.env.db, "Alone", "Solo", &format!("bandcamp_url='{SINGLE_URL}'"));
        seed_track(&f.env.db, alone, "Alone", Some(3));
        let mut rx = f.env.bus.subscribe();
        let m = StrayMerger::new(f.env.ctx.clone(), Arc::new(Fake { resolve_delay_ms: 0, calls: Default::default() }), Arc::new(LoftyRetagger));
        let started = m.start(None, None, None).unwrap();
        assert!(started.running);
        wait_done(&m).await;
        let s = m.state();
        assert_eq!(s.phase, "done", "{s:?}");
        assert_eq!((s.seen, s.merged, s.singles, s.failed), (2, 1, 1, 0));
        assert_eq!(s.albums_created, 1);
        assert!(list(&f.env.db).is_empty(), "both are gone from the candidate list, for different reasons");
        assert_eq!(q_str(&f.env.db, &format!("SELECT kind FROM releases WHERE id={alone}")).as_deref(), Some("single"));
        // library.strays events were published along the way, ending in the final state.
        let mut last = None;
        while let Ok(ev) = rx.try_recv() {
            if ev.topic == TOPIC_LIBRARY_STRAYS {
                last = Some(ev.payload);
            }
        }
        assert_eq!(last.unwrap()["phase"], "done");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_sweep_queues_the_rest_of_the_record() {
        // An album built from one stray is not left standing at "1/5".
        if !have_ffmpeg() {
            return;
        }
        let f = fx();
        f.stray_on_disk("Chl\u{e4}r");
        let m = StrayMerger::new(f.env.ctx.clone(), Arc::new(Fake { resolve_delay_ms: 0, calls: Default::default() }), Arc::new(LoftyRetagger));
        m.start(None, None, None).unwrap();
        wait_done(&m).await;
        let s = m.state();
        assert_eq!(s.phase, "done", "{s:?}");
        assert_eq!(s.fills_queued, 1);
        assert_eq!(q_str(&f.env.db, "SELECT url FROM job_items").as_deref(), Some(ALBUM_URL));
        assert_eq!(q_str(&f.env.db, "SELECT url_kind FROM job_items").as_deref(), Some("album"));
        assert!(q_str(&f.env.db, "SELECT params FROM jobs").unwrap().contains("\"force\":true"), "the fill must get past 'already in the library'");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_sweep_is_refused_while_one_is_walking() {
        // Two sweeps would fetch the same pages twice and merge against each other.
        let env = test_env();
        let r = release(&env.db, "Greedy Man", "X", &format!("bandcamp_url='{TRACK_URL}'"));
        seed_track(&env.db, r, "Greedy Man", Some(5));
        let m = StrayMerger::new(env.ctx.clone(), Arc::new(Fake { resolve_delay_ms: 5000, calls: Default::default() }), Arc::new(LoftyRetagger));
        m.start(None, None, None).unwrap();
        assert!(matches!(m.start(None, None, None), Err(AlreadyRunning)));
        m.stop().await;
        assert_eq!(m.state().error.as_deref(), Some("Cancelled"), "stopping mid-walk is reported, not hidden");
        assert!(!m.state().running);
        // And a stopped sweep can be started again.
        assert!(m.start(None, None, None).is_ok());
        m.stop().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unresolvable_stray_is_counted_not_fetched() {
        let env = test_env();
        let r = release(&env.db, "Orphan", "X", "");
        seed_track(&env.db, r, "Orphan", Some(7));
        let fake = Arc::new(Fake { resolve_delay_ms: 0, calls: Default::default() });
        let m = StrayMerger::new(env.ctx.clone(), fake.clone(), Arc::new(LoftyRetagger));
        m.start(None, Some(vec![r]), None).unwrap();
        wait_done(&m).await;
        assert_eq!((m.state().unresolved, m.state().merged), (1, 0));
        assert!(fake.calls.lock().is_empty());
    }
}
