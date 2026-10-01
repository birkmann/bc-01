//! Source adapters (port of `harvest/sources.py`).
//!
//! Each *harvest* source is a stream (`futures::Stream`) yielding results as it
//! finds them rather than a list at the end: a label discography runs to 200+
//! releases and a collection to thousands, so a run must give incremental
//! feedback and be cancellable. Fetch-one helpers (`fetch_release`,
//! `fetch_band_page`, `discover_page`, ...) are plain async fns.
//!
//! Streams yield `Result<HarvestEvent>`: `Err` is a *raised* failure that ends the
//! stream (identity expired, list unavailable, ...); a per-item failure is an
//! `Ok(HarvestEvent { error: Some(..), .. })` so one dead page cannot kill a run.

use std::collections::{BTreeMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use async_stream::try_stream;
use futures::stream::BoxStream;
use serde_json::{Value, json};

use crate::error::{HarvestError, Result};
use crate::extract::{self, Collectors, FanPage, GridItem, HarvestedRelease, RosterArtist, Tier};
use crate::net::{BandcampClient, GetOpts, PageKind};
use crate::urls;

pub type EventStream = BoxStream<'static, Result<HarvestEvent>>;

/// One unit of progress (`HarvestEvent`).
#[derive(Debug, Clone, Default)]
pub struct HarvestEvent {
    pub release: Option<HarvestedRelease>,
    pub artist: Option<RosterArtist>,
    pub error: Option<String>,
    pub cursor: Option<String>,
    pub seen: i64,
    pub total: Option<i64>,
}

/// `SourceProbe`.
#[derive(Debug, Clone, Default)]
pub struct SourceProbe {
    /// url_list | artist | label | collection | wishlist | hidden | discover | fan
    pub kind: String,
    pub label: String,
    pub total_hint: Option<i64>,
    pub requires_auth: bool,
    pub auth_ok: bool,
    pub detail: String,
    pub params: BTreeMap<String, Value>,
}

// -- shallow record from list data ------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Shallow<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub item_type: Option<&'a str>,
    pub art_id: Option<i64>,
    pub art_url: Option<String>,
    pub bc_item_id: Option<i64>,
    pub band_id: Option<i64>,
    pub is_free: bool,
    pub tags: Vec<String>,
}

/// `_shallow`: a record built from list data (no per-release fetch).
pub fn shallow(url: &str, s: Shallow<'_>) -> HarvestedRelease {
    HarvestedRelease {
        url: urls::normalise(url),
        item_type: s.item_type.unwrap_or("album").to_string(),
        title: s.title.to_string(),
        artist_name: s.artist.to_string(),
        bc_item_id: s.bc_item_id,
        band_id: s.band_id,
        art_id: s.art_id,
        art_url: s.art_url.filter(|u| !u.is_empty()).or_else(|| urls::build_art_url(s.art_id)),
        is_free_download: s.is_free,
        tags: s.tags,
        tier: Tier::Blob,
        shallow: true,
        ..Default::default()
    }
}

fn s(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}
fn i(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(x)) => x.parse().ok(),
        _ => None,
    }
}
fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(x)) => !x.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        _ => false,
    }
}

// -- URL list ---------------------------------------------------------------------------

/// `harvest_url_list`: the always-available fallback, zero API dependency.
/// `depth`: `"shallow"` | `"full"`.
pub fn harvest_url_list(client: BandcampClient, text: String, depth: String, limit: Option<usize>) -> EventStream {
    Box::pin(try_stream! {
        let mut candidates: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for line in text.replace(',', "\n").lines() {
            let raw = line.trim();
            if raw.is_empty() || raw.starts_with('#') || raw.starts_with("//") {
                continue;
            }
            let raw = urls::coerce(raw);
            if !matches!(urls::classify(&raw), urls::UrlKind::Album | urls::UrlKind::Track) {
                continue;
            }
            let normalised = urls::normalise(&raw);
            if !seen.insert(normalised.clone()) {
                continue;
            }
            candidates.push(normalised);
        }
        if let Some(l) = limit.filter(|l| *l > 0) {
            candidates.truncate(l);
        }
        let total = candidates.len() as i64;
        for (idx, url) in candidates.iter().enumerate() {
            let index = idx as i64 + 1;
            if depth == "shallow" {
                let kind = if url.contains("/track/") { "track" } else { "album" };
                yield HarvestEvent {
                    release: Some(shallow(url, Shallow { item_type: Some(kind), ..Default::default() })),
                    seen: index,
                    total: Some(total),
                    ..Default::default()
                };
                continue;
            }
            match client.get_html(url, GetOpts::kind(PageKind::Album)).await {
                Ok(body) => yield HarvestEvent {
                    release: Some(extract::parse_tralbum(&body, url)),
                    seen: index,
                    total: Some(total),
                    ..Default::default()
                },
                Err(e @ (HarvestError::IdentityExpired(_) | HarvestError::Cancelled)) => Err(e)?,
                Err(e) => yield HarvestEvent { error: Some(format!("{url}: {e}")), seen: index, total: Some(total), ..Default::default() },
            }
        }
    })
}

