//! Domain services (PLAN §9.1): constructed once in `build_state`, started in `start_services`,
//! routers nested under `/api`. Each service sits behind a cargo feature so the server still
//! builds while a sibling crate is mid-change.
use std::sync::Arc;

use axum::Router;
use bc_core::{Config, EventBus};
use bc_db::Db;

use crate::state::AppState;

#[derive(Default)]
pub struct Services {
    #[cfg(feature = "jobs")]
    pub jobs: Option<bc_jobs::JobsService>,
    #[cfg(feature = "analysis")]
    pub analysis: Option<bc_analysis::AnalysisService>,
    #[cfg(feature = "recommend")]
    pub recommend: Option<bc_recommend::RecommendService>,
    #[cfg(feature = "player")]
    pub player: Option<Arc<bc_engine::PlayerService>>,
    #[cfg(feature = "library")]
    pub library: Option<bc_library::LibraryService>,
    #[cfg(feature = "bandcamp")]
    pub bandcamp: Option<bc_bandcamp::service::BandcampService>,
}

impl Services {
    /// Construct (no background work yet).
    #[allow(unused_variables)]
    pub fn build(db: &Db, bus: &Arc<EventBus>, config: &Arc<Config>) -> Self {
        let mut s = Services::default();
        #[cfg(feature = "jobs")]
        {
            s.jobs = Some(bc_jobs::JobsService::new(db.clone(), bus.clone()));
        }
        // Bandcamp needs the job store; it also provides the lookup the library's stray merge uses.
        #[cfg(all(feature = "bandcamp", feature = "jobs"))]
        {
            if let Some(j) = &s.jobs {
                s.bandcamp = Some(bc_bandcamp::service::BandcampService::new(db.clone(), bus.clone(), (**config).clone(), j.clone()));
            }
        }
        #[cfg(feature = "library")]
        {
            let mut lib = bc_library::LibraryService::new(db.clone(), bus.clone(), (**config).clone());
            #[cfg(feature = "bandcamp")]
            if let Some(bc) = &s.bandcamp {
                // downloads queued by the library (fill missing, loved download) wake the download worker
                lib = lib.with_jobs(Arc::new(WakeDownloads { inner: bc_libcore::LocalJobs::new(bus.clone()), bc: bc.clone() }));
                lib = lib.with_bandcamp(bc.lookup());
            }
            s.library = Some(lib);
        }
        #[cfg(all(feature = "analysis", feature = "jobs"))]
        {
            if let Some(j) = &s.jobs {
                s.analysis = Some(bc_analysis::AnalysisService::new(db.clone(), bus.clone(), config.clone(), j));
            }
        }
        #[cfg(feature = "recommend")]
        {
            #[allow(unused_mut)]
            let mut rec = bc_recommend::RecommendService::new(db.clone(), bus.clone(), config.clone());
            #[cfg(feature = "analysis")]
            if let Some(a) = &s.analysis {
                rec = rec.with_cue_source(Arc::new(a.cue_source()));
            }
            s.recommend = Some(rec);
        }
        #[cfg(feature = "player")]
        {
            let base = format!("http://{}:{}", if config.host == "0.0.0.0" { "127.0.0.1" } else { &config.host }, config.port);
            #[allow(unused_mut)]
            let mut ports = bc_engine::ports::DbPorts::new(db.clone()).into_ports(&base);
            // Fan and Explore sources need the real Bandcamp service (needs a tokio runtime to block on).
            #[cfg(feature = "bandcamp")]
            if let (Some(bc), Ok(rt)) = (&s.bandcamp, tokio::runtime::Handle::try_current()) {
                ports.bandcamp = Arc::new(BcPlayerPort::new(bc.clone(), rt, ports.bandcamp.clone()));
            }
            let cfg = bc_engine::SessionConfig { output: bc_engine::service::output_from_env(), base_url: base, ..Default::default() };
            let svc = bc_engine::PlayerService::with_ports(ports, bus.clone(), cfg).with_render(db.clone(), &config.ffmpeg_bin);
            s.player = Some(Arc::new(svc));
        }
        s
    }

