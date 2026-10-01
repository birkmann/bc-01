//! Native downloader tests against local axum servers posing as Bandcamp (page,
//! `x.bandcamp.com` via a resolve override) and as the audio/art CDN.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bc_bandcamp::download::native::NativeDownloader;
use bc_bandcamp::download::slug::{self, DEFAULT_TEMPLATE, FLAT_TEMPLATE, TrackMeta};
use bc_bandcamp::download::{DownloadSpec, Downloader, Outcome, OutcomeKind, Progress};
use bc_bandcamp::net::{BandcampClient, ClientOptions};
use lofty::file::TaggedFileExt;
use lofty::picture::PictureType;
use lofty::prelude::{Accessor, ItemKey};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A few valid MPEG-1 Layer III 128 kbps / 44.1 kHz frames (417 bytes each).
fn mp3_bytes(frames: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..frames {
        let mut f = vec![0u8; 417];
        f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        out.extend_from_slice(&f);
    }
    out
}

const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0fake-jpeg-cover-bytes\xFF\xD9";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Ok,
    /// Declares the full length, errors halfway.
    Truncated,
    NotFound,
    /// Sends a few bytes, then stalls forever.
    Stall,
    /// Many small chunks with a delay in between.
    Slow,
}

#[derive(Clone)]
struct T {
    title: String,
    num: Option<i64>,
    artist: Option<String>,
    streamable: bool,
    mode: Mode,
}

fn t(title: &str, num: i64) -> T {
    T { title: title.into(), num: Some(num), artist: None, streamable: true, mode: Mode::Ok }
}

struct Site {
    tracks: Vec<T>,
    item_type: &'static str,
    art_ok: bool,
    page_status: StatusCode,
    log: Vec<String>,
    cdn_cookie_seen: bool,
}

type Shared = Arc<Mutex<Site>>;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

fn page(site: &Site, cdn: SocketAddr) -> String {
    let trackinfo: Vec<Value> = site
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let file = if t.streamable { json!({"mp3-128": format!("http://{cdn}/cdn/{i}.mp3")}) } else { Value::Null };
            json!({"title": t.title, "track_num": t.num, "duration": 10.0,
                   "title_link": format!("/track/t{i}"), "track_id": 100 + i, "artist": t.artist, "file": file})
        })
        .collect();
    let blob = json!({
        "for the curious": "scrapers, hello",
        "item_type": site.item_type,
        "id": 42,
        "artist": "The Artist",
        "album_release_date": "17 Jul 2023 00:00:00 GMT",
        "current": {"title": "The Album", "band_id": 7},
        "trackinfo": trackinfo,
    });
    format!(
        r#"<html><body><script data-tralbum="{}"></script>
<div class="tralbum-tags"><a class="tag">techno</a><a class="tag">industrial</a></div>
<div id="tralbumArt"><img src="http://{cdn}/art.jpg"/></div></body></html>"#,
        esc(&blob.to_string())
    )
}

struct Env {
    dl: NativeDownloader,
    site: Shared,
    page_url: String,
    tmp: tempfile::TempDir,
    base: PathBuf,
}

async fn serve(app: Router) -> SocketAddr {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let a = l.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(l, app).await;
    });
    a
}

async fn env(tracks: Vec<T>) -> Env {
    env_with(tracks, "album").await
}

async fn env_with(tracks: Vec<T>, item_type: &'static str) -> Env {
    env_full(tracks, item_type, None).await
}

