//! The tag enricher (`/harvest/enrich`, job kind `enrich`): fetches each inbox item's release
//! page and stamps its tags (and only blanks of the other fields) onto the row.

mod harvest_common;

use std::sync::Arc;
use std::time::Duration;

use bc_bandcamp::error::HarvestError;
use bc_bandcamp::extract::HarvestedRelease;
use bc_bandcamp::harvest::enrich::{FetchFn, MAX_ITEMS, TagEnricher};
use harvest_common::*;
use serde_json::{Value, json};

const EN: &str = "/harvest/enrich";

fn page(url: &str, tags: &[&str]) -> HarvestedRelease {
    HarvestedRelease {
        url: url.into(),
        title: "From The Page".into(),
        artist_name: "Page Artist".into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        label_name: Some("Page Label".into()),
        release_date: Some("2020-01-02".into()),
        ..Default::default()
    }
}

fn tags_of(app: &App, id: i64) -> Vec<String> {
    app.q(move |c| {
        let mut st = c.prepare("SELECT tag FROM harvest_item_tags WHERE item_id = ?1 ORDER BY tag_key")?;
        Ok(st.query_map([id], |r| r.get(0))?.collect::<Result<_, _>>()?)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enrich_tags_rows_fills_blanks_and_survives_a_dead_page() {
    let app = app().await;
    let a = app.inbox_with("https://x.bandcamp.com/album/a", "A", "X", "label", "x", false, &[]);
    let dead = app.inbox_with("https://x.bandcamp.com/album/dead", "Dead", "X", "label", "x", false, &[]);
    let untagged = app.inbox_with("https://x.bandcamp.com/album/none", "None", "X", "label", "x", false, &[]);
    app.exec(move |t| Ok(t.execute("UPDATE harvest_items SET label_name = 'Stated Label' WHERE id = ?1", [a]).map(|_| ())?));
    let fetch: FetchFn = Arc::new(|_, url| {
        Box::pin(async move {
            if url.ends_with("/dead") {
                Err(HarvestError::other("HTTP 404"))
            } else if url.ends_with("/none") {
                Ok(page(&url, &[]))
            } else {
                Ok(page(&url, &["Techno", "Deep House"]))
            }
        })
    });
    app.ctx.expect::<TagEnricher>().set_fetch(fetch);

    let (status, started) = app.post(EN, json!({"item_ids": [a, dead, untagged, a]})).await;
    assert_eq!(status, 202);
    assert_eq!((started["phase"].as_str(), started["total"].as_i64()), (Some("running"), Some(3)), "duplicates collapse");
    let state = app.poll(EN, |s| s["running"] == json!(false)).await;

    assert_eq!(state["phase"], "done", "{state}");
    assert_eq!((state["done"].as_i64(), state["tagged"].as_i64()), (Some(3), Some(1)));
    assert!(state["errors"][0].as_str().unwrap().contains("Dead"));
    assert_eq!(tags_of(&app, a), vec!["Deep House", "Techno"]);
    assert!(tags_of(&app, dead).is_empty());
    // Stamped onto the row -- and through the tag filter.
    let (_, page) = app.get("/harvest/items?tags=techno").await;
    assert_eq!(page["items"][0]["id"], json!(a));
    // Blanks filled, stated fields never overwritten, source slice untouched.
    let (label, date, kind): (Option<String>, Option<String>, String) = app.q(move |c| {
        Ok(c.query_row("SELECT label_name, release_date, source_kind FROM harvest_items WHERE id = ?1", [a], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?)
    });
    assert_eq!((label.as_deref(), date.as_deref(), kind.as_str()), (Some("Stated Label"), Some("2020-01-02"), "label"));
    // The enrich pass is a job.
    assert_eq!(app.get("/jobs?kind=enrich").await.1["total"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enrich_validates_caps_and_refuses_a_second_pass() {
    let app = app().await;
    assert_eq!(app.post(EN, json!({"item_ids": []})).await.0, 400);
    assert_eq!(app.get(EN).await.1["phase"], "idle");

    let fetch: FetchFn = Arc::new(|_, url| {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(page(&url, &[]))
        })
    });
    app.ctx.expect::<TagEnricher>().set_fetch(fetch);
    let ids: Vec<i64> = (1..=(MAX_ITEMS as i64 + 100)).collect();
    let (status, state) = app.post(EN, json!({"item_ids": ids})).await;
    assert_eq!(status, 202);
    assert_eq!(state["total"], MAX_ITEMS as i64, "capped");

    let (status, body) = app.post(EN, json!({"item_ids": [1]})).await;
    assert_eq!(status, 400);
    assert!(body["detail"].as_str().unwrap().contains("already running"));
    app.delete(EN).await;
}

/// Stopping keeps the tags already written and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_keeps_the_tags_already_written() {
    let app = app().await;
    let first = app.inbox_with("https://x.bandcamp.com/album/first", "First", "X", "label", "x", false, &[]);
    let second = app.inbox_with("https://x.bandcamp.com/album/second", "Second", "X", "label", "x", false, &[]);
    let fetch: FetchFn = Arc::new(|_, url| {
        Box::pin(async move {
            if url.ends_with("/second") {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            Ok(page(&url, &["Techno"]))
        })
    });
    app.ctx.expect::<TagEnricher>().set_fetch(fetch);

    app.post(EN, json!({"item_ids": [first, second]})).await;
    app.poll(EN, |s| s["done"].as_i64().unwrap_or(0) >= 1).await;
    let (status, state): (u16, Value) = app.delete(EN).await;

    assert_eq!(status, 200);
    assert_eq!((state["phase"].as_str(), state["error"].as_str(), state["tagged"].as_i64()), (Some("done"), Some("Stopped"), Some(1)));
    assert_eq!(tags_of(&app, first), vec!["Techno"]);
    assert!(tags_of(&app, second).is_empty());
}
