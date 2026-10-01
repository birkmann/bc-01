//! Thread-safe event bus (fixes the legacy asyncio-queue-from-threads bug).
//! `publish` may be called from any thread, sync or async.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bc_types::events::{Event, EventId};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::broadcast;

const REPLAY: usize = 2000;

pub struct EventBus {
    epoch: u64,
    seq: AtomicU64,
    tx: broadcast::Sender<Event>,
    ring: Mutex<VecDeque<Event>>,
}

/// What a reconnecting client gets.
pub enum Replay {
    Events(Vec<Event>),
    /// Unknown epoch or fell out of the ring: client must refetch.
    Resync,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let epoch = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let (tx, _) = broadcast::channel(1024);
        Self { epoch, seq: AtomicU64::new(0), tx, ring: Mutex::new(VecDeque::with_capacity(REPLAY)) }
    }

    pub fn publish<P: Serialize>(&self, topic: &str, payload: &P) {
        let payload = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
        let mut ring = self.ring.lock();
        // seq assigned under the ring lock so ring order == id order
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let ev = Event { id: EventId { epoch: self.epoch, seq }, topic: topic.to_string(), payload };
        if ring.len() == REPLAY {
            ring.pop_front();
        }
        ring.push_back(ev.clone());
        drop(ring);
        let _ = self.tx.send(ev);
    }

    /// Convenience for `invalidate` events.
    pub fn invalidate(&self, entity: &str, ids: Vec<i64>) {
        self.publish(
            bc_types::events::TOPIC_INVALIDATE,
            &bc_types::events::Invalidate { entity: entity.into(), ids },
        );
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn replay_since(&self, last: Option<EventId>) -> Replay {
        let Some(last) = last else { return Replay::Events(vec![]) };
        if last.epoch != self.epoch {
            return Replay::Resync;
        }
        let ring = self.ring.lock();
        match ring.front() {
            Some(first) if first.id.seq > last.seq + 1 => Replay::Resync,
            _ => Replay::Events(ring.iter().filter(|e| e.id.seq > last.seq).cloned().collect()),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_from_threads_and_replay() {
        let bus = std::sync::Arc::new(EventBus::new());
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let b = bus.clone();
                std::thread::spawn(move || (0..100).for_each(|i| b.publish("t", &i)))
            })
            .collect();
        hs.into_iter().for_each(|h| h.join().unwrap());
        let id = EventId { epoch: bus.epoch(), seq: 790 };
        match bus.replay_since(Some(id)) {
            Replay::Events(v) => assert_eq!(v.len(), 10),
            Replay::Resync => panic!(),
        }
        assert!(matches!(bus.replay_since(Some(EventId { epoch: 1, seq: 1 })), Replay::Resync));
    }
}
