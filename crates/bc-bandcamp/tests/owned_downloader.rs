//! Owned-quality downloads against a local server posing as Bandcamp (collection API, redownload
//! page, statdownload) and as the CDN: a purchase arrives as a FLAC zip and lands in the stream
//! downloader's layout; a release that is not a purchase falls back to the public stream.

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::Html;
use axum::routing::{get, post};
use bc_bandcamp::download::native::NativeDownloader;
use bc_bandcamp::download::slug::DEFAULT_TEMPLATE;
use bc_bandcamp::download::{DownloadSpec, Downloader, Outcome, OutcomeKind, Progress};
use bc_bandcamp::net::{BandcampClient, ClientOptions};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// A few valid MPEG-1 Layer III frames, for the public stream.
fn mp3_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..8 {
        let mut f = vec![0u8; 417];
        f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        out.extend_from_slice(&f);
    }
    out
}

/// A FLAC file with only its STREAMINFO block (44.1 kHz, stereo, 16 bit): enough to be read
/// back as complete. `tag` makes each file's bytes distinct.
fn flac_bytes(tag: u8) -> Vec<u8> {
    let mut v = b"fLaC".to_vec();
    v.extend_from_slice(&[0x80, 0x00, 0x00, 34]); // last block, STREAMINFO, 34 bytes
    v.extend_from_slice(&[0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0]); // block sizes, frame sizes
    let packed: u64 = (44_100u64 << 44) | (1 << 41) | (15 << 36);
    v.extend_from_slice(&packed.to_be_bytes());
    v.extend_from_slice(&[tag; 16]); // md5
    v
}

/// What a FLAC purchase zip of the two-track album holds.
fn purchase_zip() -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let o = zip::write::SimpleFileOptions::default();
    for (name, body) in [
        ("The Artist - The Album - 01 First Song.flac", flac_bytes(1)),
        ("The Artist - The Album - 02 Second Song.flac", flac_bytes(2)),
        ("cover.jpg", b"\xFF\xD8jpeg".to_vec()),
    ] {
        w.start_file(name, o).unwrap();
        w.write_all(&body).unwrap();
    }
    w.finish().unwrap().into_inner()
}

struct Site {
    /// The album is in the fan's collection.
    owned: bool,
    log: Vec<String>,
    cdn_cookie_seen: bool,
    stat_cookie_seen: bool,
}

