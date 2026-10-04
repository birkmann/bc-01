//! Finding a rescanned library's releases on Bandcamp again: which search hit is the release,
//! what a run writes, and that a stopped run resumes instead of searching again.

mod harvest_common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bc_bandcamp::error::{HarvestError, Result as HResult};
use bc_bandcamp::harvest::labels::PageSource;
use bc_bandcamp::harvest::relink::*;
use bc_bandcamp::net::PageKind;
use bc_bandcamp::sources::SearchHit;
use bc_db::rusqlite::params;
use harvest_common::*;

fn album(name: &str, byline: &str, url: &str) -> SearchHit {
    SearchHit { kind: "album".into(), name: name.into(), subtitle: byline.into(), url: url.into(), item_id: Some(7), ..Default::default() }
}

fn track(name: &str, byline: &str, url: &str) -> SearchHit {
    SearchHit { kind: "track".into(), name: name.into(), subtitle: byline.into(), url: url.into(), ..Default::default() }
}

/// Search answered per query text; anything else finds nothing. Records every query.
struct FakeSearch {
    answers: HashMap<(String, String), Vec<SearchHit>>,
    asked: parking_lot::Mutex<Vec<(String, String)>>,
    fail: bool,
}

impl FakeSearch {
    fn new(answers: Vec<(&str, &str, Vec<SearchHit>)>) -> Arc<Self> {
        Arc::new(Self {
            answers: answers.into_iter().map(|(q, k, h)| ((q.to_string(), k.to_string()), h)).collect(),
            asked: Default::default(),
            fail: false,
        })
    }
    fn failing() -> Arc<Self> {
        Arc::new(Self { answers: HashMap::new(), asked: Default::default(), fail: true })
    }
    fn asked(&self) -> Vec<(String, String)> {
        self.asked.lock().clone()
    }
}

#[async_trait]
impl PageSource for FakeSearch {
    async fn page(&self, url: &str, _kind: PageKind, _ttl: Option<Duration>) -> HResult<String> {
        Err(HarvestError::other(format!("not found: {url}")))
    }
    async fn search(&self, q: &str, kind: &str, _limit: usize) -> HResult<Vec<SearchHit>> {
        self.asked.lock().push((q.to_string(), kind.to_string()));
        if self.fail {
            return Err(HarvestError::other("connection refused"));
        }
        Ok(self.answers.get(&(q.to_string(), kind.to_string())).cloned().unwrap_or_default())
    }
}

async fn relink(app: &App, src: Arc<FakeSearch>) -> bc_types::bandcamp::RelinkStatus {
    let r = app.ctx.expect::<Relinker>();
    r.set_source(src);
    r.start_run().await.expect("start");
    app.poll("/harvest/relink", |s| matches!(s["phase"].as_str(), Some("done" | "failed"))).await;
    r.status()
}

fn url_of(app: &App, id: i64) -> Option<String> {
    app.q(move |c| Ok(c.query_row("SELECT bandcamp_url FROM releases WHERE id = ?1", [id], |r| r.get(0))?))
}

fn add_track(app: &App, release: i64, title: &str) {
    let title = title.to_string();
    app.exec(move |t| {
        t.execute(
            "INSERT INTO tracks(release_id, title, title_key, loved, play_count, skip_count, added_at) VALUES (?1, ?2, ?2, 0, 0, 0, datetime('now'))",
            params![release, title],
        )?;
        Ok(())
    });
}

// -- picking ---------------------------------------------------------------------------

#[test]
fn the_hit_must_be_this_title_by_this_artist() {
    let hits = vec![
        album("Patience", "Somebody Else", "https://somebody.bandcamp.com/album/patience"),
        album("Patience EP", "Sebo K", "https://rekids.bandcamp.com/album/patience-ep"),
        album("Patience", "SEBO K", "https://rekids.bandcamp.com/album/patience"),
    ];
    assert_eq!(pick(&hits, "Patience", "Sebo K").map(|h| h.url.as_str()), Some("https://rekids.bandcamp.com/album/patience"));
    assert_eq!(pick(&hits, "Patience", "Nobody We Know"), None, "same title by a stranger is not the record");
    assert_eq!(pick(&hits, "Impatience", "Sebo K"), None);
}

#[test]
fn an_exact_byline_beats_a_close_one() {
    let hits = vec![
        album("Module EP", "Niereich & Friends", "https://a.bandcamp.com/album/module-ep"),
        album("Module EP", "NIEREICH", "https://gynoidaudio.bandcamp.com/album/module-ep"),
    ];
    assert_eq!(pick(&hits, "MODULE EP", "Niereich").map(|h| h.url.as_str()), Some("https://gynoidaudio.bandcamp.com/album/module-ep"));
    // Close is still enough when it is all there is.
    assert_eq!(pick(&hits[..1], "Module EP", "Niereich").map(|h| h.url.as_str()), Some("https://a.bandcamp.com/album/module-ep"));
}

