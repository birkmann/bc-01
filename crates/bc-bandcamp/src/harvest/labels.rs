//! Recovering the labels the wishlist walk could not see (port of `services/harvest/labels.py`).
//!
//! The collection API behind a wishlist backfill carries a `label` field that is empty for
//! most releases -- Bandcamp only fills it when the release has an explicit label credit. But
//! much of a wishlist lives on label *pages*: one subdomain publishing records by many
//! different artists, where every album page names the label outright in its JSON-LD
//! `publisher`. The walk never opens those pages, so the inbox ends up with the artist and no
//! label, and `backfill_release_labels` has nothing to file the releases under.
//!
//! The label is therefore recovered per host, not per item. Any host whose items name at least
//! two different artists is a candidate label page; one of its album pages is fetched and
//! whatever label it states is applied to every item of that host still missing one. One
//! request per label instead of one per album -- about a thousand candidates against ten
//! thousand unlabelled items in the library this was built for. An artist page sampled by
//! mistake states no label (its publisher *is* the artist), so it costs a request and writes
//! nothing.
//!
//! Two later additions widen the net. The library's own releases are candidates too, not only
//! the inbox: a record downloaded from a pasted URL never enters the inbox, yet its
//! `bandcamp_url` names the same host. And each host is first asked directly -- its `/music`
//! page says whether it *is* a label and what it is called -- which is one request, cannot be
//! fooled by an artist page crediting someone else's imprint, and hands back the discography
//! the library is filed against. The album-page sample stays as the fallback for label pages
//! Bandcamp does not flag.
//!
//! As a job: a resolution is a `sweep` job with `params.sweep = "resolve"` (one item); the
//! in-memory [`LabelResolveStatus`] stays the source of truth for `GET /harvest/labels` and is
//! published as `harvest.labels`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bc_db::rusqlite::{Connection, OptionalExtension, params};
use bc_db::{Db, util::name_key};
use bc_jobs::{NewItem, NewJob};
use bc_types::bandcamp::{LabelResolveStatus, TOPIC_HARVEST_LABELS, TOPIC_LIBRARY_CHANGED};
use bc_types::jobs::KIND_SWEEP;
use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::download::dedup::{backfill_release_labels, file_known_releases_under_label, get_or_create_label};
use crate::error::{HarvestError, Result};
use crate::extract::{self, BandProfile, GridItem};
use crate::net::{BandcampClient, GetOpts, PageKind};
use crate::service::Ctx;
use crate::sources::{self, SearchHit};
use crate::urls;

/// Page fetching and search, as the label/artist services need them. The real implementation
/// is the shared [`BandcampClient`]; tests substitute canned pages (the Python `_FakeClient`).
#[async_trait]
pub trait PageSource: Send + Sync {
    /// Fetch a page's HTML. `ttl` overrides the kind's cache TTL.
    async fn page(&self, url: &str, kind: PageKind, ttl: Option<Duration>) -> Result<String>;
    /// Bandcamp's autocomplete (`kind` = Bandcamp's filter code, `"b"` = band/label).
    async fn search(&self, query: &str, kind: &str, limit: usize) -> Result<Vec<SearchHit>>;
}

#[async_trait]
impl PageSource for BandcampClient {
    async fn page(&self, url: &str, kind: PageKind, ttl: Option<Duration>) -> Result<String> {
        let mut o = GetOpts::kind(kind);
        if let Some(t) = ttl {
            o = o.ttl(t);
        }
        self.get_html(url, o).await
    }
    async fn search(&self, query: &str, kind: &str, limit: usize) -> Result<Vec<SearchHit>> {
        sources::search(self, query, kind, limit).await
    }
}

/// The lower-cased host of a URL (Python `urlsplit(url).hostname`).
pub fn hostname(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_lowercase)).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------
// Candidates
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub host: String,
    /// Up to two album URLs by *different* artists. Two, because on a label page whose sampled
    /// album happens to be by an artist named like the label itself the publisher check stays
    /// silent; a second artist's page settles it.
    pub sample_urls: Vec<String>,
}

#[derive(Default)]
struct HostEntry {
    artists: Vec<(String, String)>,
    seen: HashSet<String>,
    missing: i64,
}

