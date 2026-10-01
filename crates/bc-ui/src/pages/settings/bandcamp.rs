//! Settings > Downloads and Bandcamp: disk guard, cookie (write-only), fans and
//! label resolution.
use bc_types::bandcamp::{CookieRequest, FanOut, IdentityStatus, LabelResolveStatus};
use bc_types::jobs::{DiskIn, DiskOut};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::common::{Notice, SysCard, qh};
use super::logic::{bytes_to_gb_text, gb_to_bytes, parse_gb};
use crate::api;
use crate::data::{QuerySpec, use_query, use_topic};
use crate::ds::{Badge, Button, Icon, Meter, Size, Skeleton, StatusBadge, Tone, Variant, confirm, toast_err, toast_ok};
use crate::logic::format::{format_bytes, format_count};

#[component]
pub fn BandcampSection() -> impl IntoView {
    view! {
        <DiskGuard />
        <Cookie />
        <Fans />
    }
}

#[component]
fn DiskGuard() -> impl IntoView {
    let q = qh(use_query::<DiskOut>(|| Some(QuerySpec::new("/downloads/disk", &["downloads.disk"]))));
    use_topic::<DiskOut>("downloads.disk", move |d| {
        crate::data::cache::patch::<DiskOut>("/downloads/disk", |x| *x = d);
    });
    let draft = RwSignal::new(String::new());
    let stored = Memo::new(move |_| q.data.get().map(|d| d.min_free_bytes));
    // Follow the stored limit only: free space moves and must not wipe what is being typed.
    Effect::new(move |_| {
        if let Some(b) = stored.get() {
            draft.set(bytes_to_gb_text(b));
        }
    });
    let parsed = Memo::new(move |_| parse_gb(&draft.get()));
    let dirty = Memo::new(move |_| match (parsed.get(), stored.get()) {
        (Some(g), Some(s)) => gb_to_bytes(g) != s,
        _ => false,
    });
    let busy = RwSignal::new(false);
    let save = move || {
        let Some(g) = parsed.get_untracked() else { return };
        busy.set(true);
        spawn_local(async move {
            match api::put::<_, DiskOut>("/downloads/disk", &DiskIn { min_free_bytes: gb_to_bytes(g) }).await {
                Ok(d) => {
                    crate::data::cache::patch::<DiskOut>("/downloads/disk", |x| *x = d);
                    toast_ok("Free-space limit saved");
                }
                Err(e) => toast_err(&e.message()),
            }
            busy.set(false);
        });
    };
    let save2 = save.clone();
    view! {
        <SysCard title="Downloads and disk guard" icon="hdd"
            hint="Downloading pauses by itself when free space drops below the limit and resumes once space is freed. In-flight albums are handed back to the queue.">
            <Show when=move || q.data.get().is_none()><Skeleton height="40px" /></Show>
            <Show when=move || q.data.get().is_some()>
                <div class="row gap wrap disk-form">
                    <label for="disk-min">"Stop downloading when less than"</label>
                    <input id="disk-min" class="input mono" style="width:96px" type="number" min="0" step="0.5" inputmode="decimal"
                        aria-invalid=move || parsed.get().is_none().to_string()
                        prop:value=move || draft.get() on:input=move |ev| draft.set(event_target_value(&ev))
                        on:keydown=move |ev| if ev.key() == "Enter" && dirty.get_untracked() { save2() } />
                    <span>"GB is free"</span>
                    <Button variant=Variant::Primary busy=busy disabled=Signal::derive(move || !dirty.get()) on_click=move |_| save()>"Save"</Button>
                </div>
                {move || q.data.get().map(|d| {
                    let free = d.free_bytes.map(|f| format_bytes(f as f64)).unwrap_or_else(|| "unknown".into());
                    let ratio = d.free_bytes.map(|f| (f as f64 / d.min_free_bytes.max(1) as f64 / 4.0).clamp(0.0, 1.0));
                    view! {
                        <div class="disk-state">
                            <div class="row gap wrap">
                                <Icon name="hdd" />
                                <span class="mono">{free}</span><span class="muted">" free on "</span>
                                <span class="mono faint truncate">{d.path.clone()}</span>
                                <span class="spacer"></span>
                                {if d.held {
                                    view! { <StatusBadge tone=Tone::Warn label="Downloads on hold" /> }.into_any()
                                } else {
                                    view! { <StatusBadge tone=Tone::Ok label="Downloading allowed" /> }.into_any()
                                }}
                            </div>
                            <Meter value=ratio tone={if d.held { Tone::Warn } else { Tone::Neutral }} label="Free space relative to four times the limit" />
                        </div>
                    }
                })}
            </Show>
        </SysCard>
    }
}

