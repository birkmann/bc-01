//! The parts of `services/download/dedup.py` that the library layer needs: matching URLs against
//! what the library already holds, and the one-off repair passes (run once by the importer as
//! versioned data migrations, see [`crate::repairs`]).
//!
//! WS2 owns the download side and may keep its own copy of the URL helpers; everything here is
//! implemented locally on top of [`crate::urls`]. Every function runs in the caller's connection /
//! transaction and is written to be linear in the table sizes (one aggregate scan, then primary
//! key updates): the legacy comments name the correlated-subquery traps that once wedged startup.

use std::collections::{HashMap, HashSet};

use bc_db::rusqlite::{Connection, OptionalExtension, Transaction};
use bc_db::util::name_key;
use bc_libcore::ApiResult;

use crate::matching;
use crate::urls::{normalise, url_key};
use crate::util::{ids_json, strs_json};

/// url_key -> `(reason, release id)`; reasons: `blacklist` | `library` | `history` | `harvest` | `match`.
pub type KnownIds = HashMap<String, (String, Option<i64>)>;

/// URL -> `(artist, title)` the page shows.
pub type Names = HashMap<String, (String, String)>;

type LinkedRow = (Option<String>, Option<String>, Option<String>, Option<i64>);

/// Map url_key -> reason for URLs already downloaded (or blacklisted). A URL merely pending in
/// another job is not "known". `names` turns on the `(artist, title)` fallback
/// ([`match_ids_by_name`]); a blacklisted URL reports `blacklist` and is checked first -- the only
/// reason that does not mean "you already have it", and the only one `force` must not override.
pub fn find_known(c: &Connection, urls: &[String], names: Option<&Names>) -> ApiResult<HashMap<String, String>> {
    Ok(find_known_ids(c, urls, names)?.into_iter().map(|(k, (reason, _))| (k, reason)).collect())
}

/// [`find_known`], keeping which `releases.id` each URL matched (`None` where the evidence names
/// no row: a blacklist entry, or an old job item from before `release_id` was recorded).
pub fn find_known_ids(c: &Connection, urls: &[String], names: Option<&Names>) -> ApiResult<KnownIds> {
    let keys: HashSet<String> = urls.iter().filter(|u| !u.is_empty()).map(|u| url_key(u)).collect();
    if keys.is_empty() {
        return Ok(KnownIds::new());
    }
    let mut known = KnownIds::new();
    let by_key: HashMap<String, &(String, String)> =
        names.map(|n| n.iter().filter(|(u, _)| !u.is_empty()).map(|(u, p)| (url_key(u), p)).collect()).unwrap_or_default();
    let mut ordered: Vec<&str> = keys.iter().map(String::as_str).collect();
    ordered.sort_unstable();
    let batch = strs_json(&ordered);

    // First: a blacklisted URL is blocked whatever else is true of it.
    {
        let mut st = c.prepare("SELECT url_key FROM blacklist WHERE url_key IN (SELECT value FROM json_each(?1))")?;
        for k in st.query_map([&batch], |r| r.get::<_, Option<String>>(0))? {
            if let Some(k) = k? {
                let k = url_key(&k);
                if keys.contains(&k) {
                    known.entry(k).or_insert(("blacklist".into(), None));
                }
            }
        }
    }
    // Links from a URL to a release row; with a page name in hand, the row must agree with it.
    let absorb_linked = |known: &mut KnownIds, rows: Vec<LinkedRow>, reason: &str| {
        for (value, artist_key, title_key, release_id) in rows {
            let Some(value) = value else { continue };
            let key = url_key(&value);
            if !keys.contains(&key) || known.contains_key(&key) {
                continue;
            }
            if let (Some(wanted), Some(ak)) = (by_key.get(&key), artist_key.as_deref())
                && !ak.is_empty()
                && (name_key(&wanted.0) != ak || name_key(&wanted.1) != title_key.as_deref().unwrap_or(""))
            {
                continue;
            }
            known.insert(key, (reason.to_string(), release_id));
        }
    };
    let rows = {
        let mut st = c.prepare(
            "SELECT r.bandcamp_url, a.name_key, r.title_key, r.id FROM releases r LEFT JOIN artists a ON a.id = r.artist_id
              WHERE lower(r.bandcamp_url) IN (SELECT value FROM json_each(?1))",
        )?;
        st.query_map([&batch], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?
    };
    absorb_linked(&mut known, rows, "library");
    {
        let mut st = c.prepare(
            "SELECT url, release_id FROM job_items WHERE status = 'done' AND url IS NOT NULL AND lower(url) IN (SELECT value FROM json_each(?1))",
        )?;
        for r in st.query_map([&batch], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)))? {
            let (url, rid) = r?;
            let key = url_key(&url);
            if keys.contains(&key) && !known.contains_key(&key) {
                known.insert(key, ("history".into(), rid));
            }
        }
    }
    let rows = {
        let mut st = c.prepare(
            "SELECT h.url, a.name_key, r.title_key, r.id FROM harvest_items h
               LEFT JOIN releases r ON r.id = h.release_id LEFT JOIN artists a ON a.id = r.artist_id
              WHERE h.state = 'in_library' AND lower(h.url) IN (SELECT value FROM json_each(?1))",
        )?;
        st.query_map([&batch], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<Vec<_>, _>>()?
    };
    absorb_linked(&mut known, rows, "harvest");

    if let Some(names) = names {
        // A blacklisted record with no URL on it is still blacklisted: the folded name pair
        // answers for those. Before match_ids_by_name, so the deliberate reason wins.
        for url in blacklisted_by_name(c, names)? {
            known.insert(url_key(&url), ("blacklist".into(), None));
        }
        let unmatched: Names = names.iter().filter(|(u, _)| !u.is_empty() && !known.contains_key(&url_key(u))).map(|(u, p)| (u.clone(), p.clone())).collect();
        for (url, rid) in match_ids_by_name(c, &unmatched)? {
            known.insert(url_key(&url), ("match".into(), Some(rid)));
        }
    }
    Ok(known)
}

