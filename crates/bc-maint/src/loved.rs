//! Loved Bandcamp streams, and turning one into a loved library track (port of
//! `services/library/loved.py` plus the CRUD of the `/loved-streams` routes).
//!
//! Loving a stream and owning the record were two disconnected facts. A `loved_streams` row
//! records "I want to keep hold of this" while browsing; once the release is downloaded the
//! library holds the same music as a real track, and nothing joined the two. This module is that
//! join. It runs after every download that ingests a release ([`reconcile_release`]), as a sweep
//! over everything acquired before ([`reconcile_all`]) and as the adoption half of the
//! loved-streams download ([`adopt_streams`]).
//!
//! Matching is deliberately conservative: an exact URL match first, then an exact fold of
//! `(artist, title)`, and no fuzziness. A wrong match hearts a track the user never loved and
//! silently drops the stream that recorded what they did.

use std::collections::HashSet;

use bc_db::rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use bc_db::util::{iso, name_key, now_db};
use bc_libcore::{ApiError, ApiResult};
use bc_types::library::{LovedStreamIn, LovedStreamOut};

use crate::urls::{UrlKind, classify, normalise};
use crate::util::{set_setting, setting};

pub const AUTO_DOWNLOAD_KEY: &str = "loved.auto_download";

/// One `loved_streams` row.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LovedStream {
    pub id: i64,
    pub page_url: String,
    pub track_key: String,
    pub bc_track_id: Option<i64>,
    pub track_index: Option<i64>,
    pub title: String,
    pub artist_name: String,
    pub release_title: String,
    pub art_url: Option<String>,
    pub duration_ms: Option<i64>,
    pub added_at: String,
}

const COLS: &str = "id, page_url, track_key, bc_track_id, track_index, title, artist_name, release_title, art_url, duration_ms, added_at";

fn row(r: &Row<'_>) -> bc_db::rusqlite::Result<LovedStream> {
    Ok(LovedStream {
        id: r.get(0)?,
        page_url: r.get(1)?,
        track_key: r.get(2)?,
        bc_track_id: r.get(3)?,
        track_index: r.get(4)?,
        title: r.get(5)?,
        artist_name: r.get(6)?,
        release_title: r.get(7)?,
        art_url: r.get(8)?,
        duration_ms: r.get(9)?,
        added_at: r.get(10)?,
    })
}

/// Percent-encode like Python's `quote(s, safe='')`.
fn quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl LovedStream {
    pub fn out(&self) -> LovedStreamOut {
        LovedStreamOut {
            stream: LovedStreamIn {
                page_url: self.page_url.clone(),
                track_key: self.track_key.clone(),
                bc_track_id: self.bc_track_id,
                track_index: self.track_index,
                title: self.title.clone(),
                artist_name: self.artist_name.clone(),
                release_title: self.release_title.clone(),
                art_url: self.art_url.clone(),
                duration_ms: self.duration_ms,
            },
            id: self.id,
            added_at: Some(iso(&self.added_at)),
            // Our proxy, not Bandcamp's CDN: it re-signs on every request, which is what lets a
            // loved stream still play months later.
            stream_url: format!("/api/explore/stream?release={}&track={}", quote(&self.page_url), quote(&self.track_key)),
        }
    }
}

// ---------------------------------------------------------------------------- CRUD

/// Newest first.
pub fn list(c: &Connection) -> ApiResult<Vec<LovedStream>> {
    let mut st = c.prepare(&format!("SELECT {COLS} FROM loved_streams ORDER BY added_at DESC, id DESC"))?;
    Ok(st.query_map([], row)?.collect::<Result<_, _>>()?)
}

