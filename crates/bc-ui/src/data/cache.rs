//! Runtime of the keyed resource cache. Bookkeeping rules live in
//! `logic::cache_core::KeyIndex` (tested natively); this module adds the typed
//! values, version signals and in-flight de-duplication.
use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use leptos::prelude::*;

use crate::logic::cache_core::KeyIndex;

#[derive(Default)]
struct Runtime {
    index: KeyIndex,
    values: HashMap<String, Arc<dyn Any + Send + Sync>>,
    vers: HashMap<String, ArcRwSignal<u64>>,
    inflight: HashSet<String>,
    observers: HashMap<String, usize>,
    refetch: HashMap<String, Rc<dyn Fn()>>,
}

thread_local! {
    static RT: RefCell<Runtime> = RefCell::new(Runtime::default());
}

/// Entries kept when nobody observes them (memory bound for long sessions).
const KEEP: usize = 400;

pub fn ver_signal(key: &str) -> ArcRwSignal<u64> {
    RT.with(|rt| rt.borrow_mut().vers.entry(key.to_string()).or_insert_with(|| ArcRwSignal::new(0)).clone())
}

pub fn get<T: Send + Sync + 'static>(key: &str) -> Option<Arc<T>> {
    RT.with(|rt| rt.borrow().values.get(key).cloned()).and_then(|v| v.downcast::<T>().ok())
}

pub fn is_stale(key: &str) -> bool {
    RT.with(|rt| {
        let rt = rt.borrow();
        !rt.values.contains_key(key) || rt.index.is_stale(key)
    })
}

pub fn is_inflight(key: &str) -> bool {
    RT.with(|rt| rt.borrow().inflight.contains(key))
}

pub fn begin(key: &str) -> bool {
    RT.with(|rt| rt.borrow_mut().inflight.insert(key.to_string()))
}

pub fn finish(key: &str) {
    RT.with(|rt| {
        rt.borrow_mut().inflight.remove(key);
    });
}

pub fn put<T: Send + Sync + 'static>(key: &str, tags: Vec<String>, value: Arc<T>) {
    let ver = RT.with(|rt| {
        let mut rt = rt.borrow_mut();
        rt.values.insert(key.to_string(), value);
        rt.index.insert(key, tags);
        rt.vers.entry(key.to_string()).or_insert_with(|| ArcRwSignal::new(0)).clone()
    });
    ver.update(|v| *v += 1);
    evict();
}

/// Update a cached value in place (job progress, optimistic edits). Observers re-render.
pub fn patch<T: Clone + Send + Sync + 'static>(key: &str, f: impl FnOnce(&mut T)) {
    let Some(cur) = get::<T>(key) else { return };
    let mut next = (*cur).clone();
    f(&mut next);
    let ver = RT.with(|rt| {
        let mut rt = rt.borrow_mut();
        rt.values.insert(key.to_string(), Arc::new(next));
        rt.vers.get(key).cloned()
    });
    if let Some(v) = ver {
        v.update(|v| *v += 1);
    }
}

pub fn observe(key: &str, refetch: Rc<dyn Fn()>) {
    RT.with(|rt| {
        let mut rt = rt.borrow_mut();
        *rt.observers.entry(key.to_string()).or_insert(0) += 1;
        rt.refetch.insert(key.to_string(), refetch);
    });
}

pub fn unobserve(key: &str) {
    RT.with(|rt| {
        let mut rt = rt.borrow_mut();
        if let Some(n) = rt.observers.get_mut(key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                rt.observers.remove(key);
                rt.refetch.remove(key);
            }
        }
    });
}

fn evict() {
    RT.with(|rt| {
        let mut rt = rt.borrow_mut();
        if rt.index.len() <= KEEP {
            return;
        }
        let obs: HashSet<String> = rt.observers.keys().cloned().collect();
        let gone = rt.index.evict_unobserved(|k| obs.contains(k), KEEP);
        for k in gone {
            rt.values.remove(&k);
            rt.vers.remove(&k);
        }
    });
}

/// Refetch the observed keys among `hit` now; unobserved ones stay stale and
/// refetch when next used.
fn refetch_observed(hit: Vec<String>) {
    let fns: Vec<Rc<dyn Fn()>> = RT.with(|rt| {
        let rt = rt.borrow();
        hit.iter().filter_map(|k| rt.refetch.get(k).cloned()).collect()
    });
    for f in fns {
        f();
    }
}

/// Targeted: only entries tagged with the entity (and ids) are touched.
pub fn invalidate_entity(entity: &str, ids: &[i64]) {
    let hit = RT.with(|rt| rt.borrow_mut().index.invalidate_entity(entity, ids));
    refetch_observed(hit);
}

/// Everything except the Home snapshot shelves (see cache_core).
pub fn invalidate_all() {
    let hit = RT.with(|rt| rt.borrow_mut().index.invalidate_all());
    refetch_observed(hit);
}

pub fn invalidate_prefix(prefix: &str) {
    let hit = RT.with(|rt| rt.borrow_mut().index.invalidate_prefix(prefix));
    refetch_observed(hit);
}
