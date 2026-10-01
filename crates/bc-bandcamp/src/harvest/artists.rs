//! Pinning artists to their own Bandcamp pages (port of `services/harvest/artists.py`).
//!
//! An artist's `bandcamp_url` is what makes their page able to *do* things: show their bio and
//! photo, find releases the library is missing, and download their catalogue in one press. Most
//! of the time the library already knows the URL without asking the network -- roughly two
//! thirds of a scanned library's releases carry the `bandcamp_url` they were bought or
//! harvested from, and `urls::artist_root` reduces any of them to the band page it lives on.
//!
//! The trap is label pages. A release on `ostgut.bandcamp.com` names the host of a label, not
//! of its artist, and an artist whose whole catalogue came through one label would otherwise be
//! pinned to that label's page -- from where "find new releases" would harvest the entire
//! roster's output as theirs. So a host only counts as an artist's own page when nothing
//! suggests it publishes anybody else: it must not be a known label's URL, and neither the
//! library's releases nor the harvest inbox may show a second artist publishing there.
//!
//! For artists the evidence can't settle, [`locate_artist_page`] asks Bandcamp by name -- proven
//! against the library the same way label locate is, because search alone would happily hand
//! back a stranger with the same name.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use bc_db::rusqlite::{Connection, OptionalExtension, params};
use bc_db::{Db, util::name_key};

use super::labels::{LocateResult, PageSource, hostname, pin_url};
use crate::extract;
use crate::net::PageKind;
use crate::service::Ctx;
use crate::urls;

pub fn init(_ctx: &Arc<Ctx>) {}

pub async fn start(_ctx: &Arc<Ctx>) {}