async fn env_full(tracks: Vec<T>, item_type: &'static str, bucket: Option<(f64, u32)>) -> Env {
    let site: Shared = Arc::new(Mutex::new(Site {
        tracks,
        item_type,
        art_ok: true,
        page_status: StatusCode::OK,
        log: Vec::new(),
        cdn_cookie_seen: false,
    }));

    // CDN
    let cdn_app = Router::new()
        .route(
            "/cdn/{name}",
            get(|State(s): State<Shared>, AxPath(name): AxPath<String>, h: HeaderMap| async move {
                let idx: usize = name.trim_end_matches(".mp3").parse().unwrap_or(0);
                let mode = {
                    let mut g = s.lock();
                    g.log.push(format!("cdn:{idx}"));
                    if h.contains_key(header::COOKIE) {
                        g.cdn_cookie_seen = true;
                    }
                    g.tracks[idx].mode
                };
                audio(mode)
            }),
        )
        .route(
            "/art.jpg",
            get(|State(s): State<Shared>, h: HeaderMap| async move {
                let mut g = s.lock();
                g.log.push("art".into());
                if h.contains_key(header::COOKIE) {
                    g.cdn_cookie_seen = true;
                }
                if g.art_ok { (StatusCode::OK, JPEG.to_vec()).into_response() } else { StatusCode::NOT_FOUND.into_response() }
            }),
        )
        .with_state(site.clone());
    let cdn = serve(cdn_app).await;

    // Bandcamp page host
    let page_app = Router::new()
        .route(
            "/album/y",
            get(move |State(s): State<Shared>| async move {
                let g = s.lock();
                if g.page_status != StatusCode::OK {
                    return g.page_status.into_response();
                }
                axum::response::Html(page(&g, cdn)).into_response()
            }),
        )
        .route(
            "/track/z",
            get(move |State(s): State<Shared>| async move {
                let g = s.lock();
                axum::response::Html(page(&g, cdn)).into_response()
            }),
        )
        .with_state(site.clone());
    let bc = serve(page_app).await;

    let client = BandcampClient::new(ClientOptions {
        cookie: Some("identity=SECRET".into()),
        resolve: vec![("x.bandcamp.com".into(), bc)],
        backoff_scale: 0.001,
        rate_per_sec: bucket.map_or(ClientOptions::default().rate_per_sec, |b| b.0),
        burst: bucket.map_or(ClientOptions::default().burst, |b| b.1),
        ..Default::default()
    });
    let tmp = tempfile::tempdir().expect("tmp");
    let base = tmp.path().join("dl");
    std::fs::create_dir_all(&base).expect("base");
    Env {
        dl: NativeDownloader::new(client, DEFAULT_TEMPLATE),
        site,
        page_url: format!("http://x.bandcamp.com:{}/album/y", bc.port()),
        tmp,
        base,
    }
}

fn audio(mode: Mode) -> Response {
    let body = mp3_bytes(40);
    let len = body.len();
    let mk = |b: Body| Response::builder().header(header::CONTENT_LENGTH, len).header(header::CONTENT_TYPE, "audio/mpeg").body(b).expect("resp");
    match mode {
        Mode::Ok => mk(Body::from(body)),
        Mode::NotFound => StatusCode::NOT_FOUND.into_response(),
        Mode::Truncated => {
            let half = body[..len / 2].to_vec();
            let s = futures::stream::iter(vec![
                Ok::<_, std::io::Error>(bytes::Bytes::from(half)),
                Err(std::io::Error::other("boom")),
            ]);
            mk(Body::from_stream(s))
        }
        Mode::Stall => {
            let first = bytes::Bytes::from(body[..1000].to_vec());
            let s = futures::stream::once(async move { Ok::<_, std::io::Error>(first) })
                .chain(futures::stream::pending());
            mk(Body::from_stream(s))
        }
        Mode::Slow => {
            let chunks: Vec<bytes::Bytes> = body.chunks(512).map(|c| bytes::Bytes::from(c.to_vec())).collect();
            let s = futures::stream::unfold(chunks.into_iter(), |mut it| async move {
                tokio::time::sleep(Duration::from_millis(15)).await;
                it.next().map(|c| (Ok::<_, std::io::Error>(c), it))
            });
            mk(Body::from_stream(s))
        }
    }
}

use futures::StreamExt;

