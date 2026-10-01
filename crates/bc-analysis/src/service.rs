//! `AnalysisService`: runner + HTTP router (see docs/api/ws3.md).

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use bc_core::{Config, EventBus};
use bc_db::Db;
use bc_jobs::{JobStore, JobsService};
use bc_waveform::store::WaveformStore;
use parking_lot::Mutex;

use crate::energy::EnergyCalibration;
use crate::key::KeyProfiles;
use crate::pipeline::AnalyzeOptions;
use crate::runner::Runner;

pub const SETTING_KEY_PROFILES: &str = "analysis.key_profiles";
pub const SETTING_ENERGY_CAL: &str = "analysis.energy_calibration";
/// Default size cap of the waveform detail cache (PLAN 6.1).
pub const DEFAULT_WAVEFORM_CAP_BYTES: u64 = 8 * 1024 * 1024 * 1024;

pub struct Inner {
    pub db: Db,
    pub bus: Arc<EventBus>,
    pub cfg: Arc<Config>,
    pub jobs: JobStore,
    pub runner: Arc<Runner>,
    pub waveforms: Arc<WaveformStore>,
    /// Single-flight guard for on-demand waveform builds.
    pub building: Mutex<HashMap<i64, Arc<Mutex<()>>>>,
}

#[derive(Clone)]
pub struct AnalysisService {
    pub inner: Arc<Inner>,
}

/// Options from the settings table (fitted key profiles / energy calibration), else defaults.
pub fn load_options(db: &Db) -> AnalyzeOptions {
    let mut o = AnalyzeOptions::default();
    if let Ok(Some(s)) = db.read(|c| bc_db::settings::get(c, SETTING_KEY_PROFILES)) {
        if let Ok(p) = serde_json::from_str::<KeyProfiles>(&s) {
            o.profiles = Arc::new(p);
        }
    }
    if let Ok(Some(s)) = db.read(|c| bc_db::settings::get(c, SETTING_ENERGY_CAL)) {
        if let Ok(p) = serde_json::from_str::<EnergyCalibration>(&s) {
            o.energy = Arc::new(p);
        }
    }
    o
}

impl AnalysisService {
    pub fn new(db: Db, bus: Arc<EventBus>, cfg: Arc<Config>, jobs: &JobsService) -> Self {
        if let Err(e) = crate::schema::ensure(&db) {
            tracing::error!("analysis schema: {e}");
        }
        let waveforms = Arc::new(WaveformStore::new(cfg.waveform_dir(), DEFAULT_WAVEFORM_CAP_BYTES));
        let runner = Runner::new(db.clone(), jobs.store().clone(), bus.clone(), &cfg, waveforms.clone(), load_options(&db));
        Self {
            inner: Arc::new(Inner { db, bus, cfg, jobs: jobs.store().clone(), runner, waveforms, building: Mutex::new(HashMap::new()) }),
        }
    }

    /// Spawn the background runner.
    pub async fn start(&self) {
        let runner = self.inner.runner.clone();
        tokio::spawn(runner.run());
    }

    pub fn stop(&self) {
        self.inner.runner.stop();
    }

    /// Waveform-based mix-point planner for `RecommendService::with_cue_source`.
    pub fn cue_source(&self) -> crate::mixplan::WaveformCues {
        crate::mixplan::WaveformCues { db: self.inner.db.clone(), waveforms: self.inner.waveforms.clone() }
    }

    /// Router with state applied; paths WITHOUT the `/api` prefix.
    pub fn router(&self) -> Router {
        crate::routes::router(self.inner.clone())
    }
}
