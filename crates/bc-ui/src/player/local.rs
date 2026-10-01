//! "This device" playback target: the bc-worklet host (bc-dsp in an AudioWorklet) driven by a
//! small Rust session that mirrors the server PlayerState model so the whole UI (bar, deck,
//! queue) works unchanged. Subset of the server session: queue ops, transport, volume, gapless
//! advance, DJ mix blend, strip. Remote-control of the desktop engine is the other target.
use std::cell::RefCell;

use bc_types::player::*;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use super::store::PlayerCtx;
use crate::util::{perf_now, window};

struct Local {
    module: Option<JsValue>,
    ctx: Option<PlayerCtx>,
    /// Active deck (0 or 1) and the queue uid it holds.
    deck: u32,
    loading: bool,
    next_loaded: Option<(u32, usize)>, // (deck, queue index) preloaded
    transitioning: bool,
    poll: Option<i32>,
    uid: u64,
}

thread_local! {
    static LOCAL: RefCell<Local> = RefCell::new(Local { module: None, ctx: None, deck: 0, loading: false, next_loaded: None, transitioning: false, poll: None, uid: 1 });
}

fn js_call(name: &str, args: &[JsValue]) -> Option<JsValue> {
    let module = LOCAL.with(|l| l.borrow().module.clone())?;
    let f = js_sys::Reflect::get(&module, &JsValue::from_str(name)).ok()?;
    let f: js_sys::Function = f.dyn_into().ok()?;
    let arr = js_sys::Array::new();
    for a in args {
        arr.push(a);
    }
    f.apply(&JsValue::NULL, &arr).ok()
}

async fn js_call_async(name: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    match js_call(name, args) {
        Some(v) => JsFuture::from(js_sys::Promise::resolve(&v)).await,
        None => Err(JsValue::from_str("host not ready")),
    }
}

async fn ensure_host() -> bool {
    if LOCAL.with(|l| l.borrow().module.is_some()) {
        return true;
    }
    let import = js_sys::Function::new_no_args("return import('/bc-local.js')");
    let Ok(p) = import.call0(&JsValue::NULL) else { return false };
    let Ok(m) = JsFuture::from(js_sys::Promise::resolve(&p)).await else { return false };
    LOCAL.with(|l| l.borrow_mut().module = Some(m));
    if js_call_async("init", &[]).await.is_err() {
        LOCAL.with(|l| l.borrow_mut().module = None);
        return false;
    }
    start_polling();
    true
}

fn with_ctx<R>(f: impl FnOnce(&PlayerCtx) -> R) -> Option<R> {
    let ctx = LOCAL.with(|l| l.borrow().ctx)?;
    Some(f(&ctx))
}

fn publish(mutate: impl FnOnce(&mut PlayerState)) {
    with_ctx(|c| {
        c.state.update(|s| {
            mutate(s);
            s.rev += 1;
        })
    });
}

fn new_uid() -> u64 {
    LOCAL.with(|l| {
        let mut l = l.borrow_mut();
        l.uid += 1;
        l.uid
    })
}

fn stream_url(item: &QueueItem) -> String {
    if let Some(u) = &item.stream_url {
        return u.clone();
    }
    format!("/api/stream/{}", item.track_id)
}

/// Called by `PlayerCtx::cmd` while the target is "this device".
pub fn command(ctx: &PlayerCtx, c: &PlayerCommand) {
    LOCAL.with(|l| l.borrow_mut().ctx = Some(*ctx));
    let c = c.clone();
    spawn_local(async move {
        if !ensure_host().await {
            crate::ds::toast_err("This browser cannot play locally (AudioWorklet unavailable)");
            return;
        }
        apply(c).await;
    });
}

/// Adopt the queue of the desktop session when switching to this device (paused).
pub fn take_over(ctx: &PlayerCtx) {
    LOCAL.with(|l| l.borrow_mut().ctx = Some(*ctx));
    ctx.state.update(|s| {
        s.status = PlayerStatus::Paused;
        s.devices = DevicesInfo { backend: "browser (AudioWorklet)".into(), ..Default::default() };
        s.rev += 1;
    });
}

async fn load_index(idx: usize, start_s: f64, autoplay: bool) {
    let Some(item) = with_ctx(|c| c.state.with_untracked(|s| s.queue.get(idx).cloned())).flatten() else { return };
    let deck = LOCAL.with(|l| {
        let mut l = l.borrow_mut();
        l.loading = true;
        l.transitioning = false;
        l.next_loaded = None;
        l.deck
    });
    publish(|s| {
        s.queue_index = idx as i64;
        s.current = Some(item.clone());
        s.status = PlayerStatus::Loading;
        s.error = None;
    });
    let url = stream_url(&item);
    match js_call_async("load", &[deck.into(), url.into(), start_s.into(), 1.0.into()]).await {
        Ok(_) => {
            if autoplay {
                js_call("play", &[deck.into()]);
            }
            publish(|s| s.status = if autoplay { PlayerStatus::Playing } else { PlayerStatus::Paused });
            LOCAL.with(|l| l.borrow_mut().loading = false);
        }
        Err(e) => {
            LOCAL.with(|l| l.borrow_mut().loading = false);
            publish(|s| {
                s.status = PlayerStatus::Idle;
                s.error = Some(format!("Could not load the track: {e:?}"));
            });
        }
    }
}