// -- artist / label discography ------------------------------------------------------------

/// `probe_artist`.
pub async fn probe_artist(client: &BandcampClient, url: &str) -> Result<SourceProbe> {
    let root = urls::artist_root(url);
    let body = client.get_html(&format!("{root}/music"), GetOpts::kind(PageKind::Music)).await?;
    let (items, tier) = extract::parse_music_grid(&body, &root);
    let is_label = extract::looks_like_label(&body);
    let mut params = BTreeMap::new();
    params.insert("url".into(), json!(root));
    Ok(SourceProbe {
        kind: if is_label { "label" } else { "artist" }.into(),
        label: urls::display_name(&root),
        total_hint: Some(items.len() as i64),
        auth_ok: true,
        detail: format!(
            "{} releases{}",
            items.len(),
            if tier == Tier::Css { " (grid fallback -- may be truncated)" } else { "" }
        ),
        params,
        ..Default::default()
    })
}

/// `harvest_artist`: full discography for an artist or label -- one HTTP call
/// regardless of catalogue size (the page carries the catalogue in two halves,
/// the rendered head and `data-client-items`; `parse_music_grid` unions them).
pub fn harvest_artist(client: BandcampClient, url: String, depth: String, limit: Option<usize>) -> EventStream {
    Box::pin(try_stream! {
        let root = urls::artist_root(&url);
        let body = client.get_html(&format!("{root}/music"), GetOpts::kind(PageKind::Music)).await?;
        let (mut items, tier) = extract::parse_music_grid(&body, &root);
        if tier == Tier::Css {
            tracing::warn!("{root}/music fell back to the rendered grid; may be truncated");
        }
        if let Some(l) = limit.filter(|l| *l > 0) {
            items.truncate(l);
        }
        let total = items.len() as i64;
        for (idx, item) in items.iter().enumerate() {
            let index = idx as i64 + 1;
            if depth == "shallow" {
                yield HarvestEvent {
                    release: Some(shallow(&item.page_url, Shallow {
                        title: &item.title,
                        artist: &item.artist,
                        item_type: Some(&item.item_type),
                        art_id: item.art_id,
                        art_url: item.art_url.clone(),
                        bc_item_id: item.bc_item_id,
                        band_id: item.band_id,
                        ..Default::default()
                    })),
                    seen: index,
                    total: Some(total),
                    cursor: Some(index.to_string()),
                    ..Default::default()
                };
                continue;
            }
            match client.get_html(&item.page_url, GetOpts::kind(PageKind::Album)).await {
                Ok(page) => yield HarvestEvent {
                    release: Some(extract::parse_tralbum(&page, &item.page_url)),
                    seen: index,
                    total: Some(total),
                    cursor: Some(index.to_string()),
                    ..Default::default()
                },
                Err(e @ (HarvestError::IdentityExpired(_) | HarvestError::Cancelled)) => Err(e)?,
                Err(e) => yield HarvestEvent { error: Some(format!("{}: {e}", item.page_url)), seen: index, total: Some(total), ..Default::default() },
            }
        }
    })
}

/// `BandPage`: everything the browse UI needs for one artist or label.
#[derive(Debug, Clone, Default)]
pub struct BandPage {
    pub profile: extract::BandProfile,
    pub releases: Vec<GridItem>,
    pub roster: Vec<RosterArtist>,
    pub tier: Tier,
}

/// `fetch_band_page`: profile plus discography. The `/music` page carries the band
/// blob, the rendered grid head and `data-client-items`, so an artist costs one
/// request; only a label pays for the second (`/artists`) fetch.
pub async fn fetch_band_page(client: &BandcampClient, url: &str, with_roster: bool) -> Result<BandPage> {
    let root = urls::artist_root(url);
    let body = client.get_html(&format!("{root}/music"), GetOpts::kind(PageKind::Music)).await?;
    let profile = extract::parse_band_profile(&body, &root);
    let (items, tier) = extract::parse_music_grid(&body, &root);
    let mut page = BandPage { profile, releases: items, roster: Vec::new(), tier };
    if with_roster && page.profile.is_label {
        // A label whose /artists page 404s is still a browsable label.
        match client
            .get_html(&format!("{root}/artists"), GetOpts::kind(PageKind::Music).ttl(std::time::Duration::from_secs(604_800)))
            .await
        {
            Ok(b) => page.roster = extract::parse_roster(&b, &root),
            Err(e) => tracing::debug!("roster fetch failed for {root}: {e}"),
        }
    }
    Ok(page)
}

