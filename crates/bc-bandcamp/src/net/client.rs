//! HTTP client for Bandcamp (port of `BandcampClient` in `net.py`).
//!
//! **The rule that governs everything here:** every `bandcamp.com/api/*`
//! endpoint returns **HTTP 200 on error**, with the failure encoded in the body.
//! Verified live:
//!
//! ```text
//! {"error": true, "error_message": "must be logged in"}
//! {"__api_special__": "exception", "error_type": "Endpoints::MissingParamError"}
//! ```
//!
//! both arrive as `200 OK`, so every API response is body-inspected
//! ([`check_api_error`]).
//!
//! Cookie discipline: the identity cookie is attached **manually**, per hop,
//! only when the request is `authed` *and* the hop's host is `bandcamp.com` or
//! `*.bandcamp.com` ([`is_bandcamp_host`]). Automatic redirects and the cookie
//! jar are disabled; redirects are followed here so a hop to any other host is
//! sent without the cookie.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use parking_lot::{Mutex, RwLock};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use url::Url;

use super::bucket::{DEFAULT_BURST, DEFAULT_RATE_PER_SEC, RESERVED_BURST, RESERVED_RATE_PER_SEC, TokenBucket};
use super::cache::{PageCache, PageKind};
use crate::error::{HarvestError, Result};
use crate::identity::Secret;

/// Default `User-Agent` (matches `Config::harvest_user_agent`'s default).
pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";
const ACCEPT_HTML: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
const ACCEPT_API: &str = "application/json, text/javascript, */*; q=0.01";
const API_ORIGIN: &str = "https://bandcamp.com";
const API_REFERER: &str = "https://bandcamp.com/";
const MAX_REDIRECTS: usize = 10;

/// Which rate-limit lane a request uses. Both lanes obey the global penalty
/// (a 429 penalises both); `Reserved` has its own small bucket so stream-URL
/// pre-resolution never queues behind a crawler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Lane {
    #[default]
    Normal,
    Reserved,
}

/// Options for [`BandcampClient::get_html`].
#[derive(Debug, Clone, Default)]
pub struct GetOpts {
    /// Selects the default cache TTL (default [`PageKind::Album`]).
    pub kind: PageKind,
    /// Overrides the kind's TTL (`Duration::ZERO` = bypass the cache).
    pub ttl: Option<Duration>,
    /// Send the identity cookie (only ever to `*.bandcamp.com`).
    pub authed: bool,
    pub referer: Option<String>,
    pub lane: Lane,
}

impl GetOpts {
    pub fn kind(kind: PageKind) -> Self {
        Self { kind, ..Self::default() }
    }
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }
    pub fn authed(mut self, authed: bool) -> Self {
        self.authed = authed;
        self
    }
    pub fn referer(mut self, referer: impl Into<String>) -> Self {
        self.referer = Some(referer.into());
        self
    }
    pub fn lane(mut self, lane: Lane) -> Self {
        self.lane = lane;
        self
    }
    fn max_age(&self) -> f64 {
        self.ttl.map_or_else(|| self.kind.ttl(), |d| d.as_secs_f64())
    }
}

/// Request counters (legacy `ClientStats`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientStats {
    pub requests: u64,
    pub cache_hits: u64,
    pub errors: u64,
    /// The last 10 failure messages.
    pub last_errors: Vec<String>,
}

/// Construction options. `Default` = the legacy defaults.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub rate_per_sec: f64,
    pub burst: u32,
    pub reserved_rate_per_sec: f64,
    pub reserved_burst: u32,
    /// Global in-flight request cap.
    pub concurrency: usize,
    pub cookie: Option<String>,
    pub user_agent: Option<String>,
    /// Per-read timeout.
    pub timeout: Duration,
    pub cache: Option<Arc<PageCache>>,
    /// DNS overrides (tests: pose a local server as `x.bandcamp.com`).
    pub resolve: Vec<(String, SocketAddr)>,
    /// Multiplies retry backoff sleeps (tests use a tiny value).
    pub backoff_scale: f64,
    /// Origin API paths resolve against (default `https://bandcamp.com`). Tests point it at
    /// `http://bandcamp.com:<port>` together with a `resolve` override.
    pub api_origin: Option<String>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            rate_per_sec: DEFAULT_RATE_PER_SEC,
            burst: DEFAULT_BURST,
            reserved_rate_per_sec: RESERVED_RATE_PER_SEC,
            reserved_burst: RESERVED_BURST,
            concurrency: 4,
            cookie: None,
            user_agent: None,
            timeout: Duration::from_secs(30),
            cache: None,
            resolve: Vec::new(),
            backoff_scale: 1.0,
            api_origin: None,
        }
    }
}

/// `true` iff `host` is exactly `bandcamp.com` or ends with `.bandcamp.com`
/// (case-insensitive). The only hosts the identity cookie may be sent to.
pub fn is_bandcamp_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "bandcamp.com" || h.ends_with(".bandcamp.com")
}