#[test]
fn a_compilation_matches_on_its_title_and_searches_without_the_byline() {
    let hits = vec![album("ADE Sampler 2015", "Diynamic", "https://didrec.bandcamp.com/album/ade-sampler-2015")];
    assert!(pick(&hits, "ADE Sampler 2015", "Various Artists").is_some());
    let p = Pending { id: 1, title: "ADE Sampler 2015".into(), artist: "Various Artists".into(), tracks: 12 };
    assert_eq!(query(&p), "ADE Sampler 2015");
    let p = Pending { id: 1, title: "Patience".into(), artist: "Sebo K".into(), tracks: 3 };
    assert_eq!(query(&p), "Patience Sebo K");
}

#[test]
fn artists_and_fans_are_never_a_release() {
    let mut a = album("Patience", "Sebo K", "https://sebok.bandcamp.com");
    a.kind = "artist".into();
    assert_eq!(pick(&[a], "Patience", "Sebo K"), None);
}

// -- recording -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_page_held_by_another_release_is_left_to_the_twin_merge() {
    let app = app().await;
    let held = app.release("Sebo K", "Patience", Some("https://rekids.bandcamp.com/album/patience"));
    let twin = app.release("Sebo K.", "Patience", None);
    let hit = album("Patience", "Sebo K", "https://rekids.bandcamp.com/album/patience");

    assert!(!app.exec(move |t| record(t, twin, Some(&hit))));
    assert_eq!(url_of(&app, twin), None);
    assert_eq!(url_of(&app, held).as_deref(), Some("https://rekids.bandcamp.com/album/patience"));
    assert!(app.q(pending).is_empty(), "tried, so not searched again");
}

// -- the run ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_links_what_it_is_sure_of_and_remembers_the_rest() {
    let app = app().await;
    let sure = app.release("Sebo K", "Patience", None);
    let unsure = app.release("Rosper", "Guardian Of The Galaxy", None);
    let single = app.release("Kepler", "PLZ013LTD", None);
    add_track(&app, single, "PLZ013LTD");
    let known = app.release("Mython", "BCCX008", Some("https://bcco.bandcamp.com/album/bccx008"));
    let src = FakeSearch::new(vec![
        ("Patience Sebo K", "a", vec![album("Patience", "Sebo K", "https://rekids.bandcamp.com/album/patience")]),
        ("Guardian Of The Galaxy Rosper", "a", vec![album("Guardians Of The Galaxy", "Rosper", "https://x.bandcamp.com/album/g")]),
        ("PLZ013LTD Kepler", "t", vec![track("PLZ013LTD", "Kepler", "https://dbh-music.bandcamp.com/track/plz013ltd")]),
    ]);

    let state = relink(&app, src.clone()).await;

    assert_eq!(state.phase, "done", "{:?}", state.error);
    assert_eq!((state.total, state.seen, state.linked, state.unmatched), (3, 3, 2, 1));
    assert_eq!(url_of(&app, sure).as_deref(), Some("https://rekids.bandcamp.com/album/patience"));
    assert_eq!(url_of(&app, unsure), None);
    assert_eq!(url_of(&app, single).as_deref(), Some("https://dbh-music.bandcamp.com/track/plz013ltd"));
    assert_eq!(url_of(&app, known).as_deref(), Some("https://bcco.bandcamp.com/album/bccx008"));
    let item_id: Option<i64> = app.q(move |c| Ok(c.query_row("SELECT bandcamp_item_id FROM releases WHERE id = ?1", [sure], |r| r.get(0))?));
    assert_eq!(item_id, Some(7));
    // Only a single-track release falls back to track search.
    assert!(!src.asked().contains(&("Guardian Of The Galaxy Rosper".to_string(), "t".to_string())));

    // A second run has nothing left to ask.
    let again = FakeSearch::new(vec![]);
    let state = relink(&app, again.clone()).await;
    assert_eq!((state.phase.as_str(), state.total), ("done", 0));
    assert!(again.asked().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_search_that_keeps_failing_ends_the_run_and_marks_nothing() {
    let app = app().await;
    for i in 0..30 {
        app.release("Artist", &format!("Record {i}"), None);
    }

    let state = relink(&app, FakeSearch::failing()).await;

    assert_eq!(state.phase, "failed");
    assert!(state.error.as_deref().unwrap_or("").contains("connection refused"), "{:?}", state.error);
    assert_eq!(state.linked, 0);
    assert_eq!(app.q(pending).len(), 30, "a failed search says nothing; the next run asks again");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_routes_start_report_and_refuse_a_second_run() {
    let app = app().await;
    let (code, idle) = app.get("/harvest/relink").await;
    assert_eq!((code, idle["phase"].as_str()), (200, Some("idle")));
    app.ctx.expect::<Relinker>().set_source(FakeSearch::new(vec![]));

    let (code, _) = app.post_empty("/harvest/relink").await;
    assert_eq!(code, 202);
    let done = app.poll("/harvest/relink", |s| matches!(s["phase"].as_str(), Some("done" | "failed"))).await;
    assert_eq!(done["phase"], "done");
    let (code, stopped) = app.delete("/harvest/relink").await;
    assert_eq!((code, stopped["phase"].as_str()), (200, Some("done")), "stopping an idle relink is not an error");
}
