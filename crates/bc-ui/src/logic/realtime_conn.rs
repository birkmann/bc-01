//! Pure state machine of the realtime WebSocket (the testable core of the legacy
//! `realtimeConnection.ts`): reopen back-off, resume with the last event id,
//! duplicate suppression, epoch-change resync and a silence watchdog.
//!
//! The DOM glue (`crate::data::ws`) feeds it inputs and executes the returned
//! [`Cmd`]s. The legacy multi-tab leader election existed to spare HTTP/1.1's
//! six-connection budget for SSE; a WebSocket does not compete for it, so each
//! tab connects for itself (decision logged in docs/decisions/ws5.md).
use bc_types::events::{Event, EventId};

pub const REOPEN_BASE_MS: u64 = 2_000;
pub const REOPEN_MAX_MS: u64 = 30_000;
/// The server heartbeats every 15 s; treat this much silence as a dead socket.
pub const STALE_MS: u64 = 45_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    /// Open a socket now.
    Open,
    /// Open a socket after the delay.
    ReopenIn(u64),
    /// Send `ClientMsg::Hello` with this resume id.
    SendHello(Option<EventId>),
    /// Hand an event to subscribers.
    Deliver(Event),
    /// Subscribers must refetch (unknown epoch, gap or lag).
    Resync,
    /// Connected flag changed.
    Status(bool),
    /// Close the socket (watchdog / stop).
    CloseSocket,
}

#[derive(Debug, Default)]
pub struct Conn {
    running: bool,
    socket_open: bool,
    connected: bool,
    failures: u32,
    last_id: Option<EventId>,
    last_rx_ms: u64,
}

impl Conn {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn connected(&self) -> bool {
        self.connected
    }
    pub fn last_id(&self) -> Option<EventId> {
        self.last_id
    }
    pub fn failures(&self) -> u32 {
        self.failures
    }

    pub fn start(&mut self, _now: u64) -> Vec<Cmd> {
        if self.running {
            return vec![];
        }
        self.running = true;
        vec![Cmd::Open]
    }

    pub fn stop(&mut self) -> Vec<Cmd> {
        let mut out = vec![];
        if self.socket_open {
            out.push(Cmd::CloseSocket);
        }
        self.running = false;
        self.socket_open = false;
        out.extend(self.set_connected(false));
        out
    }

    fn set_connected(&mut self, v: bool) -> Vec<Cmd> {
        if self.connected == v {
            return vec![];
        }
        self.connected = v;
        vec![Cmd::Status(v)]
    }

    pub fn on_open(&mut self, now: u64) -> Vec<Cmd> {
        self.socket_open = true;
        self.failures = 0;
        self.last_rx_ms = now;
        vec![Cmd::SendHello(self.last_id)]
    }

    /// A text frame arrived.
    pub fn on_text(&mut self, now: u64, text: &str) -> Vec<Cmd> {
        self.last_rx_ms = now;
        let Ok(ev) = serde_json::from_str::<Event>(text) else { return vec![] };
        let mut out = self.set_connected(true);
        match ev.topic.as_str() {
            "hb" | "pong" => {}
            "stream.hello" => {
                if let Some(prev) = self.last_id {
                    if prev.epoch != ev.id.epoch {
                        // Server restarted: nothing we hold can be trusted.
                        self.last_id = None;
                        out.push(Cmd::Resync);
                    }
                }
            }
            bc_types::events::TOPIC_RESYNC => out.push(Cmd::Resync),
            _ => {
                if ev.id.seq > 0 {
                    if let Some(prev) = self.last_id {
                        if prev.epoch == ev.id.epoch && ev.id.seq <= prev.seq {
                            return out; // replayed duplicate
                        }
                    }
                    self.last_id = Some(ev.id);
                }
                out.push(Cmd::Deliver(ev));
            }
        }
        out
    }

    /// The socket closed or errored.
    pub fn on_close(&mut self, _now: u64) -> Vec<Cmd> {
        self.socket_open = false;
        let mut out = self.set_connected(false);
        if self.running {
            let delay = (REOPEN_BASE_MS << self.failures.min(10)).min(REOPEN_MAX_MS);
            self.failures += 1;
            out.push(Cmd::ReopenIn(delay));
        }
        out
    }