/// Love a stream; idempotent (the same track twice returns the existing row). Returns the row and
/// whether it is new. Only Bandcamp album/track pages may be loved (the stored page is handed to
/// an HTTP client on every play).
pub fn love(t: &Transaction<'_>, body: &LovedStreamIn) -> ApiResult<(LovedStream, bool)> {
    let page_url = body.page_url.trim();
    if page_url.is_empty() || !matches!(classify(page_url), UrlKind::Album | UrlKind::Track) {
        return Err(ApiError::bad("page_url must be a Bandcamp album or track URL"));
    }
    if body.track_key.trim().is_empty() {
        return Err(ApiError::bad("track_key is required"));
    }
    let canonical = normalise(page_url);
    let existing: Option<LovedStream> = t
        .query_row(&format!("SELECT {COLS} FROM loved_streams WHERE page_url = ?1 AND track_key = ?2"), (&canonical, &body.track_key), row)
        .optional()?;
    let is_new = existing.is_none();
    let mut e = existing.unwrap_or_else(|| LovedStream { page_url: canonical.clone(), track_key: body.track_key.clone(), added_at: now_db(), ..Default::default() });
    e.bc_track_id = body.bc_track_id;
    e.track_index = body.track_index;
    let keep = |new: &str, old: &str| if new.is_empty() { old.to_string() } else { new.to_string() };
    e.title = keep(&body.title, &e.title);
    e.artist_name = keep(&body.artist_name, &e.artist_name);
    e.release_title = keep(&body.release_title, &e.release_title);
    e.art_url = body.art_url.clone().filter(|s| !s.is_empty()).or(e.art_url);
    e.duration_ms = body.duration_ms.filter(|d| *d != 0).or(e.duration_ms);
    if is_new {
        t.execute(
            "INSERT INTO loved_streams (page_url, track_key, bc_track_id, track_index, title, artist_name, release_title, art_url, duration_ms, added_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![e.page_url, e.track_key, e.bc_track_id, e.track_index, e.title, e.artist_name, e.release_title, e.art_url, e.duration_ms, e.added_at],
        )?;
        e.id = t.last_insert_rowid();
    } else {
        t.execute(
            "UPDATE loved_streams SET bc_track_id=?1, track_index=?2, title=?3, artist_name=?4, release_title=?5, art_url=?6, duration_ms=?7 WHERE id=?8",
            params![e.bc_track_id, e.track_index, e.title, e.artist_name, e.release_title, e.art_url, e.duration_ms, e.id],
        )?;
    }
    Ok((e, is_new))
}

/// Un-love by `(page_url, track_key)`; `false` when it was not loved.
pub fn unlove(t: &Transaction<'_>, page_url: &str, track_key: &str) -> ApiResult<bool> {
    Ok(t.execute("DELETE FROM loved_streams WHERE page_url = ?1 AND track_key = ?2", (normalise(page_url), track_key))? > 0)
}

/// Whether loving a Bandcamp stream should also fetch its album.
pub fn auto_download(c: &Connection) -> ApiResult<bool> {
    Ok(setting(c, AUTO_DOWNLOAD_KEY)?.as_deref() == Some("1"))
}

pub fn set_auto_download(t: &Transaction<'_>, enabled: bool) -> ApiResult<()> {
    set_setting(t, AUTO_DOWNLOAD_KEY, if enabled { "1" } else { "0" })
}

// ---------------------------------------------------------------------------- reconcile

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReconcileReport {
    /// Library tracks hearted.
    pub loved: i64,
    /// Stream rows removed, having been superseded by a real track.
    pub streams_cleared: i64,
    pub track_ids: Vec<i64>,
    pub stream_ids: Vec<i64>,
}

impl ReconcileReport {
    fn merge(&mut self, o: ReconcileReport) {
        self.loved += o.loved;
        self.streams_cleared += o.streams_cleared;
        self.track_ids.extend(o.track_ids);
        self.stream_ids.extend(o.stream_ids);
    }
}

fn url_keys(value: &str) -> Vec<String> {
    if value.is_empty() {
        return vec![];
    }
    let mut v = vec![value.to_string()];
    let n = normalise(value);
    if n != value {
        v.push(n);
    }
    v
}

/// Undo the "Artist - Title" naming a label page uses for every track: the exact single anchored
/// cut bandcamp-dl applies when it writes the tag the library reads.
fn strip_artist_prefix<'a>(title: &'a str, artist: &str) -> &'a str {
    let prefix = format!("{artist} - ");
    if !artist.is_empty() && title.starts_with(&prefix) { &title[prefix.len()..] } else { title }
}

/// `track_index` is 0-based (the client writes `track_no - 1`).
fn track_no_of(s: &LovedStream) -> Option<i64> {
    s.track_index.map(|i| i + 1)
}

struct TrackRow {
    id: i64,
    title_key: String,
    track_no: Option<i64>,
}