/// `harvest_label_roster`.
pub fn harvest_label_roster(client: BandcampClient, url: String) -> EventStream {
    Box::pin(try_stream! {
        let root = urls::artist_root(&url);
        let body = client
            .get_html(&format!("{root}/artists"), GetOpts::kind(PageKind::Music).ttl(std::time::Duration::from_secs(604_800)))
            .await?;
        let roster = extract::parse_roster(&body, &root);
        let total = roster.len() as i64;
        for (idx, artist) in roster.into_iter().enumerate() {
            yield HarvestEvent { artist: Some(artist), seen: idx as i64 + 1, total: Some(total), ..Default::default() };
        }
    })
}

// -- search ------------------------------------------------------------------------------

pub const SEARCH_PATH: &str = "/api/bcsearch_public_api/1/autocomplete_elastic";
const SEARCH_ART_SIZE: u32 = 2;

/// `SearchHit`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchHit {
    /// artist | label | album | track | fan
    pub kind: String,
    pub name: String,
    pub url: String,
    pub subtitle: String,
    pub art_url: Option<String>,
    pub band_id: Option<i64>,
    pub item_id: Option<i64>,
    pub location: Option<String>,
}

/// Bandcamp's public autocomplete -- the only search surface it exposes.
/// `kind` is Bandcamp's own filter code: `""` all, `"b"` band/label, `"a"` album,
/// `"t"` track, `"f"` fan. Unauthenticated on purpose (sending the fan cookie would tie
/// every keystroke in the browse box to the account).
pub async fn search(client: &BandcampClient, query: &str, kind: &str, limit: usize) -> Result<Vec<SearchHit>> {
    let text = query.trim();
    if text.is_empty() {
        return Ok(vec![]);
    }
    let data = client
        .post_api(
            SEARCH_PATH,
            &json!({"search_text": text, "search_filter": kind, "full_page": false, "fan_id": null}),
            false,
            Some("https://bandcamp.com/"),
        )
        .await?;
    let results = data.get("auto").and_then(|a| a.get("results")).and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(results.iter().take(limit).filter_map(|e| e.as_object().and_then(|_| search_hit(e))).collect())
}

/// `_search_hit`.
pub fn search_hit(entry: &Value) -> Option<SearchHit> {
    // Bands carry their URL whole; albums/tracks carry root and path, but the path is itself
    // absolute on live data -- joining blindly yields a doubled, 404ing URL.
    let mut url = s(entry.get("url"));
    if url.is_empty() {
        let path = s(entry.get("item_url_path"));
        let root = s(entry.get("item_url_root"));
        url = if path.starts_with("http://") || path.starts_with("https://") {
            path
        } else if !root.is_empty() && !path.is_empty() {
            format!("{}/{}", root.trim_end_matches('/'), path.trim_start_matches('/'))
        } else if !root.is_empty() {
            root
        } else {
            path
        };
    }
    if url.is_empty() {
        return None;
    }
    let raw_type = {
        let t = s(entry.get("type"));
        if t.is_empty() { "b".to_string() } else { t }
    };
    let mut kind = match raw_type.as_str() {
        "b" => "artist",
        "a" => "album",
        "t" => "track",
        "f" => "fan",
        _ => "artist",
    }
    .to_string();
    if kind == "artist" && truthy(entry.get("is_label")) {
        kind = "label".into();
    }
    // Autocomplete's own `img` is unusable for releases (no `a` prefix): build from the ids.
    let location = Some(s(entry.get("location"))).filter(|l| !l.is_empty());
    let band_name = s(entry.get("band_name"));
    let art_url = urls::build_art_url_sized(i(entry.get("art_id")), SEARCH_ART_SIZE)
        .or_else(|| urls::build_band_image_url_sized(i(entry.get("img_id")), SEARCH_ART_SIZE))
        .or_else(|| Some(s(entry.get("img"))).filter(|x| !x.is_empty()));
    let subtitle = if matches!(kind.as_str(), "album" | "track") { band_name } else { location.clone().unwrap_or_default() };
    Some(SearchHit {
        kind,
        name: s(entry.get("name")),
        url: urls::normalise(&url),
        subtitle,
        art_url,
        band_id: i(entry.get("band_id")).filter(|v| *v != 0).or_else(|| if raw_type == "b" { i(entry.get("id")) } else { None }),
        item_id: if matches!(raw_type.as_str(), "a" | "t") { i(entry.get("id")) } else { None },
        location,
    })
}