#[component]
fn Cookie() -> impl IntoView {
    let q = qh(use_query::<IdentityStatus>(|| Some(QuerySpec::new("/harvest/identity", &["identity"]))));
    let value = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let save = move || {
        let c = value.get_untracked().trim().to_string();
        if c.is_empty() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match api::put::<_, IdentityStatus>("/harvest/identity", &CookieRequest { cookie: c }).await {
                Ok(s) => {
                    // Write-only: the field is cleared, the cookie is never shown again.
                    value.set(String::new());
                    crate::data::cache::patch::<IdentityStatus>("/harvest/identity", |x| *x = s);
                    toast_ok("Cookie saved");
                }
                Err(e) => error.set(Some(e.message())),
            }
            busy.set(false);
        });
    };
    let save2 = save.clone();
    let forget = move |_| {
        spawn_local(async move {
            if confirm("Forget the Bandcamp cookie?", "Harvesting your own collection and wishlist stops working until you paste a cookie again.", "Forget", true).await {
                match api::call("DELETE", "/harvest/identity").await {
                    Ok(()) => q.refetch(),
                    Err(e) => toast_err(&e.message()),
                }
            }
        });
    };
    view! {
        <SysCard title="Bandcamp account" icon="key"
            hint="Needed only to harvest your own collection and wishlist. Public artist, label, tag and discover pages work without it. Bandcamp has no login API (it is behind reCAPTCHA), so the cookie is pasted manually.">
            <Show when=move || q.data.get().map(|s| s.configured).unwrap_or(false)>
                {move || q.data.get().map(|s| {
                    let valid = s.valid;
                    view! {
                        <div class="cookie-state">
                            <Icon name="lock" />
                            <span>{s.username.clone().unwrap_or_else(|| "Cookie stored".into())}</span>
                            <span class="mono faint">{s.fingerprint.clone().unwrap_or_default()}</span>
                            {match valid {
                                Some(true) => view! { <StatusBadge tone=Tone::Ok label="Valid" /> }.into_any(),
                                Some(false) => view! { <StatusBadge tone=Tone::Danger label="Invalid" /> }.into_any(),
                                None => view! { <StatusBadge tone=Tone::Neutral label="Not checked" /> }.into_any(),
                            }}
                            <span class="spacer"></span>
                            <Button size=Size::Sm variant=Variant::Danger icon="trash" on_click=forget.clone()>"Forget"</Button>
                        </div>
                        {(!s.detail.is_empty()).then(|| view! { <p class="sys-hint faint">{s.detail.clone()}</p> })}
                    }
                })}
            </Show>
            <Show when=move || !q.data.get().map(|s| s.configured).unwrap_or(false)>
                <div class="row gap">
                    <input class="input mono grow" type="password" autocomplete="off" spellcheck="false" aria-label="Bandcamp cookie"
                        placeholder="identity=...  (paste the whole Cookie header)"
                        prop:value=move || value.get() on:input=move |ev| value.set(event_target_value(&ev))
                        on:keydown={ let s = save2.clone(); move |ev| if ev.key() == "Enter" { s() } } />
                    <Button variant=Variant::Primary busy=busy disabled=Signal::derive(move || value.get().trim().is_empty()) on_click=move |_| save()>
                        {move || if busy.get() { "Verifying" } else { "Save" }}
                    </Button>
                </div>
                <Notice text=error />
            </Show>
            <p class="sys-hint faint">"Stored locally in the system keyring or a file with tightened permissions, never logged, never returned by the API, and only ever sent to bandcamp.com."</p>
        </SysCard>
    }
}

