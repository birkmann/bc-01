//! HTTP range responses for audio files and cached art (axum 0.8).
//!
//! Port of `services/playback/streaming.py` with the same three fixes over a naive file
//! response: `Accept-Ranges` on the plain 200, a correct 416 with `Content-Range: bytes */N`,
//! and suffix ranges (`bytes=-65536`, what browsers issue to read an MP3's trailing metadata).
//!
//! Supported: `ETag` (from size + mtime, strong), `Last-Modified`, `If-None-Match` and
//! `If-Modified-Since` (304), `If-Range`, one range `a-b`, open range `a-`, suffix `-n`, HEAD.
//!
//! Choices where the spec is open:
//! * **Multi-range** requests (`bytes=0-1,5-9`) are answered with the **whole file, 200**. RFC 9110
//!   allows a server to ignore `Range`; no browser audio element sends multi-range, and multipart
//!   byteranges would only add surface. (The Python code answered 416 for them.)
//! * A syntactically invalid `Range` header, or a unit other than `bytes`, is ignored (200 whole).
//! * `a-b` with `b < a` is unsatisfiable (416), as in the Python code.
//!
//! The file is streamed in 64 KiB chunks from an open `tokio::fs::File` (seek + `take`); it is
//! never read whole.

use std::io::SeekFrom;
use std::path::Path;
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// Streaming chunk size.
pub const CHUNK_SIZE: usize = 64 * 1024;

/// `Cache-Control` for audio.
pub const AUDIO_CACHE_CONTROL: &str = "private, max-age=3600";
/// `Cache-Control` for art addressed by an immutable `?v=` URL.
pub const ART_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// MIME type for an audio file extension (without dot, case-insensitive).
pub fn audio_mime(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" | "mp4" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "aiff" | "aif" => "audio/aiff",
        "wma" => "audio/x-ms-wma",
        _ => "application/octet-stream",
    }
}

/// MIME type for a path, by extension.
pub fn audio_mime_for_path(path: &Path) -> &'static str {
    audio_mime(path.extension().and_then(|e| e.to_str()).unwrap_or(""))
}

/// Result of parsing a `Range` header against a file size.
#[derive(Debug, PartialEq, Eq)]
enum RangeSpec {
    /// No usable range: serve the whole file.
    Ignore,
    /// Inclusive `(start, end)`.
    Satisfiable(u64, u64),
    Unsatisfiable,
}

fn parse_range(raw: &str, size: u64) -> RangeSpec {
    let raw = raw.trim();
    let Some(spec) = raw.strip_prefix("bytes=") else {
        return RangeSpec::Ignore;
    };
    if spec.contains(',') {
        return RangeSpec::Ignore; // multi-range: whole file, see module docs
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return RangeSpec::Ignore;
    };
    let (a, b) = (a.trim(), b.trim());
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    match (a.is_empty(), b.is_empty()) {
        (true, true) => RangeSpec::Ignore,
        (true, false) => {
            // suffix: last n bytes
            if !digits(b) {
                return RangeSpec::Ignore;
            }
            let Ok(n) = b.parse::<u64>() else {
                return RangeSpec::Ignore;
            };
            if n == 0 || size == 0 {
                return RangeSpec::Unsatisfiable;
            }
            RangeSpec::Satisfiable(size.saturating_sub(n), size - 1)
        }
        (false, empty_end) => {
            if !digits(a) || (!empty_end && !digits(b)) {
                return RangeSpec::Ignore;
            }
            let Ok(start) = a.parse::<u64>() else {
                return RangeSpec::Ignore;
            };
            let end = if empty_end {
                u64::MAX
            } else {
                b.parse::<u64>().unwrap_or(u64::MAX)
            };
            if start >= size || start > end {
                return RangeSpec::Unsatisfiable;
            }
            RangeSpec::Satisfiable(start, end.min(size - 1))
        }
    }
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn strip_weak(tag: &str) -> &str {
    tag.trim().trim_start_matches("W/")
}

fn etag_matches(header_value: &str, etag: &str) -> bool {
    let ours = strip_weak(etag);
    header_value.split(',').any(|t| {
        let t = t.trim();
        t == "*" || strip_weak(t) == ours
    })
}

fn base_builder(
    etag: &str,
    last_modified: &str,
    content_type: &str,
    cache_control: &str,
) -> axum::http::response::Builder {
    let mut b = Response::builder()
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, etag)
        .header(header::LAST_MODIFIED, last_modified)
        .header(header::CACHE_CONTROL, cache_control);
    if let Ok(ct) = HeaderValue::from_str(content_type) {
        b = b.header(header::CONTENT_TYPE, ct);
    }
    b
}