// -- discover ----------------------------------------------------------------------------

pub const DISCOVER_PATH: &str = "/api/discover/1/discover_web";

/// Discover query parameters shared by `harvest_discover` and `discover_page`.
#[derive(Debug, Clone, Default)]
pub struct DiscoverQuery {
    pub tags: Vec<String>,
    pub genre: Option<String>,
    /// `new` | `top` | `rec` ...
    pub slice: String,
    pub category_id: i64,
    pub geoname_id: i64,
    pub time_facet_id: Option<i64>,
}

impl DiscoverQuery {
    pub fn new() -> Self {
        Self { slice: "new".into(), ..Default::default() }
    }
    fn tag_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tags.iter().filter(|t| !t.trim().is_empty()).map(|t| urls::norm_tag(t)).collect();
        if let Some(g) = self.genre.as_deref().filter(|g| !g.is_empty()) {
            names.insert(0, urls::norm_tag(g));
        }
        names
    }
    fn payload(&self, tag_names: &[String], size: usize, cursor: &str) -> Value {
        json!({
            // category_id is the only genuinely required field; omitting it returns
            // Endpoints::MissingParamError as HTTP 200.
            "category_id": self.category_id,
            "geoname_id": self.geoname_id,
            "slice": self.slice,
            "time_facet_id": self.time_facet_id,
            "tag_norm_names": tag_names,
            "include_result_types": ["a", "s"],
            "size": size,
            "cursor": cursor,
        })
    }
}

fn discover_release(entry: &Value, tags: Vec<String>) -> Option<HarvestedRelease> {
    let item_url = s(entry.get("item_url"));
    if item_url.is_empty() {
        return None;
    }
    let image = entry.get("primary_image");
    let art_id = image.and_then(|im| im.get("image_id")).and_then(|v| i(Some(v)));
    let artist = {
        let b = s(entry.get("band_name"));
        if b.is_empty() { s(entry.get("album_artist")) } else { b }
    };
    let item_type = if s(entry.get("result_type")) == "s" { "track" } else { "album" };
    Some(shallow(
        &item_url,
        Shallow {
            title: &s(entry.get("title")),
            artist: &artist,
            item_type: Some(item_type),
            art_id,
            bc_item_id: i(entry.get("item_id")),
            band_id: i(entry.get("band_id")),
            is_free: truthy(entry.get("is_free_download")),
            tags,
            ..Default::default()
        },
    ))
}

/// `harvest_discover`: browse the discover feed. Hard-limited on purpose (one tag filter
/// reported 434,991 results live; an unbounded run is the likeliest way to earn an IP block).
pub fn harvest_discover(client: BandcampClient, q: DiscoverQuery, limit: usize, page_size: usize) -> EventStream {
    Box::pin(try_stream! {
        let tag_names = q.tag_names();
        let mut cursor = "*".to_string();
        let mut seen: usize = 0;
        let mut total: Option<i64> = None;
        while seen < limit {
            let payload = q.payload(&tag_names, page_size.min(limit - seen), &cursor);
            let data = client.post_api(DISCOVER_PATH, &payload, false, Some("https://bandcamp.com/")).await?;
            let results = data.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
            if total.is_none() {
                total = i(data.get("result_count"));
            }
            if results.is_empty() {
                break;
            }
            for entry in &results {
                // item_url carries ?from=discover_page, stripped by normalise().
                // The API returns no per-item tags, but the query's own tags are why the
                // item is here at all -- stamping them lets the feed be filtered by them.
                let Some(release) = discover_release(entry, tag_names.clone()) else { continue };
                seen += 1;
                yield HarvestEvent {
                    release: Some(release),
                    seen: seen as i64,
                    total: Some(total.unwrap_or(limit as i64).min(limit as i64)),
                    cursor: Some(cursor.clone()),
                    ..Default::default()
                };
                if seen >= limit {
                    break;
                }
            }
            let next = s(data.get("cursor"));
            if next.is_empty() || next == cursor {
                break;
            }
            cursor = next;
        }
    })
}

/// `DiscoverPage`.
#[derive(Debug, Clone, Default)]
pub struct DiscoverPage {
    pub items: Vec<HarvestedRelease>,
    pub cursor: Option<String>,
    pub total: Option<i64>,
}