/// Whether an artist's name is spelled out in a host's subdomain: "Nina Kraviz" on
/// `ninakraviz.bandcamp.com`, "DJ Dextro" on `djdextroofficial.bandcamp.com`. That is the
/// artist's own page. An empty name proves nothing either way and counts as at home.
fn named_in_host(host: &str, artist_fold: &str) -> bool {
    let sub: String = host.trim_end_matches(".bandcamp.com").chars().filter(char::is_ascii_alphanumeric).collect();
    let name: String = artist_fold.chars().filter(|c| c.is_alphanumeric()).collect();
    name.is_empty() || sub.is_empty() || sub.contains(&name) || name.contains(&sub)
}

/// Hosts that look like label pages and still have unlabelled items.
///
/// "Looks like a label page" is a property of the data itself: two or more distinct artists
/// publishing on one host, or one artist publishing on a host that is plainly not named after
/// them (Sebo K on `rekids.bandcamp.com`) -- a library holding a single record off a label's
/// page is the common case, not the exception. A lone artist on a host spelling out their own
/// name is that artist's own page -- self-released, correctly unlabelled -- and fetching it
/// would be a request spent to learn nothing.
///
/// Both the inbox and the library's releases count as evidence, and both count as work left to
/// do: a host is a candidate while any inbox item *or* any library release on it is still
/// unlabelled. The library side is what reaches records that never passed through the inbox.
pub fn find_candidates(c: &Connection) -> bc_db::Result<Vec<Candidate>> {
    let mut by_host: BTreeMap<String, HostEntry> = BTreeMap::new();
    let mut note = |url: &str, artist: &str, missing: bool| {
        let host = hostname(url);
        if host.is_empty() || !host.ends_with(".bandcamp.com") {
            return;
        }
        let e = by_host.entry(host).or_default();
        let fold = name_key(artist);
        if !fold.is_empty() && e.seen.insert(fold.clone()) {
            e.artists.push((fold, url.to_string()));
        }
        if missing {
            e.missing += 1;
        }
    };

    {
        let mut st = c.prepare("SELECT url, artist_name, label_name FROM harvest_items WHERE url_kind = 'album' ORDER BY id")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?)))?;
        for r in rows {
            let (url, artist, label) = r?;
            note(&url, artist.as_deref().unwrap_or(""), label.is_none());
        }
    }
    {
        let mut st = c.prepare(
            "SELECT r.bandcamp_url, a.name, r.label_id FROM releases r LEFT JOIN artists a ON a.id = r.artist_id \
             WHERE r.bandcamp_url IS NOT NULL ORDER BY r.id",
        )?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?)))?;
        for r in rows {
            let (url, artist, label_id) = r?;
            note(&url, artist.as_deref().unwrap_or(""), label_id.is_none());
        }
    }

    Ok(by_host
        .into_iter()
        .filter(|(host, e)| e.missing > 0 && (e.artists.len() >= 2 || e.artists.iter().any(|(fold, _)| !named_in_host(host, fold))))
        .map(|(host, e)| Candidate { host, sample_urls: e.artists.into_iter().take(2).map(|(_, u)| u).collect() })
        .collect())
}

/// Write the host's label onto its unlabelled items.
///
/// Skipped where the label folds to one of the item's own credited artists: that is the same
/// guard the page extraction applies, and without it a mis-sampled artist page would file the
/// artist as their own label.
pub fn apply_label(c: &Connection, host: &str, label: &str) -> bc_db::Result<usize> {
    let rows: Vec<(i64, String)> = {
        let mut st = c.prepare("SELECT id, artist_name FROM harvest_items WHERE url LIKE ?1 AND label_name IS NULL")?;
        st.query_map([format!("https://{host}/%")], |r| Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default())))?
            .collect::<std::result::Result<_, _>>()?
    };
    let folded = label.to_lowercase();
    let mut count = 0;
    for (id, artist) in rows {
        if extract::artist_parts(&artist).contains(&folded) {
            continue;
        }
        c.execute("UPDATE harvest_items SET label_name = ?2 WHERE id = ?1", params![id, label])?;
        count += 1;
    }
    Ok(count)
}

