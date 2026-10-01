//! Match download URLs against what the library already holds.
//!
//! Answers one question cheaply -- "have we already downloaded this URL?" --
//! without shelling out to bandcamp-dl. Three sources of truth, in order of
//! strength: the release itself (`releases.bandcamp_url`), the durable job
//! history (done `job_items`), and harvest items already resolved into the
//! library.
//!
//! Historical `job_items.url` was written by the downloads route's normaliser
//! (which keeps host case), while new writes use the harvest canonical form
//! (lowercased host). Bandcamp slugs are always lowercase, so whole-string
//! `lower()` on both sides reconciles the two without rewriting any data.
//!
//! Everything here works on the legacy schema. Reads take a `&Connection`
//! (use `Db::read`), writes take the writer's transaction (which derefs to a
//! `&Connection`; use `Db::write`). `*_async` wrappers are provided for callers
//! on the async side.

#![allow(clippy::type_complexity, clippy::collapsible_if, clippy::map_entry)]

use std::collections::{BTreeSet, HashMap, HashSet};

use bc_db::Db;
use bc_db::rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value};


type Result<T> = bc_db::Result<T>;

/// Well under SQLite's default 999-parameter limit.
const BATCH: usize = 400;

// ---------------------------------------------------------------------------
// TODO(ws2-lead): shims for helpers that belong to other modules. `urls.rs`
// (harvest/urls.py: classify/normalise) and the library crate (`name_key`,
// `get_or_create_label`, the release index of services/library/matching.py) are
// not available yet; swap these for the shared versions when they land.
// ---------------------------------------------------------------------------

pub use crate::urls::UrlKind;
pub use crate::urls::classify as classify_url;
pub use crate::urls::normalise;
pub use bc_db::util::name_key;

/// `services/library/ingest.py::get_or_create_label` (shim).
pub fn get_or_create_label(c: &Connection, name: &str) -> Result<Option<i64>> {
    if name.trim().is_empty() {
        return Ok(None);
    }
    let key = name_key(name);
    if key.is_empty() {
        return Ok(None);
    }
    if let Some(id) = c.query_row("SELECT id FROM labels WHERE name_key=?1", [&key], |r| r.get::<_, i64>(0)).optional()? {
        return Ok(Some(id));
    }
    c.execute("INSERT INTO labels(name, name_key) VALUES (?1, ?2)", params![name.trim(), key])?;
    Ok(Some(c.last_insert_rowid()))
}

/// `services/library/matching.py::ReleaseIndex` (shim).
pub type ReleaseIndex = HashMap<(String, String), i64>;

/// Map (artist_key, title_key) -> release id for releases lacking a URL.
/// Ambiguous keys are dropped rather than resolved.
pub fn build_release_index(c: &Connection) -> Result<ReleaseIndex> {
    let mut index = ReleaseIndex::new();
    let mut ambiguous: HashSet<(String, String)> = HashSet::new();
    let mut stmt = c.prepare(
        "SELECT r.id, a.name_key, r.title_key FROM releases r JOIN artists a ON a.id = r.artist_id \
         WHERE r.bandcamp_url IS NULL",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    for row in rows {
        let (id, artist_key, title_key) = row?;
        if artist_key.is_empty() || title_key.is_empty() {
            continue;
        }
        let key = (artist_key, title_key);
        if index.contains_key(&key) {
            ambiguous.insert(key);
            continue;
        }
        index.insert(key, id);
    }
    for key in ambiguous {
        index.remove(&key);
    }
    Ok(index)
}

/// Find a release id for a harvested (artist, title) and consume the key, so a
/// release can only be the same thing as one Bandcamp URL.
pub fn take(index: &mut ReleaseIndex, artist_name: &str, title: &str) -> Option<i64> {
    if artist_name.is_empty() || title.is_empty() {
        return None;
    }
    index.remove(&(name_key(artist_name), name_key(title)))
}

// ---------------------------------------------------------------------------
// SQL helpers
// ---------------------------------------------------------------------------

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

fn text_params(batch: &[String]) -> impl Iterator<Item = Value> + '_ {
    batch.iter().map(|s| Value::Text(s.clone()))
}

/// Canonical dedupe key: harvest-normalised, then whole-string lowercase.
pub fn url_key(raw: &str) -> String {
    normalise(raw).to_lowercase()
}

/// `reason` is `'blacklist' | 'library' | 'history' | 'harvest' | 'match'`.
pub type KnownIds = HashMap<String, (String, Option<i64>)>;

/// `url_key -> (artist, title)` the page shows.
pub type NameLookup = HashMap<String, (String, String)>;

/// Map `url_key -> reason` for known URLs.
///
/// Only URLs that are already downloaded count; a URL merely pending in some
/// other job is not "known" -- whichever item runs first wins and the loser is
/// caught by the worker's pre-flight check.
///
/// `names` maps a URL to the `(artist, title)` the page shows, and turns on the
/// fallback described in [`match_by_name`]. Without it this is a URL comparison
/// and nothing else, which answers "did *we* download this" -- not the question
/// a browse view is really asking, which is "do I have this record". Callers
/// that are looking at a Bandcamp page have those two fields in hand and should
/// pass them.
///
/// A blacklisted URL reports `'blacklist'` and is checked first, so the reason
/// a caller shows the user is the deliberate decision rather than an incidental
/// one. It is the only reason here that does not mean "you already have it",
/// and the only one `force` must not override.
pub fn find_known(c: &Connection, urls: &[String], names: Option<&NameLookup>) -> Result<HashMap<String, String>> {
    Ok(find_known_ids(c, urls, names)?.into_iter().map(|(k, (reason, _))| (k, reason)).collect())
}