/// Raise if a 200 response actually carries a failure (both error shapes).
pub fn check_api_error(payload: &Value, url: &str) -> Result<()> {
    let Some(obj) = payload.as_object() else { return Ok(()) };

    if obj.get("__api_special__").and_then(Value::as_str) == Some("exception") {
        let error_type = obj.get("error_type").filter(|v| !v.is_null());
        let message = match error_type {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "unknown exception".to_string(),
        };
        return Err(HarvestError::Api {
            message,
            url: url.to_string(),
            error_type: error_type.map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string)),
            unspecified: false,
        });
    }

    let error = obj.get("error");
    if error == Some(&Value::Bool(true)) || error.and_then(Value::as_str) == Some("true") {
        let raw = obj.get("error_message").filter(|v| !v.is_null());
        // Python: str(raw or "unspecified error") -- falsy values fall back.
        let message = match raw {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            Some(Value::Bool(false)) | Some(Value::String(_)) => "unspecified error".to_string(),
            Some(Value::Number(n)) if n.as_f64() == Some(0.0) => "unspecified error".to_string(),
            Some(other) => other.to_string(),
            None => "unspecified error".to_string(),
        };
        if message.to_lowercase().contains("logged in") {
            return Err(HarvestError::IdentityExpired(format!("{message} ({url})")));
        }
        return Err(HarvestError::Api {
            message,
            url: url.to_string(),
            error_type: None,
            unspecified: raw.is_none(),
        });
    }
    Ok(())
}

/// Is this transport error message a deterministic certificate failure
/// (Python's `CERTIFICATE_VERIFY_FAILED`, or the rustls equivalents)?
/// A cert mismatch is a property of the host -- typically a lapsed custom
/// domain parked on someone else's certificate -- so retries cannot change it.
pub fn is_certificate_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("certificate_verify_failed")
        || m.contains("invalid peer certificate")
        || m.contains("invalidcertificate")
        || m.contains("certificate verify failed")
        || m.contains("unknownissuer")
        || m.contains("certnotvalidforname")
        || (m.contains("certificate") && (m.contains("expired") || m.contains("not valid for")))
}

fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        out.push_str(": ");
        out.push_str(&s.to_string());
        cur = s.source();
    }
    out
}

fn clone_error(e: &HarvestError) -> HarvestError {
    match e {
        HarvestError::Other(m) => HarvestError::Other(m.clone()),
        HarvestError::Api { message, url, error_type, unspecified } => HarvestError::Api {
            message: message.clone(),
            url: url.clone(),
            error_type: error_type.clone(),
            unspecified: *unspecified,
        },
        HarvestError::ListUnavailable(m) => HarvestError::ListUnavailable(m.clone()),
        HarvestError::IdentityExpired(m) => HarvestError::IdentityExpired(m.clone()),
        HarvestError::RateLimited(m) => HarvestError::RateLimited(m.clone()),
        HarvestError::Extraction(m) => HarvestError::Extraction(m.clone()),
        HarvestError::Cancelled => HarvestError::Cancelled,
        other => HarvestError::Other(other.to_string()),
    }
}

type FlightResult = std::result::Result<Arc<str>, Arc<HarvestError>>;
type Flight = Shared<BoxFuture<'static, FlightResult>>;

struct Inner {
    http: reqwest::Client,
    cdn: reqwest::Client,
    normal: TokenBucket,
    reserved: TokenBucket,
    sem: tokio::sync::Semaphore,
    cache: Option<Arc<PageCache>>,
    cookie: RwLock<Option<Secret>>,
    stats: Mutex<ClientStats>,
    flights: Mutex<HashMap<String, Flight>>,
    user_agent: String,
    backoff_scale: f64,
    api_origin: String,
}

/// The Bandcamp HTTP client. Cheap to clone (shared state).
#[derive(Clone)]
pub struct BandcampClient(Arc<Inner>);

impl std::fmt::Debug for BandcampClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BandcampClient").field("has_cookie", &self.has_cookie()).finish()
    }
}

/// One logical request, replayable across retries and redirect hops.
struct Req {
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Option<Bytes>,
    authed: bool,
}

fn hv(s: &str) -> HeaderValue {
    HeaderValue::from_str(s).unwrap_or_else(|_| HeaderValue::from_static(""))
}

impl BandcampClient {
    pub fn new(opts: ClientOptions) -> Self {
        let user_agent = opts.user_agent.clone().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| DEFAULT_USER_AGENT.into());

        let mut base = HeaderMap::new();
        base.insert(reqwest::header::ACCEPT_LANGUAGE, HeaderValue::from_static(ACCEPT_LANGUAGE));

        let mut builder = reqwest::Client::builder()
            .user_agent(user_agent.clone())
            .default_headers(base.clone())
            // Redirects are followed manually so the cookie rule is enforced per hop.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(opts.timeout);
        for (domain, addr) in &opts.resolve {
            builder = builder.resolve(domain, *addr);
        }
        let http = builder.build().unwrap_or_else(|e| {
            tracing::error!("falling back to a default HTTP client: {e}");
            reqwest::Client::new()
        });