fn wanted_pairs(lookup: &Names) -> HashMap<(String, String), Vec<String>> {
    let mut wanted: HashMap<(String, String), Vec<String>> = HashMap::new();
    for (url, (artist, title)) in lookup {
        let (a, t) = (name_key(artist), name_key(title));
        if !a.is_empty() && !t.is_empty() {
            wanted.entry((a, t)).or_default().push(url.clone());
        }
    }
    wanted
}

/// Which of these URLs name an `(artist, title)` pair on the blacklist.
pub fn blacklisted_by_name(c: &Connection, lookup: &Names) -> ApiResult<HashSet<String>> {
    let wanted = wanted_pairs(lookup);
    if wanted.is_empty() {
        return Ok(HashSet::new());
    }
    let mut titles: Vec<&str> = wanted.keys().map(|(_, t)| t.as_str()).collect();
    titles.sort_unstable();
    titles.dedup();
    let mut found = HashSet::new();
    let mut st = c.prepare("SELECT artist_key, title_key FROM blacklist WHERE title_key IN (SELECT value FROM json_each(?1))")?;
    for r in st.query_map([strs_json(&titles)], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)))? {
        if let (Some(a), Some(t)) = r?
            && let Some(urls) = wanted.get(&(a, t))
        {
            found.extend(urls.iter().cloned());
        }
    }
    Ok(found)
}

/// Which of these URLs name an `(artist, title)` the library already holds, with the release row
/// (first wins where two rows fold to one key). Exact equality on the folded keys, no fuzziness;
/// unlike the inbox index it does not skip releases that already carry a URL.
pub fn match_ids_by_name(c: &Connection, lookup: &Names) -> ApiResult<HashMap<String, i64>> {
    let wanted = wanted_pairs(lookup);
    if wanted.is_empty() {
        return Ok(HashMap::new());
    }
    let mut titles: Vec<&str> = wanted.keys().map(|(_, t)| t.as_str()).collect();
    titles.sort_unstable();
    titles.dedup();
    let mut found = HashMap::new();
    let mut st = c.prepare(
        "SELECT a.name_key, r.title_key, r.id FROM releases r JOIN artists a ON a.id = r.artist_id
          WHERE r.title_key IN (SELECT value FROM json_each(?1)) ORDER BY r.id",
    )?;
    for r in st.query_map([strs_json(&titles)], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))? {
        let (a, t, id) = r?;
        if let Some(urls) = wanted.get(&(a, t)) {
            for u in urls {
                found.entry(u.clone()).or_insert(id);
            }
        }
    }
    Ok(found)
}

