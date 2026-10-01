//! A fake Bandcamp (and fake CDN) for black-box tests.
//!
//! `urls::normalise` drops the port (legacy behaviour), so a client cannot be pointed at a local
//! port through a URL. Instead the whole test process routes its plain-HTTP traffic through an
//! HTTP proxy that *is* the fake: reqwest honours `http_proxy`, sends the absolute-form request
//! (`GET http://x-1.bandcamp.com/album/y`) to the fake, and the fake picks the per-test "site"
//! by `Host`. Every test registers its own uniquely named host, so tests stay independent.
//!
//! The server runs on its own thread + runtime so it outlives any single `#[tokio::test]`.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use bc_bandcamp::net::{BandcampClient, ClientOptions, PageCache};
use bc_bandcamp::service::Ctx;
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_jobs::JobsService;
use serde_json::Value;

/// `html.escape(json.dumps(v))`: how the Python fixtures embed blobs in attributes.
pub fn esc_json(v: &Value) -> String {
    esc(&v.to_string())
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

// -- requests / responses ----------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Hit {
    pub method: String,
    pub path: String,
    pub query: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Hit {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
    }
}

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self { status: 200, headers: vec![], body: body.into() }
    }
    pub fn html(body: &str) -> Self {
        Self { status: 200, headers: vec![("content-type".into(), "text/html; charset=utf-8".into())], body: body.as_bytes().to_vec() }
    }
    pub fn json(v: &Value) -> Self {
        Self { status: 200, headers: vec![("content-type".into(), "application/json".into())], body: v.to_string().into_bytes() }
    }
    pub fn status(status: u16) -> Self {
        Self { status, headers: vec![], body: vec![] }
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

type Handler = Arc<dyn Fn(&Hit) -> Resp + Send + Sync>;

#[derive(Default)]
struct SiteState {
    routes: Mutex<HashMap<(String, String), Handler>>,
    hits: Mutex<Vec<Hit>>,
}

#[derive(Default)]
struct Registry {
    sites: Mutex<HashMap<String, Arc<SiteState>>>,
}

struct Global {
    registry: Arc<Registry>,
    seq: AtomicUsize,
}

static GLOBAL: OnceLock<Global> = OnceLock::new();

fn global() -> &'static Global {
    GLOBAL.get_or_init(|| {
        let registry = Arc::new(Registry::default());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake");
        listener.set_nonblocking(true).expect("nonblocking");
        let addr: SocketAddr = listener.local_addr().expect("addr");
        // Must happen before the first reqwest client of the process is built.
        let proxy = format!("http://{addr}");
        // SAFETY: set once, before any client (and any other thread reading the env) exists.
        unsafe {
            for k in ["http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"] {
                std::env::set_var(k, &proxy);
            }
            for k in ["no_proxy", "NO_PROXY", "https_proxy", "HTTPS_PROXY"] {
                std::env::remove_var(k);
            }
        }
        let reg = registry.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("rt");
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).expect("listener");
                let app = Router::new().fallback(handle).with_state(reg);
                let _ = axum::serve(listener, app).await;
            });
        });
        Global { registry, seq: AtomicUsize::new(0) }
    })
}

async fn handle(State(reg): State<Arc<Registry>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let host = parts
        .uri
        .host()
        .map(str::to_string)
        .or_else(|| parts.headers.get("host").and_then(|h| h.to_str().ok()).map(|h| h.split(':').next().unwrap_or("").to_string()))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let bytes = to_bytes(body, 8 * 1024 * 1024).await.unwrap_or_default();
    let hit = Hit {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or("").to_string(),
        headers: parts.headers.clone(),
        body: bytes.to_vec(),
    };
    let site = reg.sites.lock().unwrap().get(&host).cloned();
    let Some(site) = site else {
        return Response::builder().status(StatusCode::BAD_GATEWAY).body(Body::from(format!("no fake site for {host}"))).unwrap();
    };
    site.hits.lock().unwrap().push(hit.clone());
    let handler = site.routes.lock().unwrap().get(&(hit.method.clone(), hit.path.clone())).cloned();
    let resp = match handler {
        Some(h) => h(&hit),
        None => Resp::status(404),
    };
    let mut b = Response::builder().status(StatusCode::from_u16(resp.status).unwrap_or(StatusCode::OK));
    for (k, v) in &resp.headers {
        b = b.header(k.as_str(), v.as_str());
    }
    b.body(Body::from(resp.body)).unwrap()
}