type Shared = Arc<Mutex<Site>>;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn tralbum(cdn: SocketAddr) -> String {
    let trackinfo: Vec<Value> = ["First Song", "Second Song"]
        .iter()
        .enumerate()
        .map(|(i, t)| {
            json!({"title": t, "track_num": i + 1, "duration": 10.0, "title_link": format!("/track/t{i}"),
                   "track_id": 100 + i, "file": {"mp3-128": format!("http://{cdn}/stream/{i}.mp3")}})
        })
        .collect();
    let blob = json!({"item_type": "album", "id": 42, "artist": "The Artist", "album_release_date": "17 Jul 2023 00:00:00 GMT",
                      "current": {"title": "The Album", "band_id": 7}, "trackinfo": trackinfo});
    format!(r#"<html><body><script data-tralbum="{}"></script></body></html>"#, esc(&blob.to_string()))
}

struct Env {
    dl: NativeDownloader,
    site: Shared,
    page_url: String,
    _tmp: tempfile::TempDir,
    base: PathBuf,
}

async fn serve(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    a
}

async fn env(owned: bool) -> Env {
    let site: Shared = Arc::new(Mutex::new(Site { owned, log: Vec::new(), cdn_cookie_seen: false, stat_cookie_seen: false }));
    let cdn_app = Router::new()
        .route(
            "/owned.zip",
            get(|State(s): State<Shared>, h: HeaderMap| async move {
                let mut g = s.lock();
                g.log.push("cdn:zip".into());
                g.cdn_cookie_seen |= h.contains_key(header::COOKIE);
                purchase_zip()
            }),
        )
        .route(
            "/stream/{name}",
            get(|State(s): State<Shared>| async move {
                s.lock().log.push("cdn:stream".into());
                mp3_bytes()
            }),
        )
        .with_state(site.clone());
    let cdn = serve(cdn_app).await;

    // One server answers for every *.bandcamp.com host (resolve overrides below).
    let bc_addr: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
    let port = {
        let bc_addr = bc_addr.clone();
        move || bc_addr.lock().expect("bound").port()
    };
    let (p1, p2) = (port.clone(), port.clone());
    let app = Router::new()
        .route("/album/y", get(move || async move { Html(tralbum(cdn)) }))
        .route(
            "/api/fan/2/collection_summary",
            get(|State(s): State<Shared>| async move {
                s.lock().log.push("api:summary".into());
                axum::Json(json!({"fan_id": 9, "collection_summary": {"fan_id": 9, "username": "me"}}))
            }),
        )
        .route(
            "/api/fancollection/1/collection_items",
            post(move |State(s): State<Shared>| async move {
                let owned = {
                    let mut g = s.lock();
                    g.log.push("api:collection".into());
                    g.owned
                };
                let page = format!("http://x.bandcamp.com:{}/album/y", p1());
                let items = if owned {
                    json!([{"sale_item_type": "p", "sale_item_id": 5, "item_url": page, "tralbum_type": "a", "tralbum_id": 42,
                            "item_title": "The Album", "band_name": "The Artist"}])
                } else {
                    json!([{"sale_item_type": "p", "sale_item_id": 6, "item_url": "https://other.bandcamp.com/album/else",
                            "tralbum_type": "a", "item_title": "Else", "band_name": "Other"}])
                };
                axum::Json(json!({"items": items, "more_available": false, "last_token": "t",
                    "redownload_urls": {"p5": format!("http://bandcamp.com:{}/download?payment_id=5&sig=s", p1()),
                                        "p6": format!("http://bandcamp.com:{}/download?payment_id=6&sig=s", p1())}}))
            }),
        )
        .route("/api/fancollection/1/hidden_items", post(|| async { axum::Json(json!({"items": [], "more_available": false})) }))
        .route(
            "/download",
            get(move |State(s): State<Shared>| async move {
                s.lock().log.push("download-page".into());
                let fmt = |enc: &str| format!("http://popplers5.bandcamp.com:{}/download/album?enc={enc}&fsig=f&id=42&ts=1", p2());
                let blob = json!({"digital_items": [{"title": "The Album", "downloads": {
                    "mp3-320": {"size_mb": "20MB", "url": fmt("mp3-320")},
                    "flac": {"size_mb": "60MB", "url": fmt("flac")}}}]});
                Html(format!(r#"<html><body><div id="pagedata" data-blob="{}"></div></body></html>"#, esc(&blob.to_string())))
            }),
        )
        .route(
            "/statdownload/album",
            get(move |State(s): State<Shared>, h: HeaderMap| async move {
                let mut g = s.lock();
                g.log.push("statdownload".into());
                g.stat_cookie_seen |= h.contains_key(header::COOKIE);
                format!(r#"if ( window.Downloads ) {{ Downloads.statResult ( {{"result":"ok","download_url":"http://{cdn}/owned.zip"}} ) }};"#)
            }),
        )
        .with_state(site.clone());
    let bc = serve(app).await;
    *bc_addr.lock() = Some(bc);

    let client = BandcampClient::new(ClientOptions {
        cookie: Some("identity=SECRET".into()),
        resolve: vec![("x.bandcamp.com".into(), bc), ("bandcamp.com".into(), bc), ("popplers5.bandcamp.com".into(), bc)],
        api_origin: Some(format!("http://bandcamp.com:{}", bc.port())),
        backoff_scale: 0.001,
        ..Default::default()
    });
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("dl");
    std::fs::create_dir_all(&base).unwrap();
    Env { dl: NativeDownloader::new(client, DEFAULT_TEMPLATE), site, page_url: format!("http://x.bandcamp.com:{}/album/y", bc.port()), _tmp: tmp, base }
}

async fn run(e: &Env, format: Option<&str>) -> (Outcome, Vec<Progress>) {
    let mut spec = DownloadSpec::new(&e.page_url, &e.base);
    spec.format = format.map(str::to_string);
    let mut events = Vec::new();
    let mut cb = |p: Progress| events.push(p);
    let o = e.dl.download(&spec, &mut cb, &CancellationToken::new()).await;
    (o, events)
}

fn files_under(dir: &Path) -> Vec<String> {
    fn rec(root: &Path, d: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                rec(root, &p, out);
            } else {
                out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut v = Vec::new();
    rec(dir, dir, &mut v);
    v.sort();
    v
}

#[tokio::test]
async fn a_purchase_comes_from_the_collection_in_flac() {
    let e = env(true).await;
    let (o, progress) = run(&e, Some("flac")).await;
    assert_eq!(o.kind, OutcomeKind::Ok, "{}", o.detail);
    assert!(o.detail.contains("FLAC"), "{}", o.detail);
    assert_eq!(o.tracks_expected, Some(2));
    assert_eq!(o.new_files.len(), 2);
    // the stream downloader's layout, the purchase's extension; no cover file, no leftovers
    assert_eq!(files_under(&e.base), ["the-artist/the-album/01 - first-song.flac", "the-artist/the-album/02 - second-song.flac"]);
    assert_eq!(std::fs::read(e.base.join("the-artist/the-album/01 - first-song.flac")).unwrap(), flac_bytes(1));
    {
        let g = e.site.lock();
        assert!(!g.log.iter().any(|l| l == "cdn:stream"), "the stream was fetched too: {:?}", g.log);
        assert!(g.log.iter().any(|l| l == "statdownload"));
        assert!(g.stat_cookie_seen, "statdownload is a Bandcamp host and needs the cookie");
        assert!(!g.cdn_cookie_seen, "the cookie must never reach the CDN");
    }
    assert_eq!(progress.last().map(|p| p.phase.as_str()), Some("Finished"));

    // a second run finds the files in place
    let (again, _) = run(&e, Some("flac")).await;
    assert_eq!(again.kind, OutcomeKind::AlreadyHave, "{}", again.detail);
}

#[tokio::test]
async fn a_release_that_is_not_a_purchase_comes_from_the_stream() {
    let e = env(false).await;
    let (o, _) = run(&e, Some("flac")).await;
    assert_eq!(o.kind, OutcomeKind::Ok, "{}", o.detail);
    assert!(o.detail.contains("FLAC was not used (not in your collection)"), "{}", o.detail);
    assert_eq!(files_under(&e.base), ["the-artist/the-album/01 - first-song.mp3", "the-artist/the-album/02 - second-song.mp3"]);
    assert!(!e.site.lock().log.iter().any(|l| l == "download-page"));
}

#[tokio::test]
async fn without_a_format_the_collection_is_never_read() {
    let e = env(true).await;
    let (o, _) = run(&e, None).await;
    assert_eq!(o.kind, OutcomeKind::Ok, "{}", o.detail);
    assert!(files_under(&e.base).iter().all(|f| f.ends_with(".mp3")));
    assert!(!e.site.lock().log.iter().any(|l| l.starts_with("api:")), "{:?}", e.site.lock().log);
}
