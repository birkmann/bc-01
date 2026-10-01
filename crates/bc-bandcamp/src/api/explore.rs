//! Browsing Bandcamp itself: search, tags, artists, labels, releases (legacy
//! `api/routes/explore.py`).
//!
//! The distinction from `harvest` is intent, not plumbing. Harvest answers "sweep this whole
//! source into my inbox"; explore answers "let me look around, listen, and take what I like".
//! Both ride the same rate-limited client. Nothing here writes to the library: the only mutating
//! routes hand URLs to the download queue, which stays the single path by which audio reaches
//! disk.
//!
//! Routes: `/explore/{search,genres,discover,collectors,band,release,related,stream}`,
//! `POST /explore/download`, `POST /explore/download/catalog`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Response, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use bc_db::rusqlite::params_from_iter;
use bc_jobs::ApiError;
use bc_types::bandcamp::{
    BandOut, CollectorOut, CollectorsOut, DiscoverOut, ExploreReleaseOut, RelatedOut, RelatedSectionOut, RosterArtistOut,
    SearchHitOut,
};
use futures::future::join_all;

use crate::download::dedup::url_key;
use crate::extract::{Collector, Tier};
use crate::service::Ctx;
use crate::sources::{self, DiscoverQuery};
use crate::stream::StreamService;
use crate::urls;

pub mod common;
pub mod downloads;

use common::{Params, bandcamp_url, blacklisted, card, grid_card, known_releases, library_release_id, owned, unprocessable};

/// Construct the stream service, register it on the context and (inside a runtime) start its
/// background refresher.
pub fn init(ctx: &Arc<Ctx>) {
    let svc = crate::stream::register(ctx);
    if tokio::runtime::Handle::try_current().is_ok() {
        svc.start_refresher();
    }
}

/// Start the background stream refresher (idempotent). Call from `BandcampService::start`.
pub async fn start(ctx: &Arc<Ctx>) {
    ctx.expect::<StreamService>().start_refresher();
}

pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/explore/search", get(search))
        .route("/explore/genres", get(genres))
        .route("/explore/discover", get(discover))
        .route("/explore/collectors", get(collectors))
        .route("/explore/band", get(band))
        .route("/explore/release", get(release))
        .route("/explore/related", get(related))
        .route("/explore/stream", get(stream))
        .route("/explore/download", post(downloads::download_releases))
        .route("/explore/download/catalog", post(downloads::download_catalog))
        .with_state(ctx)
}

type ApiResult<T> = Result<Json<T>, ApiError>;

// -- search --------------------------------------------------------------------------------

async fn search(State(ctx): State<Arc<Ctx>>, p: Params) -> ApiResult<Vec<SearchHitOut>> {
    let q = p.required("q")?;
    let n = q.chars().count();
    if !(1..=200).contains(&n) {
        return Err(unprocessable("q: must be 1 to 200 characters"));
    }
    let filter = match p.string("kind", "all").as_str() {
        "all" => "",
        "artist" => "b",
        "album" => "a",
        "track" => "t",
        "fan" => "f",
        other => return Err(unprocessable(format!("kind: unexpected value {other}"))),
    };
    let limit: usize = p.num("limit", 40, 1, 100)?;

    let hits = sources::search(&ctx.client, &q, filter, limit).await?;

    // One batched lookup for the releases among the hits. A hit's subtitle is the artist for
    // exactly those kinds, which is what turns on the name fallback for records held without a
    // Bandcamp URL.
    let releases: Vec<_> =
        hits.iter().filter(|h| matches!(h.kind.as_str(), "album" | "track")).map(|h| (h.url.clone(), h.subtitle.clone(), h.name.clone())).collect();
    let known = known_releases(&ctx.db, &releases).await?;

    Ok(Json(
        hits.into_iter()
            .map(|h| {
                let release_kind = matches!(h.kind.as_str(), "album" | "track");
                SearchHitOut {
                    in_library: release_kind && owned(&known, &h.url),
                    blacklisted: blacklisted(&known, &h.url),
                    library_release_id: if h.kind == "album" { library_release_id(&known, &h.url) } else { None },
                    kind: h.kind,
                    name: h.name,
                    url: h.url,
                    subtitle: h.subtitle,
                    art_url: h.art_url,
                    band_id: h.band_id,
                    item_id: h.item_id,
                }
            })
            .collect(),
    ))
}

