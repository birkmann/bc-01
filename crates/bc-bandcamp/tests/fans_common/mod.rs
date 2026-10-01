//! Shared fixtures for the fans / follows tests: a service context over a temp legacy-schema
//! DB, the real routers on a local port, and fake Bandcamp sources (the Python tests
//! monkeypatched `sources.*`; a fan page lives at `https://bandcamp.com/<user>`, which no local
//! server can pose as, so the walker's source seam is replaced instead).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bc_bandcamp::error::{HarvestError, Result as HResult};
use bc_bandcamp::harvest::fans::{self, FanSource, FanWalker};
use bc_bandcamp::harvest::feed::{self, FeedSource, FeedSweepHandler, FeedSweeper};
use bc_bandcamp::harvest::inbox;
use bc_bandcamp::service::Ctx;
use bc_bandcamp::sources::{self, DiscoverQuery, EventStream, HarvestEvent, Shallow, SourceProbe};
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_db::rusqlite::params;
use bc_jobs::{JobsService, KindWorker, WorkerSpec};
use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};

pub const ALICE: &str = "https://bandcamp.com/alice/wishlist";
pub const MINE: &str = "https://bandcamp.com/someone/wishlist";

pub type Row = (&'static str, &'static str, &'static str);

pub struct Fx {
    pub ctx: Arc<Ctx>,
    pub walker: Arc<FanWalker>,
    pub sweeper: Arc<FeedSweeper>,
    pub fake: Arc<FakeFans>,
    pub feed: Arc<FakeFeed>,
    pub sweep_worker: Arc<KindWorker>,
    pub base: String,
    pub http: reqwest::Client,
    pub dir: tempfile::TempDir,
}

type ListFn = Arc<dyn Fn(&str) -> EventStream + Send + Sync>;

/// Fan source whose lists are whatever the test last installed.
pub struct FakeFans {
    pub list: Mutex<ListFn>,
}

#[async_trait]
impl FanSource for FakeFans {
    async fn probe_fan(&self, url: &str) -> HResult<SourceProbe> {
        let username = fans::username_from_url(url);
        let mut params = std::collections::BTreeMap::new();
        params.insert("fan_id".into(), json!(username.bytes().map(i64::from).sum::<i64>() + 1));
        params.insert("username".into(), json!(username));
        params.insert("tab".into(), json!("wishlist"));
        params.insert("collection_count".into(), json!(7));
        params.insert("wishlist_count".into(), json!(3));
        Ok(SourceProbe {
            kind: "wishlist".into(),
            label: format!("{username}-wishlist"),
            total_hint: Some(3),
            params,
            ..Default::default()
        })
    }
    fn harvest_collection(&self, _url: &str, which: &str, _limit: usize) -> EventStream {
        (self.list.lock().clone())(which)
    }
}

impl FakeFans {
    pub fn set(&self, f: impl Fn(&str) -> EventStream + Send + Sync + 'static) {
        *self.list.lock() = Arc::new(f);
    }
    /// A fixed, ordered wishlist, and optionally a collection, chosen by the `which` asked for.
    pub fn set_fixed(&self, wish: Vec<Row>, collection: Vec<Row>) {
        self.set(move |which| rows_stream(if which == "collection" { &collection } else { &wish }));
    }
}

pub fn event(url: &str, artist: &str, title: &str, seen: i64, total: i64) -> HarvestEvent {
    HarvestEvent {
        release: Some(sources::shallow(url, Shallow { title, artist, ..Default::default() })),
        seen,
        total: Some(total),
        ..Default::default()
    }
}

pub fn rows_stream(rows: &[Row]) -> EventStream {
    let n = rows.len() as i64;
    let evs: Vec<HResult<HarvestEvent>> =
        rows.iter().enumerate().map(|(i, (u, a, t))| Ok(event(u, a, t, i as i64 + 1, n))).collect();
    futures::stream::iter(evs).boxed()
}

/// Feed source: a fixed grid per band URL (or an error), a discover list, and a record of the
/// last discover query it was asked for.
#[derive(Default)]
pub struct FakeFeed {
    pub pages: Mutex<HashMap<String, std::result::Result<Vec<Row>, String>>>,
    pub discover: Mutex<Vec<Row>>,
    pub last_query: Mutex<Option<DiscoverQuery>>,
    /// When set, every page waits this long before yielding anything.
    pub slow: Mutex<Option<Duration>>,
}

impl FeedSource for FakeFeed {
    fn harvest_artist(&self, url: &str, _limit: usize) -> EventStream {
        let page = self.pages.lock().get(url).cloned().unwrap_or(Ok(vec![]));
        let slow = *self.slow.lock();
        Box::pin(async_stream::stream! {
            if let Some(d) = slow {
                tokio::time::sleep(d).await;
            }
            match page {
                Err(e) => yield Err(HarvestError::other(e)),
                Ok(rows) => {
                    let n = rows.len() as i64;
                    for (i, (u, a, t)) in rows.iter().enumerate() {
                        yield Ok(event(u, a, t, i as i64 + 1, n));
                    }
                }
            }
        })
    }
    fn harvest_discover(&self, query: DiscoverQuery, _limit: usize) -> EventStream {
        *self.last_query.lock() = Some(query);
        rows_stream(&self.discover.lock().clone())
    }
}

pub async fn fx() -> Fx {
    fx_opts(true).await
}

/// `start_walks = false` leaves `fans::start` (self-fan migration, pending restore, worker) to the test.
pub async fn fx_opts(start_walks: bool) -> Fx {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::from_env();
    cfg.data_dir = dir.path().join("data");
    cfg.download_dir = dir.path().join("downloads");
    std::fs::create_dir_all(&cfg.data_dir).expect("data dir");
    let db = Db::open(dir.path().join("lib.db")).expect("open db");
    let bus = Arc::new(EventBus::new());
    let jobs = JobsService::new(db.clone(), bus.clone());
    let ctx = Ctx::new(db, bus, cfg, jobs);
    inbox::init(&ctx);
    fans::init(&ctx);
    feed::init(&ctx);
    let walker = ctx.expect::<FanWalker>();
    let sweeper = ctx.expect::<FeedSweeper>();
    let fake = Arc::new(FakeFans { list: Mutex::new(Arc::new(|_| futures::stream::empty().boxed())) });
    walker.set_source(fake.clone());
    let feed_src = Arc::new(FakeFeed::default());
    sweeper.set_source(feed_src.clone());

    // The `sweep` kind's single worker lives in `harvest::sweep`; here it is just the feed's.
    let sweep_worker = KindWorker::new(
        ctx.jobs.store().clone(),
        WorkerSpec::new(bc_types::jobs::KIND_SWEEP, 1),
        Arc::new(FeedSweepHandler(sweeper.clone())),
    );
    ctx.jobs.add_hooks(sweep_worker.clone());
    sweep_worker.start();
    if start_walks {
        fans::start(&ctx).await;
    }

    let router = bc_bandcamp::api::fans::router(ctx.clone()).merge(bc_bandcamp::api::follows::router(ctx.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Fx { ctx, walker, sweeper, fake, feed: feed_src, sweep_worker, base, http: reqwest::Client::new(), dir }
}

impl Fx {
    pub async fn get(&self, path: &str) -> (u16, Value) {
        let r = self.http.get(format!("{}{path}", self.base)).send().await.expect("get");
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
    pub async fn post(&self, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut rq = self.http.post(format!("{}{path}", self.base));
        if let Some(b) = body {
            rq = rq.json(&b);
        }
        let r = rq.send().await.expect("post");
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
    pub async fn put(&self, path: &str, body: Value) -> (u16, Value) {
        let r = self.http.put(format!("{}{path}", self.base)).json(&body).send().await.expect("put");
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
    pub async fn patch(&self, path: &str, body: Value) -> (u16, Value) {
        let r = self.http.patch(format!("{}{path}", self.base)).json(&body).send().await.expect("patch");
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }
    pub async fn delete(&self, path: &str) -> (u16, Value) {
        let r = self.http.delete(format!("{}{path}", self.base)).send().await.expect("delete");
        (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
    }

    pub async fn add_fan(&self, url: &str, walk: bool) -> Value {
        let (s, b) = self.post("/fans", Some(json!({"url": url, "walk": walk}))).await;
        assert_eq!(s, 202, "{b}");
        b
    }

    /// The walk is a background job; poll its state rather than sleeping.
    pub async fn wait_for_walk(&self, fan_id: i64) -> Value {
        for _ in 0..500 {
            let (_, fan) = self.get(&format!("/fans/{fan_id}")).await;
            if let Some(w) = fan.get("walk").filter(|w| !w.is_null() && matches!(w["phase"].as_str(), Some("done" | "failed"))) {
                return w.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("walk did not finish");
    }

    pub async fn wait_for_sweep(&self) -> Value {
        for _ in 0..500 {
            let (_, st) = self.get("/follows/sweep").await;
            if st["running"] == json!(false) && st["phase"] != json!("idle") {
                return st;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("sweep did not finish in time");
    }

    /// The self fan (created like `fans::start` does from the legacy key).
    pub fn make_self(&self, url: &str) -> i64 {
        let url = url.to_string();
        self.ctx
            .db
            .write_with::<_, HarvestError>(move |tx| Ok(fans::create_fan(tx, &url, true, None)?.id))
            .expect("make self")
    }

    pub fn seed_release(&self, artist: &str, title: &str, source_fan_id: Option<i64>) -> i64 {
        let (a, t) = (artist.to_string(), title.to_string());
        self.ctx
            .db
            .write(move |tx| {
                let key = bc_db::util::name_key(&a);
                tx.execute(
                    "INSERT OR IGNORE INTO artists(name, name_key, created_at) VALUES (?1, ?2, datetime('now'))",
                    params![a, key],
                )?;
                let artist_id: i64 = tx.query_row("SELECT id FROM artists WHERE name_key = ?1", [&key], |r| r.get(0))?;
                tx.execute(
                    "INSERT INTO releases(title, title_key, kind, artist_id, added_at, source_fan_id) \
                     VALUES (?1, ?2, 'album', ?3, datetime('now'), ?4)",
                    params![t, bc_db::util::name_key(&t), artist_id, source_fan_id],
                )?;
                Ok(tx.last_insert_rowid())
            })
            .expect("seed release")
    }

    pub fn q<T: Send + 'static + Default + bc_db::rusqlite::types::FromSql>(&self, sql: &'static str) -> T {
        self.ctx.db.read(move |c| Ok(c.query_row(sql, [], |r| r.get::<_, T>(0))?)).expect("query")
    }

    pub fn titles_in_order(&self, fan_id: i64) -> Vec<(String, Option<i64>)> {
        self.ctx
            .db
            .read(move |c| {
                let mut st = c.prepare(
                    "SELECT h.title, MIN(f.position) p FROM fan_items f JOIN harvest_items h ON h.id = f.item_id \
                     WHERE f.fan_id = ?1 GROUP BY h.id ORDER BY COALESCE(p, 1000000000), h.id",
                )?;
                let rows = st
                    .query_map([fan_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)))?
                    .collect::<bc_db::rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .expect("titles")
    }
}

impl Fx {
    pub async fn put_poll_hours(&self, h: f64) {
        let (s, _) = self.put("/follows/settings", json!({"poll_hours": h})).await;
        assert_eq!(s, 200);
    }
}