    /// Periodic watchdog tick.
    pub fn on_tick(&mut self, now: u64) -> Vec<Cmd> {
        if self.running && self.socket_open && now.saturating_sub(self.last_rx_ms) > STALE_MS {
            self.socket_open = false;
            let mut out = vec![Cmd::CloseSocket];
            out.extend(self.set_connected(false));
            let delay = (REOPEN_BASE_MS << self.failures.min(10)).min(REOPEN_MAX_MS);
            self.failures += 1;
            out.push(Cmd::ReopenIn(delay));
            return out;
        }
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(epoch: u64, seq: u64, topic: &str) -> String {
        format!(r#"{{"id":{{"epoch":{epoch},"seq":{seq}}},"topic":"{topic}","payload":{{}}}}"#)
    }
    fn delivered(cmds: &[Cmd]) -> Vec<u64> {
        cmds.iter().filter_map(|c| if let Cmd::Deliver(e) = c { Some(e.id.seq) } else { None }).collect()
    }

    fn live() -> Conn {
        let mut c = Conn::new();
        c.start(0);
        c.on_open(0);
        c.on_text(1, &ev(1, 0, "stream.hello"));
        c
    }

    #[test]
    fn opens_once_and_says_hello_with_nothing_to_resume() {
        let mut c = Conn::new();
        assert_eq!(c.start(0), vec![Cmd::Open]);
        assert!(c.start(0).is_empty());
        assert_eq!(c.on_open(5), vec![Cmd::SendHello(None)]);
    }

    #[test]
    fn becomes_connected_on_first_frame_and_delivers_events() {
        let mut c = live();
        assert!(c.connected());
        let out = c.on_text(2, &ev(1, 1, "job.progress"));
        assert_eq!(delivered(&out), vec![1]);
        assert_eq!(c.last_id(), Some(EventId { epoch: 1, seq: 1 }));
    }

    #[test]
    fn heartbeats_are_not_delivered() {
        let mut c = live();
        assert!(delivered(&c.on_text(2, &ev(1, 0, "hb"))).is_empty());
    }

    #[test]
    fn reopens_with_doubling_backoff_capped_at_thirty_seconds() {
        let mut c = live();
        let mut delays = vec![];
        for _ in 0..6 {
            for cmd in c.on_close(10) {
                if let Cmd::ReopenIn(d) = cmd {
                    delays.push(d);
                }
            }
        }
        assert_eq!(delays, [2_000, 4_000, 8_000, 16_000, 30_000, 30_000]);
    }

    #[test]
    fn a_successful_open_resets_backoff() {
        let mut c = live();
        c.on_close(0);
        c.on_close(0);
        assert_eq!(c.failures(), 2);
        c.on_open(1);
        assert_eq!(c.failures(), 0);
        assert!(c.on_close(2).contains(&Cmd::ReopenIn(2_000)));
    }

    #[test]
    fn resumes_from_last_event_id_and_drops_replayed_duplicates() {
        let mut c = live();
        c.on_text(2, &ev(1, 5, "x"));
        c.on_close(3);
        assert_eq!(c.on_open(4), vec![Cmd::SendHello(Some(EventId { epoch: 1, seq: 5 }))]);
        // server replays 5 and 6: only 6 is new.
        assert!(delivered(&c.on_text(5, &ev(1, 5, "x"))).is_empty());
        assert_eq!(delivered(&c.on_text(5, &ev(1, 6, "x"))), vec![6]);
    }

    #[test]
    fn epoch_change_forces_resync_and_forgets_the_old_ids() {
        let mut c = live();
        c.on_text(2, &ev(1, 9, "x"));
        c.on_close(3);
        c.on_open(4);
        let out = c.on_text(5, &ev(2, 0, "stream.hello"));
        assert!(out.contains(&Cmd::Resync));
        assert_eq!(c.last_id(), None);
        assert_eq!(delivered(&c.on_text(6, &ev(2, 1, "x"))), vec![1]);
    }

    #[test]
    fn server_resync_is_surfaced() {
        let mut c = live();
        assert!(c.on_text(2, &ev(1, 0, "stream.resync")).contains(&Cmd::Resync));
    }

    #[test]
    fn silence_watchdog_closes_and_reopens() {
        let mut c = live();
        assert!(c.on_tick(STALE_MS - 1).is_empty());
        let out = c.on_tick(STALE_MS + 10);
        assert!(out.contains(&Cmd::CloseSocket));
        assert!(out.contains(&Cmd::Status(false)));
        assert!(out.contains(&Cmd::ReopenIn(2_000)));
        assert!(!c.connected());
    }

    #[test]
    fn stop_closes_and_does_not_reopen() {
        let mut c = live();
        let out = c.stop();
        assert!(out.contains(&Cmd::CloseSocket));
        assert!(c.on_close(1).is_empty());
    }

    #[test]
    fn garbage_frames_are_ignored() {
        let mut c = live();
        assert!(c.on_text(2, "not json").is_empty());
    }
}