    /// Spawn background workers. Order matters: jobs first (crash recovery before any claim).
    pub async fn start(&self) {
        #[cfg(feature = "jobs")]
        if let Some(j) = &self.jobs {
            j.start().await;
        }
        #[cfg(feature = "library")]
        if let Some(l) = &self.library {
            l.start().await;
        }
        #[cfg(feature = "bandcamp")]
        if let Some(b) = &self.bandcamp {
            b.start().await;
        }
        #[cfg(feature = "analysis")]
        if let Some(a) = &self.analysis {
            a.start().await;
        }
        #[cfg(feature = "recommend")]
        if let Some(r) = &self.recommend {
            r.start().await;
        }
        #[cfg(feature = "player")]
        if let Some(p) = &self.player {
            p.start().await;
        }
    }
}

/// Every service router, state already applied, paths WITHOUT the `/api` prefix.
pub fn routers(state: &AppState) -> Router {
    let mut r = Router::new();
    let s = &state.services;
    let _ = s;
    #[cfg(feature = "library")]
    if let Some(l) = &s.library {
        r = r.merge(l.router());
    }
    // `/loved-streams*` lives in bc-maint but is not part of its main router: mounted exactly once here.
    #[cfg(feature = "library")]
    if let Some(l) = &s.library {
        #[cfg(feature = "bandcamp")]
        let lookup: Option<std::sync::Arc<dyn bc_maint::BandcampLookup>> =
            s.bandcamp.as_ref().map(|b| b.lookup() as std::sync::Arc<dyn bc_maint::BandcampLookup>);
        #[cfg(not(feature = "bandcamp"))]
        let lookup: Option<std::sync::Arc<dyn bc_maint::BandcampLookup>> = None;
        r = r.merge(bc_maint::loved_router(l.ctx().clone(), lookup));
    }
    #[cfg(feature = "jobs")]
    if let Some(j) = &s.jobs {
        r = r.merge(j.router());
    }
    #[cfg(feature = "analysis")]
    if let Some(a) = &s.analysis {
        r = r.merge(a.router());
    }
    #[cfg(feature = "recommend")]
    if let Some(x) = &s.recommend {
        r = r.merge(x.router());
    }
    #[cfg(feature = "player")]
    if let Some(p) = &s.player {
        r = r.merge(p.router());
    }
    #[cfg(feature = "bandcamp")]
    if let Some(b) = &s.bandcamp {
        r = r.merge(b.router());
    }
    r
}

#[cfg(feature = "player")]
pub struct PlayerSink(pub Arc<bc_engine::PlayerService>);

