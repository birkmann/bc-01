//! Embedded SPA with the fallback rules of the legacy test_spa:
//! * no mount at all unless a built `index.html` exists;
//! * client routes (no file extension) fall back to index.html;
//! * `/api/*` and missing assets (`/assets/*`, anything with an extension) stay 404.
use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::error::ApiError;
use crate::state::ServerOptions;

#[derive(RustEmbed)]
#[folder = "../bc-ui/dist"]
struct Dist;

fn load(opts: &ServerOptions, path: &str) -> Option<Vec<u8>> {
    if let Some(dir) = &opts.ui_dir {
        if path.contains("..") {
            return None;
        }
        return std::fs::read(dir.join(path)).ok();
    }
    Dist::get(path).map(|f| f.data.into_owned())
}

pub fn has_ui(opts: &ServerOptions) -> bool {
    !opts.no_ui && load(opts, "index.html").is_some()
}

pub fn serve(opts: &ServerOptions, req: &Request<Body>) -> Response {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return ApiError::new(405, "Method Not Allowed").into_response();
    }
    let raw = req.uri().path().trim_start_matches('/');
    let p = req.uri().path();
    if p == "/api" || p.starts_with("/api/") {
        return ApiError::not_found(format!("no route {p}")).into_response();
    }
    let path = if raw.is_empty() { "index.html" } else { raw };
    // Pre-compressed sibling (`app.wasm.br`, brotli q11 from scripts/build-ui.sh): ~12% smaller than
    // the on-the-fly compressor's output, which matters for the 6 MB wasm.
    let wants_br = req.headers().get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("br"));
    if wants_br && !path.ends_with(".br")
        && let Some(bytes) = load(opts, &format!("{path}.br")) {
            let mut r = asset(path, bytes);
            r.headers_mut().insert(header::CONTENT_ENCODING, HeaderValue::from_static("br"));
            r.headers_mut().insert(header::VARY, HeaderValue::from_static("accept-encoding"));
            return r;
        }
    if let Some(bytes) = load(opts, path) {
        return asset(path, bytes);
    }
    let last = path.rsplit('/').next().unwrap_or("");
    if path.starts_with("assets/") || last.contains('.') {
        return ApiError::not_found(format!("no such file {p}")).into_response();
    }
    match load(opts, "index.html") {
        Some(b) => asset("index.html", b),
        None => ApiError::not_found("no UI built").into_response(),
    }
}

fn asset(path: &str, bytes: Vec<u8>) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if path == "index.html" {
        "no-cache"
    } else if path.contains('-') || path.starts_with("assets/") {
        // trunk emits content-hashed names
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut r = (StatusCode::OK, Body::from(bytes)).into_response();
    let h = r.headers_mut();
    if let Ok(v) = HeaderValue::from_str(mime.as_ref()) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    r
}