/// Bandcamp's own genre and filter vocabulary: the discover page's lists, so the UI offers real
/// tags (free-text tags mostly return nothing).
async fn genres(State(ctx): State<Arc<Ctx>>, p: Params) -> ApiResult<BTreeMap<String, Vec<serde_json::Value>>> {
    Ok(Json(sources::fetch_discover_facets(&ctx.client, &p.string("genre", "electronic")).await?))
}

async fn discover(State(ctx): State<Arc<Ctx>>, p: Params) -> ApiResult<DiscoverOut> {
    let mut q = DiscoverQuery::new();
    q.genre = p.get("genre").filter(|g| !g.is_empty()).map(str::to_string);
    q.tags = p.all("tags");
    q.slice = p.string("slice", "new");
    q.category_id = p.num("category_id", 0, i64::MIN, i64::MAX)?;
    q.geoname_id = p.num("geoname_id", 0, i64::MIN, i64::MAX)?;
    q.time_facet_id = p.opt_i64("time_facet_id")?;
    let cursor = p.string("cursor", "*");
    let size: usize = p.num("size", 48, 1, 60)?;

    let page = sources::discover_page(&ctx.client, &q, &cursor, size).await?;
    let items: Vec<_> = page.items.iter().map(|r| (r.url.clone(), r.artist_name.clone(), r.title.clone())).collect();
    let known = known_releases(&ctx.db, &items).await?;
    Ok(Json(DiscoverOut { items: page.items.iter().map(|r| card(r, &known)).collect(), cursor: page.cursor, total: page.total }))
}

// -- collectors ----------------------------------------------------------------------------