/// Same as [`match_ids_by_name`], ignoring the row ids.
pub fn match_by_name(c: &Connection, lookup: &Names) -> ApiResult<HashSet<String>> {
    Ok(match_ids_by_name(c, lookup)?.into_keys().collect())
}

/// Get-or-create a label by folded name (the same get-or-create the scanner uses, so a name that
/// arrives from two sources lands on one row).
pub fn get_or_create_label(t: &Connection, name: &str) -> ApiResult<Option<i64>> {
    let key = name_key(name);
    if name.trim().is_empty() || key.is_empty() {
        return Ok(None);
    }
    if let Some(id) = t.query_row("SELECT id FROM labels WHERE name_key = ?1", [&key], |r| r.get::<_, i64>(0)).optional()? {
        return Ok(Some(id));
    }
    t.execute("INSERT INTO labels(name, name_key) VALUES (?1, ?2)", (name.trim(), &key))?;
    Ok(Some(t.last_insert_rowid()))
}

/// File releases under the label Bandcamp itself named for them (the harvest inbox stores the
/// label off each release page).
///
/// Joined on `(artist, title)` rather than the release's stored URL (over half of the URL-bearing
/// releases once carried another record's URL). Only Bandcamp's stated label is used. Also
/// repairs: a label that provably came from a foreign URL is taken back off. Reruns are no-ops.
/// Returns the number of releases changed.
pub fn backfill_release_labels(t: &Transaction<'_>) -> ApiResult<usize> {
    let rows: Vec<(String, String, String, String)> = {
        let mut st = t.prepare("SELECT url, artist_name, title, label_name FROM harvest_items WHERE label_name IS NOT NULL")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?
    };
    if rows.is_empty() {
        return Ok(0);
    }
    let mut stated: HashMap<(String, String), String> = HashMap::new();
    let mut disputed: HashSet<(String, String)> = HashSet::new();
    // url_key -> what that page is about, for spotting a label inherited from a foreign URL.
    let mut pages: HashMap<String, (String, String, String)> = HashMap::new();
    for (url, artist, title, label_name) in rows {
        let name = label_name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let key = (name_key(&artist), name_key(&title));
        if !url.is_empty() {
            pages.insert(url_key(&url), (key.0.clone(), key.1.clone(), name.clone()));
        }
        if key.0.is_empty() || key.1.is_empty() {
            continue;
        }
        // Two pages folding to one pair but naming different labels cannot both be right.
        if let Some(prev) = stated.get(&key)
            && name_key(prev) != name_key(&name)
        {
            disputed.insert(key.clone());
        }
        stated.entry(key).or_insert(name);
    }
    for k in &disputed {
        stated.remove(k);
    }
    if stated.is_empty() && pages.is_empty() {
        return Ok(0);
    }
    let labels: HashMap<i64, String> = {
        let mut st = t.prepare("SELECT id, name FROM labels")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
    };
    struct Rel {
        id: i64,
        title_key: String,
        url: Option<String>,
        label_id: Option<i64>,
        artist_key: String,
    }
    let releases: Vec<Rel> = {
        let mut st = t.prepare(
            "SELECT r.id, COALESCE(r.title_key,''), r.bandcamp_url, r.label_id, COALESCE(a.name_key,'')
               FROM releases r LEFT JOIN artists a ON a.id = r.artist_id",
        )?;
        st.query_map([], |r| Ok(Rel { id: r.get(0)?, title_key: r.get(1)?, url: r.get(2)?, label_id: r.get(3)?, artist_key: r.get(4)? }))?
            .collect::<Result<_, _>>()?
    };
    let mut count = 0;
    for rel in releases {
        if let Some(name) = stated.get(&(rel.artist_key.clone(), rel.title_key.clone())) {
            if let Some(lid) = get_or_create_label(t, name)?
                && rel.label_id != Some(lid)
            {
                t.execute("UPDATE releases SET label_id = ?1 WHERE id = ?2", (lid, rel.id))?;
                count += 1;
            }
            continue;
        }
        // Nothing states a label for this record. If it holds one that came off another record's
        // page, that is not its label (positive evidence on both counts).
        let (Some(label_id), Some(url)) = (rel.label_id, rel.url.as_deref().filter(|u| !u.is_empty())) else { continue };
        let Some((page_artist, page_title, page_label)) = pages.get(&url_key(url)) else { continue };
        if (page_artist.as_str(), page_title.as_str()) == (rel.artist_key.as_str(), rel.title_key.as_str()) {
            continue; // that page really is this record; its label stands
        }
        let held = labels.get(&label_id).map(String::as_str).unwrap_or("");
        if name_key(page_label) != name_key(held) {
            continue; // the label it holds came from somewhere else
        }
        t.execute("UPDATE releases SET label_id = NULL WHERE id = ?1", [rel.id])?;
        count += 1;
    }
    Ok(count)
}

