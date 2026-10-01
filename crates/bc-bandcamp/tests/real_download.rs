//! Manual end-to-end check (ignored by default; needs network): download one real release with the
//! native downloader and the real `BcLibrary` ingest into a COPY of the library DB.
//!
//! `BC_REAL_URL=https://x.bandcamp.com/album/y BC_REAL_DATA=/path/dir-containing-library.db-copy \
//!  cargo test -p bc-bandcamp --test real_download -- --ignored --nocapture`

use std::sync::Arc;
use std::time::Duration;

use bc_bandcamp::service::BandcampService;
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_jobs::{JobsService, NewItem, NewJob};

#[tokio::test]
#[ignore]
async fn real_release_native_download_and_ingest() {
    let url = std::env::var("BC_REAL_URL").expect("BC_REAL_URL");
    let data = std::path::PathBuf::from(std::env::var("BC_REAL_DATA").expect("BC_REAL_DATA"));
    let mut cfg = Config::from_env();
    cfg.data_dir = data.clone();
    cfg.download_dir = data.join("dl");
    std::fs::create_dir_all(&cfg.download_dir).unwrap();
    let db = Db::open(cfg.db_path()).unwrap();
    let bus = Arc::new(EventBus::new());
    let jobs = JobsService::new(db.clone(), bus.clone());
    jobs.start().await;
    let svc = BandcampService::new(db.clone(), bus, cfg.clone(), jobs.clone());
    svc.start().await;
    let job = jobs.store().create_job(NewJob::new("download", vec![NewItem::url(url, "album")]).label("real check")).unwrap();
    let mut status = String::new();
    for _ in 0..240 {
        status = jobs.store().get_job(&job.id).unwrap().unwrap().status;
        if matches!(status.as_str(), "completed" | "failed" | "cancelled") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let items = jobs.store().list_items(&job.id, None, &[], 0, None).unwrap();
    println!("job status {status}; item: {:?} / {:?} / {:?}", items[0].status, items[0].message, items[0].last_error);
    assert_eq!(status, "completed");
}
