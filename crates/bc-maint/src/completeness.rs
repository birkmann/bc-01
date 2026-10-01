//! What a release *should* hold, versus what it does (port of `services/library/completeness.py`).
//!
//! `releases.expected_track_count` records how many tracks the record has on Bandcamp -- learned
//! from a download run, from the files' own "3/12" numbering at ingest, or backfilled from the
//! harvest inbox's copy of the release page. NULL means "never learned". The count of actual
//! track rows is never stored; listings compute it live ([`bc_libcore::hydrate`]), so "have 4,
//! expect 12" marks an album as missing tracks and offers the one-click fill.
//!
//! The queueing half lives here too: filling one release ([`fill_release`]), filling every
//! release short of tracks ([`fill_all`], [`queue_fills`]) and the sweep the strays merger runs so
//! an album it just built does not sit at "1/12". Jobs go into the shared durable queue through
//! [`bc_libcore::queue`]; nothing here depends on the download crate.

use std::collections::{BTreeMap, HashSet};

use bc_db::rusqlite::{Connection, OptionalExtension, Transaction};
use bc_libcore::queue::{self, NewItem};
use bc_libcore::{ApiError, ApiResult, Ctx, hydrate};
use bc_types::library::{FillAllResult, FillResult};
use serde_json::json;

use crate::urls::{is_track, shelf_name};

pub use bc_libcore::hydrate::missing_release_ids;
pub use bc_libcore::queue::fill_source_url;

/// Priority of a hand-clicked fill: same tier as a loved-streams batch, ahead of harvest sweeps
/// without jumping a hand-pasted queue.
pub const PRIORITY_SINGLE: i64 = 90;
/// Priority of a library-wide sweep.
pub const PRIORITY_SWEEP: i64 = 100;

/// Stamp what a download run said the release holds. The newest run wins outright: Bandcamp's
/// page is the authority and a re-download read it again.
pub fn record_expected(c: &Connection, release_id: i64, expected: Option<i64>) -> ApiResult<()> {
    let Some(n) = expected.filter(|n| *n > 0) else { return Ok(()) };
    c.execute("UPDATE releases SET expected_track_count = ?1 WHERE id = ?2 AND expected_track_count IS NOT ?1", (n, release_id))?;
    Ok(())
}

/// Fill `expected_track_count` from the harvest inbox, once per gap. Album rows only (a `/track/`
/// item's count describes the track page, which is 1 whatever the record holds). Idempotent:
/// only releases still NULL that an inbox row links to are touched. Two steps (one aggregate
/// scan, then primary-key updates), because `release_id` once cost a full inbox scan per release.
pub fn backfill_expected_counts(t: &Transaction<'_>) -> ApiResult<usize> {
    let counts: Vec<(i64, i64)> = {
        let mut st = t.prepare(
            "SELECT release_id, MAX(track_count) FROM harvest_items
              WHERE release_id IS NOT NULL AND url_kind = 'album' AND track_count IS NOT NULL GROUP BY release_id",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
    };
    let mut filled = 0;
    let mut up = t.prepare_cached("UPDATE releases SET expected_track_count = ?1 WHERE id = ?2 AND expected_track_count IS NULL")?;
    for (rid, n) in counts {
        filled += up.execute((n, rid))?;
    }
    if filled > 0 {
        tracing::info!(filled, "learned expected track counts from the inbox");
    }
    Ok(filled)
}

/// Drop a Bandcamp URL from a release the download proved it is not.
///
/// Called when a fill queued for `release_id` from `url` was ingested as `landed_on` instead: the
/// page names that other record. The URL is not thrown away but handed to `landed_on` (only when
/// it has no URL of its own; the column is UNIQUE, so the old owner is cleared first). The
/// disowned release then has no link to Bandcamp, which drops it out of the missing-tracks count.
pub fn disown_url(t: &Transaction<'_>, release_id: i64, url: Option<&str>, landed_on: i64) -> ApiResult<()> {
    let Some(url) = url else { return Ok(()) };
    let current: Option<Option<String>> = t.query_row("SELECT bandcamp_url FROM releases WHERE id = ?1", [release_id], |r| r.get(0)).optional()?;
    if current.flatten().as_deref() != Some(url) {
        return Ok(());
    }
    let target_url: Option<Option<String>> = t.query_row("SELECT bandcamp_url FROM releases WHERE id = ?1", [landed_on], |r| r.get(0)).optional()?;
    t.execute("UPDATE releases SET bandcamp_url = NULL WHERE id = ?1", [release_id])?;
    if let Some(None) = target_url {
        t.execute("UPDATE releases SET bandcamp_url = ?1 WHERE id = ?2", (url, landed_on))?;
    }
    tracing::warn!(release_id, url, landed_on, "release disowned a URL: the fill for it was ingested as another release");
    Ok(())
}

/// What queueing fills for a batch of releases came to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FillSweep {
    /// Releases short of tracks that the sweep considered.
    pub missing: i64,
    /// Releases now sitting on a download job.
    pub queued: i64,
    /// An unfinished fill for the same URL already covers them.
    pub already_queued: i64,
    /// Nothing ever linked them to Bandcamp; there is no page to fetch.
    pub unfillable: i64,
    pub job_ids: Vec<String>,
}

