//! Every mutation a DJ set has, behind one write path. The server returns the full recomputed
//! `DjSetDetail` from each mutation, so the local `detail` signal is *replaced* with the
//! response (single source of truth; the WS invalidation then refetches the same value).
//! Reorders get an optimistic local shuffle so drags land instantly.
//!
//! PATCH bodies are raw JSON with only the changed keys: the server applies every key present,
//! and an explicit `null` clears (venue, target, cues).
use std::sync::Arc;

use bc_types::library::PlaylistAddTracks;
use bc_types::sets::{AutomixRequest, DjSetDetail, MoveItem, PoolSource, SetItemOut};
use leptos::prelude::*;
use serde_json::{Value, json};

use crate::api;
use crate::ds::toast_err;

#[derive(Clone, Copy)]
pub struct SetMut {
    pub id: i64,
    pub detail: RwSignal<Option<Arc<DjSetDetail>>>,
    pub automix_busy: RwSignal<bool>,
    /// Items as they stood before the last automix, for one-level undo.
    pub undo: RwSignal<Option<Vec<SetItemOut>>>,
    pub undo_busy: RwSignal<bool>,
}

impl SetMut {
    pub fn new(id: i64, detail: RwSignal<Option<Arc<DjSetDetail>>>) -> Self {
        Self { id, detail, automix_busy: RwSignal::new(false), undo: RwSignal::new(None), undo_busy: RwSignal::new(false) }
    }

    fn write(&self, d: DjSetDetail) {
        // sets list + pool exclusion follow through the server's `set` invalidation
        let _ = self.detail.try_set(Some(Arc::new(d)));
    }

    fn run<F>(&self, forget_undo: bool, fut: F)
    where
        F: std::future::Future<Output = Result<DjSetDetail, api::ApiErr>> + 'static,
    {
        let this = *self;
        leptos::task::spawn_local(async move {
            match fut.await {
                Ok(d) => {
                    if forget_undo {
                        let _ = this.undo.try_set(None);
                    }
                    this.write(d);
                }
                Err(e) => {
                    toast_err(&e.message());
                    crate::data::invalidate_entity("set", &[this.id]);
                }
            }
        });
    }

    pub fn add_items(&self, ids: Vec<i64>, at_index: Option<i64>) {
        if ids.is_empty() {
            return;
        }
        let id = self.id;
        self.run(true, async move {
            api::post(&format!("/sets/{id}/items"), &PlaylistAddTracks { track_ids: ids, at_index }).await
        });
    }

    pub fn move_item(&self, item_id: i64, to_index: usize) {
        let id = self.id;
        if let Some(d) = self.detail.get_untracked() {
            let mut items = d.items.clone();
            if let Some(from) = items.iter().position(|i| i.id == item_id) {
                let it = items.remove(from);
                items.insert(to_index.min(items.len()), it);
                let mut nd = (*d).clone();
                nd.items = items;
                self.detail.set(Some(Arc::new(nd)));
            }
        }
        self.run(true, async move { api::post(&format!("/sets/{id}/items/{item_id}/move"), &MoveItem { to_index: to_index as i64 }).await });
    }

    pub fn patch_item(&self, item_id: i64, body: Value) {
        let id = self.id;
        self.run(true, async move { api::patch(&format!("/sets/{id}/items/{item_id}"), &body).await });
    }

    pub fn remove_item(&self, item_id: i64) {
        let id = self.id;
        self.run(true, async move { api::send("DELETE", &format!("/sets/{id}/items/{item_id}"), &Value::Null).await });
    }

    pub fn update(&self, body: Value) {
        let id = self.id;
        self.run(false, async move { api::patch(&format!("/sets/{id}"), &body).await });
    }

    pub fn set_pool_sources(&self, sources: Vec<PoolSource>) {
        self.update(json!({ "pool_sources": sources }));
    }

    pub fn automix(&self, req: AutomixRequest) {
        let id = self.id;
        let this = *self;
        this.undo.set(self.detail.get_untracked().map(|d| d.items.clone()));
        this.automix_busy.set(true);
        leptos::task::spawn_local(async move {
            match api::post::<_, DjSetDetail>(&format!("/sets/{id}/automix"), &req).await {
                Ok(d) => this.write(d),
                Err(e) => {
                    toast_err(&e.message());
                    let _ = this.undo.try_set(None);
                }
            }
            let _ = this.automix_busy.try_set(false);
        });
    }

    /// Reverse the last automix with the endpoints that exist: delete what it added, then move
    /// the survivors back into their old order. The last response is authoritative.
    pub fn undo_automix(&self) {
        let (id, this) = (self.id, *self);
        let Some(snapshot) = self.undo.get_untracked() else { return };
        let Some(current) = self.detail.get_untracked() else { return };
        this.undo_busy.set(true);
        leptos::task::spawn_local(async move {
            let res: Result<DjSetDetail, api::ApiErr> = async {
                let keep: std::collections::HashSet<i64> = snapshot.iter().map(|i| i.id).collect();
                let mut detail = (*current).clone();
                for it in current.items.iter().filter(|i| !keep.contains(&i.id)) {
                    detail = api::send("DELETE", &format!("/sets/{id}/items/{}", it.id), &Value::Null).await?;
                }
                let wanted: Vec<i64> = snapshot.iter().map(|i| i.id).filter(|iid| detail.items.iter().any(|i| i.id == *iid)).collect();
                for (to, item_id) in wanted.iter().enumerate() {
                    let Some(from) = detail.items.iter().position(|i| i.id == *item_id) else { continue };
                    if from == to {
                        continue;
                    }
                    detail = api::post(&format!("/sets/{id}/items/{item_id}/move"), &MoveItem { to_index: to as i64 }).await?;
                }
                Ok(detail)
            }
            .await;
            match res {
                Ok(d) => {
                    let _ = this.undo.try_set(None);
                    this.write(d);
                }
                Err(e) => toast_err(&e.message()),
            }
            let _ = this.undo_busy.try_set(false);
        });
    }
}