fn status_is(s: PlayerStatus) -> bool {
    with_ctx(|c| c.state.with_untracked(|st| st.status == s)).unwrap_or(false)
}

async fn apply(c: PlayerCommand) {
    use PlayerCommand as C;
    match c {
        C::PlayQueue { items, start_index, source } => {
            let items = number(items);
            publish(|s| {
                s.queue = items;
                s.source = source;
            });
            load_index(start_index, 0.0, true).await;
        }
        C::PlayTrack { item, queue } => {
            let items = number(queue.unwrap_or_else(|| vec![item.clone()]));
            let at = items.iter().position(|i| i.track_id == item.track_id).unwrap_or(0);
            publish(|s| s.queue = items);
            load_index(at, 0.0, true).await;
        }
        C::Play | C::Toggle if status_is(PlayerStatus::Paused) => {
            js_call("resume", &[]);
            publish(|s| s.status = PlayerStatus::Playing);
        }
        C::Play => {}
        C::Pause => {
            js_call("pause", &[]);
            publish(|s| s.status = PlayerStatus::Paused);
        }
        C::Toggle => {
            if status_is(PlayerStatus::Playing) {
                js_call("pause", &[]);
                publish(|s| s.status = PlayerStatus::Paused);
            } else if with_ctx(|c| c.state.with_untracked(|s| s.current.is_some())).unwrap_or(false) {
                js_call("resume", &[]);
                publish(|s| s.status = PlayerStatus::Playing);
            }
        }
        C::Stop => {
            js_call("stop", &[]);
            publish(|s| s.status = PlayerStatus::Idle);
        }
        C::Next => advance(1).await,
        C::Previous => {
            let pos = with_ctx(|c| c.clock.with_untracked(|k| k.position_s)).unwrap_or(0.0);
            if pos > 3.0 {
                seek_to(0.0);
            } else {
                advance(-1).await;
            }
        }
        C::JumpTo { index } => load_index(index, 0.0, true).await,
        C::Seek { seconds } => seek_to(seconds),
        C::SeekRelative { delta_s } => {
            let pos = with_ctx(|c| c.clock.with_untracked(|k| super::store::position_now(k, c.clock_at.get_untracked()))).unwrap_or(0.0);
            seek_to((pos + delta_s).max(0.0));
        }
        C::AddToQueue { items } => {
            let items = number(items);
            publish(|s| s.queue.extend(items));
        }
        C::PlayNext { items } => {
            let items = number(items);
            publish(|s| {
                let at = ((s.queue_index + 1).max(0) as usize).min(s.queue.len());
                for (k, it) in items.into_iter().enumerate() {
                    s.queue.insert(at + k, it);
                }
            });
        }
        C::RemoveAt { index } => publish(|s| {
            if index < s.queue.len() && index as i64 != s.queue_index {
                s.queue.remove(index);
                if (index as i64) < s.queue_index {
                    s.queue_index -= 1;
                }
            }
        }),
        C::MoveInQueue { from, to } => publish(|s| {
            if from < s.queue.len() && to < s.queue.len() && from as i64 > s.queue_index {
                let it = s.queue.remove(from);
                s.queue.insert(to, it);
            }
        }),
        C::SetVolume { volume } => {
            js_call("volume", &[volume.into()]);
            publish(|s| {
                s.volume = volume;
                s.muted = false;
            });
        }
        C::ToggleMute => {
            let muted = with_ctx(|c| c.state.with_untracked(|s| s.muted)).unwrap_or(false);
            let vol = with_ctx(|c| c.state.with_untracked(|s| s.volume)).unwrap_or(0.8);
            js_call("volume", &[(if muted { vol } else { 0.0 }).into()]);
            publish(|s| s.muted = !muted);
        }
        C::ToggleShuffle | C::SetShuffle { .. } => publish(|s| s.shuffle = !s.shuffle),
        C::CycleRepeat => publish(|s| {
            s.repeat = match s.repeat {
                RepeatMode::Off => RepeatMode::All,
                RepeatMode::All => RepeatMode::One,
                RepeatMode::One => RepeatMode::Off,
            }
        }),
        C::SetRepeat { mode } => publish(|s| s.repeat = mode),
        C::ToggleMix => publish(|s| s.mix = !s.mix),
        C::SetMix { on } => publish(|s| s.mix = on),
        C::SetStrip { patch } => {
            let strip = with_ctx(|c| {
                c.state.update(|s| s.strip.apply(&patch));
                c.state.with_untracked(|s| s.strip.clone())
            });
            if let Some(s) = strip {
                let g = |db: f64, kill: bool| if kill { 0.0 } else { 10f64.powf(db / 20.0) };
                js_call("strip", &[g(s.low_db, s.kill_low).into(), g(s.mid_db, s.kill_mid).into(), g(s.high_db, s.kill_high).into(), s.filter.into(), s.echo_send.into()]);
            }
        }
        C::CutNow => {
            js_call("cutNow", &[]);
        }
        C::Retime { factor } => {
            let rem = with_ctx(|c| c.clock.with_untracked(|k| (k.duration_s - k.position_s).max(1.0))).unwrap_or(8.0);
            js_call("retime", &[(rem * factor).into()]);
        }
        C::Nudge { delta_s } => {
            js_call("nudge", &[delta_s.into()]);
        }
        C::SetTransitionEcho { on } => {
            js_call("setEcho", &[on.into()]);
        }
        C::SetTransitionSync { on } => {
            js_call("setSync", &[on.into()]);
        }
        C::MixNow => start_blend().await,
        C::ClearError => publish(|s| s.error = None),
        C::MarkLoved { track_id, loved } => {
            publish(|s| {
                for q in s.queue.iter_mut().chain(s.current.iter_mut()) {
                    if q.track_id == track_id {
                        q.loved = loved;
                    }
                }
            });
        }
        _ => crate::ds::toast_info("Not available when playing on this device (use desktop speakers)"),
    }
}

