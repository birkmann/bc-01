//! Toast centre: transient toasts plus a history (bell) so nothing is lost.
use leptos::prelude::*;

use super::button::{Button, Variant};
use super::dialog::Sheet;
use super::icon::Icon;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Ok,
    Info,
    Warn,
    Danger,
}
impl Level {
    fn class(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Danger => "danger",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Level::Ok => "check-circle",
            Level::Info => "info",
            Level::Warn => "alert",
            Level::Danger => "alert-circle",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub id: u64,
    pub level: Level,
    pub text: String,
    pub detail: Option<String>,
    pub live: bool,
}

#[derive(Clone, Copy)]
struct Centre {
    items: RwSignal<Vec<Toast>>,
    next: RwSignal<u64>,
}

thread_local! {
    static CENTRE: std::cell::Cell<Option<Centre>> = const { std::cell::Cell::new(None) };
}

fn centre() -> Centre {
    CENTRE.with(|c| match c.get() {
        Some(x) => x,
        None => {
            let x = Centre { items: crate::util::root_signal(vec![]), next: crate::util::root_signal(1u64) };
            c.set(Some(x));
            x
        }
    })
}

fn push(level: Level, text: &str, detail: Option<String>) {
    let c = centre();
    let id = c.next.get_untracked();
    c.next.set(id + 1);
    c.items.update(|v| {
        v.push(Toast { id, level, text: text.into(), detail, live: true });
        let n = v.len();
        if n > 60 {
            v.drain(0..n - 60);
        }
    });
    let ms = if level == Level::Danger { 8000 } else { 4500 };
    crate::util::after(ms, move || {
        centre().items.update(|v| {
            if let Some(t) = v.iter_mut().find(|t| t.id == id) {
                t.live = false;
            }
        });
    });
}

pub fn toast_ok(text: &str) {
    push(Level::Ok, text, None);
}
pub fn toast_info(text: &str) {
    push(Level::Info, text, None);
}
pub fn toast_warn(text: &str) {
    push(Level::Warn, text, None);
}
pub fn toast_err(text: &str) {
    push(Level::Danger, text, None);
}

#[component]
pub fn ToastHost() -> impl IntoView {
    let c = centre();
    let live = move || -> Vec<Toast> { c.items.get().into_iter().filter(|t| t.live).collect() };
    view! {
        <div class="toasts" aria-live="polite">
            <For each=live key=|t| t.id let:t>
                <div class=format!("toast {}", t.level.class()) role=if t.level == Level::Danger { "alert" } else { "status" }>
                    <Icon name=t.level.icon() />
                    <div class="t-body"><div class="t-title">{t.text.clone()}</div>{t.detail.clone().map(|d| view! { <div class="muted">{d}</div> })}</div>
                    <Button variant=Variant::Ghost size=super::button::Size::Sm icon="x" title="Dismiss"
                        on_click=move |_| centre().items.update(|v| { if let Some(x) = v.iter_mut().find(|x| x.id == t.id) { x.live = false; } }) />
                </div>
            </For>
        </div>
    }
}

/// Bell button with the notification history.
#[component]
pub fn ToastCentre() -> impl IntoView {
    let c = centre();
    let open = RwSignal::new(false);
    let unread = move || c.items.with(|v| v.iter().filter(|t| t.live).count());
    view! {
        <Button variant=Variant::Ghost icon="activity" title="Notifications" on_click=move |_| open.set(true) />
        <Sheet open=open title="Notifications">
            <div class="col">
                {move || { let v = c.items.get(); if v.is_empty() { view! { <p class="muted">"Nothing yet."</p> }.into_any() } else {
                    v.into_iter().rev().map(|t| view! {
                        <div class=format!("toast {}", t.level.class()) style="animation:none">
                            <Icon name=t.level.icon() /><div class="t-body">{t.text}</div></div> }).collect_view().into_any() } }}
                <Button variant=Variant::Outline on_click=move |_| c.items.set(vec![])>"Clear"</Button>
            </div>
        </Sheet>
        {move || { let _ = unread(); }}
    }
}