async fn serve(
    method: &Method,
    path: &Path,
    headers: &HeaderMap,
    content_type: &str,
    cache_control: &str,
) -> Response {
    let meta = match tokio::fs::metadata(path).await {
        Ok(m) if m.is_file() => m,
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "stat failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let size = meta.len();
    let mtime = meta.modified().ok();
    let mtime_ns = mtime
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let etag = format!("\"{size:x}-{mtime_ns:x}\"");
    let last_modified = httpdate::fmt_http_date(mtime.unwrap_or(UNIX_EPOCH));

    // Conditional GET.
    let not_modified = if let Some(inm) = header_str(headers, header::IF_NONE_MATCH) {
        etag_matches(inm, &etag)
    } else if let (Some(ims), Some(m)) = (header_str(headers, header::IF_MODIFIED_SINCE), mtime) {
        httpdate::parse_http_date(ims).is_ok_and(|since| {
            // HTTP dates have one-second resolution.
            let secs = |t: std::time::SystemTime| {
                t.duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            };
            secs(m) <= secs(since)
        })
    } else {
        false
    };
    if not_modified {
        return base_builder(&etag, &last_modified, content_type, cache_control)
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::NOT_MODIFIED.into_response());
    }

    // Range, honouring If-Range (a stale validator means "send everything").
    let mut spec = RangeSpec::Ignore;
    if let Some(raw) = header_str(headers, header::RANGE) {
        let if_range_ok = match header_str(headers, header::IF_RANGE) {
            None => true,
            Some(v) => {
                let v = v.trim();
                if v.starts_with('"') {
                    v == etag // strong comparison only
                } else if let (Ok(date), Some(m)) = (httpdate::parse_http_date(v), mtime) {
                    let secs = |t: std::time::SystemTime| {
                        t.duration_since(UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    };
                    secs(date) == secs(m)
                } else {
                    false
                }
            }
        };
        if if_range_ok {
            spec = parse_range(raw, size);
        }
    }

    let builder = base_builder(&etag, &last_modified, content_type, cache_control);
    let (status, start, len, content_range) = match spec {
        RangeSpec::Unsatisfiable => {
            return builder
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{size}"))
                .header(header::CONTENT_LENGTH, 0)
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::RANGE_NOT_SATISFIABLE.into_response());
        }
        RangeSpec::Satisfiable(s, e) => (
            StatusCode::PARTIAL_CONTENT,
            s,
            e - s + 1,
            Some(format!("bytes {s}-{e}/{size}")),
        ),
        RangeSpec::Ignore => (StatusCode::OK, 0, size, None),
    };

    let mut builder = builder.status(status).header(header::CONTENT_LENGTH, len);
    if let Some(cr) = content_range {
        builder = builder.header(header::CONTENT_RANGE, cr);
    }

    if *method == Method::HEAD || len == 0 {
        return builder
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "open failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if start > 0
        && let Err(e) = file.seek(SeekFrom::Start(start)).await
    {
        tracing::warn!(path = %path.display(), error = %e, "seek failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let stream = ReaderStream::with_capacity(file.take(len), CHUNK_SIZE);
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Serve an audio file for a GET request (see module docs). Axum drops the body for HEAD when
/// the handler is registered with `get`, and keeps `Content-Length`; use [`serve_file_method`]
/// to be explicit.
pub async fn serve_file(path: &Path, headers: &HeaderMap, content_type: &str) -> Response {
    serve(
        &Method::GET,
        path,
        headers,
        content_type,
        AUDIO_CACHE_CONTROL,
    )
    .await
}

/// Like [`serve_file`] but honouring the request method (`HEAD` returns headers only).
pub async fn serve_file_method(
    method: &Method,
    path: &Path,
    headers: &HeaderMap,
    content_type: &str,
) -> Response {
    serve(method, path, headers, content_type, AUDIO_CACHE_CONTROL).await
}

/// Serve a cached image with `Cache-Control: public, max-age=31536000, immutable`.
pub async fn serve_art(path: &Path, headers: &HeaderMap, mime: &str) -> Response {
    serve(&Method::GET, path, headers, mime, ART_CACHE_CONTROL).await
}

/// Like [`serve_art`] but honouring the request method.
pub async fn serve_art_method(
    method: &Method,
    path: &Path,
    headers: &HeaderMap,
    mime: &str,
) -> Response {
    serve(method, path, headers, mime, ART_CACHE_CONTROL).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::State;
    use axum::http::Request;
    use axum::routing::get;
    use http_body_util::BodyExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn audio(State(p): State<Arc<PathBuf>>, method: Method, headers: HeaderMap) -> Response {
        serve_file_method(&method, &p, &headers, audio_mime_for_path(&p)).await
    }
    async fn art(State(p): State<Arc<PathBuf>>, headers: HeaderMap) -> Response {
        serve_art(&p, &headers, "image/webp").await
    }

    fn app(path: PathBuf) -> Router {
        Router::new()
            .route("/audio", get(audio))
            .route("/art", get(art))
            .with_state(Arc::new(path))
    }

    fn sample(dir: &Path) -> (PathBuf, Vec<u8>) {
        let p = dir.join("t.mp3");
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        (p, data)
    }

    async fn send(
        app: &Router,
        method: &str,
        uri: &str,
        hdrs: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut req = Request::builder().method(method).uri(uri);
        for (k, v) in hdrs {
            req = req.header(*k, *v);
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (parts, body) = resp.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes().to_vec();
        (parts.status, parts.headers, bytes)
    }

    #[tokio::test]
    async fn plain_get_advertises_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let (p, data) = sample(tmp.path());
        let (st, h, body) = send(&app(p), "GET", "/audio", &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(h[header::ACCEPT_RANGES], "bytes");
        assert_eq!(h[header::CONTENT_TYPE], "audio/mpeg");
        assert_eq!(h[header::CONTENT_LENGTH], "100000");
        assert!(h.contains_key(header::ETAG) && h.contains_key(header::LAST_MODIFIED));
        assert_eq!(body, data);
    }

    #[tokio::test]
    async fn closed_open_and_suffix_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let (p, data) = sample(tmp.path());
        let a = app(p);

        let (st, h, body) = send(&a, "GET", "/audio", &[("range", "bytes=0-1023")]).await;
        assert_eq!(st, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h[header::CONTENT_RANGE], "bytes 0-1023/100000");
        assert_eq!(h[header::CONTENT_LENGTH], "1024");
        assert_eq!(body, &data[..1024]);

        let (st, h, body) = send(&a, "GET", "/audio", &[("range", "bytes=99000-")]).await;
        assert_eq!(st, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h[header::CONTENT_RANGE], "bytes 99000-99999/100000");
        assert_eq!(body, &data[99_000..]);

        let (st, h, body) = send(&a, "GET", "/audio", &[("range", "bytes=-512")]).await;
        assert_eq!(st, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h[header::CONTENT_RANGE], "bytes 99488-99999/100000");
        assert_eq!(body, &data[99_488..]);

        // end beyond EOF is clamped; oversized suffix is the whole file as 206
        let (st, h, _) = send(&a, "GET", "/audio", &[("range", "bytes=99990-200000")]).await;
        assert_eq!(st, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h[header::CONTENT_RANGE], "bytes 99990-99999/100000");
        let (st, h, body) = send(&a, "GET", "/audio", &[("range", "bytes=-999999")]).await;
        assert_eq!(st, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h[header::CONTENT_RANGE], "bytes 0-99999/100000");
        assert_eq!(body.len(), 100_000);
    }

    #[tokio::test]
    async fn unsatisfiable_is_416_with_star_range() {
        let tmp = tempfile::tempdir().unwrap();
        let (p, _) = sample(tmp.path());
        let a = app(p);
        for r in [
            "bytes=100010-",
            "bytes=100000-100001",
            "bytes=-0",
            "bytes=50-10",
        ] {
            let (st, h, body) = send(&a, "GET", "/audio", &[("range", r)]).await;
            assert_eq!(st, StatusCode::RANGE_NOT_SATISFIABLE, "{r}");
            assert_eq!(h[header::CONTENT_RANGE], "bytes */100000", "{r}");
            assert!(body.is_empty());
        }
    }

    #[tokio::test]
    async fn multi_range_and_garbage_serve_whole_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (p, data) = sample(tmp.path());
        let a = app(p);
        for r in ["bytes=0-1,5-9", "items=0-5", "bytes=abc-", "bytes=-"] {
            let (st, h, body) = send(&a, "GET", "/audio", &[("range", r)]).await;
            assert_eq!(st, StatusCode::OK, "{r}");
            assert!(!h.contains_key(header::CONTENT_RANGE));
            assert_eq!(body, data);
        }
    }

    #[tokio::test]
    async fn conditional_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let (p, _) = sample(tmp.path());
        let a = app(p);
        let (_, h, _) = send(&a, "GET", "/audio", &[]).await;
        let etag = h[header::ETAG].to_str().unwrap().to_string();
        let lm = h[header::LAST_MODIFIED].to_str().unwrap().to_string();

        let (st, h2, body) = send(&a, "GET", "/audio", &[("if-none-match", &etag)]).await;
        assert_eq!(st, StatusCode::NOT_MODIFIED);
        assert!(body.is_empty());
        assert_eq!(h2[header::ETAG].to_str().unwrap(), etag);

        let (st, _, _) = send(&a, "GET", "/audio", &[("if-none-match", "\"nope\", *")]).await;
        assert_eq!(st, StatusCode::NOT_MODIFIED);
        let (st, _, _) = send(&a, "GET", "/audio", &[("if-none-match", "\"nope\"")]).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _, _) = send(&a, "GET", "/audio", &[("if-modified-since", &lm)]).await;
        assert_eq!(st, StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn if_range_matching_and_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let (p, data) = sample(tmp.path());
        let a = app(p);
        let (_, h, _) = send(&a, "GET", "/audio", &[]).await;
        let etag = h[header::ETAG].to_str().unwrap().to_string();
        let lm = h[header::LAST_MODIFIED].to_str().unwrap().to_string();

        let (st, _, body) = send(
            &a,
            "GET",
            "/audio",
            &[("range", "bytes=0-9"), ("if-range", &etag)],
        )
        .await;
        assert_eq!(st, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body, &data[..10]);
        let (st, _, body) = send(
            &a,
            "GET",
            "/audio",
            &[("range", "bytes=0-9"), ("if-range", &lm)],
        )
        .await;
        assert_eq!(st, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body.len(), 10);
        let (st, _, body) = send(
            &a,
            "GET",
            "/audio",
            &[("range", "bytes=0-9"), ("if-range", "\"stale\"")],
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(body.len(), 100_000);
    }

    #[tokio::test]
    async fn head_has_headers_and_no_body() {
        let tmp = tempfile::tempdir().unwrap();
        let (p, _) = sample(tmp.path());
        let a = app(p);
        let (st, h, body) = send(&a, "HEAD", "/audio", &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(h[header::CONTENT_LENGTH], "100000");
        assert_eq!(h[header::ACCEPT_RANGES], "bytes");
        assert!(body.is_empty());
        let (st, h, body) = send(&a, "HEAD", "/audio", &[("range", "bytes=10-19")]).await;
        assert_eq!(st, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h[header::CONTENT_LENGTH], "10");
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn missing_file_is_404() {
        let tmp = tempfile::tempdir().unwrap();
        let (st, _, _) = send(&app(tmp.path().join("nope.mp3")), "GET", "/audio", &[]).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _, _) = send(&app(tmp.path().to_path_buf()), "GET", "/audio", &[]).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn art_is_immutable() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("1_thumb.webp");
        std::fs::write(&p, b"RIFFxxxxWEBP").unwrap();
        let (st, h, body) = send(&app(p), "GET", "/art", &[]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            h[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(h[header::CONTENT_TYPE], "image/webp");
        assert_eq!(body, b"RIFFxxxxWEBP");
    }

    #[test]
    fn mime_table() {
        assert_eq!(audio_mime("MP3"), "audio/mpeg");
        assert_eq!(audio_mime("flac"), "audio/flac");
        assert_eq!(audio_mime("m4a"), "audio/mp4");
        assert_eq!(audio_mime("opus"), "audio/ogg");
        assert_eq!(audio_mime("ogg"), "audio/ogg");
        assert_eq!(audio_mime("wav"), "audio/wav");
        assert_eq!(audio_mime("xyz"), "application/octet-stream");
    }

    #[test]
    fn range_parser_edge_cases() {
        assert_eq!(parse_range("bytes=0-0", 1), RangeSpec::Satisfiable(0, 0));
        assert_eq!(parse_range("bytes=0-", 0), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=-5", 3), RangeSpec::Satisfiable(0, 2));
    }
}