fn spec(e: &Env) -> DownloadSpec {
    DownloadSpec::new(&e.page_url, &e.base)
}

async fn run(e: &Env, spec: &DownloadSpec) -> (Outcome, Vec<Progress>) {
    let mut events = Vec::new();
    let mut cb = |p: Progress| events.push(p);
    let o = e.dl.download(spec, &mut cb, &CancellationToken::new()).await;
    (o, events)
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    fn rec(d: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                rec(&p, out);
            } else {
                out.push(p);
            }
        }
    }
    let mut v = Vec::new();
    rec(dir, &mut v);
    v.sort();
    v
}

fn ends(files: &[PathBuf], suffix: &str) -> Vec<PathBuf> {
    files.iter().filter(|p| p.to_string_lossy().ends_with(suffix)).cloned().collect()
}

fn meta(title: &str, num: Option<u32>) -> TrackMeta {
    TrackMeta {
        artist: None,
        albumartist: "The Artist".into(),
        album: "The Album".into(),
        title: title.into(),
        track: num,
        date: "2023".into(),
        label: String::new(),
    }
}

fn album3() -> Vec<T> {
    vec![t("First Song", 1), t("Second Song", 2), t("Third Song", 3)]
}

fn cdn_requests(e: &Env) -> Vec<String> {
    e.site.lock().log.iter().filter(|l| l.starts_with("cdn:")).cloned().collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_album_layout_tags_art_and_progress() {
    let mut tracks = album3();
    tracks[1].artist = Some("Guest".into());
    tracks[1].title = "Guest - Second Song".into();
    let e = env(tracks).await;
    let (o, events) = run(&e, &spec(&e)).await;

    assert_eq!(o.kind, OutcomeKind::Ok, "{o:?}");
    assert!(!o.retryable);
    assert_eq!(o.tracks_expected, Some(3));
    assert_eq!(o.tracks_finished, 3);
    assert_eq!(o.new_files.len(), 3);

    let expect = [
        slug::expected_file(&e.base, DEFAULT_TEMPLATE, &meta("First Song", Some(1))),
        slug::expected_file(&e.base, DEFAULT_TEMPLATE, &TrackMeta { artist: Some("Guest".into()), ..meta("Second Song", Some(2)) }),
        slug::expected_file(&e.base, DEFAULT_TEMPLATE, &meta("Third Song", Some(3))),
    ];
    assert_eq!(o.new_files, expect);
    assert!(expect[0].ends_with("the-artist/the-album/01 - first-song.mp3"), "{:?}", expect[0]);
    assert!(expect[1].ends_with("the-artist/the-album/02 - second-song.mp3"));

    let all = files_under(&e.base);
    assert_eq!(all.len(), 3, "only the three mp3s: {all:?}");
    assert!(ends(&all, ".part").is_empty() && ends(&all, "cover.jpg").is_empty());

    for (i, f) in expect.iter().enumerate() {
        let tf = lofty::read_from_path(f).expect("readable mp3");
        let tag = tf.primary_tag().expect("tag");
        assert_eq!(tag.track(), Some(i as u32 + 1));
        assert_eq!(tag.track_total(), Some(3));
        assert_eq!(tag.album().as_deref(), Some("The Album"));
        assert_eq!(tag.get_string(ItemKey::AlbumArtist), Some("The Artist"));
        assert_eq!(tag.get_string(ItemKey::Genre), Some("techno,industrial"));
        assert!(tag.get_string(ItemKey::Comment).is_some_and(|c| c.contains("/album/y")));
        assert_eq!(tag.get_string(ItemKey::RecordingDate), Some("2023-07-17"));
        let pics = tag.pictures();
        assert_eq!(pics.len(), 1);
        assert_eq!(pics[0].pic_type(), PictureType::CoverFront);
        assert_eq!(pics[0].data(), JPEG);
    }
    let tag = lofty::read_from_path(&expect[1]).expect("r");
    let tag = tag.primary_tag().expect("t");
    assert_eq!(tag.title().as_deref(), Some("Second Song"), "artist prefix stripped");
    assert_eq!(tag.artist().as_deref(), Some("Guest"));
    let tag0 = lofty::read_from_path(&expect[0]).expect("r");
    assert_eq!(tag0.primary_tag().expect("t").artist().as_deref(), Some("The Artist"));

    // art fetched once per release, never with the cookie
    assert_eq!(e.site.lock().log.iter().filter(|l| *l == "art").count(), 1);
    assert!(!e.site.lock().cdn_cookie_seen);

    // progress
    assert!(!events.is_empty());
    assert_eq!(events.last().expect("last").phase, "Finished");
    assert!((events.last().expect("last").fraction - 1.0).abs() < 1e-9);
    assert!(events.windows(2).all(|w| w[1].fraction >= w[0].fraction), "monotone: {events:?}");
    assert!(events.iter().all(|p| p.track_total == 3 && (0.0..=1.0).contains(&p.fraction)));
    assert_eq!(events.iter().filter(|p| p.phase == "Finished").count(), 3);
    assert!(events.iter().all(|p| p.phase == "Downloading" || p.phase == "Finished"));
}

#[tokio::test]
async fn flat_template_and_spec_template_are_honoured() {
    let e = env(vec![t("One", 1)]).await;
    let mut s = spec(&e);
    s.template = Some(FLAT_TEMPLATE.into());
    let (o, _) = run(&e, &s).await;
    assert_eq!(o.kind, OutcomeKind::Ok, "{o:?}");
    let want = e.base.join("the-artist-the-album-01-one.mp3");
    // FLAT_TEMPLATE has " - " separators which are not slugified (only the fields are).
    let want_flat = slug::expected_file(&e.base, FLAT_TEMPLATE, &meta("One", Some(1)));
    assert_eq!(o.new_files, vec![want_flat.clone()]);
    assert_eq!(want_flat.parent(), Some(e.base.as_path()), "flat: directly in base ({want:?})");
    assert!(want_flat.exists());
}

#[tokio::test]
async fn standalone_track_page_is_a_single() {
    let mut tr = t("Lonely", 1);
    tr.num = None;
    let e = env_with(vec![tr], "track").await;
    let url = e.page_url.replace("/album/y", "/track/z");
    let (o, _) = run(&e, &DownloadSpec::new(url, &e.base)).await;
    assert_eq!(o.kind, OutcomeKind::Ok, "{o:?}");
    assert_eq!(o.new_files, vec![slug::expected_file(&e.base, DEFAULT_TEMPLATE, &meta("Lonely", None))]);
    assert!(o.new_files[0].to_string_lossy().contains("Single - lonely.mp3"));
    let tf = lofty::read_from_path(&o.new_files[0]).expect("r");
    assert_eq!(tf.primary_tag().expect("t").track(), Some(1));
}

#[tokio::test]
async fn stale_part_and_tmp_files_are_purged_before_the_attempt() {
    let e = env(album3()).await;
    let dir = e.base.join("the-artist/the-album");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("01 - first-song.mp3.part"), b"stale partial").expect("w");
    std::fs::write(dir.join("zz.tmp"), b"old").expect("w");
    std::fs::write(dir.join("notes.txt"), b"keep").expect("w");
    let (o, _) = run(&e, &spec(&e)).await;
    assert_eq!(o.kind, OutcomeKind::Ok, "{o:?}");
    let all = files_under(&e.base);
    assert!(ends(&all, ".part").is_empty() && ends(&all, ".tmp").is_empty(), "{all:?}");
    assert!(dir.join("notes.txt").exists());
    assert_eq!(ends(&all, ".mp3").len(), 3);
}

