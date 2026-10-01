//! `use_query`: a reactive, cached GET. Keeps the previous result while the key
//! changes or a refetch runs (stale-while-revalidate), de-duplicates in-flight
//! requests and refetches in place when a WS invalidation names one of its tags.
use std::rc::Rc;
use std::sync::Arc;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::de::DeserializeOwned;

use super::cache;
use crate::api::{self, ApiErr};

#[derive(Debug, Clone, PartialEq)]
pub struct QuerySpec {
    /// Cache key (usually the URL). Keys starting with `home` are sparing.
    pub key: String,
    pub url: String,
    /// Invalidation tags: `"track"` (any track change), `"track:12"` (one id).
    pub tags: Vec<String>,
}

impl QuerySpec {
    pub fn new(url: impl Into<String>, tags: &[&str]) -> Self {
        let url = url.into();
        Self { key: url.clone(), url, tags: tags.iter().map(|t| t.to_string()).collect() }
    }
    pub fn keyed(key: impl Into<String>, url: impl Into<String>, tags: &[&str]) -> Self {
        Self { key: key.into(), url: url.into(), tags: tags.iter().map(|t| t.to_string()).collect() }
    }
}

pub struct Query<T: Send + Sync + 'static> {
    pub data: RwSignal<Option<Arc<T>>>,
    pub loading: RwSignal<bool>,
    pub error: RwSignal<Option<ApiErr>>,
    refetch_count: RwSignal<u64>,
}

// Manual impls: the derive would require `T: Copy`, but a Query only holds signal handles.
impl<T: Send + Sync + 'static> Clone for Query<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Send + Sync + 'static> Copy for Query<T> {}

impl<T: Send + Sync + 'static> Query<T> {
    pub fn refetch(&self) {
        self.refetch_count.update(|c| *c += 1);
    }
    pub fn get(&self) -> Option<Arc<T>> {
        self.data.get()
    }
    /// `true` only for the very first load (no data yet).
    pub fn first_load(&self) -> bool {
        self.data.with(|d| d.is_none()) && self.loading.get()
    }
}

pub fn use_query<T>(spec: impl Fn() -> Option<QuerySpec> + Send + Sync + 'static) -> Query<T>
where
    T: DeserializeOwned + Send + Sync + 'static,
{
    let data: RwSignal<Option<Arc<T>>> = RwSignal::new(None);
    let loading = RwSignal::new(false);
    let error = RwSignal::new(None::<ApiErr>);
    let refetch_count = RwSignal::new(0u64);
    let current_key = StoredValue::new(None::<String>);

    on_cleanup(move || {
        if let Some(k) = current_key.get_value() {
            cache::unobserve(&k);
        }
    });

    Effect::new(move |_| {
        let s = spec();
        refetch_count.track();
        let Some(s) = s else {
            return;
        };
        // observe lifecycle
        let prev = current_key.get_value();
        if prev.as_deref() != Some(s.key.as_str()) {
            if let Some(p) = prev {
                cache::unobserve(&p);
            }
            let (url, key, tags) = (s.url.clone(), s.key.clone(), s.tags.clone());
            let rf: Rc<dyn Fn()> = Rc::new(move || fetch::<T>(&key, &url, &tags, loading, error));
            cache::observe(&s.key, rf);
            current_key.set_value(Some(s.key.clone()));
        }
        cache::ver_signal(&s.key).get(); // track new values
        if let Some(v) = cache::get::<T>(&s.key) {
            data.set(Some(v));
        }
        if cache::is_stale(&s.key) && !cache::is_inflight(&s.key) {
            fetch::<T>(&s.key, &s.url, &s.tags, loading, error);
        }
    });

    Query { data, loading, error, refetch_count }
}

fn fetch<T>(key: &str, url: &str, tags: &[String], loading: RwSignal<bool>, error: RwSignal<Option<ApiErr>>)
where
    T: DeserializeOwned + Send + Sync + 'static,
{
    if !cache::begin(key) {
        return;
    }
    loading.set(true);
    let (key, url, tags) = (key.to_string(), url.to_string(), tags.to_vec());
    spawn_local(async move {
        match api::get::<T>(&url).await {
            Ok(v) => {
                error.set(None);
                cache::put(&key, tags, Arc::new(v));
            }
            Err(e) => error.set(Some(e)),
        }
        cache::finish(&key);
        let _ = loading.try_set(false);
    });
}
