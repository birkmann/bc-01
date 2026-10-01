//! The list of releases that must never come back (port of `services/library/blacklist.py`).
//!
//! Deleting a sample pack removes it from the shelf and the disk and does nothing about the
//! reason it is there: the wishlist still lists it and the next backfill downloads it again. So
//! a deletion can also record the decision here, and every path that could fetch a release
//! consults this list first.
//!
//! Two keys per entry -- the canonical URL key and the folded `(artist, title)` pair -- because a
//! tenth of the library has no Bandcamp URL to block on.

use std::collections::HashSet;

use bc_db::rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use bc_db::util::{iso, name_key, now_db};
use bc_libcore::ApiResult;
use bc_types::library::BlacklistOut;

use crate::urls::{normalise, url_key};
use crate::util::{ids_json, strs_json};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BlacklistEntry {
    pub id: i64,
    pub url_key: Option<String>,
    pub artist_key: Option<String>,
    pub title_key: Option<String>,
    pub url: Option<String>,
    pub artist_name: String,
    pub title: String,
    pub reason: Option<String>,
    pub added_at: String,
}

impl BlacklistEntry {
    pub fn out(&self) -> BlacklistOut {
        BlacklistOut {
            id: self.id,
            url: self.url.clone(),
            artist_name: self.artist_name.clone(),
            title: self.title.clone(),
            reason: self.reason.clone(),
            added_at: Some(iso(&self.added_at)),
        }
    }
}

const COLS: &str = "id, url_key, artist_key, title_key, url, artist_name, title, reason, added_at";

fn row(r: &Row<'_>) -> bc_db::rusqlite::Result<BlacklistEntry> {
    Ok(BlacklistEntry {
        id: r.get(0)?,
        url_key: r.get(1)?,
        artist_key: r.get(2)?,
        title_key: r.get(3)?,
        url: r.get(4)?,
        artist_name: r.get(5)?,
        title: r.get(6)?,
        reason: r.get(7)?,
        added_at: r.get(8)?,
    })
}

fn nonempty(s: &str) -> Option<String> {
    if s.is_empty() { None } else { Some(s.to_string()) }
}

/// Blacklist a release, by URL and by name. Idempotent on either key: re-blacklisting refreshes
/// display fields and can only add knowledge (an entry made from a URL-less release gains its URL).
pub fn add(t: &Transaction<'_>, url: Option<&str>, artist_name: &str, title: &str, reason: Option<&str>) -> ApiResult<BlacklistEntry> {
    let key = url.filter(|u| !u.is_empty()).map(url_key);
    let artist_key = nonempty(&name_key(artist_name));
    let title_key = nonempty(&name_key(title));

    let mut existing: Option<BlacklistEntry> = None;
    if let Some(k) = &key {
        existing = t.query_row(&format!("SELECT {COLS} FROM blacklist WHERE url_key = ?1"), [k], row).optional()?;
    }
    if existing.is_none()
        && let (Some(a), Some(ti)) = (&artist_key, &title_key)
    {
        existing = t
            .query_row(&format!("SELECT {COLS} FROM blacklist WHERE artist_key = ?1 AND title_key = ?2"), [a, ti], row)
            .optional()?;
    }
    let mut e = existing.clone().unwrap_or_else(|| BlacklistEntry { added_at: now_db(), ..Default::default() });
    e.url_key = e.url_key.or(key);
    e.url = e.url.or_else(|| url.filter(|u| !u.is_empty()).map(normalise));
    e.artist_key = e.artist_key.or(artist_key);
    e.title_key = e.title_key.or(title_key);
    if e.artist_name.is_empty() {
        e.artist_name = artist_name.to_string();
    }
    if e.title.is_empty() {
        e.title = title.to_string();
    }
    e.reason = reason.filter(|r| !r.is_empty()).map(str::to_string).or(e.reason);
    if existing.is_some() {
        t.execute(
            "UPDATE blacklist SET url_key=?1, artist_key=?2, title_key=?3, url=?4, artist_name=?5, title=?6, reason=?7 WHERE id=?8",
            params![e.url_key, e.artist_key, e.title_key, e.url, e.artist_name, e.title, e.reason, e.id],
        )?;
    } else {
        t.execute(
            "INSERT INTO blacklist (url_key, artist_key, title_key, url, artist_name, title, reason, added_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![e.url_key, e.artist_key, e.title_key, e.url, e.artist_name, e.title, e.reason, e.added_at],
        )?;
        e.id = t.last_insert_rowid();
    }
    Ok(e)
}