/// `discover_page`: one page of the feed, cursor in and cursor out (browsing wants a single
/// screenful, not the 500-item ceiling).
pub async fn discover_page(client: &BandcampClient, q: &DiscoverQuery, cursor: &str, size: usize) -> Result<DiscoverPage> {
    let tag_names = q.tag_names();
    let cursor = if cursor.is_empty() { "*" } else { cursor };
    let payload = q.payload(&tag_names, size.clamp(1, 60), cursor);
    let data = client.post_api(DISCOVER_PATH, &payload, false, Some("https://bandcamp.com/")).await?;
    let results = data.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
    let next = s(data.get("cursor"));
    Ok(DiscoverPage {
        total: i(data.get("result_count")),
        // A repeated cursor means the feed is exhausted.
        cursor: if !next.is_empty() && next != cursor { Some(next) } else { None },
        items: results.iter().filter_map(|e| discover_release(e, Vec::new())).collect(),
    })
}

// -- release / recommendations / collectors ------------------------------------------------

/// Stream URLs are signed with an expiry, so a release fetched for playback cannot ride the
/// 7-day album cache. Fifteen minutes keeps repeat visits cheap without serving a dead URL.
pub const TTL_PLAYABLE: std::time::Duration = std::time::Duration::from_secs(900);

fn playable() -> GetOpts {
    GetOpts::kind(PageKind::Stream).ttl(TTL_PLAYABLE)
}

/// `fetch_release`: a single release with its playable track URLs.
pub async fn fetch_release(client: &BandcampClient, url: &str) -> Result<HarvestedRelease> {
    let body = client.get_html(&urls::normalise(url), playable()).await?;
    Ok(extract::parse_tralbum(&body, url))
}

/// Same as [`fetch_release`] on the reserved token lane (stream pre-resolution).
pub async fn fetch_release_reserved(client: &BandcampClient, url: &str) -> Result<HarvestedRelease> {
    let body = client.get_html(&urls::normalise(url), playable().lane(crate::net::Lane::Reserved)).await?;
    Ok(extract::parse_tralbum(&body, url))
}

/// `fetch_recommendations`: "You may also like" for one release (same page and TTL as
/// [`fetch_release`], so a release view plus its related grids costs one request).
pub async fn fetch_recommendations(client: &BandcampClient, url: &str) -> Result<Vec<GridItem>> {
    let body = client.get_html(&urls::normalise(url), playable()).await?;
    Ok(extract::parse_recommendations(&body))
}

pub const COLLECTORS_THUMBS_PATH: &str = "/api/tralbumcollectors/2/thumbs";
pub const COLLECTORS_REVIEWS_PATH: &str = "/api/tralbumcollectors/2/reviews";
pub const COLLECTORS_PAGE: usize = 80;

/// `fetch_collectors`: who bought a release, and what they wrote about it. The page embeds
/// the first eighty buyers and every review (free with the page the player already fetched);
/// past that `tralbumcollectors/2/{thumbs,reviews}` pages on with the last buyer's token.
pub async fn fetch_collectors(client: &BandcampClient, url: &str, limit: usize) -> Result<(HarvestedRelease, Collectors)> {
    let canonical = urls::normalise(url);
    let body = client.get_html(&canonical, playable()).await?;
    let release = extract::parse_tralbum(&body, &canonical);
    let mut found = extract::parse_collectors(&body);

    let Some(tralbum_id) = release.bc_item_id else { return Ok((release, found)) };
    let tralbum_type = if release.item_type == "track" { "t" } else { "a" };

    async fn page_on(
        client: &BandcampClient,
        path: &str,
        tralbum_type: &str,
        tralbum_id: i64,
        mut rows: Vec<extract::Collector>,
        mut more: bool,
        cap: usize,
    ) -> Result<(Vec<extract::Collector>, bool)> {
        let mut seen: HashSet<String> = rows.iter().map(|c| c.username.clone()).collect();
        while more && !rows.is_empty() && rows.len() < cap {
            let Some(token) = rows.last().and_then(|c| c.token.clone()).filter(|t| !t.is_empty()) else { break };
            let data = client
                .post_api(
                    path,
                    &json!({
                        "tralbum_type": tralbum_type,
                        "tralbum_id": tralbum_id,
                        "token": token,
                        "count": COLLECTORS_PAGE.min(cap - rows.len()),
                    }),
                    true,
                    Some("https://bandcamp.com/"),
                )
                .await?;
            let rows_in = extract::collectors_from_results(data.get("results"));
            let fresh: Vec<_> = rows_in.into_iter().filter(|c| !seen.contains(&c.username)).collect();
            if fresh.is_empty() {
                break;
            }
            seen.extend(fresh.iter().map(|c| c.username.clone()));
            rows.extend(fresh);
            more = truthy(data.get("more_available"));
        }
        Ok((rows, more))
    }

    (found.thumbs, found.more_thumbs) =
        page_on(client, COLLECTORS_THUMBS_PATH, tralbum_type, tralbum_id, std::mem::take(&mut found.thumbs), found.more_thumbs, limit).await?;
    (found.reviews, found.more_reviews) =
        page_on(client, COLLECTORS_REVIEWS_PATH, tralbum_type, tralbum_id, std::mem::take(&mut found.reviews), found.more_reviews, limit).await?;
    Ok((release, found))
}

