//! Fixtures for the harvest tests: a whole `BandcampService` over a temp legacy-schema DB,
//! served on a local port (the Python tests used `TestClient(create_app(settings))`), plus raw-SQL
//! row builders and canned discography streams (the Python `monkeypatch` of `sources`).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bc_bandcamp::error::{HarvestError, Result as HResult};
use bc_bandcamp::harvest::sweep::{ArtistStreamFn, SweepService};
use bc_bandcamp::net::PageKind;
use bc_bandcamp::service::Ctx;
use bc_bandcamp::sources::{self, EventStream, HarvestEvent, SearchHit, Shallow};
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_db::rusqlite::{OptionalExtension, params};
use bc_db::util::name_key;
use bc_jobs::JobsService;
use serde_json::Value;

pub struct App {
    pub ctx: Arc<Ctx>,
    pub db: Db,
    pub base: String,
    pub http: reqwest::Client,
    _dir: tempfile::TempDir,
}

/// A running app over a default client (the shared one, never contacted by these tests). The
/// harvest workers are started; the download worker is NOT (a test about a job's lifecycle must
/// drive the items itself -- a worker would claim them within milliseconds).
pub async fn app() -> App {
    build(None).await
}

/// Like [`app`], but every harvest service and route talks to the local fake Bandcamp.
pub async fn app_fake(fake: &FakeBc) -> App {
    build(Some(fake.client.clone())).await
}

