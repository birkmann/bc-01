#![allow(dead_code)]
use bc_core::Config;
use bc_server::ServerOptions;

pub fn config(dir: &std::path::Path) -> Config {
    let mut c = Config::from_env();
    c.data_dir = dir.to_path_buf();
    c.download_dir = dir.join("downloads");
    c.library_root = None;
    c.port = 0;
    c.lan = false;
    c
}

pub fn opts_with_ui(ui: Option<&std::path::Path>) -> ServerOptions {
    ServerOptions { ui_dir: ui.map(|p| p.to_path_buf()), no_ui: false, no_services: true, coep: None, bandcamp_login: false }
}

pub async fn body_string(resp: axum::response::Response) -> String {
    use http_body_util::BodyExt;
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&b).to_string()
}

pub fn get(path: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder().uri(path).header("host", "localhost").body(axum::body::Body::empty()).unwrap()
}