/// `fetch_discover_facets`: snapshot the real filter vocabulary so the UI offers Bandcamp's
/// own genres/subgenres rather than free text.
pub async fn fetch_discover_facets(client: &BandcampClient, genre: &str) -> Result<BTreeMap<String, Vec<Value>>> {
    let body = client
        .get_html(
            &format!("https://bandcamp.com/discover/{genre}"),
            GetOpts::kind(PageKind::Discover).ttl(std::time::Duration::from_secs(30 * 86_400)),
        )
        .await?;
    Ok(extract::parse_discover_facets(&body))
}

// -- fans ---------------------------------------------------------------------------------

pub const COLLECTION_PATH: &str = "/api/fancollection/1/collection_items";
pub const WISHLIST_PATH: &str = "/api/fancollection/1/wishlist_items";
pub const HIDDEN_PATH: &str = "/api/fancollection/1/hidden_items";
pub const SUMMARY_PATH: &str = "/api/fan/2/collection_summary";
pub const PEEK_PAGE: usize = 60;

fn list_path(which: &str) -> &'static str {
    match which {
        "wishlist" => WISHLIST_PATH,
        "hidden" => HIDDEN_PATH,
        _ => COLLECTION_PATH,
    }
}

fn fan_page_opts() -> GetOpts {
    GetOpts::kind(PageKind::Fan).authed(true)
}

/// `_list_page`: one page of a fan list, with a private list told apart from a fault
/// (Bandcamp answers a list it will not show with a bare `{"error": true}`).
async fn list_page(client: &BandcampClient, which: &str, payload: &Value) -> Result<Value> {
    match client.post_api(list_path(which), payload, true, Some("https://bandcamp.com/")).await {
        Err(HarvestError::Api { unspecified: true, url, .. }) => {
            Err(HarvestError::ListUnavailable(format!("Bandcamp does not show this fan's {which} ({url})")))
        }
        other => other,
    }
}

/// `newest_token`: `older_than_token` for the *newest* page, built from *now*
/// (`<purchase_ts>:<tralbum_id>:<type>:<index>:`). The fan page's own `last_token` points at
/// the oldest item, so passing it returns zero rows.
pub fn newest_token() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{now}::a::")
}

