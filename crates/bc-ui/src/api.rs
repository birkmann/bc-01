//! Thin fetch client. Everything the UI knows about the backend is HTTP + WS
//! (even inside Tauri). Errors arrive as problem+json and are surfaced as [`ApiErr`].
use serde::Serialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::util::window;

pub const API: &str = "/api";

#[derive(Debug, Clone, PartialEq)]
pub struct ApiErr {
    pub status: u16,
    pub title: String,
    pub detail: Option<String>,
}

impl ApiErr {
    pub fn network(msg: impl Into<String>) -> Self {
        Self { status: 0, title: "Network error".into(), detail: Some(msg.into()) }
    }
    pub fn message(&self) -> String {
        match &self.detail {
            Some(d) if !d.is_empty() => format!("{}: {d}", self.title),
            _ => self.title.clone(),
        }
    }
}
impl std::fmt::Display for ApiErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

pub type ApiResult<T> = Result<T, ApiErr>;

fn full(path: &str) -> String {
    if path.starts_with("http") || path.starts_with("/api") { path.to_string() } else { format!("{API}{path}") }
}

async fn send_raw(method: &str, path: &str, body: Option<String>) -> ApiResult<web_sys::Response> {
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    if let Some(b) = body {
        let h = web_sys::Headers::new().map_err(|_| ApiErr::network("headers"))?;
        let _ = h.set("content-type", "application/json");
        init.set_headers(&h);
        init.set_body(&wasm_bindgen::JsValue::from_str(&b));
    }
    let promise = window().fetch_with_str_and_init(&full(path), &init);
    let resp = JsFuture::from(promise).await.map_err(|e| ApiErr::network(format!("{e:?}")))?;
    let resp: web_sys::Response = resp.unchecked_into();
    if resp.ok() {
        return Ok(resp);
    }
    let status = resp.status();
    let mut err = ApiErr { status, title: resp.status_text(), detail: None };
    if let Ok(p) = resp.json() {
        if let Ok(v) = JsFuture::from(p).await {
            if let Ok(prob) = serde_wasm_json(&v) {
                if let Some(t) = prob.get("title").and_then(|t| t.as_str()) {
                    err.title = t.to_string();
                }
                err.detail = prob.get("detail").and_then(|t| t.as_str()).map(str::to_string);
            }
        }
    }
    Err(err)
}

fn serde_wasm_json(v: &wasm_bindgen::JsValue) -> Result<serde_json::Value, ()> {
    let s = js_sys::JSON::stringify(v).map_err(|_| ())?;
    serde_json::from_str(&s.as_string().ok_or(())?).map_err(|_| ())
}

async fn read_json<T: DeserializeOwned>(resp: web_sys::Response) -> ApiResult<T> {
    if resp.status() == 204 {
        return serde_json::from_str("null").map_err(|e| ApiErr::network(e.to_string()));
    }
    let text = JsFuture::from(resp.text().map_err(|_| ApiErr::network("body"))?)
        .await
        .map_err(|e| ApiErr::network(format!("{e:?}")))?
        .as_string()
        .unwrap_or_default();
    if text.is_empty() {
        return serde_json::from_str("null").map_err(|e| ApiErr::network(e.to_string()));
    }
    serde_json::from_str(&text).map_err(|e| ApiErr { status: 200, title: "Bad response".into(), detail: Some(format!("{e} in {}", &text.chars().take(160).collect::<String>())) })
}

pub async fn get<T: DeserializeOwned>(path: &str) -> ApiResult<T> {
    read_json(send_raw("GET", path, None).await?).await
}

pub async fn send<B: Serialize, T: DeserializeOwned>(method: &str, path: &str, body: &B) -> ApiResult<T> {
    let b = serde_json::to_string(body).map_err(|e| ApiErr::network(e.to_string()))?;
    read_json(send_raw(method, path, Some(b)).await?).await
}

pub async fn post<B: Serialize, T: DeserializeOwned>(path: &str, body: &B) -> ApiResult<T> {
    send("POST", path, body).await
}
pub async fn put<B: Serialize, T: DeserializeOwned>(path: &str, body: &B) -> ApiResult<T> {
    send("PUT", path, body).await
}
pub async fn patch<B: Serialize, T: DeserializeOwned>(path: &str, body: &B) -> ApiResult<T> {
    send("PATCH", path, body).await
}

/// Bodyless request whose response is ignored (POST /x/pause, DELETE ...).
pub async fn call(method: &str, path: &str) -> ApiResult<()> {
    send_raw(method, path, None).await.map(|_| ())
}
pub async fn call_json<B: Serialize>(method: &str, path: &str, body: &B) -> ApiResult<()> {
    let b = serde_json::to_string(body).map_err(|e| ApiErr::network(e.to_string()))?;
    send_raw(method, path, Some(b)).await.map(|_| ())
}

/// Multipart/raw text upload (tracklists, URL lists).
pub async fn post_text(path: &str, content_type: &str, body: &str) -> ApiResult<serde_json::Value> {
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    let h = web_sys::Headers::new().map_err(|_| ApiErr::network("headers"))?;
    let _ = h.set("content-type", content_type);
    init.set_headers(&h);
    init.set_body(&wasm_bindgen::JsValue::from_str(body));
    let resp = JsFuture::from(window().fetch_with_str_and_init(&full(path), &init))
        .await
        .map_err(|e| ApiErr::network(format!("{e:?}")))?;
    let resp: web_sys::Response = resp.unchecked_into();
    if !resp.ok() {
        return Err(ApiErr { status: resp.status(), title: resp.status_text(), detail: None });
    }
    read_json(resp).await
}

/// Raw bytes (waveform `.bcw2`).
pub async fn get_bytes(path: &str) -> ApiResult<Vec<u8>> {
    let resp = send_raw("GET", path, None).await?;
    let buf = JsFuture::from(resp.array_buffer().map_err(|_| ApiErr::network("body"))?)
        .await
        .map_err(|e| ApiErr::network(format!("{e:?}")))?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}