fn number(mut items: Vec<QueueItem>) -> Vec<QueueItem> {
    for it in items.iter_mut() {
        if it.uid == 0 {
            it.uid = new_uid();
        }
    }
    items
}

fn seek_to(s: f64) {
    let deck = LOCAL.with(|l| l.borrow().deck);
    js_call("seek", &[deck.into(), s.into()]);
    with_ctx(|c| {
        c.clock.update(|k| k.position_s = s);
        c.clock_at.set(perf_now());
    });
}

/// Index of the next track honouring repeat (None = end of queue).
fn next_index(s: &PlayerState, dir: i64) -> Option<usize> {
    let n = s.queue.len() as i64;
    if n == 0 {
        return None;
    }
    let i = s.queue_index + dir;
    if s.repeat == RepeatMode::One && dir > 0 {
        return Some(s.queue_index.max(0) as usize);
    }
    if (0..n).contains(&i) {
        Some(i as usize)
    } else if s.repeat == RepeatMode::All {
        Some(i.rem_euclid(n) as usize)
    } else {
        None
    }
}

async fn advance(dir: i64) {
    let next = with_ctx(|c| c.state.with_untracked(|s| next_index(s, dir))).flatten();
    match next {
        Some(i) => load_index(i, 0.0, true).await,
        None => {
            js_call("stop", &[]);
            publish(|s| s.status = PlayerStatus::Idle);
        }
    }
}

/// Blend the next queue item over the current one.
async fn start_blend() {
    let (next, deck, already) = LOCAL.with(|l| {
        let l = l.borrow();
        (l.ctx.and_then(|c| c.state.with_untracked(|s| next_index(s, 1))), l.deck, l.transitioning || l.loading)
    });
    let (Some(idx), false) = (next, already) else { return };
    let inc = 1 - deck;
    let Some(item) = with_ctx(|c| c.state.with_untracked(|s| s.queue.get(idx).cloned())).flatten() else { return };
    LOCAL.with(|l| l.borrow_mut().loading = true);
    let ok = js_call_async("load", &[inc.into(), stream_url(&item).into(), 0.0.into(), 1.0.into()]).await.is_ok();
    LOCAL.with(|l| l.borrow_mut().loading = false);
    if !ok {
        return;
    }
    let len = with_ctx(|c| c.state.with_untracked(|s| (s.mix_settings.length_beats as f64) * 60.0 / 128.0)).unwrap_or(12.0).clamp(4.0, 30.0);
    js_call("transition", &[deck.into(), inc.into(), "blend".into(), len.into()]);
    LOCAL.with(|l| {
        let mut l = l.borrow_mut();
        l.transitioning = true;
        l.next_loaded = Some((inc, idx));
    });
    let started = crate::util::unix_ms();
    with_ctx(|c| {
        c.transition.set(Some(TransitionState {
            kind: TransitionKind::Blend,
            started_at_ms: started as u64,
            ends_at_ms: (started + len * 1000.0) as u64,
            echo: false,
            sync: None,
            phase: None,
            outgoing_uid: None,
            incoming_uid: Some(item.uid),
            phase_error_ms: None,
        }))
    });
}