fn url_kind_of(url: &str) -> &'static str {
    // An honest kind matters: "album" on a /track/ URL would let a failed widening stamp the
    // track URL onto the release as its album page.
    if is_track(url) { "track" } else { "album" }
}

/// The shelf folder of a fan (`fan-<username>`), when the release sits on one.
fn target_of(c: &Connection, fan_id: Option<i64>) -> ApiResult<Option<String>> {
    let Some(fid) = fan_id else { return Ok(None) };
    let row: Option<(String, String)> = c.query_row("SELECT url, username FROM fans WHERE id = ?1", [fid], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    Ok(row.map(|(url, user)| shelf_name(&url, &user)))
}

/// Queue the records behind these releases to fill their missing tracks, at sweep priority.
/// See [`queue_fills_at`].
pub fn queue_fills(ctx: &Ctx, release_ids: &[i64]) -> ApiResult<FillSweep> {
    queue_fills_at(ctx, release_ids, PRIORITY_SWEEP)
}

/// One job per shelf rather than one per release (a backlog of hundreds is a batch to watch
/// drain, and the shelf decides `target_subdir`, a job-level parameter). Each job carries
/// `force` like the single fill so the preflight "already in the library" check does not skip
/// the very releases being repaired (the blacklist still wins in the worker's preflight).
/// Deduplicated against the unfinished download items. Callers pass ids from
/// [`missing_release_ids`]; nothing here re-checks the gap. The download queue is notified once
/// something was queued.
pub fn queue_fills_at(ctx: &Ctx, release_ids: &[i64], priority: i64) -> ApiResult<FillSweep> {
    let ids = release_ids.to_vec();
    let sweep = ctx.write(move |t| {
        let mut sweep = FillSweep { missing: ids.len() as i64, ..Default::default() };
        if ids.is_empty() {
            return Ok(sweep);
        }
        // Queueing twice must not fetch twice: the unfinished-fill check, batched for the sweep.
        let mut pending: HashSet<String> = queue::pending_urls(t)?;
        let mut by_fan: BTreeMap<Option<i64>, Vec<(String, i64)>> = BTreeMap::new();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        for rid in sorted {
            let fan: Option<Option<i64>> = t.query_row("SELECT source_fan_id FROM releases WHERE id = ?1", [rid], |r| r.get(0)).optional()?;
            let Some(fan) = fan else { continue };
            match fill_source_url(t, rid)? {
                None => sweep.unfillable += 1,
                Some(url) if pending.contains(&url) => sweep.already_queued += 1,
                Some(url) => {
                    // Added as we go: two partial releases can resolve to one record.
                    pending.insert(url.clone());
                    by_fan.entry(fan).or_default().push((url, rid));
                }
            }
        }
        for (fan_id, queued) in by_fan {
            let target = target_of(t, fan_id)?;
            let n = queued.len();
            let mut label = format!("fill missing: {n} album{}", if n != 1 { "s" } else { "" });
            if let Some(tg) = &target {
                label.push_str(&format!(" on {tg}"));
            }
            let items: Vec<NewItem> = queued
                .iter()
                .map(|(url, rid)| NewItem {
                    url: Some(url.clone()),
                    url_kind: Some(url_kind_of(url).into()),
                    source: Some("fill".into()),
                    target_dir: target.clone(),
                    // The record being repaired, so a run that downloads nothing because the files
                    // are already there still teaches it how long Bandcamp says it is.
                    release_id: Some(*rid),
                    track_id: None,
                })
                .collect();
            let job = queue::create_job(t, "download", &label, priority, &json!({"target_subdir": target, "force": true, "source_fan_id": fan_id}), &items)?;
            sweep.queued += n as i64;
            sweep.job_ids.push(job);
        }
        Ok(sweep)
    })?;
    if sweep.queued > 0 {
        tracing::info!(queued = sweep.queued, jobs = sweep.job_ids.len(), "queued fills");
        ctx.jobs.notify_download_queue();
    }
    Ok(sweep)
}

/// The 409 for a fill of tracks Bandcamp has not released yet.
pub fn not_released(date: Option<&str>) -> ApiError {
    match date {
        Some(d) => ApiError::conflict(format!("not released yet (out {d})")),
        None => ApiError::conflict("not released yet"),
    }
}

/// Queue a force re-download of one release, or `Ok(None)` when nothing links it to Bandcamp.
/// 409 "not released yet" when every missing track is an unreleased pre-order track.
/// `fallback_url` is a Bandcamp URL the caller already knows names this release (the loved-streams
/// download uses it for the rare row whose own links have all gone missing). Pressing twice is one
/// download: an unfinished fill for the same URL answers both presses (`detail = "already queued"`).
pub fn queue_fill(ctx: &Ctx, release_id: i64, fallback_url: Option<&str>) -> ApiResult<Option<FillResult>> {
    let fallback = fallback_url.map(str::to_string);
    let res = ctx.write(move |t| {
        let rel: Option<(String, Option<String>, Option<i64>)> = t
            .query_row(
                "SELECT r.title, a.name, r.source_fan_id FROM releases r LEFT JOIN artists a ON a.id = r.artist_id WHERE r.id = ?1",
                [release_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((title, artist, fan_id)) = rel else { return Err(ApiError::not_found(format!("release {release_id} not found"))) };
        let Some(url) = fill_source_url(t, release_id)?.or(fallback) else { return Ok(None) };
        // A pre-order whose missing tracks are all still unreleased cannot be filled: say so
        // instead of queueing a download that would fetch nothing.
        if let Some(out) = hydrate::releases_out(t, &[release_id])?.pop()
            && out.unreleased_count > 0
            && out.fillable_missing == 0
        {
            return Err(not_released(out.release_date.as_deref()));
        }
        let existing: Option<String> = t
            .query_row(
                "SELECT ji.job_id FROM job_items ji JOIN jobs j ON j.id = ji.job_id
                  WHERE ji.url = ?1 AND ji.status IN ('pending','running') AND j.status IN ('queued','running','paused') LIMIT 1",
                [&url],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(job_id) = existing {
            return Ok(Some((FillResult { job_id, url, detail: "already queued".into() }, false)));
        }
        let target = target_of(t, fan_id)?;
        let label = match &artist {
            Some(a) => format!("fill: {a} \u{2014} {title}"),
            None => format!("fill: {title}"),
        };
        let item = NewItem {
            url: Some(url.clone()),
            url_kind: Some(url_kind_of(&url).into()),
            source: Some("fill".into()),
            target_dir: target.clone(),
            release_id: Some(release_id),
            track_id: None,
        };
        let job = queue::create_job(t, "download", &label, PRIORITY_SINGLE, &json!({"target_subdir": target, "force": true, "source_fan_id": fan_id}), &[item])?;
        Ok(Some((FillResult { job_id: job, url, detail: "queued".into() }, true)))
    })?;
    Ok(res.map(|(r, created)| {
        if created {
            ctx.jobs.notify_download_queue();
        }
        r
    }))
}

/// `POST /releases/{id}/fill`: queue the whole record again; 409 when nothing ever linked the
/// release to Bandcamp.
pub fn fill_release(ctx: &Ctx, release_id: i64) -> ApiResult<FillResult> {
    queue_fill(ctx, release_id, None)?
        .ok_or_else(|| ApiError::conflict("No Bandcamp link is recorded for this release, so it cannot be re-downloaded."))
}

/// `POST /releases/fill`: every release short of tracks, grouped into one job per shelf.
pub fn fill_all(ctx: &Ctx) -> ApiResult<FillAllResult> {
    let ids = ctx.read(|c| missing_release_ids(c, None))?;
    let sweep = queue_fills(ctx, &ids)?;
    Ok(FillAllResult { missing: sweep.missing, queued: sweep.queued, already_queued: sweep.already_queued, unfillable: sweep.unfillable, job_ids: sweep.job_ids })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use bc_libcore::hydrate;

    const ALBUM_URL: &str = "https://a.bandcamp.com/album/mutual-rytm";
    const TRACK_URL: &str = "https://a.bandcamp.com/track/one-more";

    /// A release that is linked to Bandcamp unless a test says otherwise: only a record something
    /// can re-download counts as missing tracks.
    fn rel(db: &bc_db::Db, key: &str, url: Option<&str>, expected: Option<i64>) -> i64 {
        let id = seed_release(db, "Artist", &format!("Mutual Rytm {key}"), url, None);
        if let Some(e) = expected {
            exec(db, &format!("UPDATE releases SET expected_track_count={e} WHERE id={id}"));
        }
        id
    }
    fn linked(db: &bc_db::Db, key: &str, expected: Option<i64>) -> i64 {
        rel(db, key, Some(&format!("https://a.bandcamp.com/album/{key}")), expected)
    }
    fn tracks(db: &bc_db::Db, rid: i64, nos: &[i64]) {
        for n in nos {
            seed_track(db, rid, &format!("t{n}"), Some(*n));
        }
    }
    fn expected_of(db: &bc_db::Db, rid: i64) -> Option<i64> {
        db.read(|c| Ok(hydrate::releases_out(c, &[rid]).unwrap()[0].expected_track_count)).unwrap()
    }
    fn missing(db: &bc_db::Db) -> Vec<i64> {
        db.read(|c| Ok(missing_release_ids(c, None).unwrap())).unwrap()
    }

    #[test]
    fn record_expected_overwrites_with_the_newest_run() {
        let db = test_db();
        let r = linked(&db, "x", None);
        let w = |n: Option<i64>| db.write(move |t| Ok(record_expected(t, r, n).unwrap())).unwrap();
        let stored = || q_i64(&db, &format!("SELECT COALESCE(expected_track_count,-1) FROM releases WHERE id={r}"));
        w(Some(12));
        assert_eq!(stored(), 12);
        w(Some(13));
        assert_eq!(stored(), 13, "a re-download read the page again; its count wins outright");
        w(None);
        assert_eq!(stored(), 13, "nothing learned changes nothing");
    }

    #[test]
    fn backfill_learns_from_album_inbox_rows_only() {
        let db = test_db();
        let learned = linked(&db, "x", None);
        let already = linked(&db, "other", Some(7));
        for (url, kind, n, rid) in [(ALBUM_URL, "album", 12, learned), (TRACK_URL, "track", 1, learned)] {
            exec(&db, &format!(
                "INSERT INTO harvest_items(url,url_kind,state,title,artist_name,tags,in_collection,in_wishlist,is_free_download,is_purchasable,is_preorder,discovered_at,track_count,release_id)
                 VALUES ('{url}','{kind}','new','t','a','[]',0,0,0,0,0,'x',{n},{rid})"));
        }
        exec(&db, &format!("UPDATE harvest_items SET release_id={already} WHERE url='{TRACK_URL}' AND 0"));
        assert_eq!(db.write(|t| Ok(backfill_expected_counts(t).unwrap())).unwrap(), 1);
        assert_eq!(expected_of(&db, learned), Some(12));
        assert_eq!(expected_of(&db, already), Some(7), "NULLs only");
        assert_eq!(db.write(|t| Ok(backfill_expected_counts(t).unwrap())).unwrap(), 0, "idempotent");
    }

    #[test]
    fn listing_derives_the_gap_from_count_or_numbering() {
        let db = test_db();
        let stored = linked(&db, "stored", Some(12));
        tracks(&db, stored, &[1, 2, 3, 4]);
        let numbered = linked(&db, "numbered", None);
        tracks(&db, numbered, &[1, 3, 9]);
        let whole = linked(&db, "whole", None);
        tracks(&db, whole, &[1, 2, 3]);
        let outs = db.read(|c| Ok(hydrate::releases_out(c, &[stored, numbered, whole]).unwrap())).unwrap();
        assert_eq!(outs[0].expected_track_count, Some(12));
        assert_eq!(outs[0].track_count, 4);
        assert_eq!(outs[1].expected_track_count, Some(9), "files numbered to 9 with only 3 present: an album with gaps");
        assert_eq!(outs[2].expected_track_count, None, "a whole album stays quiet");
    }

    #[test]
    fn missing_ids_read_the_count_and_the_numbering() {
        let db = test_db();
        let stored = linked(&db, "stored", Some(12));
        tracks(&db, stored, &[1, 2, 3, 4]);
        let numbered = linked(&db, "numbered", None);
        tracks(&db, numbered, &[1, 3, 9]);
        let whole = linked(&db, "whole", None);
        tracks(&db, whole, &[1, 2, 3]);
        let empty = linked(&db, "empty", Some(3));
        let mut want = vec![stored, numbered, empty];
        want.sort();
        assert_eq!(missing(&db), want);
        db.read(|c| {
            assert_eq!(missing_release_ids(c, Some(&[numbered, whole])).unwrap(), vec![numbered]);
            assert!(missing_release_ids(c, Some(&[])).unwrap().is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_stored_count_outranks_the_numbering() {
        let db = test_db();
        let gapped = linked(&db, "gapped", Some(3));
        tracks(&db, gapped, &[1, 2, 4]);
        let unmeasured = linked(&db, "unmeasured", None);
        tracks(&db, unmeasured, &[1, 2, 4]);
        let short = linked(&db, "short", Some(6));
        tracks(&db, short, &[1, 2, 4]);
        let mut want = vec![unmeasured, short];
        want.sort();
        assert_eq!(missing(&db), want);
        let outs = db.read(|c| Ok(hydrate::releases_out(c, &[gapped, unmeasured, short]).unwrap())).unwrap();
        assert_eq!(outs[0].expected_track_count, Some(3), "complete: the card says '3 tracks'");
        assert_eq!(outs[1].expected_track_count, Some(4));
        assert_eq!(outs[2].expected_track_count, Some(6));
    }

    #[test]
    fn a_stray_is_not_a_fill_candidate() {
        let db = test_db();
        let stray = rel(&db, "stray", Some(TRACK_URL), None);
        tracks(&db, stray, &[11]);
        let unlinked = rel(&db, "unlinked", None, None);
        tracks(&db, unlinked, &[5]);
        let partial = rel(&db, "partial", Some(ALBUM_URL), None);
        tracks(&db, partial, &[3]);
        assert_eq!(missing(&db), vec![partial]);
        let outs = db.read(|c| Ok(hydrate::releases_out(c, &[stray, unlinked, partial]).unwrap())).unwrap();
        assert_eq!(outs[0].expected_track_count, None, "the grid stays quiet about strays");
        assert_eq!(outs[1].expected_track_count, None);
        assert_eq!(outs[2].expected_track_count, Some(3));
    }

    #[test]
    fn a_release_with_no_bandcamp_link_is_not_counted_as_missing() {
        let db = test_db();
        let l = linked(&db, "linked", Some(12));
        tracks(&db, l, &[1, 2]);
        let orphan = rel(&db, "orphan", None, Some(9));
        tracks(&db, orphan, &[1]);
        assert_eq!(missing(&db), vec![l]);
        let outs = db.read(|c| Ok(hydrate::releases_out(c, &[l, orphan]).unwrap())).unwrap();
        assert_eq!(outs[0].expected_track_count, Some(12));
        assert_eq!(outs[1].expected_track_count, Some(9), "the card still states what is known");
    }

    #[test]
    fn an_inbox_row_or_a_track_url_counts_as_a_fill_source() {
        let db = test_db();
        let via_inbox = rel(&db, "viainbox", None, Some(12));
        tracks(&db, via_inbox, &[1]);
        exec(&db, &format!(
            "INSERT INTO harvest_items(url,url_kind,state,title,artist_name,tags,in_collection,in_wishlist,is_free_download,is_purchasable,is_preorder,discovered_at,release_id)
             VALUES ('{ALBUM_URL}','album','new','t','a','[]',0,0,0,0,0,'x',{via_inbox})"));
        let via_track = rel(&db, "viatrack", None, Some(12));
        let t = seed_track(&db, via_track, "t", Some(1));
        exec(&db, &format!("UPDATE tracks SET bandcamp_url='{TRACK_URL}' WHERE id={t}"));
        let mut want = vec![via_inbox, via_track];
        want.sort();
        assert_eq!(missing(&db), want);
        db.read(|c| {
            assert_eq!(fill_source_url(c, via_inbox).unwrap().as_deref(), Some(ALBUM_URL));
            assert_eq!(fill_source_url(c, via_track).unwrap().as_deref(), Some(TRACK_URL));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_fill_that_lands_on_another_record_disowns_its_url() {
        let db = test_db();
        let stub = rel(&db, "stub", Some(ALBUM_URL), Some(6));
        tracks(&db, stub, &[1, 2]);
        let stranger = rel(&db, "stranger", None, None);
        tracks(&db, stranger, &[1, 2, 3, 4, 5, 6]);
        assert!(missing(&db).contains(&stub));
        db.write(move |t| Ok(disown_url(t, stub, Some(ALBUM_URL), stranger).unwrap())).unwrap();
        let url = |id: i64| q_str(&db, &format!("SELECT bandcamp_url FROM releases WHERE id={id}"));
        assert_eq!(url(stub), None);
        assert_eq!(url(stranger).as_deref(), Some(ALBUM_URL), "the link moves to the record it names rather than being lost");
        assert!(!missing(&db).contains(&stub), "nothing can fetch it, so it is not counted as fillable");
        // A URL that is not the one on the release is left alone.
        let other = rel(&db, "other", Some(&format!("{ALBUM_URL}-two")), None);
        db.write(move |t| Ok(disown_url(t, other, Some(ALBUM_URL), stranger).unwrap())).unwrap();
        assert_eq!(url(other).as_deref(), Some(&*format!("{ALBUM_URL}-two")));
    }

    #[test]
    fn fill_queues_the_record_again_with_force() {
        let env = test_env();
        let db = &env.db;
        let fan = seed_fan(db, "yassinepeixoto");
        let r = rel(db, "x", Some(ALBUM_URL), None);
        exec(db, &format!("UPDATE releases SET source_fan_id={fan} WHERE id={r}"));
        let res = fill_release(&env, r).unwrap();
        assert_eq!((res.detail.as_str(), res.url.as_str()), ("queued", ALBUM_URL));
        let params = q_str(db, &format!("SELECT params FROM jobs WHERE id='{}'", res.job_id)).unwrap();
        let p: serde_json::Value = serde_json::from_str(&params).unwrap();
        assert_eq!(p["force"], true);
        assert_eq!(p["source_fan_id"], fan);
        assert_eq!(q_str(db, "SELECT url_kind FROM job_items").as_deref(), Some("album"));
        assert_eq!(q_str(db, "SELECT target_dir FROM job_items").as_deref(), Some("fan-yassinepeixoto"), "a shelf release fills onto its shelf");
        assert_eq!(q_i64(db, "SELECT priority FROM jobs"), 90);
        // Pressing the button twice is one download, not two.
        let again = fill_release(&env, r).unwrap();
        assert_eq!((again.detail.as_str(), again.job_id.as_str()), ("already queued", res.job_id.as_str()));
    }

    #[test]
    fn fill_falls_back_to_a_track_url_and_refuses_none() {
        let env = test_env();
        let db = &env.db;
        let unlinked = rel(db, "unlinked", None, None);
        tracks(db, unlinked, &[1]);
        assert_eq!(fill_release(&env, unlinked).unwrap_err().status(), 409);
        let linked_r = rel(db, "linked", None, None);
        let t = seed_track(db, linked_r, "t", None);
        exec(db, &format!("UPDATE tracks SET bandcamp_url='{TRACK_URL}' WHERE id={t}"));
        let res = fill_release(&env, linked_r).unwrap();
        assert_eq!(res.url, TRACK_URL);
        assert_eq!(q_str(db, &format!("SELECT url_kind FROM job_items WHERE job_id='{}'", res.job_id)).as_deref(), Some("track"));
        assert_eq!(q_str(db, &format!("SELECT target_dir FROM job_items WHERE job_id='{}'", res.job_id)), None, "a library release fills into the downloads root");
    }

    #[test]
    fn fill_all_queues_the_missing_grouped_by_shelf() {
        let env = test_env();
        let db = &env.db;
        let fan = seed_fan(db, "yassinepeixoto");
        let mine = rel(db, "mine", Some(ALBUM_URL), Some(12));
        tracks(db, mine, &[1]);
        let theirs = rel(db, "theirs", Some(&format!("{ALBUM_URL}-two")), Some(8));
        exec(db, &format!("UPDATE releases SET source_fan_id={fan} WHERE id={theirs}"));
        tracks(db, theirs, &[1]);
        let whole = rel(db, "whole", Some(&format!("{ALBUM_URL}-three")), Some(2));
        tracks(db, whole, &[1, 2]);
        let unlinked = rel(db, "unlinked", None, Some(5));
        tracks(db, unlinked, &[1]);

        let r = fill_all(&env).unwrap();
        assert_eq!((r.missing, r.queued, r.unfillable), (2, 2, 0), "the unlinked release is not even counted as missing");
        assert_eq!(r.already_queued, 0);
        assert_eq!(q_i64(db, "SELECT COUNT(*) FROM jobs"), 2, "one job for my library, one for the fan's shelf");
        let target = |url: &str| q_str(db, &format!("SELECT target_dir FROM job_items WHERE url='{url}'"));
        assert_eq!(target(ALBUM_URL), None);
        assert_eq!(target(&format!("{ALBUM_URL}-two")).as_deref(), Some("fan-yassinepeixoto"));
        assert_eq!(q_i64(db, &format!("SELECT COUNT(*) FROM job_items WHERE url='{ALBUM_URL}-three'")), 0, "a whole album is not re-fetched");
        assert_eq!(q_i64(db, "SELECT COUNT(*) FROM jobs WHERE params LIKE '%\"force\":true%'"), 2);
        assert_eq!(q_i64(db, "SELECT MIN(priority) FROM jobs"), 100);
        // Asking again fetches nothing: the unfinished fills already cover it.
        let again = fill_all(&env).unwrap();
        assert_eq!((again.queued, again.already_queued, again.unfillable), (0, 2, 0));
        assert_eq!(q_i64(db, "SELECT COUNT(*) FROM jobs"), 2);
        // The sweep still reports an unfillable record when handed one by id.
        let named = queue_fills(&env, &[unlinked]).unwrap();
        assert_eq!((named.missing, named.queued, named.unfillable), (1, 0, 1));
    }

    #[test]
    fn a_fill_item_remembers_the_release_it_repairs() {
        let env = test_env();
        let db = &env.db;
        let r = rel(db, "x", Some(ALBUM_URL), Some(12));
        tracks(db, r, &[1, 2]);
        let sweep = queue_fills(&env, &[r]).unwrap();
        assert_eq!(sweep.queued, 1);
        assert_eq!(q_i64(db, &format!("SELECT release_id FROM job_items WHERE job_id='{}'", sweep.job_ids[0])), r);
        assert_eq!(q_str(db, "SELECT url_kind FROM job_items").as_deref(), Some("album"));
    }

    // ---- pre-orders

    fn preorder(db: &bc_db::Db, rid: i64, date: Option<&str>, out: &[i64], total: i64) {
        let tracks: Vec<_> = (1..=total)
            .map(|n| bc_types::library::TrackAvailability { track_num: Some(n), title: format!("t{n}"), duration_sec: Some(200.0), available: out.contains(&n) })
            .collect();
        let a = bc_types::library::ReleaseAvailability { release_id: rid, is_preorder: true, release_date: date.map(Into::into), tracks, ..Default::default() };
        db.write(move |t| Ok(bc_libcore::availability::store(t, &a).unwrap())).unwrap();
    }
    fn detail(db: &bc_db::Db, rid: i64) -> bc_types::library::ReleaseOut {
        db.read(move |c| Ok(hydrate::releases_out(c, &[rid]).unwrap().remove(0))).unwrap()
    }

    #[test]
    fn availability_splits_missing_into_fillable_and_unreleased() {
        let db = test_db();
        let r = linked(&db, "ghost", Some(4));
        tracks(&db, r, &[1]);
        preorder(&db, r, Some("2999-10-09"), &[1], 4);
        let d = detail(&db, r);
        assert!(d.is_preorder);
        assert_eq!((d.unreleased_count, d.fillable_missing), (3, 0));
        assert_eq!(d.release_date.as_deref(), Some("2999-10-09"));
        assert_eq!(d.unreleased_tracks.iter().map(|t| t.track_num).collect::<Vec<_>>(), vec![Some(2), Some(3), Some(4)]);
        assert_eq!(d.unreleased_tracks[0].duration_sec, Some(200.0));
        // Two tracks out, one owned: the second is fillable, the other two are not.
        let r2 = linked(&db, "two-out", Some(4));
        tracks(&db, r2, &[1]);
        preorder(&db, r2, Some("2999-10-09"), &[1, 2], 4);
        let d2 = detail(&db, r2);
        assert_eq!((d2.unreleased_count, d2.fillable_missing), (2, 1));
        assert_eq!(missing(&db), vec![r2], "a partly fillable pre-order still counts");
        // An ordinary short album is all fillable.
        let plain = linked(&db, "plain", Some(5));
        tracks(&db, plain, &[1, 2]);
        let dp = detail(&db, plain);
        assert_eq!((dp.unreleased_count, dp.fillable_missing, dp.is_preorder), (0, 3, false));
    }

    #[test]
    fn fill_release_refuses_an_all_unreleased_preorder_with_a_409() {
        let env = test_env();
        let r = linked(&env.db, "ghost", Some(4));
        tracks(&env.db, r, &[1]);
        preorder(&env.db, r, Some("2999-10-09"), &[1], 4);
        let err = fill_release(&env, r).unwrap_err();
        assert!(matches!(err, ApiError::Conflict(_)), "{err:?}");
        assert_eq!(err.to_string(), "not released yet (out 2999-10-09)");
        assert_eq!(q_i64(&env.db, "SELECT COUNT(*) FROM jobs"), 0);
        // Some of it is out: the fill goes ahead.
        preorder(&env.db, r, Some("2999-10-09"), &[1, 2], 4);
        assert_eq!(fill_release(&env, r).unwrap().detail, "queued");
    }

    #[test]
    fn fill_all_and_the_missing_filter_skip_an_unreleased_preorder_until_its_date_passes() {
        let env = test_env();
        let db = &env.db;
        let r = linked(db, "ghost", Some(4));
        tracks(db, r, &[1]);
        let other = linked(db, "other", Some(3));
        tracks(db, other, &[1]);
        preorder(db, r, Some("2999-10-09"), &[1], 4);
        assert_eq!(missing(db), vec![other]);
        let all = fill_all(&env).unwrap();
        assert_eq!((all.missing, all.queued), (1, 1));
        assert_eq!(q_i64(db, &format!("SELECT COUNT(*) FROM job_items WHERE release_id={r}")), 0);
        // The date passes (the cached row has not been refreshed): fillable again.
        preorder(db, r, Some("2000-01-01"), &[1], 4);
        assert_eq!(missing(db), vec![r.min(other), r.max(other)]);
        let d = detail(db, r);
        assert_eq!((d.is_preorder, d.unreleased_count, d.fillable_missing), (false, 0, 3));
        assert_eq!(fill_release(&env, r).unwrap().detail, "queued");
    }
}