#[component]
fn Fans() -> impl IntoView {
    let fans = qh(use_query::<Vec<FanOut>>(|| Some(QuerySpec::new("/fans", &["fan"]))));
    let labels = qh(use_query::<LabelResolveStatus>(|| Some(QuerySpec::new("/harvest/labels", &["harvest.labels"]))));
    use_topic::<LabelResolveStatus>("harvest.labels", move |s| {
        crate::data::cache::patch::<LabelResolveStatus>("/harvest/labels", |x| *x = s);
    });
    let running = Signal::derive(move || labels.data.get().map(|l| l.running).unwrap_or(false));
    let start = move |_| {
        spawn_local(async move {
            match api::post::<_, LabelResolveStatus>("/harvest/labels/resolve", &serde_json::json!({})).await {
                Ok(s) => crate::data::cache::patch::<LabelResolveStatus>("/harvest/labels", |x| *x = s),
                Err(e) => toast_err(&e.message()),
            }
        });
    };
    view! {
        <SysCard title="Fans" icon="heart"
            hint="Your own wishlist and collection, and other people's, live on the Fans page: follow one by link or username, walk it, listen through it and queue what you want. Walking your wishlist recognises what you already have and queues only the rest. A public wishlist needs no cookie.">
            <Show when=move || fans.data.get().map(|f| !f.is_empty()).unwrap_or(false)>
                <ul class="fan-list">
                    <For each=move || fans.data.get().map(|f| (*f).clone()).unwrap_or_default() key=|f| (f.id, f.items, f.downloaded, f.walk.as_ref().map(|w| w.running)) let:fan>
                        <li>
                            <Icon name="heart" />
                            <a href=format!("/fans/{}", fan.id)>{fan.display_name.clone().unwrap_or_else(|| fan.username.clone())}</a>
                            {fan.is_self.then(|| view! { <Badge tone=Tone::Accent>"mine"</Badge> })}
                            <span class="mono faint">{format!("{} items", format_count(fan.items))}</span>
                            {(fan.downloaded > 0).then(|| view! { <span class="mono faint">{format!("{} on their shelf", format_count(fan.downloaded))}</span> })}
                            {fan.walk.as_ref().filter(|w| w.running).map(|_| view! { <StatusBadge tone=Tone::Info label="Walking" /> })}
                        </li>
                    </For>
                </ul>
            </Show>
            <div class="row gap wrap">
                <Button icon="tag" busy=running on_click=start
                    title="A wishlist walk only stores a label when Bandcamp's API states one. This opens one album page per label host and files your releases under what those pages say.">
                    {move || if running.get() { "Resolving labels" } else { "Resolve labels" }}
                </Button>
                <a class="btn btn-ghost" href="/fans"><Icon name="users" />"Open Fans"</a>
            </div>
            {move || labels.data.get().filter(|l| l.phase != "idle" && !l.phase.is_empty()).map(|l| {
                let v = l.total.filter(|t| *t > 0).map(|t| l.seen as f64 / t as f64);
                view! {
                    <div class="label-resolve" role="status">
                        {match l.phase.as_str() {
                            "running" => view! {
                                <Meter value=v label="Label resolution" />
                                <span class="mono faint">{format!("Checking label pages {}{}{}", format_count(l.seen),
                                    l.total.map(|t| format!(" / {}", format_count(t))).unwrap_or_default(),
                                    if l.labelled > 0 { format!(", {} items labelled", format_count(l.labelled)) } else { String::new() })}</span>
                            }.into_any(),
                            "done" => view! {
                                <StatusBadge tone=Tone::Ok label=format!("Found {} labels, labelled {} wishlist items, filed {} releases", format_count(l.resolved), format_count(l.labelled), format_count(l.filed)) />
                            }.into_any(),
                            "failed" => view! { <StatusBadge tone=Tone::Danger label=l.error.clone().unwrap_or_else(|| "Label resolution failed".into()) /> }.into_any(),
                            other => view! { <span class="faint">{other.to_string()}</span> }.into_any(),
                        }}
                    </div>
                }
            })}
        </SysCard>
    }
}
