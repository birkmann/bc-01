//! Realtime envelope carried over the WebSocket (`/api/ws`). Topics keep the
//! legacy SSE names (`job.item.progress`, `analysis.progress`, `library.changed`,
//! `downloads.disk`, `fans.walk`, ...). Payloads are JSON whose typed structs
//! live in each workstream's module, next to a `pub const TOPIC_*: &str`.

use serde::{Deserialize, Serialize};

/// `(boot_epoch, seq)`: a client whose last id is from another epoch or outside
/// the replay ring receives `stream.resync` and must refetch.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId {
    pub epoch: u64,
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub id: EventId,
    pub topic: String,
    pub payload: serde_json::Value,
}

/// Fine-grained cache invalidation so the UI never refetches the world.
pub const TOPIC_INVALIDATE: &str = "invalidate";
pub const TOPIC_RESYNC: &str = "stream.resync";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Invalidate {
    /// e.g. "track", "release", "artist", "label", "tag", "playlist", "set", "job", "fan"
    pub entity: String,
    /// Empty = every entity of this kind.
    pub ids: Vec<i64>,
}

/// Messages a client sends over the WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Resume after reconnect.
    Hello { last_event_id: Option<EventId> },
    /// Player remote-control command, interpreted by bc-engine (workstream 4).
    Player { command: serde_json::Value },
}