        // Unthrottled, cookie-less client for audio on bcbits/popplers CDNs.
        let mut cdn_builder = reqwest::Client::builder()
            .user_agent(user_agent.clone())
            .default_headers(base)
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60));
        for (domain, addr) in &opts.resolve {
            cdn_builder = cdn_builder.resolve(domain, *addr);
        }
        let cdn = cdn_builder.build().unwrap_or_else(|_| reqwest::Client::new());

        Self(Arc::new(Inner {
            http,
            cdn,
            normal: TokenBucket::new(opts.rate_per_sec, opts.burst),
            reserved: TokenBucket::new(opts.reserved_rate_per_sec, opts.reserved_burst),
            sem: tokio::sync::Semaphore::new(opts.concurrency.max(1)),
            cache: opts.cache,
            cookie: RwLock::new(opts.cookie.filter(|c| !c.trim().is_empty()).map(Secret::new)),
            stats: Mutex::new(ClientStats::default()),
            flights: Mutex::new(HashMap::new()),
            user_agent,
            backoff_scale: opts.backoff_scale,
            api_origin: opts.api_origin.unwrap_or_else(|| "https://bandcamp.com".to_string()),
        }))
    }

    pub fn set_cookie(&self, cookie: Option<String>) {
        *self.0.cookie.write() = cookie.filter(|c| !c.trim().is_empty()).map(Secret::new);
    }

    pub fn has_cookie(&self) -> bool {
        self.0.cookie.read().is_some()
    }

    /// The token bucket of a lane (e.g. to read `rate()` for the UI).
    pub fn limiter(&self, lane: Lane) -> &TokenBucket {
        match lane {
            Lane::Normal => &self.0.normal,
            Lane::Reserved => &self.0.reserved,
        }
    }

    /// Penalise both lanes (what a 429/403 does).
    pub fn penalise(&self, seconds: Option<f64>) {
        self.0.normal.penalise(seconds);
        self.0.reserved.penalise(seconds);
    }

    pub fn cache(&self) -> Option<&Arc<PageCache>> {
        self.0.cache.as_ref()
    }

    pub fn stats(&self) -> ClientStats {
        self.0.stats.lock().clone()
    }

    pub fn user_agent(&self) -> &str {
        &self.0.user_agent
    }

    /// A separate **unthrottled** client without cookie or limiter, for
    /// streaming audio from bcbits/popplers CDNs (explore proxy, native
    /// downloader). Follows redirects normally.
    pub fn cdn_client(&self) -> &reqwest::Client {
        &self.0.cdn
    }

    // ------------------------------------------------------------------
    // Request construction
    // ------------------------------------------------------------------

    fn html_headers(referer: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(reqwest::header::ACCEPT, HeaderValue::from_static(ACCEPT_HTML));
        if let Some(r) = referer.filter(|r| !r.is_empty()) {
            h.insert(reqwest::header::REFERER, hv(r));
        }
        h
    }

    fn api_headers(referer: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(reqwest::header::ACCEPT, HeaderValue::from_static(ACCEPT_API));
        h.insert(reqwest::header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(reqwest::header::ORIGIN, HeaderValue::from_static(API_ORIGIN));
        h.insert(HeaderName::from_static("x-requested-with"), HeaderValue::from_static("XMLHttpRequest"));
        if let Some(r) = referer.filter(|r| !r.is_empty()) {
            h.insert(reqwest::header::REFERER, hv(r));
        }
        h
    }

    fn parse_url(url: &str) -> Result<Url> {
        Url::parse(url).map_err(|e| HarvestError::Other(format!("invalid url {url}: {e}")))
    }

    fn api_url(&self, path: &str) -> String {
        if path.starts_with('/') { format!("{}{path}", self.0.api_origin) } else { path.to_string() }
    }

    /// Send one logical request, following redirects manually. The cookie is
    /// attached per hop, only for authed requests whose hop host is Bandcamp.
    async fn execute(&self, req: &Req) -> std::result::Result<reqwest::Response, String> {
        let mut method = req.method.clone();
        let mut url = req.url.clone();
        let mut body = req.body.clone();
        let mut headers = req.headers.clone();

        for _ in 0..=MAX_REDIRECTS {
            let mut rb = self.0.http.request(method.clone(), url.clone()).headers(headers.clone());
            if req.authed && url.host_str().is_some_and(is_bandcamp_host)
                && let Some(cookie) = self.0.cookie.read().as_ref() {
                    match HeaderValue::from_str(cookie.expose()) {
                        Ok(mut v) => {
                            v.set_sensitive(true);
                            rb = rb.header(reqwest::header::COOKIE, v);
                        }
                        Err(_) => tracing::warn!("identity cookie is not a valid header value; sending without it"),
                    }
                }
            if let Some(b) = &body {
                rb = rb.body(b.clone());
            }
            let resp = rb.send().await.map_err(|e| error_chain(&e))?;

            let status = resp.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
                && let Some(loc) = resp.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()) {
                    let next = url.join(loc).map_err(|e| format!("bad redirect location: {e}"))?;
                    if !matches!(next.scheme(), "http" | "https") {
                        return Err(format!("redirect to unsupported scheme {}", next.scheme()));
                    }
                    if matches!(status.as_u16(), 301..=303) && method != Method::GET && method != Method::HEAD {
                        method = Method::GET;
                        body = None;
                        headers.remove(reqwest::header::CONTENT_TYPE);
                    }
                    // A hop off Bandcamp must not inherit Bandcamp's Origin.
                    if !next.host_str().is_some_and(is_bandcamp_host) {
                        headers.remove(reqwest::header::ORIGIN);
                    }
                    url = next;
                    continue;
                }
            return Ok(resp);
        }
        Err(format!("too many redirects for {}", req.url))
    }

    fn lane_bucket(&self, lane: Lane) -> &TokenBucket {
        self.limiter(lane)
    }

    /// The retry loop (legacy `_send`). `op` performs one attempt; a
    /// transport failure is reported as its message.
    ///
    /// * 429/403 -> penalise (Retry-After digits honoured) and retry;
    /// * a 5xx response is retried; transport errors retry unless the message is a
    ///   certificate failure; backoff is `min(120, 2^attempt) * U(0.8, 1.2)` seconds;
    /// * everything that finally fails surfaces as a `HarvestError`.
    pub(crate) async fn send_with<F, Fut>(
        &self,
        lane: Lane,
        label: &str,
        attempts: u32,
        mut op: F,
    ) -> Result<reqwest::Response>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = std::result::Result<reqwest::Response, String>>,
    {
        let mut last: Option<HarvestError> = None;
        for attempt in 1..=attempts {
            self.lane_bucket(lane).acquire(1).await;
            {
                let _permit = self.0.sem.acquire().await;
                match op().await {
                    Err(msg) => {
                        self.0.stats.lock().errors += 1;
                        let cert = is_certificate_error(&msg);
                        last = Some(HarvestError::Other(format!("cannot reach {label}: {msg}")));
                        if cert {
                            break;
                        }
                    }
                    Ok(resp) => {
                        self.0.stats.lock().requests += 1;
                        let status = resp.status();
                        if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::FORBIDDEN {
                            let seconds = resp
                                .headers()
                                .get(reqwest::header::RETRY_AFTER)
                                .and_then(|v| v.to_str().ok())
                                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                                .and_then(|s| s.parse::<f64>().ok());
                            self.penalise(seconds);
                            last = Some(HarvestError::RateLimited(format!("HTTP {} for {label}", status.as_u16())));
                        } else if status.as_u16() >= 500 {
                            last = Some(HarvestError::Other(format!("HTTP {} for {label}", status.as_u16())));
                        } else {
                            self.0.normal.record_success();
                            self.0.reserved.record_success();
                            return Ok(resp);
                        }
                    }
                }
            }
            if attempt < attempts {
                // Jitter, so a batch that fails together does not retry together.
                let base = f64::from(2u32.saturating_pow(attempt.min(10))).min(120.0);
                let jitter: f64 = rand::random_range(0.8..1.2);
                tokio::time::sleep(Duration::from_secs_f64(base * jitter * self.0.backoff_scale)).await;
            }
        }

        let err = last.unwrap_or_else(|| HarvestError::Other("request failed".into()));
        {
            let mut st = self.0.stats.lock();
            st.errors += 1;
            let keep = st.last_errors.len().saturating_sub(9);
            st.last_errors.drain(..keep);
            st.last_errors.push(err.to_string());
        }
        Err(err)
    }

    async fn send(&self, lane: Lane, req: Req, attempts: u32) -> Result<reqwest::Response> {
        let label = req.url.to_string();
        let req = Arc::new(req);
        self.send_with(lane, &label, attempts, || {
            let this = self.clone();
            let req = Arc::clone(&req);
            async move { this.execute(&req).await }
        })
        .await
    }

    // ------------------------------------------------------------------
    // Public requests
    // ------------------------------------------------------------------

    /// GET an HTML page through the cache. Concurrent calls for the same URL
    /// coalesce into a single fetch (single-flight), including the
    /// cache-miss -> network -> cache-put path. 404 -> `HarvestError::Other("not found: <url>")`.
    pub async fn get_html(&self, url: &str, opts: GetOpts) -> Result<String> {
        let max_age = opts.max_age();
        if max_age > 0.0
            && let Some(body) = self.cache_get(url, max_age).await {
                self.0.stats.lock().cache_hits += 1;
                return Ok(body);
            }

        let key = format!("{}{}", if opts.authed { "A " } else { "N " }, url);
        let flight = {
            let mut flights = self.0.flights.lock();
            if let Some(f) = flights.get(&key) {
                f.clone()
            } else {
                let this = self.clone();
                let url = url.to_string();
                let k = key.clone();
                let handle = tokio::spawn(async move {
                    let r = this.fetch_html(&url, &opts, max_age).await;
                    this.0.flights.lock().remove(&k);
                    r.map(Arc::<str>::from).map_err(Arc::new)
                });
                let f: Flight = async move {
                    handle
                        .await
                        .unwrap_or_else(|e| Err(Arc::new(HarvestError::Other(format!("fetch task failed: {e}")))))
                }
                .boxed()
                .shared();
                flights.insert(key, f.clone());
                f
            }
        };
        match flight.await {
            Ok(body) => Ok(body.to_string()),
            Err(e) => Err(clone_error(&e)),
        }
    }

    /// The body of the flight: re-check the cache (another flight may have just
    /// filled it), then fetch, then cache.
    async fn fetch_html(&self, url: &str, opts: &GetOpts, max_age: f64) -> Result<String> {
        if max_age > 0.0
            && let Some(body) = self.cache_get(url, max_age).await {
                self.0.stats.lock().cache_hits += 1;
                return Ok(body);
            }
        let parsed = Self::parse_url(url)?;
        let req = Req {
            method: Method::GET,
            url: parsed,
            headers: Self::html_headers(opts.referer.as_deref()),
            body: None,
            authed: opts.authed,
        };
        let response = self.send(opts.lane, req, 4).await?;

        if response.status() == StatusCode::NOT_FOUND {
            return Err(HarvestError::Other(format!("not found: {url}")));
        }
        let ok = response.status().is_success();
        let etag = response.headers().get(reqwest::header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
        let body = response
            .text()
            .await
            .map_err(|e| HarvestError::Other(format!("cannot read {url}: {}", error_chain(&e))))?;
        if max_age > 0.0 && ok {
            self.cache_put(url, opts.kind, &body, etag).await;
        }
        Ok(body)
    }

    async fn cache_get(&self, url: &str, max_age: f64) -> Option<String> {
        let cache = self.0.cache.clone()?;
        let url = url.to_string();
        match tokio::task::spawn_blocking(move || cache.get(&url, max_age)).await {
            Ok(Ok(page)) => page.map(|p| p.body),
            Ok(Err(e)) => {
                tracing::warn!("{e}");
                None
            }
            Err(_) => None,
        }
    }

    async fn cache_put(&self, url: &str, kind: PageKind, body: &str, etag: Option<String>) {
        let Some(cache) = self.0.cache.clone() else { return };
        let (url, body) = (url.to_string(), body.to_string());
        let r = tokio::task::spawn_blocking(move || cache.put(&url, kind, &body, etag.as_deref())).await;
        if let Ok(Err(e)) = r {
            tracing::warn!("{e}");
        }
    }

    async fn api_value(&self, req: Req, url: String) -> Result<Value> {
        let response = self.send(Lane::Normal, req, 4).await?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| HarvestError::Other(format!("cannot read {url}: {}", error_chain(&e))))?;
        let data: Value = serde_json::from_slice(&bytes).map_err(|_| HarvestError::Api {
            message: format!("non-JSON response ({})", status.as_u16()),
            url: url.clone(),
            error_type: None,
            unspecified: false,
        })?;
        check_api_error(&data, &url)?;
        Ok(if data.is_object() { data } else { serde_json::json!({ "results": data }) })
    }

    /// POST a Bandcamp API endpoint and validate the *body*, not the status.
    /// `path` starting with `/` is resolved against `https://bandcamp.com`.
    /// The legacy default referer is `https://bandcamp.com/` (pass `Some(..)`).
    pub async fn post_api(&self, path: &str, payload: &Value, authed: bool, referer: Option<&str>) -> Result<Value> {
        let url = self.api_url(path);
        let body = serde_json::to_vec(payload).map_err(|e| HarvestError::Other(e.to_string()))?;
        let req = Req {
            method: Method::POST,
            url: Self::parse_url(&url)?,
            headers: Self::api_headers(referer),
            body: Some(Bytes::from(body)),
            authed,
        };
        self.api_value(req, url).await
    }

    /// GET a Bandcamp API endpoint with query `params` (legacy referer `https://bandcamp.com/`).
    pub async fn get_api(&self, path: &str, params: &[(&str, &str)], authed: bool) -> Result<Value> {
        let url = self.api_url(path);
        let mut parsed = Self::parse_url(&url)?;
        if !params.is_empty() {
            parsed.query_pairs_mut().extend_pairs(params.iter().copied());
        }
        let req = Req {
            method: Method::GET,
            url: parsed,
            headers: Self::api_headers(Some(API_REFERER)),
            body: None,
            authed,
        };
        self.api_value(req, url).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::http::{HeaderMap as AxHeaders, StatusCode as AxStatus};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use serde_json::json;

    use super::*;

    const URL: &str = "https://bandcamp.com/api/whatever";

    // ---- the rule that governs the whole layer ------------------------

    #[test]
    fn must_be_logged_in_becomes_an_identity_error() {
        // Verified live: this arrives as HTTP **200**, not 401.
        let e = check_api_error(&json!({"error": true, "error_message": "must be logged in"}), URL).unwrap_err();
        assert!(matches!(e, HarvestError::IdentityExpired(_)), "{e:?}");
    }

    #[test]
    fn api_special_exception_is_detected() {
        let e = check_api_error(
            &json!({"__api_special__": "exception", "error_type": "Endpoints::MissingParamError"}),
            URL,
        )
        .unwrap_err();
        match e {
            HarvestError::Api { error_type, .. } => {
                assert_eq!(error_type.as_deref(), Some("Endpoints::MissingParamError"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn generic_error_body_is_detected() {
        let e = check_api_error(&json!({"error": true, "error_message": "nope"}), URL).unwrap_err();
        assert!(matches!(e, HarvestError::Api { unspecified: false, .. }));
        let e = check_api_error(&json!({"error": true}), URL).unwrap_err();
        match e {
            HarvestError::Api { unspecified, message, .. } => {
                assert!(unspecified);
                assert_eq!(message, "unspecified error");
            }
            other => panic!("{other:?}"),
        }
        assert!(check_api_error(&json!({"error": "true"}), URL).is_err());
    }

    #[test]
    fn successful_bodies_pass_through() {
        check_api_error(&json!({"items": [], "more_available": false}), URL).unwrap();
        check_api_error(&json!({"error": false}), URL).unwrap();
        check_api_error(&json!([]), URL).unwrap(); // a bare list is not an error shape
    }

    #[test]
    fn error_message_display_carries_the_url() {
        let e = check_api_error(&json!({"error": true, "error_message": "nope"}), URL).unwrap_err();
        assert_eq!(e.to_string(), format!("nope ({URL})"));
    }

    // ---- cookie host rule ----------------------------------------------

    #[test]
    fn bandcamp_host_check() {
        for h in ["bandcamp.com", "a.bandcamp.com", "A.Bandcamp.COM", "x.y.bandcamp.com", "bandcamp.com."] {
            assert!(is_bandcamp_host(h), "{h}");
        }
        for h in ["evilbandcamp.com", "bandcamp.com.evil.net", "127.0.0.1", "bandcamp.co", "notbandcamp.com", ""] {
            assert!(!is_bandcamp_host(h), "{h}");
        }
    }

    #[test]
    fn certificate_errors_are_recognised() {
        assert!(is_certificate_error(
            "[SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed: Hostname mismatch"
        ));
        assert!(is_certificate_error(
            "error sending request: client error (Connect): invalid peer certificate: UnknownIssuer"
        ));
        assert!(is_certificate_error("invalid peer certificate: NotValidForName"));
        assert!(!is_certificate_error("connection refused"));
    }

    // ---- transport failures (synthetic attempts) ------------------------

    fn fast(opts: ClientOptions) -> BandcampClient {
        BandcampClient::new(ClientOptions { rate_per_sec: 1000.0, burst: 1000, backoff_scale: 0.001, ..opts })
    }

    fn synthetic(status: u16, retry_after: Option<&str>) -> reqwest::Response {
        let mut b = http::Response::builder().status(status);
        if let Some(r) = retry_after {
            b = b.header("retry-after", r);
        }
        reqwest::Response::from(b.body("x").unwrap())
    }

    #[tokio::test]
    async fn transport_errors_surface_as_harvest_errors() {
        // Callers catch HarvestError so one dead page cannot kill a whole
        // sweep; a raw transport error must not slip past that contract.
        let c = fast(ClientOptions::default());
        let e = c
            .send_with(Lane::Normal, "https://example.com/music", 1, || async { Err("connection refused".to_string()) })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("cannot reach"), "{e}");
        assert_eq!(c.stats().errors, 2); // legacy: once per failed attempt + once at the end
        assert_eq!(c.stats().last_errors.len(), 1);
    }

    #[tokio::test]
    async fn real_connection_refused_is_a_harvest_error() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let c = fast(ClientOptions::default());
        let e = c.get_html(&format!("http://127.0.0.1:{port}/x"), GetOpts::kind(PageKind::Discover)).await.unwrap_err();
        assert!(e.to_string().contains("cannot reach"), "{e}");
    }

    #[tokio::test]
    async fn certificate_failures_do_not_retry() {
        // A cert mismatch is a property of the host -- a lapsed custom domain
        // parked on someone else's certificate -- so retries cannot change it.
        let calls = Arc::new(AtomicUsize::new(0));
        let c = fast(ClientOptions::default());
        let n = calls.clone();
        let e = c
            .send_with(Lane::Normal, "https://xkalay.com/music", 4, move || {
                n.fetch_add(1, Ordering::SeqCst);
                async {
                    Err("[SSL: CERTIFICATE_VERIFY_FAILED] certificate verify failed: Hostname mismatch, \
                         certificate is not valid for 'xkalay.com'"
                        .to_string())
                }
            })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("cannot reach"));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "a deterministic TLS failure should not be retried");
    }

    #[tokio::test]
    async fn other_transport_errors_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = fast(ClientOptions::default());
        let n = calls.clone();
        let _ = c
            .send_with(Lane::Normal, "x", 4, move || {
                n.fetch_add(1, Ordering::SeqCst);
                async { Err("connection reset".to_string()) }
            })
            .await;
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn rate_limit_penalises_both_lanes_and_honours_retry_after() {
        let c = fast(ClientOptions::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let e = c
            .send_with(Lane::Normal, "x", 1, move || {
                n.fetch_add(1, Ordering::SeqCst);
                async { Ok(synthetic(429, Some("45"))) }
            })
            .await
            .unwrap_err();
        assert!(matches!(e, HarvestError::RateLimited(_)));
        for lane in [Lane::Normal, Lane::Reserved] {
            let p = c.limiter(lane).penalised_for();
            assert!((44.0..=46.0).contains(&p), "{lane:?}: {p}");
            assert!(c.limiter(lane).rate() < c.limiter(lane).base_rate());
        }
    }

    #[tokio::test]
    async fn non_digit_retry_after_falls_back_to_default_penalty() {
        let c = fast(ClientOptions::default());
        let _ = c.send_with(Lane::Normal, "x", 1, || async { Ok(synthetic(403, Some("Wed, 21 Oct"))) }).await;
        let p = c.limiter(Lane::Normal).penalised_for();
        assert!((59.0..=61.0).contains(&p), "{p}");
    }

    #[tokio::test]
    async fn server_errors_retry_then_surface() {
        let c = fast(ClientOptions::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let n = calls.clone();
        let e = c
            .send_with(Lane::Normal, "x", 3, move || {
                n.fetch_add(1, Ordering::SeqCst);
                async { Ok(synthetic(503, None)) }
            })
            .await
            .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(e.to_string(), "HTTP 503 for x");
    }

    #[tokio::test]
    async fn last_errors_keeps_ten() {
        let c = fast(ClientOptions::default());
        for i in 0..12 {
            let _ = c.send_with(Lane::Normal, &format!("u{i}"), 1, || async { Ok(synthetic(500, None)) }).await;
        }
        let st = c.stats();
        assert_eq!(st.last_errors.len(), 10);
        assert_eq!(st.last_errors.last().unwrap(), "HTTP 500 for u11");
        assert_eq!(st.last_errors.first().unwrap(), "HTTP 500 for u2");
    }

    // ---- local test servers ----------------------------------------------

    async fn serve(app: Router) -> SocketAddr {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        addr
    }

    fn cookie_of(h: &AxHeaders) -> String {
        h.get("cookie").and_then(|v| v.to_str().ok()).unwrap_or("<none>").to_string()
    }

    #[tokio::test]
    async fn headers_match_the_legacy_sets() {
        async fn echo(h: AxHeaders) -> impl IntoResponse {
            let g = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("-").to_string();
            format!(
                "ua={}|al={}|accept={}|ct={}|origin={}|xrw={}|ref={}",
                g("user-agent"),
                g("accept-language"),
                g("accept"),
                g("content-type"),
                g("origin"),
                g("x-requested-with"),
                g("referer")
            )
        }
        let addr = serve(Router::new().route("/h", get(echo)).route("/api", post(echo))).await;
        let c = fast(ClientOptions { user_agent: Some("TestUA/1".into()), ..Default::default() });

        let html = c
            .get_html(&format!("http://{addr}/h"), GetOpts::kind(PageKind::Discover).referer("https://r/"))
            .await
            .unwrap();
        assert_eq!(
            html,
            format!("ua=TestUA/1|al={ACCEPT_LANGUAGE}|accept={ACCEPT_HTML}|ct=-|origin=-|xrw=-|ref=https://r/")
        );

        // post_api returns an error since the echo is not JSON; read the body through a raw call instead.
        let e = c.post_api(&format!("http://{addr}/api"), &json!({}), false, Some(API_REFERER)).await.unwrap_err();
        assert!(e.to_string().starts_with("non-JSON response (200)"), "{e}");
    }

    #[tokio::test]
    async fn api_calls_validate_the_body() {
        async fn ok() -> impl IntoResponse {
            axum::Json(json!({"items": [1, 2]}))
        }
        async fn bare_list() -> impl IntoResponse {
            axum::Json(json!([1, 2]))
        }
        async fn logged_out() -> impl IntoResponse {
            axum::Json(json!({"error": true, "error_message": "must be logged in"}))
        }
        async fn echo_ct(h: AxHeaders, body: String) -> impl IntoResponse {
            axum::Json(json!({
                "ct": h.get("content-type").and_then(|v| v.to_str().ok()),
                "origin": h.get("origin").and_then(|v| v.to_str().ok()),
                "xrw": h.get("x-requested-with").and_then(|v| v.to_str().ok()),
                "referer": h.get("referer").and_then(|v| v.to_str().ok()),
                "body": body,
            }))
        }
        async fn q(axum::extract::Query(p): axum::extract::Query<HashMap<String, String>>) -> impl IntoResponse {
            axum::Json(json!({"q": p}))
        }
        let app = Router::new()
            .route("/ok", get(ok))
            .route("/list", get(bare_list))
            .route("/out", post(logged_out))
            .route("/echo", post(echo_ct))
            .route("/q", get(q));
        let addr = serve(app).await;
        let c = fast(ClientOptions::default());
        let base = format!("http://{addr}");

        assert_eq!(c.get_api(&format!("{base}/ok"), &[], false).await.unwrap(), json!({"items": [1, 2]}));
        assert_eq!(c.get_api(&format!("{base}/list"), &[], false).await.unwrap(), json!({"results": [1, 2]}));
        let e = c.post_api(&format!("{base}/out"), &json!({}), false, None).await.unwrap_err();
        assert!(matches!(e, HarvestError::IdentityExpired(_)));

        let v = c.post_api(&format!("{base}/echo"), &json!({"a": 1}), false, Some("https://bandcamp.com/")).await.unwrap();
        assert_eq!(v["ct"], "application/json");
        assert_eq!(v["origin"], "https://bandcamp.com");
        assert_eq!(v["xrw"], "XMLHttpRequest");
        assert_eq!(v["referer"], "https://bandcamp.com/");
        assert_eq!(v["body"], "{\"a\":1}");

        let v = c.get_api(&format!("{base}/q"), &[("tags", "dub techno"), ("p", "2")], false).await.unwrap();
        assert_eq!(v["q"], json!({"tags": "dub techno", "p": "2"}));
    }

    #[tokio::test]
    async fn html_404_is_not_found_and_5xx_retries_succeed() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h2 = hits.clone();
        let app = Router::new()
            .route("/missing", get(|| async { AxStatus::NOT_FOUND }))
            .route(
                "/flaky",
                get(move || {
                    let n = h2.fetch_add(1, Ordering::SeqCst);
                    async move { if n < 2 { (AxStatus::BAD_GATEWAY, "no".to_string()) } else { (AxStatus::OK, "fine".to_string()) } }
                }),
            );
        let addr = serve(app).await;
        let c = fast(ClientOptions::default());
        let url = format!("http://{addr}/missing");
        let e = c.get_html(&url, GetOpts::default()).await.unwrap_err();
        assert_eq!(e.to_string(), format!("not found: {url}"));
        assert!(matches!(e, HarvestError::Other(_)));
        let body = c.get_html(&format!("http://{addr}/flaky"), GetOpts::kind(PageKind::Discover)).await.unwrap();
        assert_eq!(body, "fine");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    // ---- cookie never leaves *.bandcamp.com -----------------------------------

    #[tokio::test]
    async fn cookie_never_reaches_non_bandcamp_hosts_even_after_redirect() {
        // B: a plain local server (not bandcamp). A: poses as fake.bandcamp.com via DNS override.
        let seen_b = Arc::new(Mutex::new(Vec::<String>::new()));
        let sb = seen_b.clone();
        let b = serve(Router::new().route(
            "/echo",
            get(move |h: AxHeaders| {
                let sb = sb.clone();
                async move {
                    sb.lock().push(cookie_of(&h));
                    "b"
                }
            }),
        ))
        .await;

        let seen_a = Arc::new(Mutex::new(Vec::<String>::new()));
        let sa = seen_a.clone();
        let sa2 = seen_a.clone();
        let b_url = format!("http://127.0.0.1:{}/echo", b.port());
        let b_url2 = b_url.clone();
        let a_app = Router::new()
            .route(
                "/echo",
                get(move |h: AxHeaders| {
                    let sa = sa.clone();
                    async move {
                        sa.lock().push(cookie_of(&h));
                        "a"
                    }
                }),
            )
            .route(
                "/redir",
                get(move |h: AxHeaders| {
                    let sa = sa2.clone();
                    let loc = b_url2.clone();
                    async move {
                        sa.lock().push(cookie_of(&h));
                        (AxStatus::FOUND, [("location", loc)], "")
                    }
                }),
            );
        let a = serve(a_app).await;

        let c = fast(ClientOptions {
            cookie: Some("identity=SECRETVALUE".into()),
            resolve: vec![("fake.bandcamp.com".into(), a)],
            ..Default::default()
        });
        assert!(c.has_cookie());
        let a_base = format!("http://fake.bandcamp.com:{}", a.port());
        let no_cache = |authed| GetOpts::kind(PageKind::Discover).authed(authed);

        // authed to a bandcamp host: cookie is sent
        c.get_html(&format!("{a_base}/echo"), no_cache(true)).await.unwrap();
        // not authed: not sent, even to a bandcamp host
        c.get_html(&format!("{a_base}/echo?n=1"), no_cache(false)).await.unwrap();
        // authed but redirected from a bandcamp host to a foreign host: sent to A, absent on B
        let body = c.get_html(&format!("{a_base}/redir"), no_cache(true)).await.unwrap();
        assert_eq!(body, "b");
        // authed straight to a non-bandcamp host
        c.get_html(&b_url, no_cache(true)).await.unwrap();
        // api calls too
        let _ = c.get_api(&b_url, &[], true).await;
        let _ = c.post_api(&b_url, &json!({}), true, None).await;

        assert_eq!(*seen_a.lock(), ["identity=SECRETVALUE", "<none>", "identity=SECRETVALUE"]);
        let b_seen = seen_b.lock().clone();
        assert_eq!(b_seen.len(), 3); // redirect hop, direct GET, get_api (post_api hits a GET-only route)
        assert!(b_seen.iter().all(|s| s == "<none>"), "cookie leaked: {b_seen:?}");

        // cdn client carries no cookie either
        let r = c.cdn_client().get(&b_url).send().await.unwrap();
        assert_eq!(r.text().await.unwrap(), "b");
        assert_eq!(seen_b.lock().last().unwrap(), "<none>");
    }

    #[test]
    fn debug_never_prints_the_cookie() {
        let c = BandcampClient::new(ClientOptions { cookie: Some("identity=SECRETVALUE".into()), ..Default::default() });
        assert!(!format!("{c:?}").contains("SECRETVALUE"));
    }

    // ---- single flight + cache ------------------------------------------------

    #[tokio::test]
    async fn concurrent_callers_coalesce_into_one_request() {
        let count = Arc::new(AtomicUsize::new(0));
        let n = count.clone();
        let app = Router::new().route(
            "/page",
            get(move || {
                let n = n.clone();
                async move {
                    n.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    "<html>once</html>"
                }
            }),
        );
        let addr = serve(app).await;
        let cache = PageCache::open_in_memory(super::super::cache::DEFAULT_MAX_BYTES).unwrap().shared();
        let c = fast(ClientOptions { cache: Some(cache.clone()), ..Default::default() });
        let url = format!("http://{addr}/page");

        let mut tasks = Vec::new();
        for _ in 0..24 {
            let (c, url) = (c.clone(), url.clone());
            tasks.push(tokio::spawn(async move { c.get_html(&url, GetOpts::kind(PageKind::Album)).await }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap().unwrap(), "<html>once</html>");
        }
        assert_eq!(count.load(Ordering::SeqCst), 1, "single-flight: exactly one request");
        assert_eq!(cache.stats().unwrap().entries, 1);

        // a later call is a cache hit: still one request
        assert_eq!(c.get_html(&url, GetOpts::kind(PageKind::Album)).await.unwrap(), "<html>once</html>");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(c.stats().cache_hits >= 1);

        // Discover (ttl 0) bypasses the cache but still coalesces
        let before = count.load(Ordering::SeqCst);
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let (c, url) = (c.clone(), url.clone());
            tasks.push(tokio::spawn(async move { c.get_html(&url, GetOpts::kind(PageKind::Discover)).await }));
        }
        for t in tasks {
            t.await.unwrap().unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), before + 1);
    }

    #[tokio::test]
    async fn single_flight_shares_errors() {
        let count = Arc::new(AtomicUsize::new(0));
        let n = count.clone();
        let app = Router::new().route(
            "/gone",
            get(move || {
                let n = n.clone();
                async move {
                    n.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    AxStatus::NOT_FOUND
                }
            }),
        );
        let addr = serve(app).await;
        let c = fast(ClientOptions::default());
        let url = format!("http://{addr}/gone");
        let rs = futures::future::join_all((0..6).map(|_| c.get_html(&url, GetOpts::default()))).await;
        assert!(rs.iter().all(|r| r.as_ref().unwrap_err().to_string() == format!("not found: {url}")));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ttl_override_and_non_2xx_are_not_cached() {
        let count = Arc::new(AtomicUsize::new(0));
        let n = count.clone();
        let app = Router::new().route(
            "/p",
            get(move || {
                let n = n.clone();
                async move {
                    n.fetch_add(1, Ordering::SeqCst);
                    "body"
                }
            }),
        );
        let addr = serve(app).await;
        let cache = PageCache::open_in_memory(1 << 20).unwrap().shared();
        let c = fast(ClientOptions { cache: Some(cache), ..Default::default() });
        let url = format!("http://{addr}/p");
        let zero = GetOpts::kind(PageKind::Album).ttl(Duration::ZERO);
        c.get_html(&url, zero.clone()).await.unwrap();
        c.get_html(&url, zero).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
        c.get_html(&url, GetOpts::kind(PageKind::Music)).await.unwrap();
        c.get_html(&url, GetOpts::kind(PageKind::Music)).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn reserved_lane_is_not_blocked_by_a_drained_normal_lane() {
        let app = Router::new().route("/p", get(|| async { "ok" }));
        let addr = serve(app).await;
        let c = BandcampClient::new(ClientOptions { rate_per_sec: 0.01, burst: 1, ..Default::default() });
        let url = format!("http://{addr}/p");
        c.get_html(&url, GetOpts::kind(PageKind::Discover)).await.unwrap(); // drains Normal
        let t = std::time::Instant::now();
        c.get_html(&url, GetOpts::kind(PageKind::Discover).lane(Lane::Reserved)).await.unwrap();
        assert!(t.elapsed() < Duration::from_secs(2), "reserved lane stalled");
    }
}
