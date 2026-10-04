//! WebSocket client: drives `logic::realtime_conn::Conn`, fans events out to topic
//! subscribers, turns `invalidate` events into targeted cache invalidations and
//! `stream.resync` into a global refetch.
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use bc_types::events::{ClientMsg, Event, Invalidate};
use leptos::prelude::*;
use serde::de::DeserializeOwned;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use super::cache;
use crate::logic::realtime_conn::{Cmd, Conn};
use crate::util::{perf_now, window};

type Handler = Rc<dyn Fn(&serde_json::Value)>;

type InvHandler = Rc<dyn Fn(&[i64])>;

struct Hub {
    conn: Conn,
    inv_handlers: Vec<(u64, String, InvHandler)>,
    socket: Option<web_sys::WebSocket>,
    handlers: Vec<(u64, String, Handler)>,
    next_id: u64,
    started: bool,
}

thread_local! {
    static HUB: RefCell<Option<Hub>> = const { RefCell::new(None) };
    static STATUS: Cell<Option<RwSignal<bool>>> = const { Cell::new(None) };
    static RESYNC: Cell<Option<RwSignal<u64>>> = const { Cell::new(None) };
}

/// Reactive connected flag.
pub fn ws_connected() -> RwSignal<bool> {
    STATUS.with(|s| match s.get() {
        Some(x) => x,
        None => {
            let x = crate::util::root_signal(false);
            s.set(Some(x));
            x
        }
    })
}

/// Bumped on every `stream.resync` (pages with local state refetch on it).
pub fn resync_counter() -> RwSignal<u64> {
    RESYNC.with(|s| match s.get() {
        Some(x) => x,
        None => {
            let x = crate::util::root_signal(0u64);
            s.set(Some(x));
            x
        }
    })
}

fn with_hub<R>(f: impl FnOnce(&mut Hub) -> R) -> Option<R> {
    HUB.with(|h| h.borrow_mut().as_mut().map(f))
}

fn ws_url() -> String {
    let loc = window().location();
    let proto = if loc.protocol().unwrap_or_default() == "https:" { "wss" } else { "ws" };
    format!("{proto}://{}/api/ws", loc.host().unwrap_or_default())
}

/// Start the connection (idempotent). Call once at app start.
pub fn start() {
    ws_connected();
    resync_counter();
    HUB.with(|h| {
        if h.borrow().is_none() {
            *h.borrow_mut() =
                Some(Hub { conn: Conn::new(), inv_handlers: vec![], socket: None, handlers: vec![], next_id: 1, started: false });
        }
    });
    let cmds = with_hub(|h| {
        if h.started {
            return vec![];
        }
        h.started = true;
        h.conn.start(perf_now() as u64)
    })
    .unwrap_or_default();
    run(cmds);
    // watchdog tick
    let tick = Closure::<dyn FnMut()>::new(move || {
        let cmds = with_hub(|h| h.conn.on_tick(perf_now() as u64)).unwrap_or_default();
        run(cmds);
    });
    let _ = window().set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), 5_000);
    tick.forget();
    // wake up quickly when the tab becomes visible / network returns
    let online = Closure::<dyn FnMut()>::new(move || {
        let need = with_hub(|h| h.socket.is_none() && !h.conn.connected()).unwrap_or(false);
        if need {
            open_socket();
        }
    });
    let _ = window().add_event_listener_with_callback("online", online.as_ref().unchecked_ref());
    online.forget();
}

fn run(cmds: Vec<Cmd>) {
    for c in cmds {
        match c {
            Cmd::Open => open_socket(),
            Cmd::ReopenIn(ms) => {
                let cb = Closure::once_into_js(open_socket);
                let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), ms as i32);
            }
            Cmd::SendHello(id) => send_msg(&ClientMsg::Hello { last_event_id: id }),
            Cmd::Deliver(ev) => deliver(&ev),
            Cmd::Resync => {
                cache::invalidate_all();
                resync_counter().update(|n| *n += 1);
            }
            Cmd::Status(v) => ws_connected().set(v),
            Cmd::CloseSocket => {
                if let Some(Some(s)) = with_hub(|h| h.socket.take()) {
                    let _ = s.close();
                }
            }
        }
    }
}