/// Delete an entry; `false` when it does not exist.
pub fn remove(t: &Transaction<'_>, entry_id: i64) -> ApiResult<bool> {
    Ok(t.execute("DELETE FROM blacklist WHERE id = ?1", [entry_id])? > 0)
}

/// Newest first, optionally filtered by a case-insensitive substring of title, artist or URL.
pub fn listing(c: &Connection, q: Option<&str>, offset: i64, limit: i64) -> ApiResult<(Vec<BlacklistEntry>, i64)> {
    let needle = q.filter(|q| !q.is_empty()).map(|q| format!("%{}%", q.to_lowercase()));
    let filter = "(?1 IS NULL OR lower(title) LIKE ?1 OR lower(artist_name) LIKE ?1 OR lower(COALESCE(url,'')) LIKE ?1)";
    let total: i64 = c.query_row(&format!("SELECT COUNT(*) FROM blacklist WHERE {filter}"), [&needle], |r| r.get(0))?;
    let mut st = c.prepare(&format!("SELECT {COLS} FROM blacklist WHERE {filter} ORDER BY added_at DESC, id DESC LIMIT ?2 OFFSET ?3"))?;
    let rows = st.query_map(params![needle, limit, offset], row)?.collect::<Result<Vec<_>, _>>()?;
    Ok((rows, total))
}

pub fn get(c: &Connection, id: i64) -> ApiResult<Option<BlacklistEntry>> {
    Ok(c.query_row(&format!("SELECT {COLS} FROM blacklist WHERE id = ?1"), [id], row).optional()?)
}

/// Which of these canonical URL keys are blacklisted.
pub fn blocked_url_keys<S: AsRef<str>>(c: &Connection, keys: &[S]) -> ApiResult<HashSet<String>> {
    let wanted: Vec<&str> = keys.iter().map(|k| k.as_ref()).filter(|k| !k.is_empty()).collect();
    if wanted.is_empty() {
        return Ok(HashSet::new());
    }
    let mut st = c.prepare("SELECT url_key FROM blacklist WHERE url_key IN (SELECT value FROM json_each(?1))")?;
    Ok(st.query_map([strs_json(&wanted)], |r| r.get::<_, String>(0))?.collect::<Result<_, _>>()?)
}

/// Which of these folded `(artist_key, title_key)` pairs are blacklisted.
pub fn blocked_names(c: &Connection, pairs: &[(String, String)]) -> ApiResult<HashSet<(String, String)>> {
    let wanted: HashSet<&(String, String)> = pairs.iter().filter(|(a, t)| !a.is_empty() && !t.is_empty()).collect();
    if wanted.is_empty() {
        return Ok(HashSet::new());
    }
    let mut titles: Vec<&str> = wanted.iter().map(|(_, t)| t.as_str()).collect();
    titles.sort_unstable();
    titles.dedup();
    let mut st = c.prepare(
        "SELECT artist_key, title_key FROM blacklist WHERE title_key IN (SELECT value FROM json_each(?1))",
    )?;
    let mut found = HashSet::new();
    let rows = st.query_map([strs_json(&titles)], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)))?;
    for r in rows {
        if let (Some(a), Some(t)) = r? {
            let pair = (a, t);
            if wanted.contains(&pair) {
                found.insert(pair);
            }
        }
    }
    Ok(found)
}