/// Last path component, lowercased -- of a URL or a filesystem path.
fn slug(value: &str) -> String {
    value.replace('\\', "/").trim_end_matches('/').rsplit('/').next().unwrap_or("").to_lowercase()
}

/// Fix releases carrying another record's `bandcamp_url`. Returns `(corrected, cleared)`.
///
/// A release is corrupt only on positive evidence: the harvest inbox scraped the page at that
/// URL, the page names a different `(artist, title)`, and the URL's slug does not match the
/// release's own folder. Each corrupt URL is re-derived from harvest data by exact name fold,
/// else by the folder slug when the page corroborates the artist or title; no confident match
/// means the URL is cleared. Idempotent.
pub fn repair_release_urls(t: &Transaction<'_>) -> ApiResult<(usize, usize)> {
    let rows: Vec<(String, String, String)> = {
        let mut st = t.prepare("SELECT url, artist_name, title FROM harvest_items WHERE url_kind = 'album'")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?
    };
    if rows.is_empty() {
        return Ok((0, 0));
    }
    let mut page_names: HashMap<String, (String, String)> = HashMap::new();
    let mut by_name: HashMap<(String, String), String> = HashMap::new();
    let mut by_slug: HashMap<String, String> = HashMap::new();
    let (mut amb_names, mut amb_slugs): (HashSet<(String, String)>, HashSet<String>) = (HashSet::new(), HashSet::new());
    for (url, artist, title) in rows {
        let pair = (name_key(&artist), name_key(&title));
        page_names.insert(url_key(&url), pair.clone());
        if !pair.0.is_empty() && !pair.1.is_empty() {
            match by_name.entry(pair) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    amb_names.insert(e.key().clone());
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(url.clone());
                }
            }
        }
        let s = slug(&url);
        if !s.is_empty() {
            match by_slug.entry(s) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    amb_slugs.insert(e.key().clone());
                }
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(url.clone());
                }
            }
        }
    }
    for p in amb_names {
        by_name.remove(&p);
    }
    for s in amb_slugs {
        by_slug.remove(&s);
    }

    struct Rel {
        id: i64,
        pair: (String, String),
        url: String,
        folder: String,
    }
    let releases: Vec<Rel> = {
        let mut st = t.prepare(
            "SELECT r.id, COALESCE(a.name_key,''), COALESCE(r.title_key,''), r.bandcamp_url, r.folder_path
               FROM releases r LEFT JOIN artists a ON a.id = r.artist_id WHERE r.bandcamp_url IS NOT NULL",
        )?;
        st.query_map([], |r| {
            Ok(Rel {
                id: r.get(0)?,
                pair: (r.get(1)?, r.get(2)?),
                url: r.get(3)?,
                folder: r.get::<_, Option<String>>(4)?.map(|f| slug(&f)).unwrap_or_default(),
            })
        })?
        .collect::<Result<_, _>>()?
    };
    let mut suspects: Vec<Rel> = Vec::new();
    let mut taken: HashSet<String> = HashSet::new();
    for rel in releases {
        let page = page_names.get(&url_key(&rel.url));
        if page.is_none() || page == Some(&rel.pair) || (!rel.folder.is_empty() && rel.folder == slug(&rel.url)) {
            taken.insert(url_key(&rel.url));
        } else {
            suspects.push(rel);
        }
    }
    if suspects.is_empty() {
        return Ok((0, 0));
    }
    let mut proposals: HashMap<i64, String> = HashMap::new();
    let mut claims: HashMap<String, usize> = HashMap::new();
    for rel in &suspects {
        let mut url = if !rel.pair.0.is_empty() && !rel.pair.1.is_empty() { by_name.get(&rel.pair).cloned() } else { None };
        if url.is_none()
            && !rel.folder.is_empty()
            && let Some(candidate) = by_slug.get(&rel.folder)
            && let Some((pa, pt)) = page_names.get(&url_key(candidate))
            && ((!pa.is_empty() && *pa == rel.pair.0) || (!pt.is_empty() && *pt == rel.pair.1))
        {
            url = Some(candidate.clone());
        }
        if let Some(u) = url {
            *claims.entry(url_key(&u)).or_insert(0) += 1;
            proposals.insert(rel.id, u);
        }
    }
    // Clear first: in a swapped pair each release is assigned the URL its partner still holds,
    // and UNIQUE is checked per statement.
    for rel in &suspects {
        t.execute("UPDATE releases SET bandcamp_url = NULL WHERE id = ?1", [rel.id])?;
    }
    let (mut corrected, mut cleared) = (0, 0);
    for rel in &suspects {
        match proposals.get(&rel.id) {
            Some(u) if claims.get(&url_key(u)) == Some(&1) && !taken.contains(&url_key(u)) => {
                t.execute("UPDATE releases SET bandcamp_url = ?1 WHERE id = ?2", (normalise(u), rel.id))?;
                corrected += 1;
            }
            _ => cleared += 1,
        }
    }
    Ok((corrected, cleared))
}