#[tokio::test]
async fn truncated_body_is_partial_retryable_and_leaves_no_final_file() {
    let mut tracks = album3();
    tracks[1].mode = Mode::Truncated;
    let e = env(tracks).await;
    let (o, _) = run(&e, &spec(&e)).await;
    assert_eq!(o.kind, OutcomeKind::Partial, "{o:?}");
    assert!(o.retryable);
    assert_eq!(o.new_files.len(), 1);
    assert_eq!(o.tracks_finished, 1);
    assert_eq!(o.tracks_expected, Some(3));
    let all = files_under(&e.base);
    assert_eq!(ends(&all, ".mp3").len(), 1, "{all:?}");
    assert!(ends(&all, ".part").is_empty(), "the .part is deleted: {all:?}");
    assert_eq!(cdn_requests(&e), vec!["cdn:0", "cdn:1"], "stops at the failure");
}

#[tokio::test]
async fn first_track_truncated_is_partial_with_nothing_on_disk() {
    let mut tracks = album3();
    tracks[0].mode = Mode::Truncated;
    let e = env(tracks).await;
    let (o, _) = run(&e, &spec(&e)).await;
    assert_eq!(o.kind, OutcomeKind::Partial);
    assert!(o.retryable && o.new_files.is_empty());
    assert!(files_under(&e.base).is_empty());
}