fn open_socket() {
    let already = with_hub(|h| h.socket.as_ref().map(|s| s.ready_state() <= 1).unwrap_or(false)).unwrap_or(false);
    if already {
        return;
    }
    let Ok(sock) = web_sys::WebSocket::new(&ws_url()) else {
        let cmds = with_hub(|h| h.conn.on_close(perf_now() as u64)).unwrap_or_default();
        run(cmds);
        return;
    };
    let on_open = Closure::<dyn FnMut(JsValue)>::new(move |_| {
        let cmds = with_hub(|h| h.conn.on_open(perf_now() as u64)).unwrap_or_default();
        run(cmds);
    });
    let on_msg = Closure::<dyn FnMut(JsValue)>::new(move |e: JsValue| {
        let me: web_sys::MessageEvent = e.unchecked_into();
        if let Some(text) = me.data().as_string() {
            let cmds = with_hub(|h| h.conn.on_text(perf_now() as u64, &text)).unwrap_or_default();
            run(cmds);
        }
    });
    let sock2 = sock.clone();
    let on_close = Closure::<dyn FnMut(JsValue)>::new(move |_| {
        // ignore close events of sockets we already replaced
        let is_current = with_hub(|h| h.socket.as_ref().map(|s| s == &sock2).unwrap_or(true)).unwrap_or(true);
        if !is_current {
            return;
        }
        with_hub(|h| h.socket = None);
        let cmds = with_hub(|h| h.conn.on_close(perf_now() as u64)).unwrap_or_default();
        run(cmds);
    });
    sock.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    sock.set_onmessage(Some(on_msg.as_ref().unchecked_ref()));
    sock.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    sock.set_onerror(Some(on_close.as_ref().unchecked_ref()));
    with_hub(|h| {
        h.socket = Some(sock);
    });
    // Leaked on purpose: a closed socket may still deliver its close event.
    on_open.forget();
    on_msg.forget();
    on_close.forget();
}

fn send_msg(m: &ClientMsg) {
    if let Ok(text) = serde_json::to_string(m) {
        with_hub(|h| {
            if let Some(s) = &h.socket {
                if s.ready_state() == 1 {
                    let _ = s.send_with_str(&text);
                }
            }
        });
    }
}

/// Send a client message; returns false when the socket is not open.
pub fn send_client_msg(m: &ClientMsg) -> bool {
    let open = with_hub(|h| h.socket.as_ref().map(|s| s.ready_state() == 1).unwrap_or(false)).unwrap_or(false);
    if open {
        send_msg(m);
    }
    open
}

/// Handlers run one by one from a snapshot, and one of them (or the cache invalidation before
/// them) can re-render a view and dispose the owners of later ones. Each handler is therefore
/// re-checked just before its turn: a handler unsubscribed meanwhile would touch disposed state.
fn deliver(ev: &Event) {
    if ev.topic == bc_types::events::TOPIC_INVALIDATE {
        if let Ok(inv) = serde_json::from_value::<Invalidate>(ev.payload.clone()) {
            cache::invalidate_entity(&inv.entity, &inv.ids);
            let hs: Vec<(u64, InvHandler)> = with_hub(|h| {
                h.inv_handlers.iter().filter(|(_, e, _)| *e == inv.entity).map(|(i, _, f)| (*i, f.clone())).collect()
            })
            .unwrap_or_default();
            for (id, f) in hs {
                if with_hub(|h| h.inv_handlers.iter().any(|(i, _, _)| *i == id)).unwrap_or(false) {
                    f(&inv.ids);
                }
            }
        }
    }
    let hs: Vec<(u64, Handler)> = with_hub(|h| {
        h.handlers.iter().filter(|(_, t, _)| t == &ev.topic || t == "*").map(|(i, _, f)| (*i, f.clone())).collect()
    })
    .unwrap_or_default();
    for (id, f) in hs {
        if with_hub(|h| h.handlers.iter().any(|(i, _, _)| *i == id)).unwrap_or(false) {
            f(&ev.payload);
        }
    }
}

/// Subscribe to a topic for the lifetime of the current reactive owner.
pub fn subscribe(topic: &str, f: impl Fn(&serde_json::Value) + 'static) {
    let id = with_hub(|h| {
        let id = h.next_id;
        h.next_id += 1;
        h.handlers.push((id, topic.to_string(), Rc::new(f)));
        id
    });
    if let Some(id) = id {
        on_cleanup(move || {
            with_hub(|h| h.handlers.retain(|(i, _, _)| *i != id));
        });
    }
}

/// Typed subscription: payload is parsed as `T` (bad payloads are dropped).
pub fn use_topic<T: DeserializeOwned + 'static>(topic: &str, f: impl Fn(T) + 'static) {
    subscribe(topic, move |v| {
        if let Ok(t) = serde_json::from_value::<T>(v.clone()) {
            f(t);
        }
    });
}

/// Run `f(ids)` whenever an `invalidate` event names one of `entities` (empty ids =
/// everything of that kind). For components that own paged state outside the
/// keyed cache (DataTable, CardGrid). Unsubscribes with the reactive owner.
pub fn use_invalidation(entities: &[&str], f: impl Fn(&[i64]) + 'static) {
    let f: InvHandler = Rc::new(f);
    let mut ids_added = vec![];
    for e in entities {
        let id = with_hub(|h| {
            let id = h.next_id;
            h.next_id += 1;
            h.inv_handlers.push((id, e.to_string(), f.clone()));
            id
        });
        if let Some(id) = id {
            ids_added.push(id);
        }
    }
    on_cleanup(move || {
        with_hub(|h| h.inv_handlers.retain(|(i, _, _)| !ids_added.contains(i)));
    });
}
