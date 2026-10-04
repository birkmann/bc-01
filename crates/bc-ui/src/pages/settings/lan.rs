//! Settings > LAN and pairing (PLAN 3.2). Pairing is only manageable from the host
//! machine (the server answers 403 to anyone else); other peers just see status.
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;

use super::common::SysCard;
use crate::api;
use crate::data::{QuerySpec, use_query};
use super::common::{QueryError, qh};
use crate::ds::{Badge, Button, EmptyState, Icon, Skeleton, StatusBadge, Tone, Variant, confirm, toast_err, toast_ok};
use crate::util::{copy_text, enc};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AuthStatus {
    pub lan: bool,
    #[serde(default)]
    pub forced: bool,
    pub local: bool,
    pub authenticated: bool,
    pub bind: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct PairingOut {
    pub code: String,
    pub expires_in_s: u64,
    pub lan_ip: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DeviceOut {
    pub id: i64,
    pub name: String,
    pub created_at: String,
}

#[component]
pub fn LanSection() -> impl IntoView {
    let status = qh(use_query::<AuthStatus>(|| Some(QuerySpec::new("/auth/status", &["auth"]))));
    let done = Callback::new(move |_| status.refetch());
    view! {
        <SysCard title="LAN access" icon="wifi"
            hint="Open bc on your phone or another computer on the same network. LAN mode is opt-in and every other device must be paired with a short-lived code.">
            <QueryError q=status />
            <Show when=move || status.data.get().is_none() && status.error.get().is_none()><Skeleton height="48px" /></Show>
            {move || status.data.get().map(|s| { let (lan, local, forced, bind) = (s.lan, s.local, s.forced, s.bind.clone()); view! {
                <div class="row gap wrap">
                    {if lan {
                        view! { <StatusBadge tone=Tone::Ok label="LAN mode on" /> }.into_any()
                    } else {
                        view! { <StatusBadge tone=Tone::Neutral label="LAN mode off" /> }.into_any()
                    }}
                    <Badge icon="monitor">{if local { "This is the host machine" } else { "Remote device" }}</Badge>
                    <span class="mono faint">{format!("bind {bind}")}</span>
                    {(lan && local).then(|| view! { <LanSwitch on=false label="Turn off" done=done /> })}
                </div>
                {(lan && local && forced).then(|| view! {
                    <p class="sys-hint faint">"bc was started with --lan or BC_LAN, so LAN mode is on again at the next start even if you turn it off here."</p>
                })}
            }})}
        </SysCard>
        {move || status.data.get().map(|s| {
            if !s.lan && s.local {
                view! { <EnableLan done=done /> }.into_any()
            } else if !s.lan {
                view! { <></> }.into_any()
            } else if !s.local {
                view! { <RemoteNote authenticated=s.authenticated /> }.into_any()
            } else {
                view! { <Pairing /> <Devices /> }.into_any()
            }
        })}
    }
}

/// Turns LAN mode on or off. The server rebinds on the same port, so this window keeps working.
#[component]
fn LanSwitch(on: bool, #[prop(into)] label: String, done: Callback<()>) -> impl IntoView {
    let busy = RwSignal::new(false);
    let click = move |_| {
        busy.set(true);
        spawn_local(async move {
            match api::post::<_, serde_json::Value>("/auth/lan", &serde_json::json!({ "on": on })).await {
                Ok(_) => {
                    toast_ok(if on { "LAN mode is on" } else { "LAN mode is off" });
                    done.run(());
                }
                Err(e) => toast_err(&e.message()),
            }
            busy.set(false);
        });
    };
    let variant = if on { Variant::Primary } else { Variant::Outline };
    view! {
        <Button variant=variant size=crate::ds::Size::Sm icon="wifi" busy=busy on_click=click>{label}</Button>
    }
}

#[component]
fn EnableLan(done: Callback<()>) -> impl IntoView {
    view! {
        <SysCard title="Enable LAN mode" icon="lock"
            hint="LAN mode listens on your network address instead of localhost only. Only this computer can switch it, and it stays on across restarts until you turn it off."
            actions=crate::ds::children(move || view! { <LanSwitch on=true label="Turn on LAN mode" done=done /> })>
            <ol class="sys-steps">
                <li>"Turn on LAN mode."</li>
                <li>"Create a pairing code for each device and scan it with the phone."</li>
                <li>"Allow incoming connections on this port if a firewall is running."</li>
            </ol>
            <p class="sys-hint faint">"Localhost is always trusted; every other device needs a pairing token. Starting bc with --lan or BC_LAN=1 works too."</p>
        </SysCard>
    }
}

#[component]
fn RemoteNote(authenticated: bool) -> impl IntoView {
    view! {
        <SysCard title="Pairing" icon="qr">
            <EmptyState icon="lock" title="Pairing is managed on the host machine"
                hint={if authenticated { "This device is paired. Open Settings on the computer running bc to pair more devices or revoke access." } else { "Open Settings on the computer running bc to create a pairing code." }} />
        </SysCard>
    }
}

#[component]
fn Pairing() -> impl IntoView {
    let pairing = RwSignal::new(None::<PairingOut>);
    let busy = RwSignal::new(false);
    let left = RwSignal::new(0i64);
    let create = move |_| {
        busy.set(true);
        spawn_local(async move {
            match api::post::<_, PairingOut>("/auth/pairing", &serde_json::json!({})).await {
                Ok(p) => {
                    left.set(p.expires_in_s as i64);
                    pairing.set(Some(p));
                }
                Err(e) => toast_err(&e.message()),
            }
            busy.set(false);
        });
    };
    // Countdown (1 Hz while a code is showing).
    let tick = StoredValue::new(None::<i32>);
    Effect::new(move |_| {
        use wasm_bindgen::JsCast;
        let active = pairing.with(|p| p.is_some());
        let w = crate::util::window();
        if let Some(id) = tick.get_value() {
            w.clear_interval_with_handle(id);
            tick.set_value(None);
        }
        if active {
            let cb = wasm_bindgen::closure::Closure::<dyn Fn()>::new(move || {
                let _ = left.try_update(|l| *l -= 1);
            });
            let id = w.set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 1000).ok();
            cb.forget();
            tick.set_value(id);
        }
    });
    on_cleanup(move || {
        if let Some(id) = tick.try_get_value().flatten() {
            crate::util::window().clear_interval_with_handle(id);
        }
    });
    let expired = Signal::derive(move || pairing.with(|p| p.is_some()) && left.get() <= 0);
    view! {
        <SysCard title="Pair a device" icon="qr"
            hint="Scan the QR code with the phone's camera, or type the address. The code works once and expires after a few minutes."
            actions=crate::ds::children(move || view! {
                <Button variant=Variant::Primary size=crate::ds::Size::Sm icon="plus" busy=busy on_click=create>"New pairing code"</Button>
            })>
            <Show when=move || pairing.get().is_none()>
                <p class="sys-empty">"No active code. Create one to pair a phone, tablet or another computer."</p>
            </Show>
            {move || pairing.get().map(|p| {
                let url = p.url.clone();
                let qr = url.as_ref().map(|u| format!("/api/auth/pairing/qr.svg?data={}", enc(u)));
                view! {
                    <div class="pair-box" class:expired=move || expired.get()>
                        {qr.map(|src| view! { <img class="qr" src=src alt="QR code with the pairing address" width="200" height="200" /> })}
                        <div class="pair-info">
                            <div class="k">"Code"</div>
                            <div class="pair-code mono">{p.code.clone()}</div>
                            {url.clone().map(|u| view! {
                                <div class="k">"Address"</div>
                                <div class="code-line"><code class="mono truncate">{u.clone()}</code>
                                    <Button size=crate::ds::Size::Sm icon="copy" title="Copy address" on_click=move |_| { copy_text(&u); toast_ok("Address copied"); }>"Copy"</Button></div>
                            })}
                            {p.lan_ip.clone().map(|ip| view! { <div class="faint mono">{format!("Host address {ip}")}</div> })}
                            <div role="timer" aria-live="off">
                                {move || if expired.get() {
                                    view! { <StatusBadge tone=Tone::Warn label="Expired: create a new code" /> }.into_any()
                                } else {
                                    let l = left.get();
                                    view! { <StatusBadge tone=Tone::Info label=format!("Expires in {}:{:02}", l / 60, l % 60) /> }.into_any()
                                }}
                            </div>
                        </div>
                    </div>
                }
            })}
        </SysCard>
    }
}

#[component]
fn Devices() -> impl IntoView {
    let q = qh(use_query::<Vec<DeviceOut>>(|| Some(QuerySpec::new("/auth/devices", &["auth.devices"]))));
    let revoke = move |d: DeviceOut| {
        spawn_local(async move {
            if confirm("Revoke this device?", &format!("\"{}\" will have to be paired again to open bc.", d.name), "Revoke", true).await {
                match api::call("DELETE", &format!("/auth/devices/{}", d.id)).await {
                    Ok(()) => q.refetch(),
                    Err(e) => toast_err(&e.message()),
                }
            }
        });
    };
    view! {
        <SysCard title="Paired devices" icon="phone">
            <QueryError q=q />
            <Show when=move || q.data.get().map(|d| d.is_empty()).unwrap_or(false)>
                <p class="sys-empty">"No paired devices yet."</p>
            </Show>
            <ul class="device-list">
                <For each=move || q.data.get().map(|d| (*d).clone()).unwrap_or_default() key=|d| d.id let:d>
                    {
                        let d2 = d.clone();
                        view! {
                            <li>
                                <Icon name="phone" />
                                <div class="grow"><div>{d.name.clone()}</div><div class="mono faint">{format!("paired {}", d.created_at)}</div></div>
                                <Button size=crate::ds::Size::Sm variant=Variant::Danger icon="trash" title=format!("Revoke {}", d.name)
                                    on_click=move |_| revoke(d2.clone())>"Revoke"</Button>
                            </li>
                        }
                    }
                </For>
            </ul>
        </SysCard>
    }
}