// -- sites -----------------------------------------------------------------------------------

/// One fake host (`<label>-<n>.bandcamp.com`, or `.bcbits.com` for a CDN).
#[derive(Clone)]
pub struct Site {
    pub host: String,
    state: Arc<SiteState>,
}

impl Site {
    /// A fake Bandcamp host.
    pub fn new(label: &str) -> Site {
        Self::on(label, "bandcamp.com")
    }

    /// A fake CDN host (`*.bcbits.com`).
    pub fn cdn(label: &str) -> Site {
        Self::on(label, "bcbits.com")
    }

    pub fn on(label: &str, domain: &str) -> Site {
        let g = global();
        let host = format!("{label}-{}.{domain}", g.seq.fetch_add(1, Ordering::SeqCst));
        let state = Arc::new(SiteState::default());
        g.registry.sites.lock().unwrap().insert(host.clone(), state.clone());
        Site { host, state }
    }

    /// `http://<host><path>`
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.host)
    }

    pub fn route(&self, method: &str, path: &str, f: impl Fn(&Hit) -> Resp + Send + Sync + 'static) -> &Self {
        self.state.routes.lock().unwrap().insert((method.to_string(), path.to_string()), Arc::new(f));
        self
    }

    pub fn page(&self, path: &str, html: &str) -> &Self {
        let html = html.to_string();
        self.route("GET", path, move |_| Resp::html(&html))
    }

    pub fn post_json(&self, path: &str, v: Value) -> &Self {
        self.route("POST", path, move |_| Resp::json(&v))
    }

    pub fn hits(&self) -> Vec<Hit> {
        self.state.hits.lock().unwrap().clone()
    }

    pub fn hits_to(&self, method: &str, path: &str) -> Vec<Hit> {
        self.hits().into_iter().filter(|h| h.method == method && h.path == path).collect()
    }

    pub fn count(&self, path: &str) -> usize {
        self.hits().iter().filter(|h| h.path == path).count()
    }

    /// A client whose API origin is this site (`/api/...` calls land here) and whose page cache
    /// is in memory.
    pub fn client(&self) -> BandcampClient {
        self.client_with(|_| {})
    }

    pub fn client_with(&self, tweak: impl FnOnce(&mut ClientOptions)) -> BandcampClient {
        let mut o = ClientOptions {
            rate_per_sec: 1000.0,
            burst: 1000,
            reserved_rate_per_sec: 1000.0,
            reserved_burst: 1000,
            concurrency: 8,
            api_origin: Some(format!("http://{}", self.host)),
            backoff_scale: 0.001,
            cache: PageCache::open_in_memory(64 * 1024 * 1024).ok().map(PageCache::shared),
            ..ClientOptions::default()
        };
        tweak(&mut o);
        BandcampClient::new(o)
    }
}