async fn build(client: Option<BandcampClient>) -> App {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = Config::from_env();
    cfg.data_dir = dir.path().join("data");
    cfg.download_dir = dir.path().join("downloads");
    cfg.library_root = None;
    std::fs::create_dir_all(&cfg.data_dir).expect("data dir");
    let db = Db::open(cfg.db_path()).expect("open db");
    let bus = Arc::new(EventBus::new());
    let jobs = JobsService::new(db.clone(), bus.clone());
    // `with_client` also pins the cookie store to the 0600 file: never the developer's keyring.
    let client = client.unwrap_or_else(|| bc_bandcamp::net::client_from_config(&cfg, None));
    let ctx = Ctx::with_client(db.clone(), bus, cfg, jobs.clone(), client);
    bc_bandcamp::harvest::init(&ctx);
    bc_bandcamp::api::init(&ctx);
    bc_bandcamp::harvest::start(&ctx).await;

    let router = axum::Router::new().nest("/api", bc_bandcamp::api::router(&ctx).merge(jobs.router()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
    App { ctx, db, base: format!("http://{addr}/api"), http: reqwest::Client::new(), _dir: dir }
}

impl App {
    pub async fn get(&self, path: &str) -> (u16, Value) {
        send(self.http.get(format!("{}{path}", self.base))).await
    }
    pub async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        send(self.http.post(format!("{}{path}", self.base)).json(&body)).await
    }
    pub async fn post_empty(&self, path: &str) -> (u16, Value) {
        send(self.http.post(format!("{}{path}", self.base))).await
    }
    pub async fn put(&self, path: &str, body: Value) -> (u16, Value) {
        send(self.http.put(format!("{}{path}", self.base)).json(&body)).await
    }
    pub async fn delete(&self, path: &str) -> (u16, Value) {
        send(self.http.delete(format!("{}{path}", self.base))).await
    }

    /// Poll a GET until `done(body)`; the services are background jobs, so poll rather than sleep.
    pub async fn poll(&self, path: &str, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (_, body) = self.get(path).await;
            if done(&body) {
                return body;
            }
            assert!(tokio::time::Instant::now() < deadline, "{path} did not reach the expected state; last: {body}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub fn sweeps(&self) -> Arc<SweepService> {
        self.ctx.expect::<SweepService>()
    }

    // -- raw-SQL row builders -------------------------------------------------------------

    pub fn q<T: Send + 'static>(&self, f: impl FnOnce(&bc_db::rusqlite::Connection) -> bc_db::Result<T>) -> T {
        self.db.read(f).expect("query")
    }

    pub fn exec<T: Send + 'static>(&self, f: impl FnOnce(&bc_db::rusqlite::Transaction<'_>) -> bc_db::Result<T> + Send + 'static) -> T {
        self.db.write(f).expect("write")
    }

    pub fn artist(&self, name: &str, url: Option<&str>) -> i64 {
        let (name, key, url) = (name.to_string(), name_key(name), url.map(str::to_string));
        self.exec(move |t| {
            if let Some(id) = t.query_row("SELECT id FROM artists WHERE name_key = ?1", [&key], |r| r.get(0)).optional()? {
                return Ok(id);
            }
            t.execute("INSERT INTO artists(name, name_key, bandcamp_url, created_at) VALUES (?1, ?2, ?3, datetime('now'))", params![name, key, url])?;
            Ok(t.last_insert_rowid())
        })
    }

    pub fn label(&self, name: &str, url: Option<&str>) -> i64 {
        let (name, key, url) = (name.to_string(), name_key(name), url.map(str::to_string));
        self.exec(move |t| {
            t.execute("INSERT INTO labels(name, name_key, bandcamp_url) VALUES (?1, ?2, ?3)", params![name, key, url])?;
            Ok(t.last_insert_rowid())
        })
    }

    /// A release as a scan off disk leaves it (no URL unless given).
    pub fn release(&self, artist: &str, title: &str, url: Option<&str>) -> i64 {
        let artist_id = self.artist(artist, None);
        let (title, key, url) = (title.to_string(), name_key(title), url.map(str::to_string));
        self.exec(move |t| {
            t.execute(
                "INSERT INTO releases(title, title_key, kind, artist_id, bandcamp_url, added_at) VALUES (?1, ?2, 'album', ?3, ?4, datetime('now'))",
                params![title, key, artist_id, url],
            )?;
            Ok(t.last_insert_rowid())
        })
    }

    pub fn release_label_id(&self, id: i64) -> Option<i64> {
        self.q(move |c| Ok(c.query_row("SELECT label_id FROM releases WHERE id = ?1", [id], |r| r.get(0))?))
    }

    pub fn label_url(&self, id: i64) -> Option<String> {
        self.q(move |c| Ok(c.query_row("SELECT bandcamp_url FROM labels WHERE id = ?1", [id], |r| r.get(0))?))
    }

    pub fn artist_url(&self, id: i64) -> Option<String> {
        self.q(move |c| Ok(c.query_row("SELECT bandcamp_url FROM artists WHERE id = ?1", [id], |r| r.get(0))?))
    }

    pub fn favorite_artist(&self, id: i64) {
        self.exec(move |t| Ok(t.execute("INSERT INTO favorites(artist_id, created_at) VALUES (?1, datetime('now'))", [id]).map(|_| ())?));
    }
    pub fn favorite_label(&self, id: i64) {
        self.exec(move |t| Ok(t.execute("INSERT INTO favorites(label_id, created_at) VALUES (?1, datetime('now'))", [id]).map(|_| ())?));
    }
    pub fn favorite_tag(&self, name: &str) {
        let (name, key) = (name.to_string(), name_key(name));
        self.exec(move |t| {
            t.execute("INSERT INTO tags(name, name_key, kind, track_count) VALUES (?1, ?2, 'genre', 0)", params![name, key])?;
            let id = t.last_insert_rowid();
            t.execute("INSERT INTO favorites(tag_id, created_at) VALUES (?1, datetime('now'))", [id])?;
            Ok(())
        });
    }

    /// An inbox row in the legacy test helper's shape.
    pub fn inbox(&self, url: &str, source_label: &str, in_wishlist: bool) -> i64 {
        self.inbox_with(url, "A Record", "An Artist", if in_wishlist { "wishlist" } else { "label" }, source_label, in_wishlist, &[])
    }

    #[allow(clippy::too_many_arguments)]
    pub fn inbox_with(&self, url: &str, title: &str, artist: &str, source_kind: &str, source_label: &str, in_wishlist: bool, tags: &[&str]) -> i64 {
        let (url, title, artist, sk, sl) = (url.to_string(), title.to_string(), artist.to_string(), source_kind.to_string(), source_label.to_string());
        let tag_list: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
        self.exec(move |t| {
            t.execute(
                "INSERT INTO harvest_items(url, url_kind, state, title, artist_name, tags, source_kind, source_label, in_collection, in_wishlist, \
                 is_free_download, is_purchasable, is_preorder, discovered_at) VALUES (?1,'album','new',?2,?3,?4,?5,?6,0,?7,0,1,0,datetime('now'))",
                params![url, title, artist, serde_json::to_string(&tag_list).unwrap_or_default(), sk, sl, in_wishlist],
            )?;
            let id = t.last_insert_rowid();
            bc_bandcamp::harvest::inbox::sync_tags(t, id, &tag_list).map_err(|e| bc_db::DbError::Other(e.to_string()))?;
            Ok(id)
        })
    }

    pub fn set_state(&self, id: i64, state: &str) {
        let state = state.to_string();
        self.exec(move |t| Ok(t.execute("UPDATE harvest_items SET state = ?2 WHERE id = ?1", params![id, state]).map(|_| ())?));
    }

    pub fn item_state(&self, id: i64) -> String {
        self.q(move |c| Ok(c.query_row("SELECT state FROM harvest_items WHERE id = ?1", [id], |r| r.get(0))?))
    }

    pub fn item_by_title(&self, title: &str) -> (String, Option<String>) {
        let title = title.to_string();
        self.q(move |c| Ok(c.query_row("SELECT state, label_name FROM harvest_items WHERE title = ?1", [title], |r| Ok((r.get(0)?, r.get(1)?)))?))
    }

    pub fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        self.q(move |c| Ok(c.query_row(&sql, [], |r| r.get(0))?))
    }

    /// `job_items.target_dir` of every download item (what the Python `_target_dirs` read).
    pub fn target_dirs(&self) -> Vec<Option<String>> {
        self.q(|c| {
            let mut st = c.prepare(
                "SELECT ji.target_dir FROM job_items ji JOIN jobs j ON j.id = ji.job_id WHERE j.kind = 'download' ORDER BY ji.id",
            )?;
            Ok(st.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// Params JSON of the first job.
    pub fn first_job_params(&self) -> Value {
        let text: String = self.q(|c| Ok(c.query_row("SELECT params FROM jobs WHERE kind = 'download' ORDER BY created_at LIMIT 1", [], |r| r.get(0))?));
        serde_json::from_str(&text).expect("params json")
    }
}

async fn send(rb: reqwest::RequestBuilder) -> (u16, Value) {
    let resp = rb.send().await.expect("request");
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

// -- canned streams (Python `_fake_pages`) ------------------------------------------------

pub type Grid = Vec<(String, String, String)>;

pub fn grid(entries: &[(&str, &str, &str)]) -> Grid {
    entries.iter().map(|(u, a, t)| (u.to_string(), a.to_string(), t.to_string())).collect()
}

pub fn shallow_event(url: &str, artist: &str, title: &str, seen: i64, total: i64) -> HarvestEvent {
    HarvestEvent {
        release: Some(sources::shallow(url, Shallow { title, artist, ..Default::default() })),
        seen,
        total: Some(total),
        ..Default::default()
    }
}

/// Stand in for `sources::harvest_artist` with a fixed grid per page URL (or a failure).
pub fn fake_pages(pages: Vec<(&str, Result<Grid, &str>)>) -> ArtistStreamFn {
    let map: HashMap<String, Result<Grid, String>> =
        pages.into_iter().map(|(u, g)| (u.to_string(), g.map_err(str::to_string))).collect();
    let map = Arc::new(map);
    Arc::new(move |_client, url, _depth, _limit| {
        let entry = map.get(&url).cloned();
        let s: EventStream = Box::pin(async_stream::try_stream! {
            match entry {
                Some(Ok(entries)) => {
                    let total = entries.len() as i64;
                    for (i, (u, a, t)) in entries.iter().enumerate() {
                        yield shallow_event(u, a, t, i as i64 + 1, total);
                    }
                }
                Some(Err(msg)) => Err(HarvestError::other(msg))?,
                None => Err(HarvestError::other(format!("unexpected page {url}")))?,
            }
        });
        s
    })
}

/// A page source serving canned pages; unknown URLs fail like a removed album (Python
/// `_FakeClient`), search answers from `results`.
pub struct FakePages {
    pub pages: HashMap<String, String>,
    pub results: Vec<SearchHit>,
    pub fetched: parking_lot::Mutex<Vec<String>>,
}

impl FakePages {
    pub fn new(pages: Vec<(String, String)>) -> Arc<Self> {
        Arc::new(Self { pages: pages.into_iter().collect(), results: vec![], fetched: Default::default() })
    }
    pub fn with_search(pages: Vec<(String, String)>, results: Vec<SearchHit>) -> Arc<Self> {
        Arc::new(Self { pages: pages.into_iter().collect(), results, fetched: Default::default() })
    }
    pub fn fetched(&self) -> Vec<String> {
        self.fetched.lock().clone()
    }
}

#[async_trait]
impl bc_bandcamp::harvest::labels::PageSource for FakePages {
    async fn page(&self, url: &str, _kind: PageKind, _ttl: Option<Duration>) -> HResult<String> {
        self.fetched.lock().push(url.to_string());
        self.pages.get(url).cloned().ok_or_else(|| HarvestError::other(format!("not found: {url}")))
    }
    async fn search(&self, _q: &str, _kind: &str, _limit: usize) -> HResult<Vec<SearchHit>> {
        Ok(self.results.clone())
    }
}

// -- HTML builders --------------------------------------------------------------------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A release page whose JSON-LD names `publisher`.
pub fn album_html(artist: &str, title: &str, publisher: &str) -> String {
    let ld = serde_json::json!({
        "@type": "MusicAlbum",
        "name": title,
        "byArtist": {"@type": "MusicGroup", "name": artist},
        "publisher": {"@type": "MusicGroup", "name": publisher},
    });
    format!(r#"<html><body><script type="application/ld+json">{ld}</script></body></html>"#)
}

/// A label's /music page: Bandcamp's own flag set, plus the discography.
pub fn label_page(name: &str, entries: &[(&str, &str, &str)]) -> String {
    let items: Vec<Value> = entries
        .iter()
        .map(|(url, artist, title)| serde_json::json!({"page_url": url, "title": title, "artist": artist, "type": "album"}))
        .collect();
    let band = serde_json::json!({"id": 1, "name": name, "is_label": true});
    format!(
        r#"<html><body><script data-band="{}"></script><ol id="music-grid" data-client-items="{}"></ol></body></html>"#,
        esc(&band.to_string()),
        esc(&Value::Array(items).to_string())
    )
}

/// A plain /music page (no label flag).
pub fn music_page(entries: &[(&str, &str, &str)]) -> String {
    let items: Vec<Value> = entries
        .iter()
        .map(|(url, artist, title)| serde_json::json!({"page_url": url, "title": title, "artist": artist, "type": "album"}))
        .collect();
    format!(r#"<html><body><ol id="music-grid" data-client-items="{}"></ol></body></html>"#, esc(&Value::Array(items).to_string()))
}


/// A plain event stream of shallow releases, as `sources::harvest_*` would yield them.
pub fn stream_of(entries: Grid) -> EventStream {
    Box::pin(async_stream::try_stream! {
        let total = entries.len() as i64;
        for (i, (u, a, t)) in entries.iter().enumerate() {
            yield shallow_event(u, a, t, i as i64 + 1, total);
        }
    })
}

impl App {
    /// An `album` inbox row in `state` (the Python `_item` helper of the label tests).
    pub fn album_item(&self, url: &str, artist: &str, title: &str, label: Option<&str>, state: &str) -> i64 {
        let (url, artist, title, label, state) =
            (url.to_string(), artist.to_string(), title.to_string(), label.map(str::to_string), state.to_string());
        self.exec(move |t| {
            t.execute(
                "INSERT INTO harvest_items(url, url_kind, state, title, artist_name, label_name, tags, in_collection, in_wishlist, \
                 is_free_download, is_purchasable, is_preorder, discovered_at) VALUES (?1,'album',?2,?3,?4,?5,'[]',0,0,0,1,0,datetime('now'))",
                params![url, state, title, artist, label],
            )?;
            Ok(t.last_insert_rowid())
        })
    }

    pub fn item_label(&self, id: i64) -> Option<String> {
        self.q(move |c| Ok(c.query_row("SELECT label_name FROM harvest_items WHERE id = ?1", [id], |r| r.get(0))?))
    }

    pub fn set_release_url(&self, id: i64, url: &str) {
        let url = url.to_string();
        self.exec(move |t| Ok(t.execute("UPDATE releases SET bandcamp_url = ?2 WHERE id = ?1", params![id, url]).map(|_| ())?));
    }

    pub fn set_release_label(&self, id: i64, label: i64) {
        self.exec(move |t| Ok(t.execute("UPDATE releases SET label_id = ?2 WHERE id = ?1", params![id, label]).map(|_| ())?));
    }
}

// -- a local fake Bandcamp (pages by host + path, JSON by API path) ------------------------

use axum::extract::State as AxState;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use bc_bandcamp::net::{BandcampClient, ClientOptions};
use parking_lot::Mutex;

#[derive(Default)]
struct FakeState {
    pages: HashMap<String, String>,
    api: HashMap<String, Value>,
    cookies: Vec<String>,
    hits: Vec<String>,
}

pub struct FakeBc {
    pub port: u16,
    pub client: BandcampClient,
    state: Arc<Mutex<FakeState>>,
}

impl FakeBc {
    /// Serve `body` for `http://<host>:PORT<path>`.
    pub fn page(&self, host: &str, path: &str, body: &str) {
        self.state.lock().pages.insert(format!("{host}{path}"), body.to_string());
    }
    /// Serve `body` as JSON for the API `path` on bandcamp.com.
    pub fn api(&self, path: &str, body: Value) {
        self.state.lock().api.insert(path.to_string(), body);
    }
    /// `http://<host>:PORT` for building URLs the client will reach.
    pub fn origin(&self, host: &str) -> String {
        format!("http://{host}:{}", self.port)
    }
    pub fn hits(&self) -> Vec<String> {
        self.state.lock().hits.clone()
    }
    /// The `Cookie` header of every API request, in order.
    pub fn cookies(&self) -> Vec<String> {
        self.state.lock().cookies.clone()
    }
}

async fn fake_handler(AxState(st): AxState<Arc<Mutex<FakeState>>>, headers: HeaderMap, uri: Uri) -> impl IntoResponse {
    let host = headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or("").split(':').next().unwrap_or("").to_string();
    let path = uri.path().to_string();
    let mut g = st.lock();
    g.hits.push(format!("{host}{path}"));
    if path.starts_with("/api/") {
        g.cookies.push(headers.get("cookie").and_then(|h| h.to_str().ok()).unwrap_or("<none>").to_string());
        return match g.api.get(&path) {
            Some(v) => (StatusCode::OK, axum::Json(v.clone())).into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    match g.pages.get(&format!("{host}{path}")) {
        Some(body) => (StatusCode::OK, [("content-type", "text/html")], body.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A fake Bandcamp and a client that reaches `hosts` (plus `bandcamp.com`, for the API) on it.
pub async fn fake_bandcamp(hosts: &[&str]) -> Arc<FakeBc> {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let router = axum::Router::new().fallback(fake_handler).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
    let mut resolve: Vec<(String, std::net::SocketAddr)> = hosts.iter().map(|h| (h.to_string(), addr)).collect();
    resolve.push(("bandcamp.com".into(), addr));
    let client = BandcampClient::new(ClientOptions {
        rate_per_sec: 1000.0,
        burst: 1000,
        backoff_scale: 0.001,
        resolve,
        api_origin: Some(format!("http://bandcamp.com:{}", addr.port())),
        ..Default::default()
    });
    Arc::new(FakeBc { port: addr.port(), client, state })
}

impl App {
    /// Everything published on the bus from now on.
    pub fn listen(&self) -> tokio::sync::broadcast::Receiver<bc_types::events::Event> {
        self.ctx.bus.subscribe()
    }
}

/// Drain whatever is already queued on a bus receiver.
pub fn drain_events(rx: &mut tokio::sync::broadcast::Receiver<bc_types::events::Event>) -> Vec<bc_types::events::Event> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e);
    }
    v
}
