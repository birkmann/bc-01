//! Test helpers: a hand-built library (the Python `Lib` fixture) over a temp [`Db`], and a tiny
//! HTTP client driving the router with `tower::ServiceExt::oneshot`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_db::rusqlite::params;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

use crate::service::RecommendService;
use crate::sqlutil::name_key;

pub struct Lib {
    pub db: Db,
    _dir: tempfile::TempDir,
    root_id: i64,
    artists: HashMap<String, i64>,
    labels: HashMap<String, i64>,
    tags: HashMap<String, i64>,
}

/// One track to add (`Lib::add`).
#[derive(Clone)]
pub struct T {
    pub title: String,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub energy: Option<f64>,
    pub tags: Vec<String>,
    pub artist: String,
    pub label: Option<String>,
    pub missing: bool,
    pub analysed: bool,
    pub duration_ms: Option<i64>,
    pub loved: bool,
}

impl T {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.into(),
            bpm: None,
            camelot: None,
            energy: None,
            tags: vec![],
            artist: "Someone".into(),
            label: None,
            missing: false,
            analysed: true,
            duration_ms: Some(300_000),
            loved: false,
        }
    }
    pub fn bpm(mut self, b: f64) -> Self {
        self.bpm = Some(b);
        self
    }
    pub fn key(mut self, k: &str) -> Self {
        self.camelot = Some(k.into());
        self
    }
    pub fn energy(mut self, e: f64) -> Self {
        self.energy = Some(e);
        self
    }
    pub fn tags(mut self, t: &[&str]) -> Self {
        self.tags = t.iter().map(|s| s.to_string()).collect();
        self
    }
    pub fn artist(mut self, a: &str) -> Self {
        self.artist = a.into();
        self
    }
    pub fn label(mut self, l: &str) -> Self {
        self.label = Some(l.into());
        self
    }
    pub fn missing(mut self) -> Self {
        self.missing = true;
        self
    }
    pub fn unanalysed(mut self) -> Self {
        self.analysed = false;
        self
    }
    pub fn duration(mut self, d: i64) -> Self {
        self.duration_ms = Some(d);
        self
    }
    pub fn loved(mut self) -> Self {
        self.loved = true;
        self
    }
}

const NOW: &str = "strftime('%Y-%m-%d %H:%M:%f','now')";

