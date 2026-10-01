//! Timing / plan checks against a real imported database. Ignored by default:
//!   BC_TEST_DB=/dev/shm/ws1-data/library.db cargo test -p bc-library --test real_db -- --ignored --nocapture

use std::sync::Arc;
use std::time::Instant;

use bc_db::Db;
use bc_libcore::{Ctx, Scope};
use bc_library::{artists, home, labels, releases, stats, tags, tracks};
use bc_types::library::*;

fn ctx() -> Option<Ctx> {
    let p = std::env::var("BC_TEST_DB").ok()?;
    // work on a copy-on-open: Db::open migrates in place, so point at a scratch copy.
    let db = Db::open(&p).ok()?;
    let cfg = bc_core::Config::from_env();
    Some(Ctx::new(db, Arc::new(bc_core::EventBus::new()), cfg))
}

fn time<T>(label: &str, n: usize, mut f: impl FnMut() -> T) -> f64 {
    let _ = f();
    let t = Instant::now();
    for _ in 0..n {
        let _ = f();
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
    println!("{label:<48} {ms:>9.2} ms");
    ms
}

#[test]
#[ignore]
fn timings_on_real_db() {
    let Some(ctx) = ctx() else { return };
    ctx.read(|c| {
        let scope = Scope::resolve(c, None, None).unwrap();
        println!("scope: {scope:?}");
        let tq = |f: &dyn Fn(&mut TrackQuery)| {
            let mut q = TrackQuery::default();
            f(&mut q);
            q
        };
        time("tracks default offset=0 limit=100", 5, || tracks::list_tracks(c, &tq(&|_| {}), &scope).unwrap());
        time("tracks offset=74000", 5, || tracks::list_tracks(c, &tq(&|q| q.offset = Some(74000)), &scope).unwrap());
        time("tracks offset=185000", 5, || tracks::list_tracks(c, &tq(&|q| q.offset = Some(185000)), &scope).unwrap());
        for (name, sort) in [("title", TrackSort::Title), ("artist", TrackSort::Artist), ("album", TrackSort::Album), ("duration", TrackSort::Duration), ("bpm", TrackSort::Bpm), ("play_count", TrackSort::PlayCount), ("year", TrackSort::Year), ("random", TrackSort::Random)] {
            time(&format!("tracks sort={name} offset=74000"), 3, || tracks::list_tracks(c, &tq(&|q| { q.sort = Some(sort); q.offset = Some(74000); q.seed = Some(7) }), &scope).unwrap());
        }
        time("tracks q=dub techno (relevance)", 5, || tracks::list_tracks(c, &tq(&|q| q.q = Some("dub techno".into())), &scope).unwrap());
        for qq in ["d", "th", "the", "dub", "kiss", "dub techno", "boards of can"] {
            time(&format!("search q={qq}"), 5, || tracks::list_tracks(c, &tq(&|q| q.q = Some(qq.into())), &scope).unwrap());
        }
        time("tracks q=d (broad prefix)", 3, || tracks::list_tracks(c, &tq(&|q| q.q = Some("d".into())), &scope).unwrap());
        time("tracks q=dub sort=title", 5, || tracks::list_tracks(c, &tq(&|q| { q.q = Some("dub".into()); q.sort = Some(TrackSort::Title) }), &scope).unwrap());
        time("tracks tags=techno", 5, || tracks::list_tracks(c, &tq(&|q| q.tags = vec!["techno".into()]), &scope).unwrap());
        time("tracks tags=techno+dub techno offset=1000", 5, || tracks::list_tracks(c, &tq(&|q| { q.tags = vec!["techno".into(), "dub techno".into()]; q.offset = Some(1000) }), &scope).unwrap());
        time("tracks loved=true", 5, || tracks::list_tracks(c, &tq(&|q| q.loved = Some(true)), &scope).unwrap());
        time("tracks artist_id=1", 5, || tracks::list_tracks(c, &tq(&|q| q.artist_id = Some(1)), &scope).unwrap());
        time("tracks favorites=true", 3, || tracks::list_tracks(c, &tq(&|q| q.favorites = Some(true)), &scope).unwrap());
        time("tracks year_min=2020 sort=year", 3, || tracks::list_tracks(c, &tq(&|q| { q.year_min = Some(2020); q.sort = Some(TrackSort::Year) }), &scope).unwrap());
        let rq = |f: &dyn Fn(&mut ReleaseQuery)| {
            let mut q = ReleaseQuery::default();
            f(&mut q);
            q
        };
        time("releases default", 5, || releases::list_releases(c, &rq(&|_| {}), &scope).unwrap());
        time("releases offset=30000", 5, || releases::list_releases(c, &rq(&|q| q.offset = Some(30000)), &scope).unwrap());
        time("releases sort=artist", 5, || releases::list_releases(c, &rq(&|q| q.sort = Some(ReleaseSort::Artist)), &scope).unwrap());
        time("releases random seed", 5, || releases::list_releases(c, &rq(&|q| { q.sort = Some(ReleaseSort::Random); q.seed = Some(5) }), &scope).unwrap());
        time("releases q=dub", 5, || releases::list_releases(c, &rq(&|q| q.q = Some("dub".into())), &scope).unwrap());
        time("releases tags=techno", 5, || releases::list_releases(c, &rq(&|q| q.tags = vec!["techno".into()]), &scope).unwrap());
        time("releases missing=true", 3, || releases::list_releases(c, &rq(&|q| q.missing = Some(true)), &scope).unwrap());
        time("releases/ids", 3, || releases::release_stubs(c, &rq(&|_| {}), &scope).unwrap());
        time("release related #100", 3, || releases::related_releases(c, 100, 18, &scope).unwrap());
        time("artists default", 5, || artists::list_artists(c, &ArtistQuery::default(), &scope).unwrap());
        time("artists sort=plays", 5, || artists::list_artists(c, &ArtistQuery { sort: Some(ArtistSort::Plays), ..Default::default() }, &scope).unwrap());
        time("artists sort=releases", 5, || artists::list_artists(c, &ArtistQuery { sort: Some(ArtistSort::Releases), ..Default::default() }, &scope).unwrap());
        time("artists sort=tracks", 3, || artists::list_artists(c, &ArtistQuery { sort: Some(ArtistSort::Tracks), ..Default::default() }, &scope).unwrap());
        time("artists q=an", 5, || artists::list_artists(c, &ArtistQuery { q: Some("an".into()), ..Default::default() }, &scope).unwrap());
        time("artist detail", 5, || artists::get_artist(c, 1, &scope).unwrap());
        time("artist related", 3, || artists::artist_related(c, 1, 18, &scope).unwrap());
        time("labels default", 5, || labels::list_labels(c, &LabelQuery::default(), &scope).unwrap());
        time("labels sort=name", 5, || labels::list_labels(c, &LabelQuery { sort: Some(LabelSort::Name), order: Some(SortDir::Asc), ..Default::default() }, &scope).unwrap());
        time("labels shuffle", 3, || labels::shuffle_labels(c, "", 500, &scope).unwrap());
        time("tags default", 5, || tags::list_tags(c, &TagsQuery::default(), &scope).unwrap());
        time("tags q=dub (narrowed)", 3, || tags::list_tags(c, &TagsQuery { q: Some("dub".into()), ..Default::default() }, &scope).unwrap());
        time("facets", 3, || tags::facets(c, 30, &scope).unwrap());
        time("stats", 3, || stats::library_stats(c, &scope).unwrap());
        time("home", 3, || home::home(c, Some(3), &scope).unwrap());
        if let Ok(st) = std::fs::read_to_string("/proc/self/status") {
            for l in st.lines().filter(|l| l.starts_with("VmRSS") || l.starts_with("VmHWM")) {
                println!("{l}");
            }
        }
        Ok(())
    })
    .unwrap();
}