/// Which track of the release the stream is. Title first (what the user actually hearted), position
/// second.
fn pick_track(c: &Connection, release_id: i64, s: &LovedStream) -> ApiResult<Option<i64>> {
    let tracks: Vec<TrackRow> = {
        let mut st = c.prepare("SELECT id, title_key, track_no FROM tracks WHERE release_id = ?1 ORDER BY id")?;
        st.query_map([release_id], |r| Ok(TrackRow { id: r.get(0)?, title_key: r.get(1)?, track_no: r.get(2)? }))?.collect::<Result<_, _>>()?
    };
    if tracks.is_empty() {
        return Ok(None);
    }
    let track_no = track_no_of(s);
    // Both spellings, page form first: stripping is right for a label upload and wrong for a
    // track genuinely titled "X - Y" by an artist called "X".
    let mut wanted: Vec<String> = Vec::new();
    for k in [name_key(&s.title), name_key(strip_artist_prefix(&s.title, &s.artist_name))] {
        if !k.is_empty() && !wanted.contains(&k) {
            wanted.push(k);
        }
    }
    for w in &wanted {
        let exact: Vec<&TrackRow> = tracks.iter().filter(|t| &t.title_key == w).collect();
        if exact.len() == 1 {
            return Ok(Some(exact[0].id));
        }
        if !exact.is_empty() {
            // Two tracks folding to the same title on one release: prefer the one whose position
            // also agrees, rather than picking arbitrarily.
            if let Some(n) = track_no
                && let Some(t) = exact.iter().find(|t| t.track_no == Some(n))
            {
                return Ok(Some(t.id));
            }
            return Ok(Some(exact[0].id));
        }
    }
    if let Some(n) = track_no
        && let Some(t) = tracks.iter().find(|t| t.track_no == Some(n))
    {
        return Ok(Some(t.id));
    }
    // A single-track release can only be the one thing, whatever it is called.
    if tracks.len() == 1 {
        return Ok(Some(tracks[0].id));
    }
    Ok(None)
}

/// The library release a stream refers to, by URL then by name.
fn release_for(c: &Connection, s: &LovedStream) -> ApiResult<Option<i64>> {
    for k in url_keys(&s.page_url) {
        if let Some(id) = c.query_row("SELECT id FROM releases WHERE bandcamp_url = ?1", [&k], |r| r.get::<_, i64>(0)).optional()? {
            return Ok(Some(id));
        }
    }
    if s.artist_name.is_empty() || s.release_title.is_empty() {
        return Ok(None);
    }
    let (a, ti) = (name_key(&s.artist_name), name_key(&s.release_title));
    if a.is_empty() || ti.is_empty() {
        return Ok(None);
    }
    Ok(c.query_row(
        "SELECT r.id FROM releases r JOIN artists a ON a.id = r.artist_id WHERE a.name_key = ?1 AND r.title_key = ?2 ORDER BY r.id LIMIT 1",
        (&a, &ti),
        |r| r.get::<_, i64>(0),
    )
    .optional()?)
}

fn adopt_one(t: &Transaction<'_>, s: &LovedStream, release_id: i64, report: &mut ReconcileReport) -> ApiResult<bool> {
    let Some(track_id) = pick_track(t, release_id, s)? else { return Ok(false) };
    let was_loved: bool = t.query_row("SELECT loved FROM tracks WHERE id = ?1", [track_id], |r| r.get(0))?;
    if !was_loved {
        t.execute("UPDATE tracks SET loved = 1 WHERE id = ?1", [track_id])?;
        report.loved += 1;
        report.track_ids.push(track_id);
    }
    report.stream_ids.push(s.id);
    report.streams_cleared += 1;
    t.execute("DELETE FROM loved_streams WHERE id = ?1", [s.id])?;
    Ok(true)
}