impl Lib {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        let db = Db::open(&path).unwrap();
        let root_id = db
            .write(|t| {
                t.execute("INSERT INTO library_roots(path, kind, watch, enabled) VALUES ('/music','library',0,1)", [])?;
                Ok(t.last_insert_rowid())
            })
            .unwrap();
        Self { db, _dir: dir, root_id, artists: HashMap::new(), labels: HashMap::new(), tags: HashMap::new() }
    }

    pub fn artist_id(&self, name: &str) -> i64 {
        self.artists[name]
    }
    pub fn label_id(&self, name: &str) -> i64 {
        self.labels[name]
    }

    pub fn add(&mut self, spec: T) -> i64 {
        let root = self.root_id;
        let mut artists = std::mem::take(&mut self.artists);
        let mut labels = std::mem::take(&mut self.labels);
        let mut tags = std::mem::take(&mut self.tags);
        let (id, artists, labels, tags) = self
            .db
            .write(move |t| {
                let artist_id = match artists.get(&spec.artist) {
                    Some(i) => *i,
                    None => {
                        t.execute(
                            &format!("INSERT INTO artists(name, name_key, created_at) VALUES (?1, ?2, {NOW})"),
                            params![spec.artist, name_key(&spec.artist)],
                        )?;
                        let i = t.last_insert_rowid();
                        artists.insert(spec.artist.clone(), i);
                        i
                    }
                };
                let label_id = match &spec.label {
                    None => None,
                    Some(l) => Some(match labels.get(l) {
                        Some(i) => *i,
                        None => {
                            t.execute("INSERT INTO labels(name, name_key) VALUES (?1, ?2)", params![l, name_key(l)])?;
                            let i = t.last_insert_rowid();
                            labels.insert(l.clone(), i);
                            i
                        }
                    }),
                };
                let rt = format!("{} EP", spec.title);
                t.execute(
                    &format!("INSERT INTO releases(title, title_key, artist_id, label_id, kind, added_at) VALUES (?1, ?2, ?3, ?4, 'album', {NOW})"),
                    params![rt, name_key(&rt), artist_id, label_id],
                )?;
                let release_id = t.last_insert_rowid();
                t.execute(
                    &format!(
                        "INSERT INTO tracks(release_id, artist_id, title, title_key, duration_ms, loved, play_count, skip_count, added_at) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, {NOW})"
                    ),
                    params![release_id, artist_id, spec.title, name_key(&spec.title), spec.duration_ms, spec.loved],
                )?;
                let tid = t.last_insert_rowid();
                t.execute(
                    &format!(
                        "INSERT INTO files(track_id, root_id, path, rel_path, ext, size_bytes, mtime_ns, first_seen_at, last_seen_at, missing_since) \
                         VALUES (?1, ?2, ?3, ?4, '.mp3', 1, 0, {NOW}, {NOW}, {})",
                        if spec.missing { NOW } else { "NULL" }
                    ),
                    params![tid, root, format!("/music/{tid}.mp3"), format!("{tid}.mp3")],
                )?;
                if spec.analysed {
                    t.execute(
                        &format!(
                            "INSERT INTO analysis(track_id, analyzer_version, backend, status, analyzed_at, bpm, camelot, energy) \
                             VALUES (?1, 1, 'test', 'ok', {NOW}, ?2, ?3, ?4)"
                        ),
                        params![tid, spec.bpm, spec.camelot, spec.energy],
                    )?;
                }
                for name in &spec.tags {
                    let tag_id = match tags.get(name) {
                        Some(i) => *i,
                        None => {
                            t.execute(
                                "INSERT INTO tags(name, name_key, kind, track_count) VALUES (?1, ?2, 'genre', 0)",
                                params![name, name_key(name)],
                            )?;
                            let i = t.last_insert_rowid();
                            tags.insert(name.clone(), i);
                            i
                        }
                    };
                    t.execute("INSERT INTO track_tags(track_id, tag_id, source, weight) VALUES (?1, ?2, 'bandcamp', 1.0)", params![tid, tag_id])?;
                    t.execute("UPDATE tags SET track_count = track_count + 1 WHERE id = ?1", [tag_id])?;
                }
                Ok((tid, artists, labels, tags))
            })
            .unwrap();
        self.artists = artists;
        self.labels = labels;
        self.tags = tags;
        id
    }

    pub fn exec(&self, sql: &str, p: impl bc_db::rusqlite::Params + Send + 'static) {
        let sql = sql.to_string();
        self.db.write(move |t| Ok(t.execute(&sql, p)?)).unwrap();
    }

    pub fn love(&self, id: i64) {
        self.exec("UPDATE tracks SET loved = 1 WHERE id = ?1", [id]);
    }

    /// A playlist holding `ids` in order.
    pub fn playlist(&self, name: &str, ids: &[i64]) -> i64 {
        let (name, ids) = (name.to_string(), ids.to_vec());
        self.db
            .write(move |t| {
                t.execute(
                    &format!("INSERT INTO playlists(name, kind, created_at, updated_at) VALUES (?1, 'manual', {NOW}, {NOW})"),
                    [name],
                )?;
                let pid = t.last_insert_rowid();
                for (i, tid) in ids.iter().enumerate() {
                    t.execute(
                        &format!("INSERT INTO playlist_items(playlist_id, track_id, position, added_at) VALUES (?1, ?2, ?3, {NOW})"),
                        params![pid, tid, 1024.0 * (i as f64 + 1.0)],
                    )?;
                }
                Ok(pid)
            })
            .unwrap()
    }

    /// A DJ set with the given pool-sources JSON and optional target.
    pub fn dj_set(&self, name: &str, target_minutes: Option<i64>, pool_sources: &str) -> i64 {
        let (name, ps) = (name.to_string(), pool_sources.to_string());
        self.db
            .write(move |t| {
                t.execute(
                    &format!(
                        "INSERT INTO dj_sets(name, target_minutes, status, created_at, updated_at, pool_sources) VALUES (?1, ?2, 'draft', {NOW}, {NOW}, ?3)"
                    ),
                    params![name, target_minutes, ps],
                )?;
                Ok(t.last_insert_rowid())
            })
            .unwrap()
    }

    /// Append items to a set (what the legacy `POST /sets/{id}/items` did); returns item ids.
    pub fn set_add(&self, set_id: i64, track_ids: &[i64]) -> Vec<i64> {
        let ids = track_ids.to_vec();
        self.db
            .write(move |t| {
                let mut last: f64 = t.query_row("SELECT COALESCE(max(position), 0) FROM dj_set_items WHERE set_id = ?1", [set_id], |r| r.get(0))?;
                let mut out = vec![];
                for tid in ids {
                    last += 1024.0;
                    t.execute(
                        "INSERT INTO dj_set_items(set_id, track_id, position, tempo_adjust_pct, key_lock, snapshot) VALUES (?1, ?2, ?3, 0, 1, '{}')",
                        params![set_id, tid, last],
                    )?;
                    out.push(t.last_insert_rowid());
                }
                Ok(out)
            })
            .unwrap()
    }

    /// Index every track into the FTS table (the legacy `fts.rebuild_all`).
    pub fn reindex(&self) {
        self.exec(
            "INSERT INTO search_index(title, artist, album, label, tags, track_id) \
             SELECT t.title, COALESCE(ar.name,''), COALESCE(r.title,''), COALESCE(lb.name,''), \
                    COALESCE((SELECT group_concat(g.name, ' ') FROM track_tags tt JOIN tags g ON g.id = tt.tag_id WHERE tt.track_id = t.id), ''), t.id \
             FROM tracks t LEFT JOIN releases r ON r.id = t.release_id \
             LEFT JOIN artists ar ON ar.id = COALESCE(t.artist_id, r.artist_id) LEFT JOIN labels lb ON lb.id = r.label_id",
            [],
        );
    }

    pub fn service(&self) -> RecommendService {
        RecommendService::new(self.db.clone(), Arc::new(EventBus::new()), Arc::new(Config::from_env()))
    }

    pub fn count(&self, sql: &str) -> i64 {
        let sql = sql.to_string();
        self.db.read(move |c| Ok(c.query_row(&sql, [], |r| r.get(0))?)).unwrap()
    }
}

/// One request against the router: `(status, json body)`.
pub async fn call(lib: &Lib, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let app = lib.service().router();
    let mut req = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

pub async fn post(lib: &Lib, uri: &str, body: Value) -> (StatusCode, Value) {
    call(lib, Method::POST, uri, Some(body)).await
}

pub async fn get(lib: &Lib, uri: &str) -> (StatusCode, Value) {
    call(lib, Method::GET, uri, None).await
}

/// `items[*].track.id` of a suggest-style response.
pub fn ids(body: &Value) -> Vec<i64> {
    body["items"].as_array().unwrap().iter().map(|i| i["track"]["id"].as_i64().unwrap()).collect()
}
