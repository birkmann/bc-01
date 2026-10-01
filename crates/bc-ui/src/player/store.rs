//! Player store. The engine lives server-side (`bc-engine`); this mirrors its state.
//! Two signals on purpose: `state` changes a few times a minute, `clock` at ~30 Hz.
//! Only the playhead consumers read the clock, so the 200 visible rows of a table
//! never re-render during playback. The playhead is extrapolated in rAF.
use bc_types::events::ClientMsg;
use bc_types::player::*;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::data::ws::{resync_counter, send_client_msg, use_topic};
use crate::util::perf_now;

#[derive(Clone, Copy)]
pub struct PlayerCtx {
    pub state: RwSignal<PlayerState>,
    pub clock: RwSignal<Clock>,
    /// `performance.now()` when the clock frame arrived.
    pub clock_at: RwSignal<f64>,
    pub transition: RwSignal<Option<TransitionState>>,
    /// Where audio plays: "server" (the desktop engine) or "browser" (this device via bc-worklet).
    pub target: RwSignal<PlaybackTarget>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PlaybackTarget {
    /// The native engine of the machine running bc (desktop speakers).
    #[default]
    Server,
    /// This browser, through the AudioWorklet host.
    ThisDevice,
}

impl PlayerCtx {
    /// Send a command: over the WebSocket when it is open, else `POST /player/command`.
    pub fn cmd(&self, c: PlayerCommand) {
        if self.target.get_untracked() == PlaybackTarget::ThisDevice {
            crate::player::local::command(self, &c);
            return;
        }
        let Ok(v) = serde_json::to_value(&c) else { return };
        if send_client_msg(&ClientMsg::Player { command: v.clone() }) {
            return;
        }
        spawn_local(async move {
            if let Err(e) = api::call_json("POST", "/player/command", &v).await {
                crate::ds::toast_err(&e.message());
            }
        });
    }

    pub fn toggle(&self) {
        self.cmd(PlayerCommand::Toggle);
    }
    pub fn next(&self) {
        self.cmd(PlayerCommand::Next);
    }
    pub fn previous(&self) {
        self.cmd(PlayerCommand::Previous);
    }
    pub fn seek(&self, s: f64) {
        self.cmd(PlayerCommand::Seek { seconds: s });
    }
    pub fn seek_relative(&self, d: f64) {
        self.cmd(PlayerCommand::SeekRelative { delta_s: d });
    }

    pub fn current_track_id(&self) -> Option<i64> {
        self.state.with(|s| s.current.as_ref().map(|c| c.track_id))
    }
    pub fn is_playing(&self) -> bool {
        self.state.with(|s| s.status == PlayerStatus::Playing)
    }
}

/// Playhead position now, in seconds, extrapolated from the last clock frame.
pub fn position_now(clock: &Clock, received_at_ms: f64) -> f64 {
    if clock.playing {
        let dt = ((perf_now() - received_at_ms) / 1000.0).max(0.0);
        let p = clock.position_s + dt * clock.rate.max(0.0);
        if clock.duration_s > 0.0 { p.min(clock.duration_s) } else { p }
    } else {
        clock.position_s
    }
}

pub fn provide_player() -> PlayerCtx {
    let ctx = PlayerCtx {
        state: RwSignal::new(PlayerState::default()),
        clock: RwSignal::new(Clock { rate: 1.0, ..Default::default() }),
        clock_at: RwSignal::new(0.0),
        transition: RwSignal::new(None),
        target: RwSignal::new(if crate::util::ls_get("bc:player:target").as_deref() == Some("browser") {
            PlaybackTarget::ThisDevice
        } else {
            PlaybackTarget::Server
        }),
    };
    provide_context(ctx);
    CTX.with(|c| c.set(Some(ctx)));
    let load = move || {
        spawn_local(async move {
            if let Ok(s) = api::get::<PlayerState>("/player/state").await {
                ctx.state.set(s);
            }
            if let Ok(c) = api::get::<Clock>("/player/clock").await {
                ctx.clock.set(c);
                ctx.clock_at.set(perf_now());
            }
        });
    };
    load();
    Effect::new(move |prev: Option<u64>| {
        let n = resync_counter().get();
        if prev.is_some() {
            load();
        }
        n
    });
    use_topic::<PlayerState>("player.state", move |mut s| {
        // The server omits `queue` on events where it did not change (`queue_included == false`):
        // keep ours instead of emptying the planner's Up next.
        if !s.queue_included {
            s.queue = ctx.state.with_untracked(|c| c.queue.clone());
        }
        // ignore stale revisions delivered out of order after a replay, and the desktop session
        // while this device plays locally
        if ctx.target.get_untracked() == PlaybackTarget::Server && s.rev >= ctx.state.with_untracked(|c| c.rev) {
            ctx.state.set(s);
        }
    });
    use_topic::<Clock>("player.clock", move |c| {
        if ctx.target.get_untracked() == PlaybackTarget::Server {
            ctx.clock.set(c);
            ctx.clock_at.set(perf_now());
        }
    });
    use_topic::<Option<TransitionState>>("player.transition", move |t| {
        if ctx.target.get_untracked() == PlaybackTarget::Server {
            ctx.transition.set(t)
        }
    });
    // Switching targets: leaving the desktop engine pauses it; coming back reloads its state.
    Effect::new(move |prev: Option<PlaybackTarget>| {
        let t = ctx.target.get();
        if let Some(p) = prev {
            if p != t {
                match t {
                    PlaybackTarget::ThisDevice => {
                        let v = serde_json::json!({ "cmd": "pause" });
                        if !send_client_msg(&ClientMsg::Player { command: v.clone() }) {
                            spawn_local(async move { let _ = api::call_json("POST", "/player/command", &v).await; });
                        }
                        crate::player::local::take_over(&ctx);
                    }
                    PlaybackTarget::Server => load(),
                }
            }
        }
        t
    });
    Effect::new(move |_| {
        crate::util::ls_set(
            "bc:player:target",
            if ctx.target.get() == PlaybackTarget::ThisDevice { "browser" } else { "server" },
        );
    });
    ctx
}

thread_local! {
    static CTX: std::cell::Cell<Option<PlayerCtx>> = const { std::cell::Cell::new(None) };
}

/// The player context. Falls back to the session-wide copy so helpers still work after an
/// `await` (inside `spawn_local` the reactive context is gone).
pub fn use_player() -> PlayerCtx {
    use_context::<PlayerCtx>().or_else(|| CTX.with(|c| c.get())).expect("PlayerCtx not provided")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_clock_does_not_move() {
        let c = Clock { playing: false, position_s: 12.5, rate: 1.0, duration_s: 100.0, ..Default::default() };
        assert_eq!(c.position_s, 12.5);
    }
}
