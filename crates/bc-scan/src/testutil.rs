//! Test fixtures shared by unit tests (compiled only for tests).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_libcore::Ctx;

pub struct Env {
    pub dir: tempfile::TempDir,
    pub ctx: Ctx,
}

impl Env {
    pub fn music(&self) -> PathBuf {
        self.dir.path().join("music")
    }
}

pub fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(dir.path().join("music")).unwrap();
    let mut config = Config::from_env();
    config.data_dir = data.clone();
    config.library_root = Some(dir.path().join("music"));
    config.download_dir = data.join("downloads");
    let db = Db::open(data.join("library.db")).unwrap();
    let ctx = Ctx::new(db, Arc::new(EventBus::new()), config);
    Env { dir, ctx }
}

pub fn add_root(ctx: &Ctx, path: &Path, kind: &str) -> i64 {
    crate::roots::add_root(ctx, &path.to_string_lossy(), kind).unwrap().id
}

pub fn scalar(ctx: &Ctx, sql: &str) -> i64 {
    let sql = sql.to_string();
    ctx.db.read(|c| Ok(c.query_row(&sql, [], |r| r.get::<_, i64>(0))?)).unwrap()
}

pub fn write_junk_mp3(path: &Path, extra: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut b = vec![0xff, 0xfb, 0x90, 0x00];
    b.extend(std::iter::repeat_n(0u8, extra));
    std::fs::write(path, b).unwrap();
}