/// [`find_known`], keeping which release row each URL matched.
///
/// Same matching and the same reasons; each entry also carries the
/// `releases.id` behind the answer, or `None` where the evidence names no row --
/// a blacklist entry, or an old job item from before `release_id` was recorded.
/// The id is what lets a browse view hand the player the shelf's own files
/// instead of streaming a record it has just badged as owned.
pub fn find_known_ids(c: &Connection, urls: &[String], names: Option<&NameLookup>) -> Result<KnownIds> {
    let keys: BTreeSet<String> = urls.iter().filter(|u| !u.is_empty()).map(|u| url_key(u)).collect();
    if keys.is_empty() {
        return Ok(KnownIds::new());
    }
    let mut known = KnownIds::new();
    let by_key: HashMap<String, &(String, String)> =
        names.into_iter().flatten().filter(|(u, _)| !u.is_empty()).map(|(u, pair)| (url_key(u), pair)).collect();

    let absorb = |known: &mut KnownIds, rows: Vec<(Option<String>, Option<i64>)>, reason: &str| {
        for (value, release_id) in rows {
            let Some(value) = value else { continue };
            let key = url_key(&value);
            if keys.contains(&key) && !known.contains_key(&key) {
                known.insert(key, (reason.to_string(), release_id));
            }
        }
    };
    // URLs linked to a release row, checked against what the page says.
    //
    // Both the release's own `bandcamp_url` and the inbox's "in_library" state
    // are links from a URL to a row, and a link can point at the wrong record --
    // at which point a browse view claims something the shelf has never had, and
    // hides the download button for a record the user does not own. When the
    // caller knows what the page is about, the row it points at has to agree; a
    // row with no artist to compare is accepted, since there is nothing there to
    // contradict it.
    let absorb_linked =
        |known: &mut KnownIds, rows: Vec<(Option<String>, Option<String>, Option<String>, Option<i64>)>, reason: &str| {
            for (value, artist_key, title_key, release_id) in rows {
                let Some(value) = value else { continue };
                let key = url_key(&value);
                if !keys.contains(&key) || known.contains_key(&key) {
                    continue;
                }
                if let Some(wanted) = by_key.get(&key)
                    && let Some(ak) = artist_key.as_deref().filter(|a| !a.is_empty())
                    && (name_key(&wanted.0) != ak || name_key(&wanted.1) != title_key.unwrap_or_default())
                {
                    continue;
                }
                known.insert(key, (reason.to_string(), release_id));
            }
        };

    let ordered: Vec<String> = keys.iter().cloned().collect();
    for batch in ordered.chunks(BATCH) {
        let ph = placeholders(batch.len());
        // First: a blacklisted URL is blocked whatever else is true of it.
        {
            let mut stmt = c.prepare(&format!("SELECT url_key FROM blacklist WHERE url_key IN ({ph})"))?;
            let rows = stmt
                .query_map(params_from_iter(text_params(batch)), |r| Ok((r.get::<_, Option<String>>(0)?, None)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            absorb(&mut known, rows, "blacklist");
        }
        {
            let mut stmt = c.prepare(&format!(
                "SELECT r.bandcamp_url, a.name_key, r.title_key, r.id FROM releases r \
                 LEFT JOIN artists a ON a.id = r.artist_id WHERE lower(r.bandcamp_url) IN ({ph})"
            ))?;
            let rows = stmt
                .query_map(params_from_iter(text_params(batch)), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            absorb_linked(&mut known, rows, "library");
        }
        {
            let mut stmt = c.prepare(&format!(
                "SELECT url, release_id FROM job_items WHERE status = 'done' AND url IS NOT NULL \
                 AND lower(url) IN ({ph})"
            ))?;
            let rows = stmt
                .query_map(params_from_iter(text_params(batch)), |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            absorb(&mut known, rows, "history");
        }
        {
            let mut stmt = c.prepare(&format!(
                "SELECT h.url, a.name_key, r.title_key, r.id FROM harvest_items h \
                 LEFT JOIN releases r ON r.id = h.release_id LEFT JOIN artists a ON a.id = r.artist_id \
                 WHERE h.state = 'in_library' AND lower(h.url) IN ({ph})"
            ))?;
            let rows = stmt
                .query_map(params_from_iter(text_params(batch)), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            absorb_linked(&mut known, rows, "harvest");
        }
    }

    if let Some(names) = names.filter(|n| !n.is_empty()) {
        // A blacklisted record with no URL on it is still blacklisted: about a
        // tenth of the library has no Bandcamp URL to key on, so the folded name
        // pair has to answer for those. Before match_by_name, so the deliberate
        // reason wins over the incidental one.
        for url in blacklisted_by_name(c, names)? {
            known.insert(url_key(&url), ("blacklist".to_string(), None));
        }
        let unmatched: NameLookup = names
            .iter()
            .filter(|(u, _)| !u.is_empty() && !known.contains_key(&url_key(u)))
            .map(|(u, p)| (u.clone(), p.clone()))
            .collect();
        for (url, release_id) in match_ids_by_name(c, &unmatched)? {
            known.insert(url_key(&url), ("match".to_string(), Some(release_id)));
        }
    }
    Ok(known)
}

/// `(artist_key, title_key) -> urls` for the entries that have both halves.
fn wanted_pairs(lookup: &NameLookup) -> HashMap<(String, String), Vec<String>> {
    let mut wanted: HashMap<(String, String), Vec<String>> = HashMap::new();
    for (url, (artist, title)) in lookup {
        let (ak, tk) = (name_key(artist), name_key(title));
        if !ak.is_empty() && !tk.is_empty() {
            wanted.entry((ak, tk)).or_default().push(url.clone());
        }
    }
    wanted
}

/// Which of these URLs name an `(artist, title)` pair on the blacklist.
///
/// Same fold and the same select-on-title-then-pair-in-Rust shape as
/// [`match_by_name`]; SQLite has no row-value `IN`.
pub fn blacklisted_by_name(c: &Connection, lookup: &NameLookup) -> Result<HashSet<String>> {
    let wanted = wanted_pairs(lookup);
    let mut found = HashSet::new();
    if wanted.is_empty() {
        return Ok(found);
    }
    let titles: Vec<String> = wanted.keys().map(|(_, t)| t.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    for batch in titles.chunks(BATCH) {
        let mut stmt = c.prepare(&format!(
            "SELECT artist_key, title_key FROM blacklist WHERE title_key IN ({})",
            placeholders(batch.len())
        ))?;
        let rows = stmt
            .query_map(params_from_iter(text_params(batch)), |r| {
                Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (ak, tk) in rows {
            if let (Some(ak), Some(tk)) = (ak, tk)
                && let Some(urls) = wanted.get(&(ak, tk))
            {
                found.extend(urls.iter().cloned());
            }
        }
    }
    Ok(found)
}

/// Which of these URLs name an `(artist, title)` the library already holds.
///
/// The URL is the exact answer to "do I have this", and it is missing for most of
/// a collection: a library scanned in off disk never had one, and a release can
/// carry one that belongs to a different record. Either way a browse view would
/// offer to download something already on the shelf.
///
/// So this is the same fold the harvest inbox matches on: exact equality on
/// `artists.name_key` and `releases.title_key`, no fuzziness, no guessing.
pub fn match_by_name(c: &Connection, lookup: &NameLookup) -> Result<HashSet<String>> {
    Ok(match_ids_by_name(c, lookup)?.into_keys().collect())
}

/// [`match_by_name`], keeping which release row each URL matched. Where two
/// library rows fold to one key the first wins: for "is it on the shelf" either
/// answers.
pub fn match_ids_by_name(c: &Connection, lookup: &NameLookup) -> Result<HashMap<String, i64>> {
    let wanted = wanted_pairs(lookup);
    let mut found: HashMap<String, i64> = HashMap::new();
    if wanted.is_empty() {
        return Ok(found);
    }
    let titles: Vec<String> = wanted.keys().map(|(_, t)| t.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    for batch in titles.chunks(BATCH) {
        // Selected on the title alone and paired up in Rust: SQLite has no
        // row-value IN, and a title is selective enough that the artist check
        // costs nothing on top.
        let mut stmt = c.prepare(&format!(
            "SELECT a.name_key, r.title_key, r.id FROM releases r JOIN artists a ON r.artist_id = a.id \
             WHERE r.title_key IN ({}) ORDER BY r.id",
            placeholders(batch.len())
        ))?;
        let rows = stmt
            .query_map(params_from_iter(text_params(batch)), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (ak, tk, id) in rows {
            if let Some(urls) = wanted.get(&(ak, tk)) {
                for url in urls {
                    found.entry(url.clone()).or_insert(id);
                }
            }
        }
    }
    Ok(found)
}

// ---------------------------------------------------------------------------
// Write-side maintenance (run inside `Db::write`)
// ---------------------------------------------------------------------------

/// File releases under the label Bandcamp itself named for them.
///
/// A label reaches the library through the files' publisher tag, and Bandcamp
/// downloads carry no such tag -- so a collection built from Bandcamp has none
/// at all, and the Labels shelf would stay empty however much is on it. The
/// harvest inbox, though, already stores the label off each release page, so the
/// information is in the database; it has simply never been joined onto the
/// release.
///
/// Joined on `(artist, title)` rather than on the release's stored URL. That URL
/// is not trustworthy: in the legacy library over half of the releases that carry
/// one carry a URL belonging to a *different* record, and joining through it
/// files albums under whichever label that other record came out on. The folded
/// name pair is the same evidence the harvest inbox matches on.
///
/// Only Bandcamp's stated label is used. The page a release came from is *not*
/// treated as one: most Bandcamp subdomains are artists, and filing every
/// self-released record under a label named after its artist would turn the shelf
/// into a second, worse Artists page.
///
/// Also repairs: a label that provably came from a foreign URL is taken back off.
/// Reruns are otherwise no-ops -- nothing is invented, and a release the inbox
/// says nothing about is left exactly as it is.
pub fn backfill_release_labels(c: &Connection) -> Result<usize> {
    let rows: Vec<(String, String, String, String)> = {
        let mut stmt = c.prepare(
            "SELECT url, artist_name, title, label_name FROM harvest_items WHERE label_name IS NOT NULL",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    if rows.is_empty() {
        return Ok(0);
    }

    let mut stated: HashMap<(String, String), String> = HashMap::new();
    let mut disputed: HashSet<(String, String)> = HashSet::new();
    // url_key -> what that page is about, for spotting a label inherited from a
    // URL that turned out to belong to another record.
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
        // Two pages folding to one name pair but naming different labels cannot
        // both be right about this release, and there is nothing here to choose
        // between them -- so neither is used.
        if let Some(existing) = stated.get(&key)
            && name_key(existing) != name_key(&name)
        {
            disputed.insert(key.clone());
        }
        stated.entry(key).or_insert(name);
    }
    for key in disputed {
        stated.remove(&key);
    }
    if stated.is_empty() && pages.is_empty() {
        return Ok(0);
    }

    struct Rel {
        id: i64,
        title_key: String,
        artist_key: String,
        label_id: Option<i64>,
        url: Option<String>,
        label_name: Option<String>,
    }
    let releases: Vec<Rel> = {
        let mut stmt = c.prepare(
            "SELECT r.id, r.title_key, a.name_key, r.label_id, r.bandcamp_url, l.name FROM releases r \
             LEFT JOIN artists a ON a.id = r.artist_id LEFT JOIN labels l ON l.id = r.label_id ORDER BY r.id",
        )?;
        stmt.query_map([], |r| {
            Ok(Rel {
                id: r.get(0)?,
                title_key: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                artist_key: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                label_id: r.get(3)?,
                url: r.get(4)?,
                label_name: r.get(5)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };

    let mut count = 0;
    // Labels are resolved through the same get-or-create the scanner uses, so a
    // name that arrives from both sources lands on one row rather than two
    // spellings of the same imprint.
    for rel in releases {
        if let Some(name) = stated.get(&(rel.artist_key.clone(), rel.title_key.clone())) {
            if let Some(label_id) = get_or_create_label(c, name)?
                && rel.label_id != Some(label_id)
            {
                c.execute("UPDATE releases SET label_id=?1 WHERE id=?2", params![label_id, rel.id])?;
                count += 1;
            }
            continue;
        }

        // Nothing states a label for this record. If it is holding one that came
        // off another record's page, that is not its label: the page the URL
        // points at is about something else, and it is the source the old join
        // read. Positive evidence on both counts, so this only ever undoes that
        // mistake.
        let Some(url) = rel.url.as_deref().filter(|u| !u.is_empty()) else { continue };
        if rel.label_id.is_none() {
            continue;
        }
        let Some((page_artist, page_title, page_label)) = pages.get(&url_key(url)) else { continue };
        if (page_artist, page_title) == (&rel.artist_key, &rel.title_key) {
            continue; // that page really is this record; its label stands
        }
        let held = rel.label_name.as_deref().unwrap_or("");
        if name_key(page_label) != name_key(held) {
            continue; // the label it holds came from somewhere else
        }
        c.execute("UPDATE releases SET label_id=NULL WHERE id=?1", [rel.id])?;
        count += 1;
    }
    Ok(count)
}

/// File library releases matching a label page's catalogue under that label.
///
/// `entries` is the page's discography as `(url, artist, title)` rows. The caller
/// vouches that the page *is* a label -- this function never decides that -- so
/// everything on it that is already on the shelf belongs to the label, including
/// releases downloaded before labels were recorded. That is what lets a
/// catalogue re-run heal an unlabelled library.
///
/// Matching mirrors [`find_known`]: by `bandcamp_url` first, and where the
/// release row carries an artist the page's (artist, title) must agree, so a URL
/// that historically got stamped onto the wrong record does not drag that record
/// onto the label. URL-less releases fall back to exact folded (artist, title).
/// Only an empty `label_id` is filled -- a publisher named by the files
/// themselves always wins.
pub fn file_known_releases_under_label(
    c: &Connection,
    label_name: &str,
    label_url: Option<&str>,
    entries: &[(String, String, String)],
) -> Result<usize> {
    let cleaned: Vec<&(String, String, String)> = entries.iter().filter(|(u, _, _)| !u.is_empty()).collect();
    if cleaned.is_empty() {
        return Ok(0);
    }
    let Some(label_id) = get_or_create_label(c, label_name)? else { return Ok(0) };

    let current_url: Option<String> =
        c.query_row("SELECT bandcamp_url FROM labels WHERE id=?1", [label_id], |r| r.get(0))?;
    if current_url.is_none()
        && let Some(label_url) = label_url.filter(|u| !u.is_empty())
    {
        let canonical = normalise(label_url);
        let taken: Option<i64> = c
            .query_row(
                "SELECT id FROM labels WHERE bandcamp_url=?1 AND id != ?2",
                params![canonical, label_id],
                |r| r.get(0),
            )
            .optional()?;
        if taken.is_none() {
            c.execute("UPDATE labels SET bandcamp_url=?1 WHERE id=?2", params![canonical, label_id])?;
        }
    }

    let by_key: HashMap<String, (&str, &str)> =
        cleaned.iter().map(|(u, a, t)| (url_key(u), (a.as_str(), t.as_str()))).collect();
    let mut matched_keys: HashSet<String> = HashSet::new();
    let ordered: Vec<String> = by_key.keys().cloned().collect::<BTreeSet<_>>().into_iter().collect();
    let mut count = 0;
    for batch in ordered.chunks(BATCH) {
        let rows: Vec<(i64, Option<String>, Option<String>, Option<String>, Option<i64>)> = {
            let mut stmt = c.prepare(&format!(
                "SELECT r.id, r.bandcamp_url, a.name_key, r.title_key, r.label_id FROM releases r \
                 LEFT JOIN artists a ON a.id = r.artist_id WHERE lower(r.bandcamp_url) IN ({})",
                placeholders(batch.len())
            ))?;
            stmt.query_map(params_from_iter(text_params(batch)), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (id, url, artist_key, title_key, rel_label) in rows {
            let key = url_key(url.as_deref().unwrap_or(""));
            let Some(wanted) = by_key.get(&key) else { continue };
            if let Some(ak) = artist_key.as_deref().filter(|a| !a.is_empty())
                && (name_key(wanted.0) != ak || name_key(wanted.1) != title_key.unwrap_or_default())
            {
                continue; // the URL points at some other record
            }
            matched_keys.insert(key);
            if rel_label.is_none() {
                c.execute("UPDATE releases SET label_id=?1 WHERE id=?2", params![label_id, id])?;
                count += 1;
            }
        }
    }

    let remaining: Vec<(&str, &str)> = cleaned
        .iter()
        .filter(|(u, _, _)| !matched_keys.contains(&url_key(u)))
        .map(|(_, a, t)| (a.as_str(), t.as_str()))
        .collect();
    if !remaining.is_empty() {
        let mut index = build_release_index(c)?;
        for (artist, title) in remaining {
            let Some(release_id) = take(&mut index, artist, title) else { continue };
            let current: Option<Option<i64>> =
                c.query_row("SELECT label_id FROM releases WHERE id=?1", [release_id], |r| r.get(0)).optional()?;
            if let Some(None) = current {
                c.execute("UPDATE releases SET label_id=?1 WHERE id=?2", params![label_id, release_id])?;
                count += 1;
            }
        }
    }
    Ok(count)
}

/// Last path component, lowercased -- of a URL or a filesystem path.
fn slug_of(value: &str) -> String {
    value.replace('\\', "/").trim_end_matches('/').rsplit('/').next().unwrap_or("").to_lowercase()
}

/// Fix releases carrying another record's `bandcamp_url`.
///
/// The old download worker diffed the *shared* downloads root to see what a
/// bandcamp-dl run produced, so with two downloads running at once an item
/// routinely ingested its sibling's files -- and stamped its own URL onto the
/// sibling's release. In the legacy library over half of the URL-bearing releases
/// ended up with a URL belonging to a different record.
///
/// A release is treated as corrupt only on positive evidence, never on a hunch:
/// the harvest inbox must have scraped the page at that URL, the page must name a
/// different `(artist, title)` than the release, and the URL's album slug must
/// not match the release's own folder. A URL harvest knows nothing about, or one
/// whose slug agrees with the folder it downloaded into, is left alone.
///
/// Each corrupt URL is then re-derived from the same harvest data: by exact
/// `(artist, title)` name fold first, else by the folder's slug when the page
/// found there corroborates the artist or the title. No confident match means the
/// URL is cleared -- a missing URL costs a preflight skip; a wrong one corrupts
/// everything downstream of it.
///
/// Idempotent: a repaired release no longer meets the corrupt predicate, so
/// reruns return `(0, 0)`. Returns `(corrected, cleared)`.
pub fn repair_release_urls(c: &Connection) -> Result<(usize, usize)> {
    let rows: Vec<(String, String, String)> = {
        let mut stmt = c.prepare(
            "SELECT url, artist_name, title FROM harvest_items WHERE url_kind = 'album' ORDER BY id",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    if rows.is_empty() {
        return Ok((0, 0));
    }

    let mut page_names: HashMap<String, (String, String)> = HashMap::new();
    let mut by_name: HashMap<(String, String), String> = HashMap::new();
    let mut by_slug: HashMap<String, String> = HashMap::new();
    let mut ambiguous_names: HashSet<(String, String)> = HashSet::new();
    let mut ambiguous_slugs: HashSet<String> = HashSet::new();
    for (url, artist, title) in &rows {
        let pair = (name_key(artist), name_key(title));
        page_names.insert(url_key(url), pair.clone());
        if !pair.0.is_empty() && !pair.1.is_empty() {
            if by_name.contains_key(&pair) {
                ambiguous_names.insert(pair);
            } else {
                by_name.insert(pair, url.clone());
            }
        }
        let slug = slug_of(url);
        if !slug.is_empty() {
            if by_slug.contains_key(&slug) {
                ambiguous_slugs.insert(slug);
            } else {
                by_slug.insert(slug, url.clone());
            }
        }
    }
    // Two pages folding to one key cannot be told apart, so neither is a usable
    // answer -- same rule the release-matching index applies.
    for pair in ambiguous_names {
        by_name.remove(&pair);
    }
    for slug in ambiguous_slugs {
        by_slug.remove(&slug);
    }

    struct Rel {
        id: i64,
        url: String,
        pair: (String, String),
        folder: String,
    }
    let releases: Vec<Rel> = {
        let mut stmt = c.prepare(
            "SELECT r.id, r.bandcamp_url, a.name_key, r.title_key, r.folder_path FROM releases r \
             LEFT JOIN artists a ON a.id = r.artist_id WHERE r.bandcamp_url IS NOT NULL ORDER BY r.id",
        )?;
        stmt.query_map([], |r| {
            Ok(Rel {
                id: r.get(0)?,
                url: r.get(1)?,
                pair: (
                    r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                ),
                folder: r.get::<_, Option<String>>(4)?.filter(|f| !f.is_empty()).map(|f| slug_of(&f)).unwrap_or_default(),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };

    let mut suspects: Vec<&Rel> = Vec::new();
    let mut taken: HashSet<String> = HashSet::new();
    for rel in &releases {
        let page = page_names.get(&url_key(&rel.url));
        if page.is_none()
            || page == Some(&rel.pair)
            || (!rel.folder.is_empty() && rel.folder == slug_of(&rel.url))
        {
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
        if url.is_none() && !rel.folder.is_empty() {
            if let Some(candidate) = by_slug.get(&rel.folder) {
                let (page_artist, page_title) = &page_names[&url_key(candidate)];
                if (!page_artist.is_empty() && *page_artist == rel.pair.0)
                    || (!page_title.is_empty() && *page_title == rel.pair.1)
                {
                    url = Some(candidate.clone());
                }
            }
        }
        if let Some(url) = url {
            *claims.entry(url_key(&url)).or_insert(0) += 1;
            proposals.insert(rel.id, url);
        }
    }

    // Clear first: in a swapped pair each release is assigned the URL its partner
    // is still holding, and the UNIQUE constraint is checked per statement, not
    // at commit.
    for rel in &suspects {
        c.execute("UPDATE releases SET bandcamp_url=NULL WHERE id=?1", [rel.id])?;
    }
    let (mut corrected, mut cleared) = (0, 0);
    for rel in &suspects {
        match proposals.get(&rel.id) {
            Some(url) if claims[&url_key(url)] == 1 && !taken.contains(&url_key(url)) => {
                c.execute("UPDATE releases SET bandcamp_url=?1 WHERE id=?2", params![normalise(url), rel.id])?;
                corrected += 1;
            }
            _ => cleared += 1,
        }
    }
    Ok((corrected, cleared))
}

/// Reset harvest items linked to a release that is not their record.
///
/// The inbox marked an item `in_library` on an exact URL match against
/// `releases.bandcamp_url` -- which, while the URLs were corrupt (see
/// [`repair_release_urls`]), linked items to foreign releases. A link is kept
/// only if the release's (repaired) URL matches the item's, or the folded
/// `(artist, title)` pair does; anything else goes back to `new` for the next
/// harvest absorb to re-resolve.
pub fn repair_harvest_links(c: &Connection) -> Result<usize> {
    let rows: Vec<(i64, String, String, String, Option<String>, Option<String>, Option<String>)> = {
        let mut stmt = c.prepare(
            "SELECT h.id, h.url, h.artist_name, h.title, r.bandcamp_url, a.name_key, r.title_key \
             FROM harvest_items h LEFT JOIN releases r ON r.id = h.release_id \
             LEFT JOIN artists a ON a.id = r.artist_id \
             WHERE h.state = 'in_library' AND h.release_id IS NOT NULL",
        )?;
        stmt.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut count = 0;
    for (id, url, artist, title, rel_url, rel_artist_key, rel_title_key) in rows {
        // `release is not None` in the Python: a dangling link has no release row.
        let release_exists: bool = c
            .query_row(
                "SELECT 1 FROM harvest_items h JOIN releases r ON r.id = h.release_id WHERE h.id = ?1",
                [id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if release_exists {
            if rel_url.as_deref().is_some_and(|u| !u.is_empty() && url_key(u) == url_key(&url)) {
                continue;
            }
            let pair = (name_key(&artist), name_key(&title));
            if !pair.0.is_empty()
                && !pair.1.is_empty()
                && pair == (rel_artist_key.unwrap_or_default(), rel_title_key.unwrap_or_default())
            {
                continue;
            }
        }
        c.execute("UPDATE harvest_items SET state='new', release_id=NULL WHERE id=?1", [id])?;
        count += 1;
    }
    Ok(count)
}

/// Re-resolve inbox rows marked `queued` with no job behind them.
///
/// Queuing wrote `state = "queued"` and, until `inbox.settle` existed, nothing
/// ever wrote anything else -- so the state accumulated for every item ever
/// queued, whatever became of it. Deleting the finished jobs cascaded the
/// `job_items` away and left no trace of the downloads having happened: one real
/// library reached 12,208 rows claiming to be downloading with the jobs table
/// empty.
///
/// A row with a live job item is left alone -- that one really is downloading.
/// The rest are re-matched the way the inbox matches: exact `bandcamp_url` first,
/// then the folded `(artist, title)` pair for the majority of a scanned-in
/// library that has no URL. A hit becomes `in_library`. A miss goes back to
/// `new`, which is the honest answer -- nothing is fetching it -- and is also the
/// state the "awaiting download" action acts on, so the user can queue it again.
///
/// Returns `(resolved, reopened)`. Idempotent; reruns settle to `(0, 0)`.
pub fn resolve_stale_queued(c: &Connection) -> Result<(usize, usize)> {
    let live: HashSet<String> = {
        let mut stmt = c.prepare(
            "SELECT url FROM job_items WHERE url IS NOT NULL AND status IN ('pending','running')",
        )?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .iter()
            .map(|u| url_key(u))
            .collect()
    };
    let stale: Vec<(i64, String, String, String)> = {
        let mut stmt = c.prepare(
            "SELECT id, url, artist_name, title FROM harvest_items WHERE state = 'queued' ORDER BY id",
        )?;
        stmt.query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|(_, url, _, _)| !live.contains(&url_key(url)))
            .collect()
    };
    if stale.is_empty() {
        return Ok((0, 0));
    }

    // Exact first, in batches: one query per 400 URLs rather than per item.
    let mut by_url: HashMap<String, i64> = HashMap::new();
    let keys: Vec<String> = stale.iter().map(|(_, u, _, _)| url_key(u)).collect::<BTreeSet<_>>().into_iter().collect();
    for batch in keys.chunks(BATCH) {
        let mut stmt = c.prepare(&format!(
            "SELECT id, bandcamp_url FROM releases WHERE lower(bandcamp_url) IN ({}) ORDER BY id",
            placeholders(batch.len())
        ))?;
        let rows = stmt
            .query_map(params_from_iter(text_params(batch)), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (id, url) in rows {
            by_url.entry(url_key(&url)).or_insert(id);
        }
    }

    // Built once, and consuming: two inbox rows must not claim one release.
    let mut index = build_release_index(c)?;

    let (mut resolved, mut reopened) = (0, 0);
    for (id, url, artist, title) in stale {
        let release_id = match by_url.get(&url_key(&url)) {
            Some(id) => Some(*id),
            None => take(&mut index, &artist, &title),
        };
        let Some(release_id) = release_id else {
            c.execute(
                "UPDATE harvest_items SET state='new', release_id=NULL, resolved_at=NULL WHERE id=?1",
                [id],
            )?;
            reopened += 1;
            continue;
        };
        c.execute("UPDATE harvest_items SET state='in_library', release_id=?1 WHERE id=?2", params![release_id, id])?;
        // Same repair the inbox does on a name match: record the URL while we
        // know it, so the exact path -- and find_known, which the worker
        // pre-flights on -- covers this release from here on. The URL is free by
        // construction: a URL already on a release matched above and never
        // reached take().
        c.execute(
            "UPDATE releases SET bandcamp_url=?1 WHERE id=?2 AND bandcamp_url IS NULL",
            params![normalise(&url), release_id],
        )?;
        resolved += 1;
    }
    Ok((resolved, reopened))
}

/// Copy done album job_items URLs onto releases missing a `bandcamp_url`.
///
/// Idempotent: releases with a URL are left alone, so reruns are no-ops. First
/// URL per release wins; URLs already claimed by another release are skipped to
/// respect the unique constraint.
pub fn backfill_release_urls(c: &Connection) -> Result<usize> {
    let rows: Vec<(String, i64)> = {
        let mut stmt = c.prepare(
            "SELECT url, release_id FROM job_items WHERE status = 'done' AND url_kind = 'album' \
             AND url IS NOT NULL AND release_id IS NOT NULL ORDER BY finished_at, id",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<Vec<_>, _>>()?
    };
    if rows.is_empty() {
        return Ok(0);
    }
    let mut taken: HashSet<String> = {
        let mut stmt = c.prepare("SELECT bandcamp_url FROM releases WHERE bandcamp_url IS NOT NULL")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(|u| u.to_lowercase())
            .collect()
    };
    let mut count = 0;
    for (url, release_id) in rows {
        let canonical = normalise(&url);
        if taken.contains(&canonical.to_lowercase()) {
            continue;
        }
        let current: Option<Option<String>> = c
            .query_row("SELECT bandcamp_url FROM releases WHERE id=?1", [release_id], |r| r.get(0))
            .optional()?;
        match current {
            Some(None) => {}
            _ => continue, // missing release, or it already has a URL
        }
        c.execute("UPDATE releases SET bandcamp_url=?1 WHERE id=?2", params![canonical, release_id])?;
        taken.insert(canonical.to_lowercase());
        count += 1;
    }
    Ok(count)
}

/// Fold release rows that are the same record recorded twice.
///
/// A release is identified by `(artist, title, year)`, and the year is the part
/// Bandcamp changes: a reissue, a corrected date, a digital release catching up
/// with the vinyl. Re-download such a record and the tags carry the new year, so
/// ingest made a *second* release row for an album already on the shelf --
/// pointing at the same folder, holding the freshly downloaded files, while the
/// original kept the two or three tracks it had.
///
/// That original is then permanently short. It shows "2/10 tracks" and offers a
/// fill; the fill downloads the record perfectly and ingests it onto the twin;
/// the count never moves.
///
/// Non-destructive: every track moves onto the keeper (the row holding the most,
/// so the completed download wins) and nothing on disk is touched, so plays,
/// ratings, loved flags, playlists and sets all follow the tracks they are
/// attached to. Where both rows hold the same track under differently folded
/// filenames both survive the fold as two tracks of one album, which is what the
/// two files on disk actually are.
///
/// Returns the number of release rows folded away.
pub fn merge_folder_twins(c: &Connection) -> Result<usize> {
    // A folder that is home to several *different* album titles is a shelf root,
    // not an album folder -- the downloads base itself, or a fan's shelf, both of
    // which hundreds of releases point at when no per-album folder was ever
    // recorded. Two records sharing only such a root are no evidence of anything,
    // and folding on it would merge genuinely different albums that happen to
    // share an artist and a title.
    //
    // Two independent aggregate passes, filtered against each other here, rather
    // than one query with the container set as a `NOT IN (subquery)`: SQLite
    // re-runs a grouped subquery in that position for every candidate row, which
    // on 37k releases stopped answering altogether.
    let containers: HashSet<String> = {
        let mut stmt = c.prepare(
            "SELECT folder_path FROM releases WHERE folder_path IS NOT NULL GROUP BY folder_path \
             HAVING COUNT(DISTINCT title_key) > 1",
        )?;
        stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<std::result::Result<_, _>>()?
    };
    let twins: Vec<(Option<i64>, String, String)> = {
        let mut stmt = c.prepare(
            "SELECT artist_id, title_key, folder_path FROM releases WHERE folder_path IS NOT NULL \
             GROUP BY artist_id, title_key, folder_path HAVING COUNT(id) > 1",
        )?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(_, _, folder)| !containers.contains(folder))
            .collect()
    };

    let mut folded = 0;
    for (artist_id, title_key, folder_path) in twins {
        struct Rel {
            id: i64,
            tracks: i64,
            url: Option<String>,
            label_id: Option<i64>,
            added_at: String,
        }
        let rows: Vec<Rel> = {
            let mut stmt = c.prepare(
                "SELECT r.id, (SELECT COUNT(*) FROM tracks t WHERE t.release_id = r.id), r.bandcamp_url, \
                 r.label_id, r.added_at FROM releases r \
                 WHERE r.artist_id IS ?1 AND r.title_key = ?2 AND r.folder_path = ?3 ORDER BY r.id",
            )?;
            stmt.query_map(params![artist_id, title_key, folder_path], |r| {
                Ok(Rel {
                    id: r.get(0)?,
                    tracks: r.get(1)?,
                    url: r.get(2)?,
                    label_id: r.get(3)?,
                    added_at: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };
        if rows.len() < 2 {
            continue;
        }
        // The completed download keeps the shelf: most tracks first, then the
        // oldest row, so the choice is stable across runs.
        let keeper = rows.iter().max_by_key(|r| (r.tracks, std::cmp::Reverse(r.id))).expect("two or more rows");
        let mut keeper_url = keeper.url.clone();
        let mut keeper_label = keeper.label_id;
        let mut keeper_added = keeper.added_at.clone();
        for loser in rows.iter().filter(|r| r.id != keeper.id) {
            c.execute("UPDATE tracks SET release_id=?1 WHERE release_id=?2", params![keeper.id, loser.id])?;
            c.execute("UPDATE harvest_items SET release_id=?1 WHERE release_id=?2", params![keeper.id, loser.id])?;
            // Keep whatever the loser knew that the keeper does not: a Bandcamp
            // URL above all, which is what a later fill needs. Cleared on the
            // loser first -- the column is UNIQUE, so both rows holding it for
            // even one statement is a constraint violation.
            if let Some(inherited) = &loser.url
                && keeper_url.is_none()
            {
                c.execute("UPDATE releases SET bandcamp_url=NULL WHERE id=?1", [loser.id])?;
                c.execute("UPDATE releases SET bandcamp_url=?1 WHERE id=?2", params![inherited, keeper.id])?;
                keeper_url = Some(inherited.clone());
            }
            if keeper_label.is_none() && loser.label_id.is_some() {
                c.execute("UPDATE releases SET label_id=?1 WHERE id=?2", params![loser.label_id, keeper.id])?;
                keeper_label = loser.label_id;
            }
            if loser.added_at < keeper_added {
                c.execute("UPDATE releases SET added_at=?1 WHERE id=?2", params![loser.added_at, keeper.id])?;
                keeper_added = loser.added_at.clone();
            }
            c.execute("DELETE FROM releases WHERE id=?1", [loser.id])?;
            folded += 1;
        }
    }
    if folded > 0 {
        tracing::info!("folded {folded} duplicate release row(s) sharing a folder");
    }
    Ok(folded)
}

// ---------------------------------------------------------------------------
// Async wrappers
// ---------------------------------------------------------------------------

pub async fn find_known_async(
    db: &Db,
    urls: Vec<String>,
    names: Option<NameLookup>,
) -> Result<HashMap<String, String>> {
    db.read_async(move |c| find_known(c, &urls, names.as_ref())).await
}

pub async fn find_known_ids_async(db: &Db, urls: Vec<String>, names: Option<NameLookup>) -> Result<KnownIds> {
    db.read_async(move |c| find_known_ids(c, &urls, names.as_ref())).await
}

pub async fn backfill_release_labels_async(db: &Db) -> Result<usize> {
    db.write_async(|t| backfill_release_labels(t)).await
}

pub async fn backfill_release_urls_async(db: &Db) -> Result<usize> {
    db.write_async(|t| backfill_release_urls(t)).await
}

pub async fn repair_release_urls_async(db: &Db) -> Result<(usize, usize)> {
    db.write_async(|t| repair_release_urls(t)).await
}

pub async fn repair_harvest_links_async(db: &Db) -> Result<usize> {
    db.write_async(|t| repair_harvest_links(t)).await
}

pub async fn resolve_stale_queued_async(db: &Db) -> Result<(usize, usize)> {
    db.write_async(|t| resolve_stale_queued(t)).await
}

pub async fn merge_folder_twins_async(db: &Db) -> Result<usize> {
    db.write_async(|t| merge_folder_twins(t)).await
}

pub async fn file_known_releases_under_label_async(
    db: &Db,
    label_name: String,
    label_url: Option<String>,
    entries: Vec<(String, String, String)>,
) -> Result<usize> {
    db.write_async(move |t| file_known_releases_under_label(t, &label_name, label_url.as_deref(), &entries)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_matches_the_python() {
        assert_eq!(normalise("https://Artist.Bandcamp.com/album/great-record/"), "https://artist.bandcamp.com/album/great-record");
        assert_eq!(
            normalise("https://a.bandcamp.com/album/x?action=buy&b=2&a=1&from=d&blank="),
            "https://a.bandcamp.com/album/x?a=1&b=2"
        );
        assert_eq!(normalise("a.bandcamp.com/album/x"), "https://a.bandcamp.com/album/x");
    }

    #[test]
    fn classify_matches_the_python() {
        let k = |u: &str| classify_url(u);
        assert_eq!(k("https://x.bandcamp.com/album/y"), UrlKind::Album);
        assert_eq!(k("https://x.bandcamp.com/track/y"), UrlKind::Track);
        assert_eq!(k("https://music.example.com/album/y"), UrlKind::Album);
        assert_eq!(k("https://lemos.bandcamp.com/music"), UrlKind::Music);
        assert_eq!(k("https://lemos.bandcamp.com"), UrlKind::Artist);
        assert_eq!(k("https://lemos.bandcamp.com/artists"), UrlKind::Artists);
        assert_eq!(k("https://bandcamp.com/someone"), UrlKind::Fan);
        assert_eq!(k("https://bandcamp.com/discover/techno"), UrlKind::Discover);
        assert_eq!(k("https://bandcamp.com/login"), UrlKind::Unknown);
        assert_eq!(k("ftp://x.bandcamp.com/album/y"), UrlKind::Unknown);
    }

    #[test]
    fn name_key_folds_like_the_python() {
        assert_eq!(name_key("Motörhead"), "motorhead");
        assert_eq!(name_key("  Kind   Of Green! "), "kind of green");
        assert_eq!(name_key("O.M.Theorem"), "o m theorem");
        assert_eq!(name_key(""), "");
    }
}
