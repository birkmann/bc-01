//! Data layer: keyed resource cache with stale-while-revalidate and targeted
//! invalidation from WS events, plus the realtime client.
pub mod cache;
pub mod jobs;
pub mod query;
pub mod ws;

pub use cache::{invalidate_all, invalidate_entity, invalidate_prefix, patch};
pub use jobs::{JobsStore, provide_jobs, use_jobs};
pub use query::{Query, QuerySpec, use_query};
pub use ws::{send_client_msg, use_invalidation, use_topic, ws_connected};
