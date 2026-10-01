//! `GET /library/home`: the Home page's shelves in one snapshot. Random shelves are drawn with the
//! same seeded-hash shuffle as everything else, so one `seed` is one stable snapshot.

use bc_db::rusqlite::Connection;
use bc_db::util::{iso_now, db_from_unix};
use bc_libcore::{ApiResult, Scope};
use bc_types::library::*;

use crate::{artists, favorites, history, releases, stats, tags, tracks};

fn days_ago_iso(days: i64) -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0) - days * 86_400;
    db_from_unix(secs).replacen(' ', "T", 1)
}

pub fn home(c: &Connection, seed: Option<i64>, scope: &Scope) -> ApiResult<HomeShelves> {
    let seed = seed.filter(|s| *s > 0).unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| (d.as_secs() % 1_000_000) as i64 + 1).unwrap_or(1)
    });
    let rq = |sort: ReleaseSort, limit: i64| ReleaseQuery { sort: Some(sort), limit: Some(limit), seed: Some(seed), ..Default::default() };
    let new_in_library = releases::list_releases(c, &ReleaseQuery { order: Some(SortDir::Desc), ..rq(ReleaseSort::Added, 12) }, scope)?.items;
    let top_tags = tags::list_tags(c, &TagsQuery { limit: Some(12), ..Default::default() }, scope)?;
    // The crate: random records of one of the top tags, rotating with the seed.
    let crate_tags = if top_tags.is_empty() { vec![] } else { vec![top_tags[(seed as usize) % top_tags.len()].name.clone()] };
    let crate_dig = releases::list_releases(c, &ReleaseQuery { tags: crate_tags, ..rq(ReleaseSort::Random, 12) }, scope)?.items;
    let dust_off = releases::list_releases(c, &rq(ReleaseSort::Random, 12), scope)?.items;
    let recently_played = history::recent(c, 12, scope)?;
    let top_artists = artists::list_artists(c, &ArtistQuery { sort: Some(ArtistSort::Plays), limit: Some(8), ..Default::default() }, scope)?.items;
    let tq = |q: TrackQuery| TrackQuery { sort: Some(TrackSort::Random), seed: Some(seed), limit: Some(8), ..q };
    let rediscover = tracks::list_tracks(
        c,
        &tq(TrackQuery { loved: Some(true), played: Some(true), last_played_before: Some(days_ago_iso(90)), ..Default::default() }),
        scope,
    )?
    .page
    .items;
    let mut buried = tracks::list_tracks(c, &tq(TrackQuery { played: Some(false), added_before: Some(days_ago_iso(30)), ..Default::default() }), scope)?.page.items;
    if buried.is_empty() {
        buried = tracks::list_tracks(c, &tq(TrackQuery { played: Some(false), ..Default::default() }), scope)?.page.items;
    }
    let top_ten = tracks::list_tracks(
        c,
        &TrackQuery { sort: Some(TrackSort::PlayCount), order: Some(SortDir::Desc), limit: Some(10), played: Some(true), ..Default::default() },
        scope,
    )?
    .page
    .items;
    Ok(HomeShelves {
        seed,
        generated_at: iso_now(),
        stats: stats::library_stats(c, scope)?,
        new_in_library,
        crate_dig,
        top_tags,
        recently_played,
        top_artists,
        rediscover,
        dust_off,
        buried_treasure: buried,
        top_ten,
        favorites: favorites::list(c, scope)?,
    })
}
