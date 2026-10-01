//! The *write* half of `services/library/scope.py`: moving releases between "my library" and a
//! fan's shelf. (The read predicates -- which part of the library a listing shows -- live in
//! [`bc_libcore::scope`].)
//!
//! Records downloaded from another person's wishlist are real rows but not *my* library:
//! `releases.source_fan_id` marks them. Adopting clears that marker; assigning sets it. Both are
//! reversible and idempotent.

use std::collections::HashSet;

use bc_db::rusqlite::{Connection, Transaction};
use bc_libcore::ApiResult;
use bc_libcore::scope::UNIFIED_KEY;

use crate::urls::url_key;
use crate::util::{ids_json, set_setting, strs_json};

/// Persist the "unified" switch (listings show other people's downloads alongside mine).
pub fn set_unified(t: &Transaction<'_>, value: bool) -> ApiResult<()> {
    set_setting(t, UNIFIED_KEY, if value { "1" } else { "0" })
}

/// Move releases into my library: clear their source fan. Returns how many moved.
pub fn adopt_releases(t: &Transaction<'_>, ids: &[i64]) -> ApiResult<usize> {
    let wanted = crate::util::dedup_sorted(ids.iter().copied());
    let mut moved = 0;
    for chunk in wanted.chunks(5000) {
        moved += t.execute(
            "UPDATE releases SET source_fan_id = NULL WHERE id IN (SELECT value FROM json_each(?1)) AND source_fan_id IS NOT NULL",
            [ids_json(chunk)],
        )?;
    }
    Ok(moved)
}

/// Move every release downloaded from one fan into my library.
pub fn adopt_fan(t: &Transaction<'_>, fan_id: i64) -> ApiResult<usize> {
    Ok(t.execute("UPDATE releases SET source_fan_id = NULL WHERE source_fan_id = ?1", [fan_id])?)
}

/// The inverse of adopt: file releases under a fan's shelf. Returns how many rows were set.
pub fn assign_releases(t: &Transaction<'_>, ids: &[i64], fan_id: i64) -> ApiResult<usize> {
    let wanted = crate::util::dedup_sorted(ids.iter().copied());
    let mut moved = 0;
    for chunk in wanted.chunks(5000) {
        moved += t.execute("UPDATE releases SET source_fan_id = ?1 WHERE id IN (SELECT value FROM json_each(?2))", (fan_id, ids_json(chunk)))?;
    }
    Ok(moved)
}

/// Releases still filed under a fan that the given Bandcamp URLs resolve to. Three ways a URL is
/// known to name a release, the same three `find_known` consults: the release's own recorded URL,
/// an inbox row already matched to it, and a finished job item that produced it.
pub fn foreign_release_ids_for_urls<S: AsRef<str>>(c: &Connection, urls: &[S]) -> ApiResult<HashSet<i64>> {
    let mut keys: Vec<String> = urls.iter().map(|u| u.as_ref()).filter(|u| !u.is_empty()).map(url_key).collect();
    keys.sort();
    keys.dedup();
    let mut found = HashSet::new();
    if keys.is_empty() {
        return Ok(found);
    }
    let j = strs_json(&keys);
    for sql in [
        "SELECT id FROM releases WHERE lower(bandcamp_url) IN (SELECT value FROM json_each(?1)) AND source_fan_id IS NOT NULL",
        "SELECT r.id FROM releases r JOIN harvest_items h ON h.release_id = r.id
          WHERE lower(h.url) IN (SELECT value FROM json_each(?1)) AND r.source_fan_id IS NOT NULL",
        "SELECT r.id FROM releases r JOIN job_items ji ON ji.release_id = r.id
          WHERE lower(ji.url) IN (SELECT value FROM json_each(?1)) AND ji.status = 'done' AND r.source_fan_id IS NOT NULL",
    ] {
        let mut st = c.prepare(sql)?;
        for r in st.query_map([&j], |r| r.get::<_, i64>(0))? {
            found.insert(r?);
        }
    }
    Ok(found)
}

/// A personal download of something a fan's shelf already holds adopts it instead of fetching it
/// again (it should neither re-download nor stay hidden). Returns how many releases moved.
pub fn adopt_for_urls<S: AsRef<str>>(t: &Transaction<'_>, urls: &[S]) -> ApiResult<usize> {
    let ids: Vec<i64> = foreign_release_ids_for_urls(t, urls)?.into_iter().collect();
    adopt_releases(t, &ids)
}