/// `whoami`: validate the stored cookie. `IdentityExpired` if invalid.
pub async fn whoami(client: &BandcampClient) -> Result<Whoami> {
    if !client.has_cookie() {
        return Err(HarvestError::IdentityExpired("no identity cookie configured".into()));
    }
    let data = client.get_api(SUMMARY_PATH, &[], true).await?;
    let summary = data.get("collection_summary").cloned().unwrap_or(Value::Null);
    Ok(Whoami {
        fan_id: i(data.get("fan_id")).filter(|v| *v != 0).or_else(|| i(summary.get("fan_id"))),
        username: Some(s(summary.get("username"))).filter(|x| !x.is_empty()),
        url: Some(s(summary.get("url"))).filter(|x| !x.is_empty()),
    })
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Whoami {
    pub fan_id: Option<i64>,
    pub username: Option<String>,
    pub url: Option<String>,
}

/// `probe_fan`.
pub async fn probe_fan(client: &BandcampClient, fan_url: &str) -> Result<SourceProbe> {
    // The tab lives in the URL, but only the base page is fetchable.
    let body = client.get_html(&urls::fan_base_url(fan_url), fan_page_opts().ttl(std::time::Duration::from_secs(300))).await?;
    let page = extract::parse_fan_page(&body)?;
    let tab = urls::fan_tab(fan_url);
    let count = match tab {
        "wishlist" => page.wishlist_count,
        "hidden" => page.hidden_count,
        _ => page.collection_count,
    };
    let mut params = BTreeMap::new();
    params.insert("fan_id".into(), json!(page.fan_id));
    params.insert("username".into(), json!(page.username));
    params.insert("tab".into(), json!(tab));
    params.insert("collection_count".into(), json!(page.collection_count));
    params.insert("wishlist_count".into(), json!(page.wishlist_count));
    Ok(SourceProbe {
        kind: tab.to_string(),
        label: if page.username.is_empty() { urls::display_name(fan_url) } else { format!("{}-{tab}", page.username) },
        total_hint: Some(count),
        // Public fan pages need no cookie; the hidden tab always does.
        requires_auth: tab == "hidden",
        auth_ok: true,
        detail: format!("{count} in {tab} — {} collection, {} wishlist", page.collection_count, page.wishlist_count),
        params,
    })
}

/// `_release_from_collection_item`.
pub fn release_from_collection_item(entry: &Value) -> Option<HarvestedRelease> {
    let mut item_url = s(entry.get("item_url"));
    if item_url.is_empty() {
        if let Some(hints) = entry.get("url_hints").filter(|h| h.is_object()) {
            let t = {
                let t = s(entry.get("tralbum_type"));
                if t.is_empty() { "a".to_string() } else { t }
            };
            item_url = urls::item_key_to_url(hints, &t).unwrap_or_default();
        }
    }
    if item_url.is_empty() {
        return None;
    }
    let title = {
        let t = s(entry.get("item_title"));
        if t.is_empty() { s(entry.get("album_title")) } else { t }
    };
    let mut release = shallow(
        &item_url,
        Shallow {
            title: &title,
            artist: &s(entry.get("band_name")),
            item_type: Some(if s(entry.get("tralbum_type")) == "t" { "track" } else { "album" }),
            art_id: i(entry.get("item_art_id")),
            art_url: Some(s(entry.get("item_art_url"))).filter(|x| !x.is_empty()),
            bc_item_id: i(entry.get("tralbum_id")).filter(|v| *v != 0).or_else(|| i(entry.get("item_id"))),
            band_id: i(entry.get("band_id")),
            ..Default::default()
        },
    );
    release.label_name = Some(s(entry.get("label"))).filter(|x| !x.is_empty());
    release.is_preorder = truthy(entry.get("is_preorder"));
    release.is_purchasable = entry.get("is_purchasable").map(|v| truthy(Some(v))).unwrap_or(true);
    Some(release)
}

fn releases_of(entries: &[Value]) -> Vec<HarvestedRelease> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for e in entries {
        let Some(r) = e.as_object().and_then(|_| release_from_collection_item(e)) else { continue };
        if seen.insert(r.url.clone()) {
            out.push(r);
        }
    }
    out
}

/// `peek_fan`: a fan at a glance from their page alone (one request, nothing written).
pub async fn peek_fan(client: &BandcampClient, fan_url: &str) -> Result<(FanPage, Vec<HarvestedRelease>, Vec<HarvestedRelease>)> {
    let body = client.get_html(&urls::fan_base_url(fan_url), fan_page_opts().ttl(std::time::Duration::from_secs(300))).await?;
    let page = extract::parse_fan_page(&body)?;
    let coll = releases_of(&page.cached_items("collection"));
    let wish = releases_of(&page.cached_items("wishlist"));
    Ok((page, coll, wish))
}