#[tokio::test]
async fn stream_404_is_handled_as_retryable_partial() {
    let mut tracks = album3();
    tracks[2].mode = Mode::NotFound;
    let e = env(tracks).await;
    let (o, _) = run(&e, &spec(&e)).await;
    assert_eq!(o.kind, OutcomeKind::Partial, "{o:?}");
    assert!(o.retryable);
    assert!(o.detail.contains("404"), "{}", o.detail);
    assert_eq!(o.new_files.len(), 2);
    assert!(ends(&files_under(&e.base), ".part").is_empty());
}

#[tokio::test]
async fn resume_skips_files_already_present_and_complete() {
    let mut tracks = album3();
    tracks[1].mode = Mode::Truncated;
    let e = env(tracks).await;
    let (o1, _) = run(&e, &spec(&e)).await;
    assert_eq!(o1.kind, OutcomeKind::Partial);
    assert_eq!(cdn_requests(&e), vec!["cdn:0", "cdn:1"]);

    // The CDN recovers: only the missing tracks are fetched.
    {
        let mut g = e.site.lock();
        g.tracks[1].mode = Mode::Ok;
        g.log.clear();
    }
    let (o2, events) = run(&e, &spec(&e)).await;
    assert_eq!(o2.kind, OutcomeKind::Ok, "{o2:?}");
    assert_eq!(o2.new_files.len(), 2, "tracks 2 and 3 only");
    assert_eq!(o2.tracks_finished, 3);
    assert_eq!(cdn_requests(&e), vec!["cdn:1", "cdn:2"]);
    assert!(events.windows(2).all(|w| w[1].fraction >= w[0].fraction));

    // Everything is there now: nothing is fetched at all, not even the art.
    e.site.lock().log.clear();
    let (o3, _) = run(&e, &spec(&e)).await;
    assert_eq!(o3.kind, OutcomeKind::AlreadyHave, "{o3:?}");
    assert!(o3.ok() && !o3.retryable && o3.new_files.is_empty());
    assert_eq!(o3.tracks_finished, 3);
    assert_eq!(o3.tracks_expected, Some(3));
    assert!(e.site.lock().log.is_empty(), "{:?}", e.site.lock().log);
}

#[tokio::test]
async fn a_corrupt_final_file_is_replaced() {
    let e = env(vec![t("One", 1)]).await;
    let f = slug::expected_file(&e.base, DEFAULT_TEMPLATE, &meta("One", Some(1)));
    std::fs::create_dir_all(f.parent().expect("p")).expect("d");
    std::fs::write(&f, b"not audio at all").expect("w");
    let (o, _) = run(&e, &spec(&e)).await;
    assert_eq!(o.kind, OutcomeKind::Ok, "{o:?}");
    assert!(lofty::read_from_path(&f).is_ok());
}