/// Give each artist their own Bandcamp page, where the evidence is unambiguous.
///
/// Zero network: the releases table already ties artists to the hosts their records were
/// fetched from. A host is accepted as *the* artist's page when no known label lives there, no
/// second artist's releases or harvest items live there, and a strict majority of the artist's
/// URL-bearing releases do.
///
/// Never overwrites: a URL set by hand, or by locate, stands.
pub fn backfill_artist_urls(c: &Connection) -> bc_db::Result<usize> {
    let rows: Vec<(i64, String)> = {
        let mut st = c.prepare("SELECT artist_id, bandcamp_url FROM releases WHERE bandcamp_url IS NOT NULL AND artist_id IS NOT NULL")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
    };
    if rows.is_empty() {
        return Ok(0);
    }

    let mut artist_hosts: HashMap<i64, HashMap<String, i64>> = HashMap::new();
    let mut host_artists: HashMap<String, HashSet<i64>> = HashMap::new();
    for (artist_id, url) in rows {
        let host = hostname(&url);
        if host.is_empty() {
            continue;
        }
        *artist_hosts.entry(artist_id).or_default().entry(host.clone()).or_insert(0) += 1;
        host_artists.entry(host).or_default().insert(artist_id);
    }

    let label_hosts: HashSet<String> = {
        let mut st = c.prepare("SELECT bandcamp_url FROM labels WHERE bandcamp_url IS NOT NULL")?;
        st.query_map([], |r| r.get::<_, String>(0))?.collect::<std::result::Result<Vec<_>, _>>()?.iter().map(|u| hostname(u)).collect()
    };

    // The inbox sees more of a host than the library does -- a label page harvested once shows
    // its whole roster, even the artists never bought.
    let mut inbox_hosts: HashMap<String, HashSet<String>> = HashMap::new();
    {
        let mut st = c.prepare("SELECT url, artist_name FROM harvest_items WHERE url_kind = 'album'")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
        for r in rows {
            let (url, artist) = r?;
            let host = hostname(&url);
            let fold = name_key(artist.as_deref().unwrap_or(""));
            if !host.is_empty() && !fold.is_empty() {
                inbox_hosts.entry(host).or_default().insert(fold);
            }
        }
    }

    let artists: Vec<(i64, Option<String>)> = {
        let mut st = c.prepare("SELECT id, bandcamp_url FROM artists ORDER BY id")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<std::result::Result<_, _>>()?
    };
    let mut taken: HashSet<String> = artists.iter().filter_map(|(_, u)| u.clone()).collect();
    let mut count = 0;
    for (id, url) in artists {
        let Some(hosts) = artist_hosts.get(&id) else { continue };
        if url.is_some() {
            continue;
        }
        let total: i64 = hosts.values().sum();
        let best = hosts
            .iter()
            .filter(|(host, _)| {
                !label_hosts.contains(*host)
                    && host_artists.get(*host).map_or(0, HashSet::len) < 2
                    && inbox_hosts.get(*host).map_or(0, HashSet::len) < 2
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
        c.execute("UPDATE artists SET bandcamp_url = ?2 WHERE id = ?1", params![id, url])?;
        taken.insert(url);
        count += 1;
    }
    Ok(count)
}

/// Find an artist's Bandcamp page by search, proven against the library.
///
/// Same shape as `locate_label_page`, with the matching adjusted for whose page it is: on an
/// artist's *own* `/music` grid the artist column is usually blank (the page is the byline), so
/// a candidate item counts when its title matches an owned release and its artist is either
/// absent or folds to this artist. Requiring the name would reject nearly every true page;
/// ignoring it would let a label page match on title alone.
///
/// The winner is stored on the artist row, so this runs at most once per artist. `None` = the
/// artist row does not exist.
pub async fn locate_artist_page(db: &Db, src: &dyn PageSource, artist_id: i64) -> Option<LocateResult> {
    let lookup = db
        .read_async(move |c| {
            let artist: Option<(String, String, Option<String>)> = c
                .query_row("SELECT name, name_key, bandcamp_url FROM artists WHERE id = ?1", [artist_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })
                .optional()?;
            let Some((name, key, url)) = artist else { return Ok(None) };
            let owned: HashSet<String> = {
                let mut st = c.prepare("SELECT title_key FROM releases WHERE artist_id = ?1")?;
                st.query_map([artist_id], |r| r.get::<_, Option<String>>(0))?
                    .collect::<std::result::Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .filter(|t| !t.is_empty())
                    .collect()
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
        return Some(LocateResult::detail("no releases by this artist to validate against"));
    }

    let mut hits = match src.search(&name, "b", 8).await {
        Ok(h) => h,
        Err(e) => return Some(LocateResult::detail(format!("search failed: {e}"))),
    };
    // Exact name folds first: "Kaiserdisco" should try the page called Kaiserdisco before a fan
    // account that merely contains the word.
    hits.sort_by_key(|h| name_key(&h.name) != key);

    for hit in hits.into_iter().take(4) {
        let root = urls::artist_root(&hit.url);
        let body = match src.page(&format!("{root}/music"), PageKind::Music, Some(Duration::from_secs(43_200))).await {
            Ok(b) => b,
            Err(e) => {
                tracing::info!("locate artist: {root} unreachable: {e}");
                continue;
            }
        };
        let (items, _tier) = extract::parse_music_grid(&body, &root);
        let matched = items
            .iter()
            .filter(|i| owned.contains(&name_key(&i.title)) && (i.artist.is_empty() || name_key(&i.artist) == key))
            .count() as i64;
        if matched == 0 {
            continue;
        }
        let url = urls::normalise(&root);
        return Some(match pin_url(db, "artists", artist_id, &url).await {
            Ok(None) => LocateResult { url: Some(url), matched, detail: String::new() },
            Ok(Some(holder)) => LocateResult::detail(format!("{url} already belongs to \u{201c}{holder}\u{201d}")),
            Err(e) => LocateResult::detail(format!("database: {e}")),
        });
    }
    Some(LocateResult::detail("no Bandcamp page matching this artist's releases was found"))
}