/// The "supported by" section of a release page: buyers to peek at and follow, and the reviews
/// with their favourite tracks. Up to eighty comes off the page itself (the same fetch and cache
/// the player uses); beyond that the buyers are paged in eighties through Bandcamp's API, one
/// polite request each, which is why a bigger `limit` is an explicit ask.
async fn collectors(State(ctx): State<Arc<Ctx>>, p: Params) -> ApiResult<CollectorsOut> {
    let target = bandcamp_url(&p.required("url")?, "url")?;
    let limit: usize = p.num("limit", 80, 1, 1000)?;
    let (_release, found) = sources::fetch_collectors(&ctx.client, &target, limit).await?;

    // Already followed here? Match on Bandcamp's numeric id where we have it, else on the
    // username -- the two ways a fan row names an account.
    let all: Vec<&Collector> = found.thumbs.iter().chain(found.reviews.iter()).collect();
    let names: Vec<String> = {
        let mut n: Vec<String> = all.iter().map(|c| c.username.to_lowercase()).collect::<HashSet<_>>().into_iter().collect();
        n.sort();
        n
    };
    let ids: Vec<i64> = {
        let mut i: Vec<i64> = all.iter().filter_map(|c| c.fan_id).collect::<HashSet<_>>().into_iter().collect();
        i.sort();
        i
    };
    let mut followed: HashMap<String, i64> = HashMap::new();
    if !names.is_empty() || !ids.is_empty() {
        let rows: Vec<(i64, String, Option<i64>)> = ctx
            .db
            .read_async(move |c| {
                let ph = |n: usize| vec!["?"; n.max(1)].join(",");
                let sql = format!(
                    "SELECT id, username, bc_fan_id FROM fans WHERE lower(username) IN ({}) OR bc_fan_id IN ({})",
                    ph(names.len()),
                    ph(ids.len())
                );
                let mut st = c.prepare(&sql)?;
                let mut args: Vec<bc_db::rusqlite::types::Value> = names.iter().map(|n| n.clone().into()).collect();
                if names.is_empty() {
                    args.push("".to_string().into());
                }
                args.extend(ids.iter().map(|i| (*i).into()));
                if ids.is_empty() {
                    args.push(bc_db::rusqlite::types::Value::Null);
                }
                let rows = st.query_map(params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
            .await?;
        for (id, username, bc) in rows {
            followed.insert(username.to_lowercase(), id);
            if let Some(b) = bc {
                followed.insert(format!("#{b}"), id);
            }
        }
    }

    let out = |c: &Collector| CollectorOut {
        username: c.username.clone(),
        name: c.name.clone(),
        url: c.url(),
        bc_fan_id: c.fan_id,
        image_url: urls::build_fan_image_url(c.image_id),
        why: c.why.clone(),
        fav_track: c.fav_track.clone(),
        followed_id: followed
            .get(&c.username.to_lowercase())
            .or_else(|| c.fan_id.and_then(|f| followed.get(&format!("#{f}"))))
            .copied(),
    };

    // Reviewers are buyers too; one row each, reviews first so what people wrote is not buried
    // under the avatars.
    let mut seen: HashSet<&str> = HashSet::new();
    let mut supporters = Vec::new();
    for c in found.reviews.iter().chain(found.thumbs.iter()) {
        if seen.insert(c.username.as_str()) {
            supporters.push(out(c));
        }
    }
    Ok(Json(CollectorsOut { url: target, supporters, reviews: found.reviews.iter().map(out).collect(), more: found.more_thumbs }))
}

// -- band / release ------------------------------------------------------------------------

async fn band(State(ctx): State<Arc<Ctx>>, p: Params) -> ApiResult<BandOut> {
    let target = bandcamp_url(&p.required("url")?, "url")?;
    let page = sources::fetch_band_page(&ctx.client, &target, true).await?;
    let profile = &page.profile;
    // The grid omits the artist on a single-artist page, so the band's own name stands in --
    // the same fallback the cards are rendered with.
    let items: Vec<_> = page
        .releases
        .iter()
        .map(|i| (i.page_url.clone(), if i.artist.is_empty() { profile.name.clone() } else { i.artist.clone() }, i.title.clone()))
        .collect();
    let known = known_releases(&ctx.db, &items).await?;
    Ok(Json(BandOut {
        url: profile.url.clone(),
        name: profile.name.clone(),
        kind: if profile.is_label { "label" } else { "artist" }.into(),
        location: profile.location.clone(),
        bio: profile.bio.clone(),
        image_url: profile.image_url.clone(),
        links: profile.links.clone(),
        releases: page.releases.iter().map(|i| grid_card(i, &known, &profile.name)).collect(),
        roster: page.roster.iter().map(|a| RosterArtistOut { name: a.name.clone(), url: a.url.clone(), location: a.location.clone() }).collect(),
        truncated: page.tier == Tier::Css,
    }))
}

/// A release with its streams. Playing a feed item is the one moment its tags become known for
/// free, so they are written back to the inbox row; the parsed tralbum stays in the stream cache.
async fn release(State(ctx): State<Arc<Ctx>>, p: Params) -> ApiResult<ExploreReleaseOut> {
    let target = bandcamp_url(&p.required("url")?, "url")?;
    Ok(Json(ctx.expect::<StreamService>().release_tracks(&target).await?))
}

// -- related -------------------------------------------------------------------------------

/// Everything adjacent to one release, as grids you can scan at a glance: Bandcamp's own "you
/// may also like" strip, the rest of the artist's or label's catalogue, then one feed per tag.
/// Each is fetched independently and a failure drops only its own section -- a rate-limited tag
/// feed must not blank the page.
///
/// `include_band=false` for callers that render the discography themselves.
async fn related(State(ctx): State<Arc<Ctx>>, p: Params) -> ApiResult<RelatedOut> {
    let target = bandcamp_url(&p.required("url")?, "url")?;
    let tags = p.all("tags");
    let size: usize = p.num("size", 48, 6, 60)?;
    let tag_limit: usize = p.num("tag_limit", 3, 0, 6)?;
    let slice = p.string("slice", "top");
    let include_band = p.boolean("include_band", true)?;

    let found = sources::fetch_release(&ctx.client, &target).await?;
    let band_root = urls::artist_root(&found.url);
    // The release's own tags are the default, but the UI can name others: a record tagged
    // "berlin" and "dub techno" is far better explored through the second than the first.
    let feed_tags: Vec<String> =
        (if tags.is_empty() { found.tags.clone() } else { tags }).into_iter().filter(|t| !t.trim().is_empty()).take(tag_limit).collect();

    // Gathered rather than awaited in turn: the client's own token bucket is what paces
    // Bandcamp, so serialising here would only add latency.
    let client = &ctx.client;
    let recs = async { sources::fetch_recommendations(client, &target).await };
    let catalogue = async {
        if include_band { Some(sources::fetch_band_page(client, &band_root, false).await) } else { None }
    };
    let feeds = join_all(feed_tags.iter().map(|tag| {
        let slice = slice.clone();
        async move {
            let mut q = DiscoverQuery::new();
            q.tags = vec![tag.clone()];
            q.slice = slice;
            sources::discover_page(client, &q, "*", size).await
        }
    }));
    let (rec_res, band_res, feed_res) = tokio::join!(recs, catalogue, feeds);

    let rec_items = rec_res.unwrap_or_else(|e| {
        tracing::info!("related: recommendations failed for {target}: {e}");
        Vec::new()
    });
    let band_page = band_res.and_then(|r| {
        r.map_err(|e| tracing::info!("related: catalogue failed for {target}: {e}")).ok()
    });
    let feeds: Vec<Option<sources::DiscoverPage>> = feed_res
        .into_iter()
        .zip(&feed_tags)
        .map(|(r, t)| r.map_err(|e| tracing::info!("related: tag {t} failed for {target}: {e}")).ok())
        .collect();

    // The release you are looking at is not a neighbour of itself, and a record that shows up in
    // three feeds should still be scanned once.
    let mut seen: HashSet<String> = HashSet::from([url_key(&found.url)]);
    let mut take = |items: Vec<(String, bc_types::bandcamp::ReleaseCardOut)>, limit: usize| {
        let mut out = Vec::new();
        for (key, c) in items {
            if !seen.insert(key) {
                continue;
            }
            out.push(c);
            if out.len() >= limit {
                break;
            }
        }
        out
    };

    let mut sections: Vec<RelatedSectionOut> = Vec::new();
    let none = HashMap::new();
    if !rec_items.is_empty() {
        let cards = rec_items.iter().map(|i| (url_key(&i.page_url), grid_card(i, &none, &found.artist_name))).collect();
        sections.push(RelatedSectionOut {
            key: "recommended".into(),
            title: "You may also like".into(),
            source: "recommended".into(),
            items: take(cards, size),
            ..Default::default()
        });
    }
    if let Some(bp) = band_page.filter(|b| !b.releases.is_empty()) {
        let name = if bp.profile.name.is_empty() { urls::display_name(&band_root) } else { bp.profile.name.clone() };
        let cards = bp.releases.iter().map(|i| (url_key(&i.page_url), grid_card(i, &none, &name))).collect();
        sections.push(RelatedSectionOut {
            key: "band".into(),
            title: format!("More from {name}"),
            source: "band".into(),
            items: take(cards, size),
            url: Some(band_root.clone()),
            total: Some(bp.releases.len() as i64),
            ..Default::default()
        });
    }
    for (tag, page) in feed_tags.iter().zip(feeds) {
        let Some(page) = page.filter(|p| !p.items.is_empty()) else { continue };
        let cards = page.items.iter().map(|r| (url_key(&r.url), card(r, &none))).collect();
        sections.push(RelatedSectionOut {
            key: format!("tag:{}", urls::norm_tag(tag)),
            title: tag.clone(),
            source: "tag".into(),
            tag: Some(urls::norm_tag(tag)),
            items: take(cards, size),
            cursor: page.cursor,
            total: page.total,
            ..Default::default()
        });
    }

    // One library lookup for the whole page rather than one per section.
    let all: Vec<_> = sections.iter().flat_map(|s| s.items.iter().map(|i| (i.url.clone(), i.artist_name.clone(), i.title.clone()))).collect();
    let known = known_releases(&ctx.db, &all).await?;
    for section in &mut sections {
        for item in &mut section.items {
            item.in_library = owned(&known, &item.url);
            item.blacklisted = blacklisted(&known, &item.url);
            item.library_release_id = library_release_id(&known, &item.url);
        }
    }
    sections.retain(|s| !s.items.is_empty());
    Ok(Json(RelatedOut { sections }))
}

// -- stream proxy --------------------------------------------------------------------------

async fn stream(State(ctx): State<Arc<Ctx>>, headers: HeaderMap, p: Params) -> Result<Response<Body>, ApiError> {
    let release_url = bandcamp_url(&p.required("release")?, "release")?;
    let track = p.required("track")?;
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    ctx.expect::<StreamService>().proxy(&release_url, &track, range).await
}