/// `harvest_collection`: walk a fan's collection or wishlist, newest first. Public
/// collections work without a cookie; a private collection and the hidden list need one.
pub fn harvest_collection(
    client: BandcampClient,
    fan_id: Option<i64>,
    fan_url: Option<String>,
    which: String,
    limit: usize,
    page_size: usize,
    since_token: Option<String>,
) -> EventStream {
    Box::pin(try_stream! {
        let mut cached: Vec<Value> = Vec::new();
        let mut total: Option<i64> = None;
        let mut fan_id = fan_id;

        if fan_id.is_none() {
            let Some(fan_url) = fan_url.as_deref() else {
                Err(HarvestError::other("either fan_id or fan_url is required"))?
            };
            let body = client.get_html(&urls::fan_base_url(fan_url), fan_page_opts().ttl(std::time::Duration::from_secs(300))).await?;
            let page = extract::parse_fan_page(&body)?;
            fan_id = Some(page.fan_id);
            // item_cache is keyed by tab: select the right sub-dict.
            cached = page.cached_items(&which);
            total = Some(match which.as_str() {
                "wishlist" => page.wishlist_count,
                "hidden" => page.hidden_count,
                _ => page.collection_count,
            });
            // The page says the list is empty and carried no items: nothing to page through
            // (asking anyway is what turns a private wishlist into a failed walk).
            if total == Some(0) && cached.is_empty() {
                return;
            }
        }

        let mut seen: usize = 0;
        let mut emitted: HashSet<String> = HashSet::new();

        // The fan page blob already carries the newest items: use them before spending a request.
        for entry in cached.iter().take(page_size) {
            if seen >= limit {
                break;
            }
            let Some(release) = entry.as_object().and_then(|_| release_from_collection_item(entry)) else { continue };
            if !emitted.insert(release.url.clone()) {
                continue;
            }
            seen += 1;
            yield HarvestEvent { release: Some(release), seen: seen as i64, total, ..Default::default() };
        }

        let mut token = since_token.unwrap_or_else(newest_token);
        while seen < limit {
            let data = list_page(
                &client,
                &which,
                &json!({"fan_id": fan_id, "older_than_token": token, "count": page_size.min(limit - seen)}),
            )
            .await?;
            let items = data.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            if items.is_empty() {
                break;
            }
            for entry in &items {
                let Some(release) = entry.as_object().and_then(|_| release_from_collection_item(entry)) else { continue };
                if !emitted.insert(release.url.clone()) {
                    continue;
                }
                seen += 1;
                yield HarvestEvent { release: Some(release), seen: seen as i64, total, cursor: Some(token.clone()), ..Default::default() };
                if seen >= limit {
                    break;
                }
            }
            let next = s(data.get("last_token"));
            if next.is_empty() || next == token || !data.get("more_available").map(|v| truthy(Some(v))).unwrap_or(true) {
                break;
            }
            token = next;
        }
    })
}

/// One page of a fan's list for a peek.
#[derive(Debug, Clone, Default)]
pub struct PeekPage {
    pub rows: Vec<HarvestedRelease>,
    /// Cursor for the page behind these rows.
    pub cursor: Option<String>,
    pub more: bool,
    /// The list's total, when this call happened to read the fan page.
    pub total: Option<i64>,
}

/// `peek_fan_items`: one page of a fan's collection/wishlist, newest first; nothing written.
/// The first page (no cursor) is free (the embedded batch); every later page costs one API
/// call. `count` sizes the API pages only (the embedded batch comes back whole).
pub async fn peek_fan_items(
    client: &BandcampClient,
    fan_url: &str,
    which: &str,
    fan_id: Option<i64>,
    cursor: Option<&str>,
    count: usize,
) -> Result<PeekPage> {
    let mut total: Option<i64> = None;
    let mut page: Option<FanPage> = None;
    let mut fan_id = fan_id;
    if cursor.is_none() || fan_id.is_none() {
        let body = client.get_html(&urls::fan_base_url(fan_url), fan_page_opts().ttl(std::time::Duration::from_secs(300))).await?;
        let p = extract::parse_fan_page(&body)?;
        fan_id = fan_id.or(Some(p.fan_id)).filter(|v| *v != 0).or(Some(p.fan_id));
        total = Some(if which == "collection" { p.collection_count } else { p.wishlist_count });
        page = Some(p);
    }
    if cursor.is_none() {
        let p = page.as_ref().expect("page fetched when cursor is None");
        let rows = releases_of(&p.cached_items(which));
        // The tab's own `last_token` is the cursor for the record after its embedded batch.
        let next_cursor = p.last_tokens.get(which).cloned().unwrap_or_else(newest_token);
        let more = (rows.len() as i64) < total.unwrap_or(0);
        return Ok(PeekPage { rows, cursor: Some(next_cursor), more, total });
    }
    let cursor = cursor.unwrap_or_default();
    let data = list_page(client, which, &json!({"fan_id": fan_id, "older_than_token": cursor, "count": count})).await?;
    let items: Vec<Value> = data.get("items").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter(Value::is_object).collect();
    let rows = releases_of(&items);
    let behind = Some(s(data.get("last_token"))).filter(|t| !t.is_empty());
    let more = !items.is_empty()
        && behind.as_deref().is_some_and(|b| b != cursor)
        && data.get("more_available").map(|v| truthy(Some(v))).unwrap_or(true);
    Ok(PeekPage { rows, cursor: behind, more, total })
}

/// `resolve_album_url`: the album a URL belongs to, falling back to the URL itself.
/// An album URL is returned unchanged and costs no request; a standalone single resolves to itself.
pub async fn resolve_album_url(client: &BandcampClient, url: &str) -> Result<String> {
    let canonical = urls::normalise(url);
    if urls::classify(&canonical) != urls::UrlKind::Track {
        return Ok(canonical);
    }
    let body = client.get_html(&canonical, playable()).await?;
    Ok(extract::parse_track_album(&body, &canonical).unwrap_or(canonical))
}