/// Poll the worklet snapshot: clock, advance, auto-mix.
fn start_polling() {
    if LOCAL.with(|l| l.borrow().poll.is_some()) {
        return;
    }
    // worklet events: advanced / ended
    let cb = Closure::<dyn FnMut(web_sys::CustomEvent)>::new(move |ev: web_sys::CustomEvent| {
        let name = js_sys::Reflect::get(&ev.detail(), &"name".into()).ok().and_then(|v| v.as_string()).unwrap_or_default();
        if name == "advanced" || name == "fade-done" {
            // the incoming deck became the active one
            let promoted = LOCAL.with(|l| {
                let mut l = l.borrow_mut();
                l.next_loaded.take().map(|(deck, idx)| {
                    l.deck = deck;
                    l.transitioning = false;
                    idx
                })
            });
            if let Some(idx) = promoted {
                publish(|s| {
                    s.queue_index = idx as i64;
                    s.current = s.queue.get(idx).cloned();
                    s.status = PlayerStatus::Playing;
                });
                with_ctx(|c| c.transition.set(None));
            }
        } else if name == "ended" {
            let busy = LOCAL.with(|l| l.borrow().transitioning || l.borrow().loading);
            if !busy {
                spawn_local(async { advance(1).await });
            }
        }
    });
    let _ = window().add_event_listener_with_callback("bc-local", cb.as_ref().unchecked_ref());
    cb.forget();

    let tick = Closure::<dyn FnMut()>::new(move || {
        let Some(json) = js_call("state", &[]).and_then(|v| v.as_string()).filter(|s| !s.is_empty()) else { return };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) else { return };
        let active = LOCAL.with(|l| l.borrow().deck) as usize;
        let deck = &v["decks"][active];
        let (pos, len, rate) = (deck["positionS"].as_f64().unwrap_or(0.0), deck["lengthS"].as_f64().unwrap_or(0.0), deck["rate"].as_f64().unwrap_or(1.0));
        let paused = v["paused"].as_bool().unwrap_or(false);
        let buffered = deck["bufferedS"].as_f64().unwrap_or(0.0);
        let peak = v["peak"].as_array().map(|a| (a.first().and_then(|x| x.as_f64()).unwrap_or(0.0), a.get(1).and_then(|x| x.as_f64()).unwrap_or(0.0))).unwrap_or((0.0, 0.0));
        with_ctx(|c| {
            c.clock.set(Clock {
                frames_played: v["framesPlayed"].as_u64().unwrap_or(0),
                sample_rate: 48_000,
                position_s: pos,
                duration_s: len,
                buffered_s: buffered,
                rate,
                playing: !paused && deck["state"] == "playing",
                peak_l: peak.0 as f32,
                peak_r: peak.1 as f32,
                xruns: v["xruns"].as_u64().unwrap_or(0),
                ..Default::default()
            });
            c.clock_at.set(perf_now());
        });
        // auto-mix: blend near the end when mix is on (else the worklet's gapless handles `ended`)
        let (mix, transitioning, loading) = with_ctx(|c| c.state.with_untracked(|s| s.mix)).map(|m| LOCAL.with(|l| (m, l.borrow().transitioning, l.borrow().loading))).unwrap_or((false, true, true));
        if mix && !transitioning && !loading && len > 30.0 && len - pos < 16.0 && !paused {
            spawn_local(async { start_blend().await });
        }
    });
    let id = window().set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), 100).ok();
    tick.forget();
    LOCAL.with(|l| l.borrow_mut().poll = id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(n: usize, idx: i64, repeat: RepeatMode) -> PlayerState {
        PlayerState { queue: (0..n).map(|i| QueueItem { track_id: i as i64 + 1, ..Default::default() }).collect(), queue_index: idx, repeat, ..Default::default() }
    }

    #[test]
    fn next_index_honours_repeat_modes() {
        assert_eq!(next_index(&st(3, 0, RepeatMode::Off), 1), Some(1));
        assert_eq!(next_index(&st(3, 2, RepeatMode::Off), 1), None);
        assert_eq!(next_index(&st(3, 2, RepeatMode::All), 1), Some(0));
        assert_eq!(next_index(&st(3, 0, RepeatMode::All), -1), Some(2));
        assert_eq!(next_index(&st(3, 1, RepeatMode::One), 1), Some(1));
        assert_eq!(next_index(&st(0, -1, RepeatMode::Off), 1), None);
    }

    #[test]
    fn stream_url_prefers_the_item_url() {
        let a = QueueItem { track_id: 7, ..Default::default() };
        assert_eq!(stream_url(&a), "/api/stream/7");
        let b = QueueItem { track_id: -1, stream_url: Some("/api/explore/stream?x=1".into()), ..Default::default() };
        assert_eq!(stream_url(&b), "/api/explore/stream?x=1");
    }
}