#[cfg(feature = "player")]
impl crate::state::CommandSink for PlayerSink {
    fn handle_command(
        &self,
        command: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, bc_types::Problem>> + Send>> {
        let p = self.0.clone();
        Box::pin(async move {
            p.handle_command(command).await.map_err(|e| {
                let status = match &e {
                    bc_engine::PlayerError::BadCommand(_) => 400,
                    bc_engine::PlayerError::NotFound(_) => 404,
                    bc_engine::PlayerError::Conflict(_) => 409,
                    bc_engine::PlayerError::Unavailable(_) => 503,
                };
                bc_types::Problem::new(status, "Player command failed").detail(e.to_string())
            })
        })
    }
}

/// `JobHost` of the library: in-memory tasks (scan, move, art) plus a wake-up of the download
/// worker whenever the library queues downloads.
#[cfg(all(feature = "library", feature = "bandcamp"))]
struct WakeDownloads {
    inner: bc_libcore::LocalJobs,
    bc: bc_bandcamp::service::BandcampService,
}

#[cfg(all(feature = "library", feature = "bandcamp"))]
impl bc_libcore::JobHost for WakeDownloads {
    fn begin(&self, kind: &str, label: &str) -> bc_libcore::JobHandle {
        self.inner.begin(kind, label)
    }
    fn get(&self, id: &str) -> Option<bc_libcore::jobs::TaskInfo> {
        self.inner.get(id)
    }
    fn list(&self) -> Vec<bc_libcore::jobs::TaskInfo> {
        self.inner.list()
    }
    fn request_cancel(&self, id: &str) -> bool {
        self.inner.request_cancel(id)
    }
    fn notify_download_queue(&self) {
        self.bc.notify_downloads();
    }
}

/// Player port over the real Bandcamp service: fan-list walking (`QueueSource::Fan`) and Explore
/// release tracks (`QueueSource::Explore`). Plug in with `PlayerService` once it accepts a port
/// override (`Ports { bandcamp: Arc::new(BcPlayerPort::new(..)), .. }`); the engine's DB shim
/// answers "unavailable" for these two sources until then.
#[cfg(all(feature = "player", feature = "bandcamp"))]
pub struct BcPlayerPort {
    bc: bc_bandcamp::service::BandcampService,
    rt: tokio::runtime::Handle,
    fallback: Arc<dyn bc_engine::ports::BandcampPort>,
}

#[cfg(all(feature = "player", feature = "bandcamp"))]
impl BcPlayerPort {
    pub fn new(bc: bc_bandcamp::service::BandcampService, rt: tokio::runtime::Handle, fallback: Arc<dyn bc_engine::ports::BandcampPort>) -> Self {
        Self { bc, rt, fallback }
    }
}

#[cfg(all(feature = "player", feature = "bandcamp"))]
impl bc_engine::ports::BandcampPort for BcPlayerPort {
    fn resolve_stream(&self, item: &bc_types::player::QueueItem) -> bc_engine::ports::PortResult<String> {
        // Queue items carry the same-origin proxy URL (`/api/explore/stream?...`), which the
        // server resolves and refreshes itself.
        self.fallback.resolve_stream(item)
    }

    fn fan_next(
        &self,
        c: &bc_types::player::FanCursor,
        after: Option<i64>,
        limit: usize,
    ) -> bc_engine::ports::PortResult<bc_engine::ports::FanPage> {
        use bc_bandcamp::harvest::fans::{FanOrder, FanTab};
        use bc_types::player as p;
        let order = match c.order {
            p::FanOrder::Seq => FanOrder::Seq,
            p::FanOrder::Shuffle => FanOrder::Shuffle,
        };
        let tab = match c.tab {
            Some(p::FanTab::Wishlist) => FanTab::Wishlist,
            Some(p::FanTab::Collection) => FanTab::Collection,
            None => FanTab::All,
        };
        let out = self
            .rt
            .block_on(self.bc.fan_next(c.fan_id, after, order, c.seed as u32, &c.states, tab, limit))
            .map_err(|e| bc_engine::ports::PortError::Other(e.to_string()))?;
        Ok(bc_engine::ports::FanPage {
            items: out.items.into_iter().map(|i| bc_engine::ports::FanItem { item_id: i.item_id, url: i.url, release_id: i.release_id }).collect(),
            exhausted: out.exhausted,
        })
    }

    fn release_tracks(&self, url: &str) -> bc_engine::ports::PortResult<Vec<bc_types::player::QueueItem>> {
        let rel = self
            .rt
            .block_on(self.bc.release_tracks(url))
            .map_err(|e| bc_engine::ports::PortError::Other(e.to_string()))?;
        Ok(rel
            .tracks
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                let stream = t.stream_url.clone()?;
                // Synthetic negative ids: a streamed track keeps one identity across Explore and the Loved shelf.
                let id = -(t.bc_track_id.unwrap_or(i as i64 + 1).abs().max(1));
                Some(bc_types::player::QueueItem {
                    track_id: id,
                    title: t.title.clone(),
                    artist: t.artist.clone().or_else(|| Some(rel.artist_name.clone())),
                    album: Some(rel.title.clone()),
                    track_no: t.track_num.map(|n| n as i32),
                    duration_ms: t.duration_sec.map(|s| (s * 1000.0) as i64),
                    art_url: rel.art_url.clone(),
                    stream_url: Some(stream),
                    origin: bc_types::player::ItemOrigin::Bandcamp,
                    page_url: Some(rel.url.clone()),
                    track_url: t.url.clone().filter(|u| !u.is_empty()),
                    ..Default::default()
                })
            })
            .collect())
    }
}
