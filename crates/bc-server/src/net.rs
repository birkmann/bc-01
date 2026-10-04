//! Runtime LAN switch (PLAN §3.2): the flag the auth guard reads, and the accept loop that
//! rebinds the listener between 127.0.0.1 and 0.0.0.0 on the same port, so the desktop
//! window's origin (and its local storage) survives the switch. The choice is saved in
//! `settings` and applies again at the next start; `--lan` / `BC_LAN` still force it on.
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};

pub const SETTING_KEY: &str = "lan.enabled";

pub fn host(lan: bool) -> &'static str {
    if lan { "0.0.0.0" } else { "127.0.0.1" }
}

/// The saved choice, read before the server opens the DB (the bind comes first). Missing
/// file, table or key all mean off.
pub fn saved(db_path: &Path) -> bool {
    use bc_db::rusqlite::{Connection, OpenFlags, OptionalExtension};
    let Ok(c) = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX) else { return false };
    c.query_row("SELECT value FROM settings WHERE key=?1", [SETTING_KEY], |r| r.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
        .is_some_and(|v| v.trim() == "true")
}

struct Rebind {
    lan: bool,
    done: oneshot::Sender<std::io::Result<()>>,
}

pub struct Net {
    lan: watch::Sender<bool>,
    forced: AtomicBool,
    rebind: Mutex<Option<mpsc::UnboundedSender<Rebind>>>,
}

impl Net {
    pub fn new(lan: bool) -> Self {
        Self { lan: watch::Sender::new(lan), forced: AtomicBool::new(false), rebind: Mutex::new(None) }
    }
    pub fn lan(&self) -> bool {
        *self.lan.borrow()
    }
    /// Ends when LAN mode goes off; remote WebSocket sessions hang up on it.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.lan.subscribe()
    }
    /// Started with `--lan` / `BC_LAN`: LAN mode comes back on at the next start whatever is saved.
    pub fn forced(&self) -> bool {
        self.forced.load(Ordering::Relaxed)
    }
    /// Switch LAN mode, rebinding the listener when there is one (an in-process app only flips
    /// the flag). Returns once the new listener accepts connections.
    pub async fn set_lan(&self, on: bool) -> std::io::Result<()> {
        if self.lan() == on {
            return Ok(());
        }
        let tx = self.rebind.lock().clone();
        let Some(tx) = tx else {
            self.lan.send_replace(on);
            return Ok(());
        };
        let (done, rx) = oneshot::channel();
        tx.send(Rebind { lan: on, done }).map_err(|_| std::io::Error::other("server is shutting down"))?;
        rx.await.map_err(|_| std::io::Error::other("server is shutting down"))?
    }
}

pub(crate) async fn bind(lan: bool, port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind((host(lan), port)).await
}

/// Bind once the old listener is gone: axum drops it as soon as its graceful shutdown starts,
/// which happens on the server task, a moment after the signal.
async fn bind_again(lan: bool, port: u16) -> std::io::Result<TcpListener> {
    let mut tries = 0;
    loop {
        match bind(lan, port).await {
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && tries < 40 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            r => return r,
        }
    }
}

/// Serve `listener` until `stop`, swapping it for a new one on each LAN switch. A replaced
/// server drains its open connections in the background.
pub(crate) fn spawn(net: Arc<Net>, listener: TcpListener, app: Router, forced: bool, stop: oneshot::Receiver<()>) -> tokio::task::JoinHandle<()> {
    let (tx, mut rebinds) = mpsc::unbounded_channel::<Rebind>();
    *net.rebind.lock() = Some(tx);
    net.forced.store(forced, Ordering::Relaxed);
    let mut stop = stop;
    tokio::spawn(async move {
        let mut listener = listener;
        loop {
            let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
            let (cut, cut_rx) = oneshot::channel::<()>();
            let svc = app.clone().into_make_service_with_connect_info::<SocketAddr>();
            let server = tokio::spawn(async move {
                let _ = axum::serve(listener, svc)
                    .with_graceful_shutdown(async move {
                        let _ = cut_rx.await;
                    })
                    .await;
            });
            let req = tokio::select! {
                _ = &mut stop => None,
                Some(req) = rebinds.recv() => Some(req),
            };
            let _ = cut.send(());
            let Some(req) = req else {
                let _ = server.await;
                return;
            };
            // Raise the flag before 0.0.0.0 accepts anyone and lower it only once nobody else can
            // connect, so a remote peer never meets the loopback-only rules.
            let was = net.lan();
            if req.lan {
                net.lan.send_replace(true);
            }
            match bind_again(req.lan, port).await {
                Ok(l) => {
                    listener = l;
                    net.lan.send_replace(req.lan);
                    tracing::info!("LAN mode {}: listening on {}:{port}", if req.lan { "on" } else { "off" }, host(req.lan));
                    let _ = req.done.send(Ok(()));
                }
                Err(e) => {
                    tracing::warn!("LAN switch failed: cannot bind {}:{port}: {e}", host(req.lan));
                    match bind_again(was, port).await {
                        Ok(l) => {
                            listener = l;
                            net.lan.send_replace(was);
                            let _ = req.done.send(Err(e));
                        }
                        Err(e2) => {
                            tracing::error!("cannot bind {}:{port} again: {e2}; the server is down", host(was));
                            let _ = req.done.send(Err(e));
                            return;
                        }
                    }
                }
            }
        }
    })
}
