//! Timing of the recommenders on a COPY of the real library (never the original):
//!
//! ```text
//! cp ~/.local/share/bc-rust/library.db /tmp/library-copy.db
//! cargo run --release -p bc-recommend --example similar_timing -- /tmp/library-copy.db
//! ```
//!
//! Prints median ms over >= 10 runs (after warm-up) for `POST /suggest/similar` through the
//! router (typical techno seed, worst-case "Electronic only" seed), the legacy ("before") versus
//! fixed ("after") SQL shapes of the artist branch and of the files-presence test, plus
//! `nextup::suggest` and `taste::suggest_loved`.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::Request;
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_recommend::scope::{Scope, ScopeExt};
use bc_recommend::{RecommendService, nextup, similar, taste};
use bc_types::suggest::{LovedQuery, SimilarRequest, SuggestRequest};
use http_body_util::BodyExt;
use tower::ServiceExt;

const RUNS: usize = 15;
const WARMUP: usize = 3;

fn median_ms<T>(mut f: impl FnMut() -> T) -> f64 {
    for _ in 0..WARMUP {
        std::hint::black_box(f());
    }
    let mut v: Vec<f64> = (0..RUNS)
        .map(|_| {
            let t = Instant::now();
            std::hint::black_box(f());
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn count(db: &Db, sql: &str) -> usize {
    let sql = sql.to_string();
    db.read(move |c| {
        let mut st = c.prepare(&sql)?;
        let n = st.query_map([], |_| Ok(()))?.count();
        Ok(n)
    })
    .unwrap()
}

fn one<T: bc_db::rusqlite::types::FromSql>(db: &Db, sql: &str) -> Option<T> {
    let sql = sql.to_string();
    db.read(move |c| Ok(c.query_row(&sql, [], |r| r.get::<_, T>(0)).ok())).unwrap()
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: similar_timing <copy-of-library.db>");
    assert!(!path.contains("/data/old/"), "refusing to open the original: pass a COPY");
    let db = Db::open(&path).expect("open the copy");
    let total: i64 = one(&db, "SELECT count(*) FROM tracks").unwrap();
    println!("library copy: {path} ({total} tracks)");

    // --- seeds ------------------------------------------------------------------------------
    let electronic: i64 = one(&db, "SELECT id FROM tags WHERE name_key = 'electronic'").expect("electronic tag");
    let present = "EXISTS (SELECT 1 FROM files f WHERE f.track_id = t.id AND f.missing_since IS NULL)";
    let typical: i64 = one(
        &db,
        &format!(
            "SELECT t.id FROM tracks t JOIN track_tags tt ON tt.track_id = t.id JOIN tags g ON g.id = tt.tag_id \
             JOIN releases r ON r.id = t.release_id JOIN analysis a ON a.track_id = t.id \
             WHERE g.name_key = 'hypnotic techno' AND r.label_id IS NOT NULL AND {present} LIMIT 1 OFFSET 40"
        ),
    )
    .expect("typical seed");
    let worst: i64 = one(
        &db,
        &format!(
            "SELECT tt.track_id FROM track_tags tt JOIN tracks t ON t.id = tt.track_id WHERE tt.tag_id = {electronic} \
             AND NOT EXISTS (SELECT 1 FROM track_tags x WHERE x.track_id = tt.track_id AND x.tag_id <> {electronic}) \
             AND {present} LIMIT 1"
        ),
    )
    .expect("electronic-only seed");
    println!("typical seed = track {typical} (hypnotic techno), worst-case seed = track {worst} (Electronic only)");

    // --- endpoint through the router ----------------------------------------------------------
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let svc = RecommendService::new(db.clone(), Arc::new(EventBus::new()), Arc::new(Config::from_env()));
    let router = svc.router();
    let call = |seed: i64, limit: i64| {
        let body = serde_json::json!({"seed_track_id": seed, "limit": limit}).to_string();
        let router = router.clone();
        rt.block_on(async move {
            let req = Request::builder().method("POST").uri("/suggest/similar").header("content-type", "application/json").body(Body::from(body)).unwrap();
            let resp = router.oneshot(req).await.unwrap();
            assert!(resp.status().is_success());
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        })
    };
    let sample = call(typical, 30);
    println!(
        "typical: pool_tags={:?} pool_size={} items={}",
        sample["pool_tags"],
        sample["pool_size"],
        sample["items"].as_array().map(|a| a.len()).unwrap_or(0)
    );
    let sample_w = call(worst, 30);
    println!("worst:   pool_tags={:?} pool_size={}", sample_w["pool_tags"], sample_w["pool_size"]);
    let t_typ = median_ms(|| call(typical, 30));
    let t_worst = median_ms(|| call(worst, 30));

    // --- before / after SQL shapes ---------------------------------------------------------------
    // Branch statements as the legacy code shaped them vs. as this crate does.
    let row: (Option<i64>, Option<i64>) = db
        .read(move |c| {
            Ok(c.query_row("SELECT COALESCE(t.artist_id, r.artist_id), r.label_id FROM tracks t LEFT JOIN releases r ON r.id = t.release_id WHERE t.id = ?1", [typical], |r| Ok((r.get(0)?, r.get(1)?)))?)
        })
        .unwrap();
    let (artist, label) = (row.0.expect("artist"), row.1.expect("label"));
    let cols = "t.id, t.artist_id, t.release_id, r.artist_id, r.label_id, t.loved, t.play_count, a.bpm, a.camelot, a.energy";
    let from = "FROM tracks t LEFT JOIN releases r ON r.id = t.release_id LEFT JOIN analysis a ON a.track_id = t.id";
    let old_present = "t.id IN (SELECT track_id FROM files WHERE missing_since IS NULL)";

    let artist_before = format!("SELECT {cols} {from} WHERE {old_present} AND (t.artist_id = {artist} OR r.artist_id = {artist}) LIMIT 300");
    let artist_before_exists = format!("SELECT {cols} {from} WHERE {present} AND (t.artist_id = {artist} OR r.artist_id = {artist}) LIMIT 300");
    let a1 = format!("SELECT {cols} {from} WHERE {present} AND t.artist_id = {artist} LIMIT 300");
    let a2 = format!("SELECT {cols} {from} WHERE {present} AND t.release_id IN (SELECT sr.id FROM releases sr WHERE sr.artist_id = {artist}) LIMIT 300");
    let n_before = count(&db, &artist_before);
    let t_art_before = median_ms(|| count(&db, &artist_before));
    let t_art_before_exists = median_ms(|| count(&db, &artist_before_exists));
    let t_art_after = median_ms(|| (count(&db, &a1), count(&db, &a2)));
    println!("artist branch: {n_before} rows");

    // Four branches (tags / artist x2 / label / loved) with the old IN-subquery vs EXISTS, same OR-free shapes.
    let seed = similar::Seed { artist_id: Some(artist), label_id: Some(label), ..Default::default() };
    let tag_ids: Vec<i64> = db
        .read(move |c| {
            let mut st = c.prepare("SELECT tag_id FROM track_tags WHERE track_id = ?1")?;
            let ids = st.query_map([typical], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
            Ok(ids)
        })
        .unwrap();
    let rows = bc_db::Db::read(&db, |c| bc_recommend::pooling::tag_rows_by_ids(c, &tag_ids).map_err(|e| bc_db::DbError::Other(e.to_string()))).unwrap();
    let drawing = bc_recommend::pooling::drawing_tags(&rows, bc_recommend::pooling::TAG_BUDGET);
    let draw_ids: Vec<i64> = drawing.iter().map(|r| r.0).collect();
    let branches = similar::pool_statements(&seed, &Default::default(), &Scope::all(), &[typical], &draw_ids, 0);
    let after_stmts: Vec<String> = branches.iter().map(|(_, s)| s.clone()).collect();
    let before_stmts: Vec<String> = after_stmts.iter().map(|s| s.replace(present, old_present)).collect();
    let t_in = median_ms(|| before_stmts.iter().map(|s| count(&db, s)).sum::<usize>());
    let t_exists = median_ms(|| after_stmts.iter().map(|s| count(&db, s)).sum::<usize>());

    // --- other recommenders ----------------------------------------------------------------------------
    let nreq = SuggestRequest { seed_track_id: Some(typical), ..Default::default() };
    let t_next = median_ms(|| nextup::suggest(&db, &Scope::all(), &nreq).unwrap());
    let nitems = nextup::suggest(&db, &Scope::all(), &nreq).unwrap().items.len();
    let t_loved = median_ms(|| taste::suggest_loved(&db, &Scope::all(), &LovedQuery::default()).unwrap());
    let loved = taste::suggest_loved(&db, &Scope::all(), &LovedQuery::default()).unwrap();
    let t_direct = median_ms(|| similar::suggest(&db, &Scope::all(), &SimilarRequest { seed_track_id: Some(typical), ..Default::default() }).unwrap());

    println!("\n| measurement (median of {RUNS} after {WARMUP} warm-ups) | ms |");
    println!("| --- | --- |");
    println!("| POST /suggest/similar, typical techno seed (router) | {t_typ:.1} |");
    println!("| similar::suggest, typical seed (direct fn) | {t_direct:.1} |");
    println!("| POST /suggest/similar, worst-case Electronic-only seed (router) | {t_worst:.1} |");
    println!("| artist branch BEFORE: OR across tables + IN-subquery | {t_art_before:.2} |");
    println!("| artist branch, OR across tables + EXISTS | {t_art_before_exists:.2} |");
    println!("| artist branch AFTER: two statements + EXISTS | {t_art_after:.2} |");
    println!("| 5 pool branches BEFORE: files IN-subquery | {t_in:.1} |");
    println!("| 5 pool branches AFTER: correlated EXISTS | {t_exists:.1} |");
    println!("| nextup::suggest, typical seed, strict ({nitems} items) | {t_next:.1} |");
    println!("| taste::suggest_loved, limit 24 ({} loved, {} items) | {t_loved:.1} |", loved.profile.loved_count, loved.items.len());
}