/// `Range`-aware byte server for a fake CDN route (`Accept-Ranges`, 206, 416).
pub fn range_bytes(data: Vec<u8>, content_type: &'static str) -> impl Fn(&Hit) -> Resp + Send + Sync + 'static {
    move |hit: &Hit| {
        let total = data.len();
        let base = |status: u16, body: Vec<u8>| Resp {
            status,
            headers: vec![("content-type".into(), content_type.into()), ("accept-ranges".into(), "bytes".into())],
            body,
        };
        let Some(range) = hit.header("range") else {
            return base(200, data.clone());
        };
        let spec = range.trim().strip_prefix("bytes=").unwrap_or("");
        let (a, b) = spec.split_once('-').unwrap_or(("", ""));
        let (start, end) = match (a.parse::<usize>(), b.parse::<usize>()) {
            (Ok(s), Ok(e)) => (s, e.min(total.saturating_sub(1))),
            (Ok(s), Err(_)) => (s, total.saturating_sub(1)),
            (Err(_), Ok(n)) => (total.saturating_sub(n), total.saturating_sub(1)),
            _ => return base(200, data.clone()),
        };
        if start >= total || start > end {
            return base(416, vec![]).header("content-range", &format!("bytes */{total}"));
        }
        base(206, data[start..=end].to_vec()).header("content-range", &format!("bytes {start}-{end}/{total}"))
    }
}

// -- a Ctx over a temp DB ------------------------------------------------------------------------

pub struct TestCtx {
    pub ctx: Arc<Ctx>,
    pub site: Site,
    pub dir: tempfile::TempDir,
}

pub fn test_config(dir: &std::path::Path) -> Config {
    let mut cfg = Config::from_env();
    cfg.data_dir = dir.join("data");
    cfg.download_dir = dir.join("downloads");
    cfg.library_root = None;
    let _ = cfg.ensure_dirs();
    cfg
}

/// A `Ctx` (real legacy schema in a temp DB, real job store) whose client talks to `site`.
pub fn test_ctx(site: &Site) -> TestCtx {
    test_ctx_with(site, site.client())
}

pub fn test_ctx_with(site: &Site, client: BandcampClient) -> TestCtx {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open(dir.path().join("lib.db")).expect("open db");
    let bus = Arc::new(EventBus::new());
    let jobs = JobsService::new(db.clone(), bus.clone());
    let ctx = Ctx::with_client(db, bus, test_config(dir.path()), jobs, client);
    TestCtx { ctx, site: site.clone(), dir }
}

// -- HTTP helpers over a router -------------------------------------------------------------------

use tower::ServiceExt;

pub async fn call(app: &Router, req: Request) -> (StatusCode, HeaderMap, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let (parts, body) = resp.into_parts();
    let bytes = to_bytes(body, 64 * 1024 * 1024).await.expect("body").to_vec();
    (parts.status, parts.headers, bytes)
}

pub async fn get(app: &Router, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
    call(app, Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap()).await
}

pub async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
    let (s, _, b) = get(app, uri).await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

pub async fn post_json(app: &Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (s, _, b) = call(app, req).await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

/// Percent-encode a query value.
pub fn q(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

// -- page fixtures -------------------------------------------------------------------------------

use serde_json::json;

/// A release page reduced to the blob the parsers read.
pub fn tralbum_page(t: &Value) -> String {
    format!(r#"<html><body><script data-tralbum="{}"></script></body></html>"#, esc_json(t))
}

/// The `test_explore.py` TRALBUM, with the streams on `cdn` (an absolute `http://host` base; the
/// extractor keeps absolute URLs as they are).
pub fn explore_tralbum(cdn: &str) -> Value {
    json!({
        "for the curious": "note",
        "item_type": "album",
        "id": 1,
        "artist": "Somatic",
        "current": {"title": "Grid Failure", "band_id": 2, "minimum_price": 0.0},
        "trackinfo": [
            {"title": "Nocturnal Transit", "track_num": 1, "track_id": 111,
             "file": {"mp3-128": format!("{cdn}/stream/abc/mp3-128/111?token=1")}},
            {"title": "Iron Lung", "track_num": 2, "track_id": 112,
             "file": {"mp3-128": format!("{cdn}/stream/def/mp3-128/112?token=2")}},
            {"title": "Unreleased", "track_num": 3, "track_id": 113},
        ],
    })
}

/// A `/music` page: the band blob plus the grid whose `data-client-items` holds `tail`.
pub fn music_page(band: &Value, tail: Option<&[Value]>) -> String {
    let grid = match tail {
        Some(t) => format!(r#"<ol id="music-grid" data-client-items="{}"></ol>"#, esc_json(&Value::Array(t.to_vec()))),
        None => r#"<ol id="music-grid"></ol>"#.to_string(),
    };
    format!(r#"<html><body><script data-band="{}"></script>{grid}</body></html>"#, esc_json(band))
}

/// One `data-client-items` entry.
pub fn grid_entry(slug: &str, title: &str, artist: &str, id: i64) -> Value {
    json!({"page_url": format!("/album/{slug}"), "title": title, "artist": artist, "type": "album",
           "id": id, "band_id": 7, "art_id": 4000000000i64 + id})
}

/// A discover-API result row.
pub fn discover_row(url: &str, title: &str, band: &str, id: i64) -> Value {
    json!({"item_url": format!("{url}?from=discover_page"), "title": title, "band_name": band,
           "item_id": id, "band_id": 7, "result_type": "a", "primary_image": {"image_id": 4000000000i64 + id}})
}

pub fn collectors_page(blob: Option<&Value>, tralbum: Option<&Value>) -> String {
    let tr = tralbum.cloned().unwrap_or_else(|| {
        json!({"id": 503240863, "item_type": "album", "artist": "Phil Berg",
               "current": {"title": "Dārin"}, "trackinfo": [], "for the curious": "x"})
    });
    let mut parts = vec![format!(r#"<script data-tralbum="{}"></script>"#, esc_json(&tr))];
    if let Some(b) = blob {
        parts.push(format!(r#"<div id="collectors-data" data-blob="{}"></div>"#, esc_json(b)));
    }
    format!("<html><body>{}</body></html>", parts.concat())
}

/// A fan page reduced to the blob the parser reads.
pub fn fan_html(collection: &[Value], wishlist: &[Value], last_token: Option<&str>, counts: (i64, i64)) -> String {
    let keyed = |v: &[Value]| -> Value { Value::Object(v.iter().enumerate().map(|(i, e)| (format!("a{i}"), e.clone())).collect()) };
    let mut coll = json!({"item_count": counts.0});
    let mut wish = json!({"item_count": counts.1});
    if let Some(t) = last_token {
        coll["last_token"] = json!(t);
        wish["last_token"] = json!(t);
    }
    let blob = json!({
        "fan_data": {"fan_id": 77, "username": "alice", "name": "Alice"},
        "collection_data": coll,
        "wishlist_data": wish,
        "item_cache": {"collection": keyed(collection), "wishlist": keyed(wishlist)},
    });
    format!(r#"<html><body><div id="pagedata" data-blob="{}"></div></body></html>"#, esc_json(&blob))
}

pub fn fan_item(slug: &str, title: &str) -> Value {
    json!({"item_url": format!("https://a.bandcamp.com/album/{slug}"), "item_title": title,
           "band_name": "A Band", "tralbum_type": "a"})
}

// -- library rows ----------------------------------------------------------------------------------

use bc_db::rusqlite::params;

/// A release as a scan off disk leaves it: named, with no Bandcamp URL.
pub fn owned_release(db: &Db, artist: &str, title: &str) -> i64 {
    let (artist, title) = (artist.to_string(), title.to_string());
    db.write(move |tx| {
        let akey = bc_db::util::name_key(&artist);
        tx.execute("INSERT OR IGNORE INTO artists(name, name_key, created_at) VALUES (?1, ?2, datetime('now'))", params![artist, akey])?;
        let aid: i64 = tx.query_row("SELECT id FROM artists WHERE name_key = ?1", [&akey], |r| r.get(0))?;
        tx.execute(
            "INSERT INTO releases(title, title_key, kind, artist_id, added_at) VALUES (?1, ?2, 'album', ?3, datetime('now'))",
            params![title, bc_db::util::name_key(&title), aid],
        )?;
        Ok(tx.last_insert_rowid())
    })
    .expect("owned_release")
}

pub fn blacklist_url(db: &Db, url: &str) {
    let key = bc_bandcamp::download::dedup::url_key(url);
    let url = url.to_string();
    db.write(move |tx| {
        tx.execute(
            "INSERT INTO blacklist(url_key, url, artist_name, title, added_at) VALUES (?1, ?2, 'x', 'x', datetime('now'))",
            params![key, url],
        )?;
        Ok(())
    })
    .expect("blacklist");
}

/// Every item of a job, in order.
/// `(status, message, url, target_dir)`
pub type JobItemRow = (String, Option<String>, Option<String>, Option<String>);

pub fn job_items(db: &Db, job_id: &str) -> Vec<JobItemRow> {
    let id = job_id.to_string();
    db.read(move |c| {
        let mut st = c.prepare("SELECT status, message, url, target_dir FROM job_items WHERE job_id = ?1 ORDER BY seq")?;
        let rows = st.query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    })
    .expect("job_items")
}