/// File the library's releases living on a proven label page under it.
///
/// Only for a host whose own page says it is a label: everything published on such a page came
/// out on that label, whatever the discography grid happened to show (a themed page can
/// truncate it). Fills empty `label_id` only, and never files an artist as their own label --
/// the same guard every other path here applies.
pub fn file_host_releases(c: &Connection, host: &str, label_name: &str, label_url: &str) -> bc_db::Result<usize> {
    let releases: Vec<(i64, String)> = {
        let mut st = c.prepare(
            "SELECT r.id, coalesce(a.name, '') FROM releases r LEFT JOIN artists a ON a.id = r.artist_id \
             WHERE r.bandcamp_url LIKE ?1 AND r.label_id IS NULL",
        )?;
        st.query_map([format!("https://{host}/%")], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
    };
    if releases.is_empty() {
        return Ok(0);
    }
    let Some(label_id) = get_or_create_label(c, label_name)? else { return Ok(0) };
    let current: Option<String> = c.query_row("SELECT bandcamp_url FROM labels WHERE id = ?1", [label_id], |r| r.get(0))?;
    if current.is_none() {
        let taken: Option<i64> =
            c.query_row("SELECT id FROM labels WHERE bandcamp_url = ?1 AND id != ?2", params![label_url, label_id], |r| r.get(0)).optional()?;
        if taken.is_none() {
            c.execute("UPDATE labels SET bandcamp_url = ?2 WHERE id = ?1", params![label_id, label_url])?;
        }
    }
    let folded = label_name.to_lowercase();
    let mut count = 0;
    for (rid, artist) in releases {
        if extract::artist_parts(&artist).contains(&folded) {
            continue;
        }
        c.execute("UPDATE releases SET label_id = ?2 WHERE id = ?1", params![rid, label_id])?;
        count += 1;
    }
    Ok(count)
}

/// Re-credit releases on a proven label page that are bylined to the label itself.
///
/// Bandcamp shows a label's own uploads as "by <Label>" unless the label credits the album to
/// an artist, so bandcamp-dl tags the album artist as the label while each track still names
/// who made it. Such a release cannot be filed under its label (nothing files an artist as their
/// own label) and sits on the shelf as the label's "artist". Where its tracks credit someone
/// other than the label, the release takes that artist -- or "Various Artists" when the tracks
/// name several. A release whose tracks only ever name the label is left alone: there is no
/// evidence of anyone else.
///
/// The release identity `(artist, title, year)` stays unique: a twin already filed under the
/// new artist keeps the old credit, for the folder-twin merge to settle.
pub fn adopt_track_artists(c: &Connection, host: &str, label_name: &str) -> bc_db::Result<usize> {
    let folded = label_name.trim().to_lowercase();
    if folded.is_empty() {
        return Ok(0);
    }
    let releases: Vec<(i64, String, String, Option<i64>)> = {
        let mut st = c.prepare(
            "SELECT r.id, coalesce(a.name, ''), r.title_key, r.year FROM releases r LEFT JOIN artists a ON a.id = r.artist_id \
             WHERE r.bandcamp_url LIKE ?1",
        )?;
        st.query_map([format!("https://{host}/%")], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<_, _>>()?
    };
    let mut count = 0;
    for (rid, artist, title_key, year) in releases {
        if !extract::artist_parts(&artist).contains(&folded) {
            continue;
        }
        let credited: Vec<(i64, String)> = {
            let mut st = c.prepare("SELECT DISTINCT a.id, a.name FROM tracks t JOIN artists a ON a.id = t.artist_id WHERE t.release_id = ?1")?;
            st.query_map([rid], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
        };
        let others: Vec<i64> = credited.into_iter().filter(|(_, n)| !extract::artist_parts(n).contains(&folded)).map(|(id, _)| id).collect();
        let artist_id = match others.as_slice() {
            [] => continue,
            [one] => *one,
            _ => various_artists(c)?,
        };
        let twin: Option<i64> = c
            .query_row(
                "SELECT id FROM releases WHERE artist_id = ?1 AND title_key = ?2 AND year IS ?3 AND id != ?4",
                params![artist_id, title_key, year, rid],
                |r| r.get(0),
            )
            .optional()?;
        if twin.is_some() {
            continue;
        }
        c.execute("UPDATE releases SET artist_id = ?2 WHERE id = ?1", params![rid, artist_id])?;
        count += 1;
    }
    Ok(count)
}

fn various_artists(c: &Connection) -> bc_db::Result<i64> {
    const NAME: &str = "Various Artists";
    let key = name_key(NAME);
    if let Some(id) = c.query_row("SELECT id FROM artists WHERE name_key = ?1", [&key], |r| r.get(0)).optional()? {
        return Ok(id);
    }
    c.execute("INSERT INTO artists(name, name_key, created_at) VALUES (?1, ?2, ?3)", params![NAME, key, bc_db::util::now_db()])?;
    Ok(c.last_insert_rowid())
}

/// Give each label its own Bandcamp page, where the evidence is unambiguous.
///
/// The URL is what makes "find new releases" possible for a label folder. The inbox already
/// ties label names to hosts, so no request is needed. A host is accepted as *the* label's page
/// when it looks like a label page at all (two or more distinct artists publish on it, so it is
/// not one artist's own page that merely credits the label), the label is what that host's
/// items predominantly name, and a strict majority of the label's items live there.
///
/// The majority test is what keeps a sub-label's page from claiming the parent: a handful of
/// Ostgut Ton records living on unterton.bandcamp.com is not most of Ostgut Ton. And the
/// multi-artist test is what keeps an artist page out: half of Soma Records' items sit on
/// slam-djs.bandcamp.com, but only Slam publishes there -- soma-records.bandcamp.com, where the
/// other half by many artists lives, is the page.
///
/// Never overwrites: a URL set by hand, or by an earlier run, stands.
pub fn backfill_label_urls(c: &Connection) -> bc_db::Result<usize> {
    let rows: Vec<(String, Option<String>, Option<String>)> = {
        let mut st = c.prepare("SELECT url, label_name, artist_name FROM harvest_items WHERE url_kind = 'album'")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?
    };
    if rows.is_empty() {
        return Ok(0);
    }
    let mut label_hosts: HashMap<String, HashMap<String, i64>> = HashMap::new();
    let mut host_artists: HashMap<String, HashSet<String>> = HashMap::new();
    let mut host_labels: HashMap<String, HashMap<String, i64>> = HashMap::new();
    for (url, label_name, artist_name) in rows {
        let host = hostname(&url);
        if host.is_empty() {
            continue;
        }
        let af = name_key(artist_name.as_deref().unwrap_or(""));
        if !af.is_empty() {
            host_artists.entry(host.clone()).or_default().insert(af);
        }
        let fold = name_key(label_name.as_deref().unwrap_or(""));
        if fold.is_empty() {
            continue;
        }
        *label_hosts.entry(fold.clone()).or_default().entry(host.clone()).or_insert(0) += 1;
        *host_labels.entry(host).or_default().entry(fold).or_insert(0) += 1;
    }

    let labels: Vec<(i64, String, Option<String>)> = {
        let mut st = c.prepare("SELECT id, name_key, bandcamp_url FROM labels ORDER BY id")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?
    };
    let mut taken: HashSet<String> = labels.iter().filter_map(|(_, _, u)| u.clone()).collect();
    let mut count = 0;
    for (id, key, url) in labels {
        if url.is_some() {
            continue;
        }
        let Some(hosts) = label_hosts.get(&key) else { continue };
        let total: i64 = hosts.values().sum();
        let best = hosts
            .iter()
            .filter(|(host, _)| {
                let hl = host_labels.get(*host);
                host_artists.get(*host).map_or(0, HashSet::len) >= 2
                    && hl.and_then(|m| m.get(&key)).copied().unwrap_or(0) * 2 > hl.map_or(0, |m| m.values().sum::<i64>())
            })
            .map(|(host, n)| (*n, host.clone()))
            .max();
        let Some((n, host)) = best else { continue };
        if n * 2 <= total {
            continue;
        }
        let url = format!("https://{host}");
        if taken.contains(&url) {
            continue;
        }
        c.execute("UPDATE labels SET bandcamp_url = ?2 WHERE id = ?1", params![id, url])?;
        taken.insert(url);
        count += 1;
    }
    Ok(count)
}

/// Name the label on everything harvested from that label's own page.
///
/// A release sitting on `ostgut.bandcamp.com/music` is on Ostgut Ton -- that is what a label
/// page *is*, and it is stronger evidence than the JSON-LD publisher field the rest of this
/// module reads. The harvest threw it away: the `/music` grid carries no label per item, so
/// 2,533 of 3,490 label-page harvests landed with `label_name` NULL and the releases they
/// matched stayed unfiled. A label folder then showed 39 of the 137 records the user actually
/// had from it.
///
/// The link back is exact rather than fuzzy: those rows carry `source_kind='label'` and
/// `source_label` set to `urls::display_name(page)`, so a label whose `bandcamp_url` yields the
/// same slug is the label they came from.
///
/// Never overwrites a stated label -- a sub-label release sold on its parent's page keeps its
/// own imprint.
pub fn backfill_harvest_label_names(c: &Connection) -> bc_db::Result<usize> {
    let labels: Vec<(String, String)> = {
        let mut st = c.prepare("SELECT name, bandcamp_url FROM labels WHERE bandcamp_url IS NOT NULL ORDER BY id")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
    };
    let mut by_source: HashMap<String, String> = HashMap::new();
    let mut ambiguous: HashSet<String> = HashSet::new();
    for (name, url) in labels {
        let slug = urls::display_name(&url);
        if slug.is_empty() {
            continue;
        }
        if by_source.get(&slug).is_some_and(|n| *n != name) {
            ambiguous.insert(slug.clone());
        }
        by_source.insert(slug, name);
    }
    for s in ambiguous {
        by_source.remove(&s);
    }
    if by_source.is_empty() {
        return Ok(0);
    }
    let rows: Vec<(i64, String, String)> = {
        let mut st = c.prepare(
            "SELECT id, coalesce(source_label, ''), coalesce(artist_name, '') FROM harvest_items \
             WHERE source_kind = 'label' AND label_name IS NULL AND source_label IS NOT NULL",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?
    };
    let mut count = 0;
    for (id, source, artist) in rows {
        let Some(name) = by_source.get(&source) else { continue };
        // The same guard the page extraction uses: never file an artist as their own label,
        // however the evidence arrived.
        if extract::artist_parts(&artist).contains(&name.to_lowercase()) {
            continue;
        }
        c.execute("UPDATE harvest_items SET label_name = ?2 WHERE id = ?1", params![id, name])?;
        count += 1;
    }
    Ok(count)
}

// ---------------------------------------------------------------------------------------
// Locate
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocateResult {
    pub url: Option<String>,
    /// Library releases of this label found on the located page -- the proof.
    pub matched: i64,
    pub detail: String,
}

impl LocateResult {
    pub fn detail(detail: impl Into<String>) -> Self {
        Self { detail: detail.into(), ..Default::default() }
    }
}

/// Find a label's Bandcamp page by search, proven against the shelf.
///
/// Search alone is not evidence -- half the names in a label list are generic enough to hit
/// somebody else's page. So every candidate is validated the only way that cannot lie: its
/// `/music` catalogue must actually contain release(s) this library holds under the label,
/// matched on the same `(artist, title)` fold everything else here matches on. No overlap, no
/// URL; a wrong page would quietly harvest a stranger's catalogue forever.
///
/// The winner is stored on the label row, so this runs at most once per label. `None` = the
/// label row does not exist.
pub async fn locate_label_page(db: &Db, src: &dyn PageSource, label_id: i64) -> Option<LocateResult> {
    let lookup = db
        .read_async(move |c| {
            let label: Option<(String, String, Option<String>)> = c
                .query_row("SELECT name, name_key, bandcamp_url FROM labels WHERE id = ?1", [label_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            let Some((name, key, url)) = label else { return Ok(None) };
            let owned: HashSet<(String, String)> = {
                let mut st = c.prepare(
                    "SELECT a.name_key, r.title_key FROM releases r JOIN artists a ON r.artist_id = a.id WHERE r.label_id = ?1",
                )?;
                st.query_map([label_id], |r| {
                    Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get::<_, Option<String>>(1)?.unwrap_or_default()))
                })?
                .collect::<std::result::Result<_, _>>()?
            };
            Ok(Some((name, key, url, owned)))
        })
        .await;
    let (name, key, url, owned) = match lookup {
        Ok(Some(v)) => v,
        Ok(None) => return None,
        Err(e) => return Some(LocateResult::detail(format!("database: {e}"))),
    };
    if let Some(url) = url.filter(|u| !u.is_empty()) {
        return Some(LocateResult { url: Some(url), matched: 0, detail: "already known".into() });
    }
    if owned.is_empty() {
        return Some(LocateResult::detail("no releases on this label to validate against"));
    }

    let mut hits = match src.search(&name, "b", 8).await {
        Ok(h) => h,
        Err(e) => return Some(LocateResult::detail(format!("search failed: {e}"))),
    };
    // Exact name folds first: "Soma Records" should try the page called Soma Records before a
    // remix collective that merely contains the words.
    hits.sort_by_key(|h| name_key(&h.name) != key);

    for hit in hits.into_iter().take(4) {
        let root = urls::artist_root(&hit.url);
        let body = match src.page(&format!("{root}/music"), PageKind::Music, Some(Duration::from_secs(43_200))).await {
            Ok(b) => b,
            Err(e) => {
                tracing::info!("locate: {root} unreachable: {e}");
                continue;
            }
        };
        let (items, _tier) = extract::parse_music_grid(&body, &root);
        let matched = items.iter().filter(|i| owned.contains(&(name_key(&i.artist), name_key(&i.title)))).count() as i64;
        if matched == 0 {
            continue;
        }
        let url = urls::normalise(&root);
        return Some(match pin_url(db, "labels", label_id, &url).await {
            Ok(None) => LocateResult { url: Some(url), matched, detail: String::new() },
            Ok(Some(holder)) => LocateResult::detail(format!("{url} already belongs to the label \u{201c}{holder}\u{201d}")),
            Err(e) => LocateResult::detail(format!("database: {e}")),
        });
    }
    Some(LocateResult::detail("no Bandcamp page matching this label's releases was found"))
}

/// Store `url` on a label/artist row unless another row holds it (returns that holder's name).
pub(crate) async fn pin_url(db: &Db, table: &'static str, id: i64, url: &str) -> bc_db::Result<Option<String>> {
    let url = url.to_string();
    db.write_async(move |tx| {
        let holder: Option<String> = tx
            .query_row(&format!("SELECT name FROM {table} WHERE bandcamp_url = ?1 AND id != ?2"), params![url, id], |r| r.get(0))
            .optional()?;
        if holder.is_some() {
            return Ok(holder);
        }
        tx.execute(&format!("UPDATE {table} SET bandcamp_url = ?2 WHERE id = ?1"), params![id, url])?;
        Ok(None)
    })
    .await
}

// ---------------------------------------------------------------------------------------
// The resolver service
// ---------------------------------------------------------------------------------------

/// Runs at most one resolution sweep at a time and reports progress.
///
/// A thousand rate-limited page fetches is half an hour of work, far too long to hold a request
/// open, and losing the sweep to a restart costs nothing -- already-labelled hosts drop out of
/// the candidate list, so a rerun resumes where it left off.
pub struct LabelResolver {
    ctx: Arc<Ctx>,
    state: Mutex<LabelResolveStatus>,
    again: AtomicBool,
    source: RwLock<Option<Arc<dyn PageSource>>>,
    watch: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn idle() -> LabelResolveStatus {
    LabelResolveStatus { phase: "idle".into(), ..Default::default() }
}

fn fresh_running() -> LabelResolveStatus {
    LabelResolveStatus { phase: "running".into(), running: true, started_at: Some(bc_db::util::iso_now()), ..Default::default() }
}

pub fn init(ctx: &Arc<Ctx>) {
    ctx.put(Arc::new(LabelResolver {
        ctx: ctx.clone(),
        state: Mutex::new(idle()),
        again: AtomicBool::new(false),
        source: RwLock::new(None),
        watch: Mutex::new(None),
    }));
}

pub async fn start(ctx: &Arc<Ctx>) {
    // Resolve after every download job settles, unprompted.
    ctx.expect::<LabelResolver>().watch(Duration::from_secs(20));
}

impl LabelResolver {
    pub fn status(&self) -> LabelResolveStatus {
        self.state.lock().clone()
    }

    /// Replace the page source (tests; the default is the shared client).
    pub fn set_source(&self, src: Arc<dyn PageSource>) {
        *self.source.write() = Some(src);
    }

    pub fn source(&self) -> Arc<dyn PageSource> {
        self.source.read().clone().unwrap_or_else(|| Arc::new(self.ctx.client.clone()))
    }

    fn publish(&self, f: impl FnOnce(&mut LabelResolveStatus)) {
        let snapshot = {
            let mut s = self.state.lock();
            f(&mut s);
            s.running = s.phase == "running";
            s.clone()
        };
        self.ctx.bus.publish(TOPIC_HARVEST_LABELS, &snapshot);
    }

    /// Kick off a resolution as a job and return immediately. Errs when one is already running.
    pub async fn start_run(&self) -> Result<LabelResolveStatus> {
        {
            let mut s = self.state.lock();
            if s.phase == "running" {
                return Err(HarvestError::other("a label resolution is already running"));
            }
            *s = fresh_running();
        }
        let snapshot = self.status();
        self.ctx.bus.publish(TOPIC_HARVEST_LABELS, &snapshot);
        let nj = NewJob::new(KIND_SWEEP, vec![NewItem { source: Some("resolve".into()), ..Default::default() }])
            .label("Resolve labels")
            .params(serde_json::json!({"sweep": "resolve"}));
        let store = self.ctx.jobs.store().clone();
        if let Err(e) = store.run(move |s| s.create_job(nj)).await {
            self.publish(|s| {
                s.phase = "failed".into();
                s.error = Some(e.to_string());
                s.finished_at = Some(bc_db::util::iso_now());
            });
            return Err(e.into());
        }
        Ok(self.status())
    }

    /// Resolve after every download job settles, unprompted.
    ///
    /// A batch that just landed is exactly when new unlabelled hosts appear, and the user should
    /// not have to know a button in Settings exists to see the label on the shelf. Debounced: a
    /// job's items finish seconds apart, and only one sweep per settle is worth running. If a
    /// sweep is already walking, one more is queued behind it rather than dropped.
    pub fn watch(self: &Arc<Self>, delay: Duration) {
        let mut slot = self.watch.lock();
        // Calling again replaces the watcher (a new delay), it never doubles it.
        if let Some(old) = slot.take() {
            old.abort();
        }
        let me = self.clone();
        let mut rx = self.ctx.bus.subscribe();
        *slot = Some(tokio::spawn(async move {
            let mut pending: Option<tokio::task::JoinHandle<()>> = None;
            loop {
                let ev = match rx.recv().await {
                    Ok(e) => e,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                if ev.topic != bc_types::jobs::TOPIC_JOB_PROGRESS {
                    continue;
                }
                let status = ev.payload.get("status").and_then(|v| v.as_str()).unwrap_or("");
                if !matches!(status, "completed" | "failed" | "cancelled") {
                    continue;
                }
                // Our own resolution jobs (and the other sweeps) settling must not retrigger us.
                if let Some(job_id) = ev.payload.get("job_id").and_then(|v| v.as_str()) {
                    let id = job_id.to_string();
                    let kind = me.ctx.jobs.store().run(move |s| s.get_job(&id)).await.ok().flatten().map(|j| j.kind);
                    if kind.as_deref() == Some(KIND_SWEEP) {
                        continue;
                    }
                }
                if let Some(p) = pending.take() {
                    p.abort();
                }
                let me2 = me.clone();
                pending = Some(tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    me2.kick().await;
                }));
            }
        }));
    }

    pub fn stop_watch(&self) {
        if let Some(h) = self.watch.lock().take() {
            h.abort();
        }
    }

    /// Start a resolution if any host is waiting for one, or queue one behind the running one.
    pub async fn kick(&self) {
        if self.status().running {
            self.again.store(true, Ordering::SeqCst);
            return;
        }
        // Nothing to resolve: do not litter the job list with an empty run.
        match self.ctx.db.read_async(find_candidates).await {
            Ok(c) if c.is_empty() => return,
            Err(e) => {
                tracing::warn!("label resolver: candidates failed: {e}");
                return;
            }
            Ok(_) => {}
        }
        if let Err(e) = self.start_run().await {
            tracing::debug!("label resolver kick: {e}");
        }
    }

    /// The body of the `sweep` job with `params.sweep == "resolve"`. Returns false when cancelled.
    pub async fn run(&self, cancel: &CancellationToken) -> bool {
        {
            // A job re-claimed after a restart starts from a fresh in-memory state.
            let mut s = self.state.lock();
            if s.phase != "running" {
                *s = fresh_running();
            }
        }
        match self.run_inner(cancel).await {
            Ok(true) => true,
            Ok(false) => {
                self.publish(|s| {
                    s.phase = "failed".into();
                    s.error = Some("Cancelled".into());
                    s.finished_at = Some(bc_db::util::iso_now());
                });
                false
            }
            Err(e) => {
                tracing::warn!("label resolution failed: {e}");
                let msg: String = e.to_string().chars().take(500).collect();
                self.publish(|s| {
                    s.phase = "failed".into();
                    s.error = Some(msg);
                    s.finished_at = Some(bc_db::util::iso_now());
                });
                true
            }
        }
    }

    async fn run_inner(&self, cancel: &CancellationToken) -> Result<bool> {
        let src = self.source();
        let db = &self.ctx.db;
        let candidates = db.read_async(find_candidates).await?;
        let total = candidates.len() as i64;
        self.publish(|s| s.total = Some(total));

        let (mut resolved, mut labelled, mut filed, mut fixed) = (0i64, 0i64, 0i64, 0i64);
        for (i, cand) in candidates.iter().enumerate() {
            if cancel.is_cancelled() {
                return Ok(false);
            }
            if let Some((name, url, entries)) = label_from_page(&*src, cand).await {
                let host = cand.host.clone();
                let (l, f, a) = db
                    .write_async(move |tx| {
                        let l = apply_label(tx, &host, &name)?;
                        // Re-credit the label's own uploads first: filing skips a release whose
                        // artist is the label.
                        let a = adopt_track_artists(tx, &host, &name)?;
                        // The discography grid first -- it matches by URL *and* (artist, title) --
                        // then whatever else the library holds on that host, which the grid may
                        // have truncated.
                        let mut f = file_known_releases_under_label(tx, &name, Some(&url), &entries)?;
                        f += file_host_releases(tx, &host, &name, &url)?;
                        Ok((l, f, a))
                    })
                    .await?;
                labelled += l as i64;
                filed += f as i64;
                fixed += a as i64;
                resolved += 1;
            } else if let Some(stated) = label_for(&*src, cand).await {
                let host = cand.host.clone();
                labelled += db.write_async(move |tx| apply_label(tx, &host, &stated)).await? as i64;
                resolved += 1;
            }
            let seen = i as i64 + 1;
            self.publish(|s| {
                s.seen = seen;
                s.resolved = resolved;
                s.labelled = labelled;
                s.filed = filed;
                s.artists_fixed = fixed;
            });
        }

        // File the releases under what was just learned -- the whole point of the sweep.
        let extra = db
            .write_async(|tx| {
                let n = backfill_release_labels(tx)?;
                backfill_label_urls(tx)?;
                Ok(n)
            })
            .await?;
        filed += extra as i64;
        self.publish(|s| {
            s.phase = "done".into();
            s.filed = filed;
            s.finished_at = Some(bc_db::util::iso_now());
        });
        self.ctx.bus.publish(TOPIC_LIBRARY_CHANGED, &serde_json::json!({"tracks_added": 0}));
        if self.again.swap(false, Ordering::SeqCst) {
            // Something landed while we walked; go round once more.
            self.kick().await;
        }
        Ok(true)
    }
}

/// `(url, artist, title)` rows of a discography, as `file_known_releases_under_label` takes them.
type Entries = Vec<(String, String, String)>;

/// Ask the host itself: its `/music` page, if it says it is a label.
///
/// Returns the label's name, canonical URL and the discography for the file-under step, or
/// `None` when the page is an artist's (or unreachable) -- the caller then falls back to
/// sampling an album page.
async fn label_from_page(src: &dyn PageSource, cand: &Candidate) -> Option<(String, String, Entries)> {
    let root = format!("https://{}", cand.host);
    let body = match src.page(&format!("{root}/music"), PageKind::Music, None).await {
        Ok(b) => b,
        Err(e) => {
            tracing::info!("label page {root} failed: {e}");
            return None;
        }
    };
    let profile: BandProfile = extract::parse_band_profile(&body, &root);
    if !profile.is_label || profile.name.trim().is_empty() {
        return None;
    }
    let name = profile.name.trim().to_string();
    let (items, _): (Vec<GridItem>, _) = extract::parse_music_grid(&body, &root);
    let entries = items
        .into_iter()
        .map(|i| {
            let artist = if i.artist.is_empty() { name.clone() } else { i.artist };
            (i.page_url, artist, i.title)
        })
        .collect();
    Some((name, urls::normalise(&root), entries))
}

async fn label_for(src: &dyn PageSource, cand: &Candidate) -> Option<String> {
    for sample in &cand.sample_urls {
        let html = match src.page(sample, PageKind::Album, None).await {
            Ok(h) => h,
            Err(e) => {
                // One dead page (removed album, private stream) must not kill a half-hour
                // sweep; the host just stays unresolved this run.
                tracing::info!("label sample {sample} failed: {e}");
                continue;
            }
        };
        let label = extract::parse_tralbum(&html, sample).label_name.unwrap_or_default();
        let label = label.trim();
        if !label.is_empty() {
            return Some(label.to_string());
        }
    }
    None
}