/// Whether an inbox row (`url`, artist, title) names something blacklisted.
pub fn is_blocked(c: &Connection, url: &str, artist_name: &str, title: &str) -> ApiResult<bool> {
    if !blocked_url_keys(c, &[url_key(url)])?.is_empty() {
        return Ok(true);
    }
    let pair = (name_key(artist_name), name_key(title));
    Ok(!pair.0.is_empty() && !pair.1.is_empty() && !blocked_names(c, &[pair])?.is_empty())
}

/// Retire the inbox rows these entries name: without this the blacklist only stops the download,
/// the item stays `new` and every wishlist backfill offers it again. `ignored` is reversible by
/// hand. Returns how many rows changed.
pub fn ignore_matching_inbox(t: &Transaction<'_>, entries: &[BlacklistEntry]) -> ApiResult<usize> {
    let keys: HashSet<&str> = entries.iter().filter_map(|e| e.url_key.as_deref()).collect();
    let names: HashSet<(&str, &str)> = entries
        .iter()
        .filter_map(|e| match (&e.artist_key, &e.title_key) {
            (Some(a), Some(ti)) => Some((a.as_str(), ti.as_str())),
            _ => None,
        })
        .collect();
    if keys.is_empty() && names.is_empty() {
        return Ok(0);
    }
    let rows: Vec<(i64, String, String, String)> = {
        let mut st = t.prepare("SELECT id, url, artist_name, title FROM harvest_items WHERE state IN ('new','queued','failed')")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?
    };
    let mut hit = Vec::new();
    for (id, url, artist, title) in rows {
        let (a, ti) = (name_key(&artist), name_key(&title));
        if keys.contains(url_key(&url).as_str()) || (!a.is_empty() && !ti.is_empty() && names.contains(&(a.as_str(), ti.as_str()))) {
            hit.push(id);
        }
    }
    for chunk in hit.chunks(5000) {
        t.execute("UPDATE harvest_items SET state = 'ignored' WHERE id IN (SELECT value FROM json_each(?1))", [ids_json(chunk)])?;
    }
    Ok(hit.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedup;
    use crate::testutil::*;

    const ALBUM: &str = "https://soma-records.bandcamp.com/album/soma-sample-pack-kicks";

    fn add_bl(db: &bc_db::Db, url: Option<&str>, artist: &str, title: &str) -> BlacklistEntry {
        let (u, a, t) = (url.map(str::to_string), artist.to_string(), title.to_string());
        db.write(move |tx| Ok(add(tx, u.as_deref(), &a, &t, None).unwrap())).unwrap()
    }

    fn known(db: &bc_db::Db, urls: &[&str], names: Option<Vec<(String, (String, String))>>) -> std::collections::HashMap<String, String> {
        let urls: Vec<String> = urls.iter().map(|s| s.to_string()).collect();
        db.read(move |c| {
            let names = names.map(|n| n.into_iter().collect());
            Ok(dedup::find_known(c, &urls, names.as_ref()).unwrap())
        })
        .unwrap()
    }

    #[test]
    fn a_blacklisted_url_is_reported_as_such() {
        let db = test_db();
        add_bl(&db, Some(ALBUM), "Soma", "Kicks");
        let k = known(&db, &[ALBUM], None);
        assert_eq!(k.get(&url_key(ALBUM)).map(String::as_str), Some("blacklist"));
        assert_eq!(k.len(), 1);
    }

    #[test]
    fn blacklist_wins_over_already_owning_it() {
        let db = test_db();
        seed_release(&db, "Soma", "Kicks", Some(&normalise(ALBUM)), None);
        add_bl(&db, Some(ALBUM), "Soma", "Kicks");
        assert_eq!(known(&db, &[ALBUM], None)[&url_key(ALBUM)], "blacklist");
    }

    #[test]
    fn a_release_with_no_url_is_blocked_by_its_name() {
        let db = test_db();
        let page = "https://elsewhere.bandcamp.com/album/analog-kicks";
        add_bl(&db, None, "Christian Burkhardt", "CB Analog Kicks");
        assert!(known(&db, &[page], None).is_empty(), "no URL to match on");
        let names = vec![(page.to_string(), ("Christian Burkhardt".to_string(), "CB Analog Kicks".to_string()))];
        assert_eq!(known(&db, &[page], Some(names))[&url_key(page)], "blacklist");
    }

    #[test]
    fn removing_an_entry_lets_it_through_again() {
        let db = test_db();
        let e = add_bl(&db, Some(ALBUM), "Soma", "Kicks");
        assert!(!known(&db, &[ALBUM], None).is_empty());
        assert!(db.write(move |tx| Ok(remove(tx, e.id).unwrap())).unwrap());
        assert!(known(&db, &[ALBUM], None).is_empty());
    }

    #[test]
    fn blacklisting_twice_is_one_entry() {
        let db = test_db();
        let first = add_bl(&db, Some(ALBUM), "Soma", "Kicks");
        let again = add_bl(&db, Some(ALBUM), "Soma", "Kicks");
        assert_eq!(first.id, again.id);
        let by_name = add_bl(&db, None, "Soma", "Kicks");
        assert_eq!(by_name.id, first.id);
    }

    // The worker preflight ("force does not override the blacklist") belongs to the download
    // crate; its contract is `find_known` reporting "blacklist" over "library", asserted above.

    fn inbox_item(db: &bc_db::Db, url: &str, artist: &str, title: &str) -> i64 {
        let (u, a, t) = (normalise(url), artist.to_string(), title.to_string());
        db.write(move |tx| {
            tx.execute(
                "INSERT INTO harvest_items(url,url_kind,state,title,artist_name,tags,in_collection,in_wishlist,is_free_download,is_purchasable,is_preorder,discovered_at)
                 VALUES (?1,'album','new',?2,?3,'[]',1,0,0,0,0,'2024-01-01 00:00:00')",
                (&u, &t, &a),
            )?;
            Ok(tx.last_insert_rowid())
        })
        .unwrap()
    }

    #[test]
    fn blacklisting_retires_the_inbox_rows_it_names() {
        let db = test_db();
        let by_url = inbox_item(&db, ALBUM, "Soma", "Kicks");
        let by_name = inbox_item(&db, "https://other.bandcamp.com/album/x", "Producer", "Loop Kit");
        let untouched = inbox_item(&db, "https://keep.bandcamp.com/album/y", "Someone", "A Record");
        let entries = vec![add_bl(&db, Some(ALBUM), "Soma", "Kicks"), add_bl(&db, None, "Producer", "Loop Kit")];
        let n = db.write(move |tx| Ok(ignore_matching_inbox(tx, &entries).unwrap())).unwrap();
        assert_eq!(n, 2);
        let state = |id: i64| q_str(&db, &format!("SELECT state FROM harvest_items WHERE id={id}")).unwrap();
        assert_eq!(state(by_url), "ignored");
        assert_eq!(state(by_name), "ignored");
        assert_eq!(state(untouched), "new");
    }

    #[test]
    fn a_blacklisted_release_does_not_claim_to_be_in_the_library() {
        let db = test_db();
        add_bl(&db, Some(ALBUM), "Soma", "Kicks");
        let ids = db.read(|c| Ok(dedup::find_known_ids(c, &[ALBUM.to_string()], None).unwrap())).unwrap();
        let (reason, release_id) = &ids[&url_key(ALBUM)];
        assert_eq!(reason, "blacklist");
        assert_eq!(*release_id, None, "a blacklist entry names no release row");
    }

    #[test]
    fn listing_filters_and_pages() {
        let db = test_db();
        add_bl(&db, Some(ALBUM), "Soma", "Kicks");
        let (rows, total) = db.read(|c| Ok(listing(c, Some("kicks"), 0, 200).unwrap())).unwrap();
        assert_eq!((rows.len(), total), (1, 1));
        let (_, none) = db.read(|c| Ok(listing(c, Some("nothing"), 0, 200).unwrap())).unwrap();
        assert_eq!(none, 0);
    }
}