/// Reset harvest items linked to a release that is not their record: a link survives only if
/// the release's (repaired) URL matches the item's, or the folded `(artist, title)` pair does;
/// anything else goes back to `new` for the next harvest absorb to re-resolve.
pub fn repair_harvest_links(t: &Transaction<'_>) -> ApiResult<usize> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(i64, String, String, String, Option<String>, String, String)> = {
        let mut st = t.prepare(
            "SELECT h.id, h.url, h.artist_name, h.title, r.bandcamp_url, COALESCE(a.name_key,''), COALESCE(r.title_key,'')
               FROM harvest_items h LEFT JOIN releases r ON r.id = h.release_id LEFT JOIN artists a ON a.id = r.artist_id
              WHERE h.state = 'in_library' AND h.release_id IS NOT NULL",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?.collect::<Result<_, _>>()?
    };
    let mut bad = Vec::new();
    for (id, url, artist, title, rel_url, rel_artist, rel_title) in rows {
        if let Some(ru) = rel_url.as_deref().filter(|u| !u.is_empty())
            && url_key(ru) == url_key(&url)
        {
            continue;
        }
        let pair = (name_key(&artist), name_key(&title));
        if !pair.0.is_empty() && !pair.1.is_empty() && pair == (rel_artist, rel_title) {
            continue;
        }
        bad.push(id);
    }
    for chunk in bad.chunks(5000) {
        t.execute("UPDATE harvest_items SET state = 'new', release_id = NULL WHERE id IN (SELECT value FROM json_each(?1))", [ids_json(chunk)])?;
    }
    Ok(bad.len())
}

