//! Background jobs of the label shelf as live state: the label sweep ("find & download new"),
//! the label resolver ("find missing labels") and the stray-release merge. Each is one status
//! value fetched once and then patched in place from its WebSocket topic; polling is only a
//! fallback while the socket is down.
use bc_types::bandcamp::{LabelResolveStatus, SweepStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::logic::{self as lg, Tone};
use crate::api;
use crate::data::{use_topic, ws_connected};

/// A background job's status plus the "report" flag that keeps the final line on screen until dismissed.
pub struct JobState<T: Send + Sync + 'static> {
    pub status: RwSignal<Option<T>>,
    pub report: RwSignal<bool>,
    pub error: RwSignal<Option<String>>,
    pub starting: RwSignal<bool>,
}

impl<T: Send + Sync + 'static> Clone for JobState<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Send + Sync + 'static> Copy for JobState<T> {}

fn job_state<T>(topic: &'static str, url: &'static str, running: fn(&T) -> bool, on_finished: Callback<()>) -> JobState<T>
where
    T: serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
{
    let st = JobState { status: RwSignal::new(None::<T>), report: RwSignal::new(false), error: RwSignal::new(None), starting: RwSignal::new(false) };
    spawn_local(async move {
        if let Ok(s) = api::get::<T>(url).await {
            let _ = st.status.try_set(Some(s));
        }
    });
    use_topic::<T>(topic, move |s| {
        st.status.set(Some(s));
    });
    // The fallback poll only runs while a job is active and the socket is down.
    let connected = ws_connected();
    Effect::new(move |_| {
        let r = st.status.with(|s| s.as_ref().map(running).unwrap_or(false));
        if r && !connected.get() {
            let id = StoredValue::new(true);
            spawn_local(async move {
                while id.try_get_value().unwrap_or(false) && st.status.try_with(|s| s.as_ref().map(running).unwrap_or(false)).unwrap_or(false) {
                    gloo_timers::future::TimeoutFuture::new(2000).await;
                    if let Ok(s) = api::get::<T>(url).await {
                        let _ = st.status.try_set(Some(s));
                    }
                }
            });
            on_cleanup(move || id.set_value(false));
        }
    });
    Effect::new(move |prev: Option<bool>| {
        let r = st.status.with(|s| s.as_ref().map(running).unwrap_or(false));
        if prev == Some(true) && !r {
            st.report.set(true);
            on_finished.run(());
        }
        r
    });
    st
}

pub fn use_label_sweep(on_finished: Callback<()>) -> JobState<SweepStatus> {
    job_state::<SweepStatus>(bc_types::bandcamp::TOPIC_LABELS_SWEEP, "/harvest/labels/sweep", |s| s.running, on_finished)
}

pub fn use_label_resolve(on_finished: Callback<()>) -> JobState<LabelResolveStatus> {
    job_state::<LabelResolveStatus>(bc_types::bandcamp::TOPIC_HARVEST_LABELS, "/harvest/labels", |s| s.running, on_finished)
}

impl JobState<SweepStatus> {
    pub fn running(&self) -> Signal<bool> {
        let st = self.status;
        Signal::derive(move || st.with(|s| s.as_ref().map(|s| s.running).unwrap_or(false)))
    }

    /// Start a sweep over the given labels (empty = the whole shelf).
    pub fn start(&self, ids: Vec<i64>) {
        let me = *self;
        me.starting.set(true);
        me.error.set(None);
        spawn_local(async move {
            let r = api::post::<_, SweepStatus>("/harvest/labels/sweep", &serde_json::json!({ "label_ids": ids })).await;
            let _ = me.starting.try_set(false);
            match r {
                Ok(s) => {
                    let _ = me.status.try_set(Some(s));
                    let _ = me.report.try_set(false);
                }
                Err(e) => {
                    let _ = me.error.try_set(Some(e.message()));
                }
            }
        });
    }

    pub fn stop(&self) {
        let me = *self;
        spawn_local(async move {
            if let Ok(s) = api::send::<_, SweepStatus>("DELETE", "/harvest/labels/sweep", &serde_json::json!({})).await {
                let _ = me.status.try_set(Some(s));
            }
        });
    }

    pub fn line(&self) -> Signal<Option<(Tone, String)>> {
        let (st, report, err) = (self.status, self.report, self.error);
        Signal::derive(move || {
            if let Some(e) = err.get() {
                return Some((Tone::Err, e));
            }
            st.with(|s| {
                let s = s.as_ref()?;
                if s.running || (report.get() && (s.phase == "done" || s.phase == "failed")) { lg::sweep_text(s) } else { None }
            })
        })
    }

    pub fn dismiss(&self) {
        self.report.set(false);
        self.error.set(None);
    }
}

impl JobState<LabelResolveStatus> {
    pub fn running(&self) -> Signal<bool> {
        let st = self.status;
        Signal::derive(move || st.with(|s| s.as_ref().map(|s| s.running).unwrap_or(false)))
    }

    pub fn start(&self) {
        let me = *self;
        me.starting.set(true);
        me.error.set(None);
        spawn_local(async move {
            let r = api::post::<_, LabelResolveStatus>("/harvest/labels/resolve", &serde_json::json!({})).await;
            let _ = me.starting.try_set(false);
            match r {
                Ok(s) => {
                    let _ = me.status.try_set(Some(s));
                    let _ = me.report.try_set(false);
                }
                Err(e) => {
                    let _ = me.error.try_set(Some(e.message()));
                }
            }
        });
    }

    pub fn line(&self) -> Signal<Option<(Tone, String)>> {
        let (st, report, err) = (self.status, self.report, self.error);
        Signal::derive(move || {
            if let Some(e) = err.get() {
                return Some((Tone::Err, e));
            }
            st.with(|s| {
                let s = s.as_ref()?;
                if s.running || (report.get() && (s.phase == "done" || s.phase == "failed")) { lg::resolve_text(s) } else { None }
            })
        })
    }

    pub fn dismiss(&self) {
        self.report.set(false);
        self.error.set(None);
    }
}

pub fn use_stray_merge(on_finished: Callback<()>) -> JobState<bc_types::library::StraySweepStatus> {
    job_state::<bc_types::library::StraySweepStatus>(bc_types::library::TOPIC_LIBRARY_STRAYS, "/releases/strays/merge", |s| s.running, on_finished)
}
