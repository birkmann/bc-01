//! `Query<T>` is only `Copy` for `T: Copy`; closures in views need a Copy handle.
use std::sync::Arc;

use leptos::prelude::*;
use serde::de::DeserializeOwned;

use crate::api::ApiErr;
use crate::data::{QuerySpec, use_query};

pub struct Qh<T: Send + Sync + 'static> {
    pub data: RwSignal<Option<Arc<T>>>,
    pub loading: RwSignal<bool>,
    pub error: RwSignal<Option<ApiErr>>,
    refetch: Callback<()>,
}

impl<T: Send + Sync + 'static> Clone for Qh<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Send + Sync + 'static> Copy for Qh<T> {}

impl<T: Send + Sync + 'static> Qh<T> {
    pub fn refetch(&self) {
        self.refetch.run(());
    }
}

pub fn use_qh<T>(spec: impl Fn() -> Option<QuerySpec> + Send + Sync + 'static) -> Qh<T>
where
    T: DeserializeOwned + Send + Sync + 'static,
{
    let q = use_query::<T>(spec);
    let (data, loading, error) = (q.data, q.loading, q.error);
    Qh { data, loading, error, refetch: Callback::new(move |_| q.refetch()) }
}