/// How many releases sit on one fan's shelf.
pub fn count_for_fan(c: &Connection, fan_id: i64) -> ApiResult<i64> {
    Ok(c.query_row("SELECT COUNT(*) FROM releases WHERE source_fan_id = ?1", [fan_id], |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;
    use bc_db::Db;

    fn shelf(db: &Db) -> (i64, i64, i64) {
        let fan = seed_fan(db, "alice");
        let mine = seed_release(db, "Mine", "mine album", Some("https://mine.bandcamp.com/album/mine-album"), Some(2024));
        let shelved = seed_release(db, "Shelf", "shelf album", Some("https://shelf.bandcamp.com/album/shelf-album"), Some(2024));
        let other = seed_release(db, "Other", "other album", Some("https://other.bandcamp.com/album/other-album"), Some(2024));
        exec(db, &format!("UPDATE releases SET source_fan_id={fan} WHERE id={shelved}"));
        let _ = (mine, other);
        (fan, shelved, other)
    }

    fn fan_of(db: &Db, id: i64) -> Option<i64> {
        db.read(move |c| Ok(c.query_row("SELECT source_fan_id FROM releases WHERE id=?1", [id], |r| r.get(0))?)).unwrap()
    }

    #[test]
    fn adopt_moves_a_release_or_a_whole_shelf_into_my_library() {
        let db = test_db();
        let (fan, shelved, other) = shelf(&db);
        assert_eq!(db.write(move |t| Ok(adopt_releases(t, &[shelved]).unwrap())).unwrap(), 1);
        assert_eq!(fan_of(&db, shelved), None);
        // Idempotent.
        assert_eq!(db.write(move |t| Ok(adopt_releases(t, &[shelved]).unwrap())).unwrap(), 0);

        exec(&db, &format!("UPDATE releases SET source_fan_id={fan} WHERE id IN ({shelved},{other})"));
        assert_eq!(db.read(move |c| Ok(count_for_fan(c, fan).unwrap())).unwrap(), 2);
        assert_eq!(db.write(move |t| Ok(adopt_fan(t, fan).unwrap())).unwrap(), 2);
        assert_eq!(db.read(move |c| Ok(count_for_fan(c, fan).unwrap())).unwrap(), 0);
    }

    #[test]
    fn assign_is_the_inverse_of_adopt() {
        let db = test_db();
        let (fan, shelved, other) = shelf(&db);
        assert_eq!(db.write(move |t| Ok(assign_releases(t, &[other, shelved, other], fan).unwrap())).unwrap(), 2);
        assert_eq!(fan_of(&db, other), Some(fan));
        assert_eq!(db.write(move |t| Ok(adopt_releases(t, &[other]).unwrap())).unwrap(), 1);
        assert_eq!(fan_of(&db, other), None);
        assert_eq!(db.write(move |t| Ok(assign_releases(t, &[], fan).unwrap())).unwrap(), 0);
    }

    #[test]
    fn adopt_for_urls_knows_every_way_a_url_names_a_release() {
        let db = test_db();
        let (_fan, shelved, _other) = shelf(&db);
        // Known only through an inbox row matched to it.
        exec(&db, &format!(
            "INSERT INTO harvest_items(url,url_kind,state,title,artist_name,tags,in_collection,in_wishlist,is_free_download,is_purchasable,is_preorder,discovered_at,release_id)
             VALUES ('https://elsewhere.bandcamp.com/album/x','album','in_library','t','a','[]',0,0,0,0,0,'x',{shelved})"));
        assert_eq!(db.write(|t| Ok(adopt_for_urls(t, &["https://nowhere.bandcamp.com/album/y"]).unwrap())).unwrap(), 0);
        assert_eq!(db.write(|t| Ok(adopt_for_urls(t, &["https://elsewhere.bandcamp.com/album/x"]).unwrap())).unwrap(), 1);
        assert_eq!(fan_of(&db, shelved), None);
    }

    #[test]
    fn a_release_url_or_a_finished_job_item_also_names_it() {
        let db = test_db();
        let (fan, shelved, other) = shelf(&db);
        // By the release's own (case-variant) URL.
        assert_eq!(db.write(|t| Ok(adopt_for_urls(t, &["https://SHELF.bandcamp.com/album/shelf-album/"]).unwrap())).unwrap(), 1);
        // By a finished job item.
        exec(&db, &format!("UPDATE releases SET source_fan_id={fan} WHERE id={other}"));
        exec(&db, "INSERT INTO jobs(id,kind,status,priority,params,total,completed,failed,skipped,cancel_requested,created_at) VALUES ('j','download','completed',1,'{}',1,1,0,0,0,'x')");
        exec(&db, &format!("INSERT INTO job_items(job_id,seq,status,url,attempts,max_attempts,progress,release_id) VALUES ('j',0,'done','https://via-job.bandcamp.com/album/z',0,3,1,{other})"));
        assert_eq!(db.write(|t| Ok(adopt_for_urls(t, &["https://via-job.bandcamp.com/album/z"]).unwrap())).unwrap(), 1);
        let _ = shelved;
    }

    #[test]
    fn the_unified_switch_round_trips() {
        let db = test_db();
        assert!(!db.read(|c| Ok(bc_libcore::scope::unified(c).unwrap())).unwrap());
        db.write(|t| Ok(set_unified(t, true).unwrap())).unwrap();
        assert!(db.read(|c| Ok(bc_libcore::scope::unified(c).unwrap())).unwrap());
        db.write(|t| Ok(set_unified(t, false).unwrap())).unwrap();
        assert!(!db.read(|c| Ok(bc_libcore::scope::unified(c).unwrap())).unwrap());
    }
}