/// Re-resolve inbox rows marked `queued` with no job behind them (the state accumulated for every
/// item ever queued, whatever became of it). A row with a live job item is left alone. The rest
/// are re-matched: exact `bandcamp_url` first, then the folded `(artist, title)` pair; a hit
/// becomes `in_library` (recording the URL on the release when it had none), a miss goes back to
/// `new`. Returns `(resolved, reopened)`; idempotent.
pub fn resolve_stale_queued(t: &Transaction<'_>) -> ApiResult<(usize, usize)> {
    let live: HashSet<String> = {
        let mut st = t.prepare("SELECT url FROM job_items WHERE url IS NOT NULL AND status IN ('pending','running')")?;
        st.query_map([], |r| r.get::<_, String>(0))?.map(|r| r.map(|u| url_key(&u))).collect::<Result<_, _>>()?
    };
    let stale: Vec<(i64, String, String, String)> = {
        let mut st = t.prepare("SELECT id, url, artist_name, title FROM harvest_items WHERE state = 'queued'")?;
        let rows: Vec<(i64, String, String, String)> =
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?;
        rows.into_iter().filter(|(_, url, _, _)| !live.contains(&url_key(url))).collect()
    };
    if stale.is_empty() {
        return Ok((0, 0));
    }
    let keys: Vec<String> = {
        let s: HashSet<String> = stale.iter().map(|(_, u, _, _)| url_key(u)).collect();
        let mut v: Vec<String> = s.into_iter().collect();
        v.sort();
        v
    };
    let mut by_url: HashMap<String, i64> = HashMap::new();
    {
        let mut st = t.prepare(
            "SELECT id, bandcamp_url FROM releases WHERE lower(bandcamp_url) IN (SELECT value FROM json_each(?1)) ORDER BY id",
        )?;
        for r in st.query_map([strs_json(&keys)], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
            let (id, url) = r?;
            by_url.entry(url_key(&url)).or_insert(id);
        }
    }
    // Built once, and consuming: two inbox rows must not claim one release.
    let mut index = matching::build_release_index(t)?;
    let (mut resolved, mut reopened) = (0, 0);
    for (id, url, artist, title) in stale {
        let release_id = by_url.get(&url_key(&url)).copied().or_else(|| matching::take(&mut index, &artist, &title));
        match release_id {
            None => {
                t.execute("UPDATE harvest_items SET state = 'new', release_id = NULL, resolved_at = NULL WHERE id = ?1", [id])?;
                reopened += 1;
            }
            Some(rid) => {
                t.execute("UPDATE harvest_items SET state = 'in_library', release_id = ?1 WHERE id = ?2", (rid, id))?;
                // Record the URL while we know it, so find_known and the preflight cover this
                // release from here on. Free by construction: a URL already on a release matched
                // above and never reached `take`.
                t.execute("UPDATE releases SET bandcamp_url = ?1 WHERE id = ?2 AND bandcamp_url IS NULL", (normalise(&url), rid))?;
                resolved += 1;
            }
        }
    }
    Ok((resolved, reopened))
}