/// Convert the loved streams that pointed at a release just downloaded. Owns no transaction of its
/// own: the heart and the removal of the stream it supersedes land together in the caller's.
pub fn reconcile_release(t: &Transaction<'_>, release_id: i64) -> ApiResult<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let rel: Option<(Option<String>, Option<String>, Option<i64>)> = t
        .query_row("SELECT bandcamp_url, title_key, artist_id FROM releases WHERE id = ?1", [release_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let Some((url, title_key, artist_id)) = rel else { return Ok(report) };
    let mut candidates: Vec<LovedStream> = Vec::new();
    for k in url_keys(url.as_deref().unwrap_or("")) {
        let mut st = t.prepare(&format!("SELECT {COLS} FROM loved_streams WHERE page_url = ?1"))?;
        candidates.extend(st.query_map([&k], row)?.collect::<Result<Vec<_>, _>>()?);
    }
    // And by name, for a release whose URL was never recorded -- roughly one in ten -- or a
    // stream loved from a track page under a different URL.
    if let (Some(tk), Some(aid)) = (title_key.filter(|k| !k.is_empty()), artist_id) {
        let artist_key: Option<String> = t.query_row("SELECT name_key FROM artists WHERE id = ?1", [aid], |r| r.get(0)).optional()?;
        if let Some(ak) = artist_key {
            let seen: HashSet<i64> = candidates.iter().map(|s| s.id).collect();
            let mut st = t.prepare(&format!("SELECT {COLS} FROM loved_streams"))?;
            for s in st.query_map([], row)? {
                let s = s?;
                if !seen.contains(&s.id) && name_key(&s.artist_name) == ak && name_key(&s.release_title) == tk {
                    candidates.push(s);
                }
            }
        }
    }
    for s in &candidates {
        if report.stream_ids.contains(&s.id) {
            continue;
        }
        adopt_one(t, s, release_id, &mut report)?;
    }
    if report.streams_cleared > 0 {
        tracing::info!(n = report.streams_cleared, release_id, "loved: stream(s) became library tracks");
    }
    Ok(report)
}

/// Adopt streams into releases someone else already resolved (the loved-streams download resolves
/// track page -> parent album with the Bandcamp client in hand). Returns the report and the pairs
/// that did *not* adopt -- a stream whose track is absent from the local copy of its release,
/// which is the caller's cue to fill the record rather than call it owned.
pub fn adopt_streams(t: &Transaction<'_>, pairs: &[(LovedStream, i64)]) -> ApiResult<(ReconcileReport, Vec<(LovedStream, i64)>)> {
    let mut report = ReconcileReport::default();
    let mut leftover = Vec::new();
    for (s, rid) in pairs {
        if !adopt_one(t, s, *rid, &mut report)? {
            leftover.push((s.clone(), *rid));
        }
    }
    if report.streams_cleared > 0 {
        tracing::info!(n = report.streams_cleared, "loved: stream(s) adopted through their resolved album URL");
    }
    Ok((report, leftover))
}

/// Sweep every loved stream against what the library already holds (cheap: there are tens of
/// these rows, not tens of thousands).
pub fn reconcile_all(t: &Transaction<'_>) -> ApiResult<ReconcileReport> {
    let mut report = ReconcileReport::default();
    let streams = {
        let mut st = t.prepare(&format!("SELECT {COLS} FROM loved_streams ORDER BY id"))?;
        st.query_map([], row)?.collect::<Result<Vec<_>, _>>()?
    };
    for s in streams {
        if let Some(rid) = release_for(t, &s)? {
            let mut one = ReconcileReport::default();
            adopt_one(t, &s, rid, &mut one)?;
            report.merge(one);
        }
    }
    if report.streams_cleared > 0 {
        tracing::info!(n = report.streams_cleared, "loved: stream(s) already in the library");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use bc_db::Db;

    const PAGE: &str = "https://sportinglife.bandcamp.com/album/slam-dunk-vol-i";

    fn release(db: &Db, url: Option<&str>, tracks: &[&str]) -> i64 {
        let rid = seed_release(db, "Sporting Life", "Slam Dunk Vol. I", url, None);
        for (i, name) in tracks.iter().enumerate() {
            seed_track(db, rid, name, Some(i as i64 + 1));
        }
        rid
    }
    fn release_default(db: &Db) -> i64 {
        release(db, Some(PAGE), &["Hydrate The Hustle", "Badminton"])
    }

    #[derive(Clone)]
    struct S {
        page_url: String,
        title: String,
        artist: String,
        index: Option<i64>,
    }
    fn stream() -> S {
        S { page_url: PAGE.into(), title: "Hydrate The Hustle".into(), artist: "Sporting Life".into(), index: Some(0) }
    }
    fn add_stream(db: &Db, s: S) {
        db.write(move |t| {
            t.execute(
                "INSERT INTO loved_streams(page_url,track_key,track_index,title,artist_name,release_title,added_at) VALUES (?1,'12345',?2,?3,?4,'Slam Dunk Vol. I','2024-01-01 00:00:00')",
                (&s.page_url, s.index, &s.title, &s.artist),
            )?;
            Ok(())
        })
        .unwrap();
    }
    fn reconcile_rel(db: &Db, rid: i64) -> ReconcileReport {
        db.write(move |t| Ok(reconcile_release(t, rid).unwrap())).unwrap()
    }
    fn streams(db: &Db) -> i64 {
        q_i64(db, "SELECT COUNT(*) FROM loved_streams")
    }
    fn track_title(db: &Db, id: i64) -> String {
        q_str(db, &format!("SELECT title FROM tracks WHERE id={id}")).unwrap()
    }

    #[test]
    fn a_stream_becomes_the_track_its_album_provided() {
        let db = test_db();
        let rid = release_default(&db);
        add_stream(&db, stream());
        let report = reconcile_rel(&db, rid);
        assert_eq!((report.loved, report.streams_cleared), (1, 1));
        // The heart lands on the right track, not merely on some track.
        assert_eq!(track_title(&db, report.track_ids[0]), "Hydrate The Hustle");
        assert_eq!(q_i64(&db, &format!("SELECT loved FROM tracks WHERE id={}", report.track_ids[0])), 1);
        assert_eq!(streams(&db), 0);
    }

    #[test]
    fn a_release_with_no_url_still_matches_by_name() {
        let db = test_db();
        let rid = release(&db, None, &["Hydrate The Hustle", "Badminton"]);
        add_stream(&db, stream());
        assert_eq!(reconcile_rel(&db, rid).streams_cleared, 1);
    }

    #[test]
    fn the_sweep_adopts_streams_already_in_the_library() {
        let db = test_db();
        release_default(&db);
        add_stream(&db, stream());
        let report = db.write(|t| Ok(reconcile_all(t).unwrap())).unwrap();
        assert_eq!(report.loved, 1);
        assert_eq!(streams(&db), 0);
    }

    #[test]
    fn a_stream_for_a_record_not_held_is_left_alone() {
        let db = test_db();
        add_stream(&db, S { page_url: "https://other.bandcamp.com/album/nope".into(), ..stream() });
        let report = db.write(|t| Ok(reconcile_all(t).unwrap())).unwrap();
        assert_eq!((report.loved, report.streams_cleared), (0, 0));
        assert_eq!(streams(&db), 1);
    }

    #[test]
    fn a_wrong_title_does_not_heart_an_arbitrary_track() {
        let db = test_db();
        let rid = release_default(&db);
        add_stream(&db, S { title: "Not On This Record".into(), index: None, ..stream() });
        let report = reconcile_rel(&db, rid);
        assert_eq!((report.loved, report.streams_cleared), (0, 0));
        assert_eq!(streams(&db), 1);
        assert_eq!(q_i64(&db, "SELECT COUNT(*) FROM tracks WHERE loved=1"), 0);
    }

    #[test]
    fn a_single_track_release_matches_despite_a_renamed_title() {
        let db = test_db();
        let rid = release(&db, Some(PAGE), &["01 Hydrate The Hustle (Original Mix)"]);
        add_stream(&db, S { index: None, ..stream() });
        assert_eq!(reconcile_rel(&db, rid).loved, 1);
    }

    #[test]
    fn a_label_pages_artist_prefix_is_not_a_different_track() {
        let db = test_db();
        let rid = release(&db, Some(PAGE), &["Live and Let Live", "Do It Right"]);
        add_stream(&db, S { title: "DJ Alone Again - Live and Let Live".into(), artist: "DJ Alone Again".into(), index: None, ..stream() });
        let report = reconcile_rel(&db, rid);
        assert_eq!(report.loved, 1);
        assert_eq!(track_title(&db, report.track_ids[0]), "Live and Let Live");
    }

    #[test]
    fn a_title_that_only_looks_prefixed_still_matches_itself() {
        let db = test_db();
        let rid = release(&db, Some(PAGE), &["Ekman - Vertical Mind", "Ekman"]);
        add_stream(&db, S { title: "Ekman - Vertical Mind".into(), artist: "Ekman".into(), index: None, ..stream() });
        let report = reconcile_rel(&db, rid);
        assert_eq!(track_title(&db, report.track_ids[0]), "Ekman - Vertical Mind");
    }

    #[test]
    fn the_position_fallback_reads_the_index_as_zero_based() {
        let db = test_db();
        let rid = release(&db, Some(PAGE), &["Hydrate The Hustle", "Badminton", "Crossover"]);
        // A title the tags never carried, so only the position can decide.
        add_stream(&db, S { title: "Badminton (Original Mix)".into(), index: Some(1), ..stream() });
        let report = reconcile_rel(&db, rid);
        assert_eq!(report.loved, 1);
        assert_eq!(track_title(&db, report.track_ids[0]), "Badminton");
    }

    #[test]
    fn the_first_track_of_a_release_can_be_matched_by_position() {
        let db = test_db();
        let rid = release_default(&db);
        add_stream(&db, S { title: "Hydrate The Hustle (Bonus Edit)".into(), index: Some(0), ..stream() });
        let report = reconcile_rel(&db, rid);
        assert_eq!(report.loved, 1);
        assert_eq!(track_title(&db, report.track_ids[0]), "Hydrate The Hustle");
    }

    #[test]
    fn reconciling_twice_hearts_nothing_extra() {
        let db = test_db();
        let rid = release_default(&db);
        add_stream(&db, stream());
        reconcile_rel(&db, rid);
        let again = reconcile_rel(&db, rid);
        assert_eq!((again.loved, again.streams_cleared), (0, 0));
    }

    // ---- test_loved_streams.py (service half; the routes are tested in routes::tests)

    fn body(over: impl FnOnce(&mut LovedStreamIn)) -> LovedStreamIn {
        let mut b = LovedStreamIn {
            page_url: PAGE.into(),
            track_key: "12345".into(),
            bc_track_id: Some(12345),
            track_index: None,
            title: "Hydrate The Hustle".into(),
            artist_name: "Sporting Life".into(),
            release_title: "Slam Dunk Vol. I".into(),
            art_url: None,
            duration_ms: Some(263_000),
        };
        over(&mut b);
        b
    }

    #[test]
    fn a_stream_can_be_loved_and_stays_playable() {
        let db = test_db();
        let (row, is_new) = db.write(|t| Ok(love(t, &body(|_| {})).unwrap())).unwrap();
        assert!(is_new);
        let out = row.out();
        assert!(out.stream_url.starts_with("/api/explore/stream?release="));
        assert!(out.stream_url.contains("track=12345"));
        assert!(!out.stream_url.contains("bcbits"), "must not pin a signed CDN URL");
        assert_eq!(db.read(|c| Ok(list(c).unwrap())).unwrap()[0].title, "Hydrate The Hustle");
    }

    #[test]
    fn loving_the_same_track_twice_is_one_row() {
        let db = test_db();
        let (a, _) = db.write(|t| Ok(love(t, &body(|_| {})).unwrap())).unwrap();
        let (b, new2) = db.write(|t| Ok(love(t, &body(|_| {})).unwrap())).unwrap();
        assert!(!new2);
        assert_eq!(a.id, b.id);
        assert_eq!(streams(&db), 1);
    }

    #[test]
    fn unloving_removes_it() {
        let db = test_db();
        db.write(|t| Ok(love(t, &body(|_| {})).unwrap())).unwrap();
        assert!(db.write(|t| Ok(unlove(t, PAGE, "12345").unwrap())).unwrap());
        assert_eq!(streams(&db), 0);
        assert!(!db.write(|t| Ok(unlove(t, PAGE, "12345").unwrap())).unwrap());
    }

    #[test]
    fn only_bandcamp_pages_may_be_loved() {
        let db = test_db();
        let bad = db.write(|t| Ok(love(t, &body(|b| b.page_url = "https://evil.example.com/x".into())).unwrap_err().status())).unwrap();
        assert_eq!(bad, 400);
        let bad = db.write(|t| Ok(love(t, &body(|b| b.track_key = "".into())).unwrap_err().status())).unwrap();
        assert_eq!(bad, 400);
    }

    #[test]
    fn a_release_without_track_ids_is_keyed_positionally() {
        let db = test_db();
        let (row, _) = db.write(|t| Ok(love(t, &body(|b| {
            b.track_key = "i2".into();
            b.bc_track_id = None;
            b.track_index = Some(2);
        })).unwrap())).unwrap();
        assert!(row.out().stream_url.contains("track=i2"));
        assert_eq!((row.bc_track_id, row.track_index), (None, Some(2)));
    }

    #[test]
    fn auto_download_is_off_until_asked_for() {
        let db = test_db();
        assert!(!db.read(|c| Ok(auto_download(c).unwrap())).unwrap());
        db.write(|t| Ok(set_auto_download(t, true).unwrap())).unwrap();
        assert!(db.read(|c| Ok(auto_download(c).unwrap())).unwrap());
        db.write(|t| Ok(set_auto_download(t, false).unwrap())).unwrap();
        assert!(!db.read(|c| Ok(auto_download(c).unwrap())).unwrap());
    }
}
