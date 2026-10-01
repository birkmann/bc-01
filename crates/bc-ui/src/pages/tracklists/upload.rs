//! Multipart upload of tracklist CSVs (`POST /tracklists/parse`, field `files`).
use bc_types::bandcamp::ParsedTracklists;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::api::ApiErr;
use crate::util::window;

pub fn files_of(list: Option<web_sys::FileList>) -> Vec<web_sys::File> {
    let Some(list) = list else { return vec![] };
    (0..list.length()).filter_map(|i| list.get(i)).collect()
}

pub async fn parse_files(files: Vec<web_sys::File>) -> Result<ParsedTracklists, ApiErr> {
    let form = web_sys::FormData::new().map_err(|_| ApiErr::network("form"))?;
    for f in &files {
        form.append_with_blob_and_filename("files", f, &f.name()).map_err(|_| ApiErr::network("form"))?;
    }
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_body(&form);
    let resp = JsFuture::from(window().fetch_with_str_and_init("/api/tracklists/parse", &init)).await.map_err(|e| ApiErr::network(format!("{e:?}")))?;
    let resp: web_sys::Response = resp.unchecked_into();
    let status = resp.status();
    let text = JsFuture::from(resp.text().map_err(|_| ApiErr::network("body"))?).await.map_err(|e| ApiErr::network(format!("{e:?}")))?.as_string().unwrap_or_default();
    if !resp.ok() {
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        return Err(ApiErr {
            status,
            title: v.get("title").and_then(|t| t.as_str()).map(str::to_string).unwrap_or_else(|| resp.status_text()),
            detail: v.get("detail").and_then(|t| t.as_str()).map(str::to_string),
        });
    }
    serde_json::from_str(&text).map_err(|e| ApiErr { status: 200, title: "Bad response".into(), detail: Some(e.to_string()) })
}