/// Copy done album job_items URLs onto releases missing a `bandcamp_url`. First URL per release
/// wins; URLs already claimed by another release are skipped (UNIQUE). Idempotent.
pub fn backfill_release_urls(t: &Transaction<'_>) -> ApiResult<usize> {
    let rows: Vec<(String, i64)> = {
        let mut st = t.prepare(
            "SELECT url, release_id FROM job_items WHERE status = 'done' AND url_kind = 'album' AND url IS NOT NULL AND release_id IS NOT NULL
              ORDER BY finished_at, id",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
    };
    if rows.is_empty() {
        return Ok(0);
    }
    let mut taken: HashSet<String> = {
        let mut st = t.prepare("SELECT bandcamp_url FROM releases WHERE bandcamp_url IS NOT NULL")?;
        st.query_map([], |r| r.get::<_, String>(0))?.map(|r| r.map(|u| u.to_lowercase())).collect::<Result<_, _>>()?
    };
    let mut count = 0;
    for (url, rid) in rows {
        let canonical = normalise(&url);
        if taken.contains(&canonical.to_lowercase()) {
            continue;
        }
        let n = t.execute("UPDATE releases SET bandcamp_url = ?1 WHERE id = ?2 AND bandcamp_url IS NULL", (&canonical, rid))?;
        if n > 0 {
            taken.insert(canonical.to_lowercase());
            count += 1;
        }
    }
    Ok(count)
}

/// Fold release rows that are the same record recorded twice: the same `(artist, title)` in the
/// same folder, which happens when Bandcamp changes the year (a reissue) and a re-download made a
/// second release row for an album already on the shelf. Non-destructive: every track moves onto
/// the keeper (the row holding most tracks, then the oldest), nothing on disk is touched. A
/// folder shared by several *different* titles is a shelf root, not an album folder, and is never
/// folded on. Returns the number of release rows folded away.
pub fn merge_folder_twins(t: &Transaction<'_>) -> ApiResult<usize> {
    // Two independent aggregate passes, filtered against each other in Rust, rather than a
    // `NOT IN (grouped subquery)` which SQLite re-runs per candidate row.
    let containers: HashSet<String> = {
        let mut st = t.prepare(
            "SELECT folder_path FROM releases WHERE folder_path IS NOT NULL GROUP BY folder_path HAVING COUNT(DISTINCT title_key) > 1",
        )?;
        st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<_, _>>()?
    };
    let twins: Vec<(Option<i64>, String, String)> = {
        let mut st = t.prepare(
            "SELECT artist_id, title_key, folder_path FROM releases WHERE folder_path IS NOT NULL
              GROUP BY artist_id, title_key, folder_path HAVING COUNT(id) > 1",
        )?;
        let rows: Vec<(Option<i64>, String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
        rows.into_iter().filter(|(_, _, f)| !containers.contains(f)).collect()
    };
    let mut folded = 0;
    for (artist_id, title_key, folder) in twins {
        #[allow(clippy::type_complexity)]
        let rows: Vec<(i64, Option<String>, Option<i64>, String, i64)> = {
            let mut st = t.prepare(
                "SELECT r.id, r.bandcamp_url, r.label_id, r.added_at, (SELECT COUNT(*) FROM tracks tk WHERE tk.release_id = r.id)
                   FROM releases r WHERE r.artist_id IS ?1 AND r.title_key = ?2 AND r.folder_path = ?3 ORDER BY r.id",
            )?;
            st.query_map((artist_id, &title_key, &folder), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
                .collect::<Result<_, _>>()?
        };
        if rows.len() < 2 {
            continue;
        }
        // The completed download keeps the shelf: most tracks first, then the oldest row.
        let keeper_idx = (0..rows.len()).max_by_key(|i| (rows[*i].4, std::cmp::Reverse(rows[*i].0))).unwrap_or(0);
        let keeper = rows[keeper_idx].0;
        let mut keeper_url = rows[keeper_idx].1.clone();
        let mut keeper_label = rows[keeper_idx].2;
        let mut keeper_added = rows[keeper_idx].3.clone();
        for (i, (lid, lurl, llabel, ladded, _)) in rows.iter().enumerate() {
            if i == keeper_idx {
                continue;
            }
            t.execute("UPDATE tracks SET release_id = ?1 WHERE release_id = ?2", (keeper, lid))?;
            t.execute("UPDATE harvest_items SET release_id = ?1 WHERE release_id = ?2", (keeper, lid))?;
            // Keep what the loser knew that the keeper does not: a Bandcamp URL above all. Cleared
            // on the loser first (UNIQUE).
            if let (Some(u), None) = (lurl, &keeper_url) {
                t.execute("UPDATE releases SET bandcamp_url = NULL WHERE id = ?1", [lid])?;
                t.execute("UPDATE releases SET bandcamp_url = ?1 WHERE id = ?2", (u, keeper))?;
                keeper_url = Some(u.clone());
            }
            if keeper_label.is_none() && llabel.is_some() {
                t.execute("UPDATE releases SET label_id = ?1 WHERE id = ?2", (llabel, keeper))?;
                keeper_label = *llabel;
            }
            if *ladded < keeper_added {
                t.execute("UPDATE releases SET added_at = ?1 WHERE id = ?2", (ladded, keeper))?;
                keeper_added = ladded.clone();
            }
            t.execute("DELETE FROM releases WHERE id = ?1", [lid])?;
            folded += 1;
        }
    }
    if folded > 0 {
        tracing::info!(folded, "folded duplicate release row(s) sharing a folder");
    }
    Ok(folded)
}

/// Get-or-create an artist by folded name (the scanner's get-or-create).
pub fn get_or_create_artist(t: &Connection, name: &str) -> ApiResult<Option<i64>> {
    let key = name_key(name);
    if name.trim().is_empty() || key.is_empty() {
        return Ok(None);
    }
    if let Some(id) = t.query_row("SELECT id FROM artists WHERE name_key = ?1", [&key], |r| r.get::<_, i64>(0)).optional()? {
        return Ok(Some(id));
    }
    t.execute("INSERT INTO artists(name, name_key, created_at) VALUES (?1, ?2, ?3)", (name.trim(), &key, bc_db::util::now_db()))?;
    Ok(Some(t.last_insert_rowid()))
}