#[tokio::test]
async fn cancel_mid_stream_removes_the_part_and_reports_cancelled() {
    let mut tracks = album3();
    tracks[0].mode = Mode::Stall;
    let e = env(tracks).await;
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    let site = e.site.clone();
    tokio::spawn(async move {
        loop {
            if site.lock().log.iter().any(|l| l == "cdn:0") {
                tokio::time::sleep(Duration::from_millis(200)).await;
                c2.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let mut cb = |_p: Progress| {};
    let o = tokio::time::timeout(Duration::from_secs(20), e.dl.download(&spec(&e), &mut cb, &cancel)).await.expect("cancel returns promptly");
    assert_eq!(o.kind, OutcomeKind::Crash);
    assert!(!o.retryable);
    assert_eq!(o.detail, "Cancelled.");
    assert!(files_under(&e.base).is_empty(), "{:?}", files_under(&e.base));
}

#[tokio::test]
async fn timeout_is_retryable_and_cleans_up() {
    let mut tracks = album3();
    tracks[1].mode = Mode::Stall;
    let e = env(tracks).await;
    let mut s = spec(&e);
    s.timeout = Duration::from_millis(1500);
    let (o, _) = run(&e, &s).await;
    assert_eq!(o.kind, OutcomeKind::Timeout, "{o:?}");
    assert!(o.retryable);
    assert_eq!(o.new_files.len(), 1);
    assert!(ends(&files_under(&e.base), ".part").is_empty());
}

#[tokio::test]
async fn dropping_the_future_mid_stream_leaves_only_a_part_never_a_final_file() {
    // SIGKILL at the function level: the future is dropped with no chance to clean up.
    let mut tracks = album3();
    tracks[0].mode = Mode::Slow;
    let e = env(tracks).await;
    let s = spec(&e);
    let mut cb = |_p: Progress| {};
    let r = tokio::time::timeout(Duration::from_millis(250), e.dl.download(&s, &mut cb, &CancellationToken::new())).await;
    assert!(r.is_err(), "still downloading when killed");
    let all = files_under(&e.base);
    assert_eq!(ends(&all, ".mp3").len(), 0, "no final file: {all:?}");
    let parts = ends(&all, ".mp3.part");
    assert_eq!(parts.len(), 1, "{all:?}");

    // The next attempt purges it and completes the album.
    e.site.lock().tracks[0].mode = Mode::Ok;
    let (o, _) = run(&e, &s).await;
    assert_eq!(o.kind, OutcomeKind::Ok, "{o:?}");
    let all = files_under(&e.base);
    assert!(ends(&all, ".part").is_empty());
    assert_eq!(ends(&all, ".mp3").len(), 3);
}

#[tokio::test]
async fn nothing_streamable_is_no_output() {
    let mut tracks = album3();
    for t in &mut tracks {
        t.streamable = false;
    }
    let e = env(tracks).await;
    let (o, _) = run(&e, &spec(&e)).await;
    assert_eq!(o.kind, OutcomeKind::NoOutput);
    assert!(!o.retryable);
    assert_eq!(o.detail, "None of this release is publicly streamable.");
    assert_eq!(o.tracks_expected, Some(3));
    assert!(cdn_requests(&e).is_empty());
}

#[tokio::test]
async fn partly_streamable_downloads_what_streams() {
    let mut tracks = album3();
    tracks[1].streamable = false;
    let e = env(tracks).await;
    let (o, _) = run(&e, &spec(&e)).await;
    assert_eq!(o.kind, OutcomeKind::Ok, "{o:?}");
    assert_eq!(o.new_files.len(), 2);
    assert_eq!(o.tracks_expected, Some(3));
    assert_eq!(o.detail, "Only part of this release is publicly streamable; the rest is download-only on Bandcamp.");
    // Track numbers keep their place on the release (01 and 03).
    let names: Vec<String> = o.new_files.iter().map(|p| p.file_name().expect("n").to_string_lossy().into_owned()).collect();
    assert_eq!(names, ["01 - first-song.mp3", "03 - third-song.mp3"]);
    let (o2, _) = run(&e, &spec(&e)).await;
    assert_eq!(o2.kind, OutcomeKind::AlreadyHave);
    assert_eq!(o2.tracks_expected, Some(3));
}

#[tokio::test]
async fn art_failure_is_tolerated() {
    let e = env(album3()).await;
    e.site.lock().art_ok = false;
    let (o, _) = run(&e, &spec(&e)).await;
    assert_eq!(o.kind, OutcomeKind::Ok, "{o:?}");
    assert_eq!(o.new_files.len(), 3);
    let tf = lofty::read_from_path(&o.new_files[0]).expect("r");
    let tag = tf.primary_tag().expect("t");
    assert!(tag.pictures().is_empty());
    assert_eq!(tag.title().as_deref(), Some("First Song"));
}

#[tokio::test]
async fn hostile_titles_stay_inside_the_base_dir() {
    let e = env(vec![
        t("../../escape", 1),
        t("a/b\\c/../../d", 2),
        t("日本語 ♥ Ünïcode", 3),
        t("/etc/passwd", 4),
        t("", 5),
    ])
    .await;
    let (o, _) = run(&e, &spec(&e)).await;
    assert_eq!(o.kind, OutcomeKind::Ok, "{o:?}");
    assert_eq!(o.new_files.len(), 5);
    let base = e.base.canonicalize().expect("canon");
    for f in &o.new_files {
        let c = f.canonicalize().expect("exists");
        assert!(c.starts_with(&base), "{c:?} escaped {base:?}");
        assert!(bc_core::paths::safe_join(&e.base, f.strip_prefix(&e.base).expect("rel")).is_ok());
    }
    // nothing leaked next to the base dir
    let siblings: Vec<_> = std::fs::read_dir(e.tmp.path()).expect("rd").flatten().collect();
    assert_eq!(siblings.len(), 1, "only dl/: {siblings:?}");
    assert!(o.new_files.iter().any(|f| f.to_string_lossy().contains("日本語-ünïcode")));
}

#[tokio::test]
async fn unsupported_and_missing_pages_are_classified() {
    let e = env(album3()).await;

    let (o, _) = run(&e, &DownloadSpec::new("http://x.bandcamp.com/music", &e.base)).await;
    assert_eq!(o.kind, OutcomeKind::NoOutput);
    assert!(!o.retryable);

    e.site.lock().page_status = StatusCode::NOT_FOUND;
    let (o, _) = run(&e, &spec(&e)).await;
    assert_eq!(o.kind, OutcomeKind::NotFound);
    assert!(!o.retryable);
    assert!(files_under(&e.base).is_empty());
}

#[tokio::test]
async fn page_server_errors_are_retryable_network() {
    let e = env(album3()).await;
    e.site.lock().page_status = StatusCode::INTERNAL_SERVER_ERROR;
    let (o, _) = run(&e, &spec(&e)).await;
    assert_eq!(o.kind, OutcomeKind::Network, "{o:?}");
    assert!(o.retryable);
}

#[tokio::test]
async fn audio_bytes_do_not_consume_page_tokens() {
    // One token, glacial refill: the page fetch takes it; the album's audio and
    // art (cdn client) must not wait for another.
    let e = env_full(album3(), "album", Some((0.01, 1))).await;
    let t0 = std::time::Instant::now();
    let (o, _) = run(&e, &spec(&e)).await;
    assert_eq!(o.kind, OutcomeKind::Ok, "{o:?}");
    assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
    assert_eq!(e.dl.name(), "native");
}
