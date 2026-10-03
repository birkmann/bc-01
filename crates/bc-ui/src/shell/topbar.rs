use leptos::prelude::*;
use leptos_router::hooks::{use_location, use_navigate, use_query_map};

use bc_types::jobs::{KIND_ANALYZE, KIND_DOWNLOAD};

use crate::app::use_app;
use crate::data::{use_jobs, ws_connected};
use crate::ds::{Button, Icon, SearchInput, ToastCentre, Variant, use_debounced};
use crate::history::{self, Dir};

/// One status light: a dot and a small-caps label, with the detail in the tooltip.
#[component]
fn Led(label: &'static str, #[prop(into)] state: Signal<&'static str>, #[prop(into)] tip: Signal<String>) -> impl IntoView {
    view! {
        <span class=move || format!("hb-led {}", state.get()) title=move || tip.get()>
            <i></i><b>{label}</b>
        </span>
    }
}

/// Back / forward through the window's history, as a browser toolbar would (the app window
/// has none). Alt+← / Alt+→ and the mouse's side buttons do the same natively.
#[component]
fn HistoryButton(dir: Dir) -> impl IntoView {
    let (icon, verb, keys) = match dir {
        Dir::Back => ("arrow-left", "Back", "Alt+\u{2190}"),
        Dir::Forward => ("arrow-right", "Forward", "Alt+\u{2192}"),
    };
    let label = Memo::new(move |_| history::label(dir));
    let title = move || match label.get() {
        Some(l) if !l.is_empty() => format!("{verb} to {l} ({keys})"),
        _ => format!("{verb} ({keys})"),
    };
    view! {
        <button type="button" class="btn btn-ghost btn-icon btn-sm" aria-label=verb title=title
            disabled=move || label.get().is_none() on:click=move |_| history::go(dir)>
            <Icon name=icon />
        </button>
    }
}

/// `hh:mm`, ticking on the minute.
#[component]
fn Clock() -> impl IntoView {
    let now = || {
        let d = js_sys::Date::new_0();
        format!("{:02}:{:02}", d.get_hours(), d.get_minutes())
    };
    let text = RwSignal::new(now());
    let iv = send_wrapper::SendWrapper::new(gloo_timers::callback::Interval::new(5_000, move || {
        let t = now();
        if text.get_untracked() != t {
            text.set(t);
        }
    }));
    on_cleanup(move || drop(iv));
    view! { <span class="hb-clock" title="Local time">{move || text.get()}</span> }
}

/// App header, full width above sidebar and content: an empty cell as wide as the sidebar
/// column, status lights, library search (debounced 120 ms), clock, palette and settings.
#[component]
pub fn TopBar() -> impl IntoView {
    let app = use_app();
    let loc = use_location();
    let navigate = use_navigate();
    let query = use_query_map();
    let input = NodeRef::<leptos::html::Input>::new();
    let text = RwSignal::new(query.get_untracked().get("q").unwrap_or_default());
    let debounced = use_debounced(text, 120);
    // Explore carries its own search; this one stands down there.
    let owns_search = Memo::new(move |_| loc.pathname.get() == "/explore");
    let keeps_tags = move || matches!(loc.pathname.get_untracked().as_str(), "/tracks" | "/loved" | "/albums");
    Effect::new(move |prev: Option<String>| {
        let q = debounced.get();
        if let Some(p) = &prev {
            if *p != q {
                let mut url = String::from("/tracks");
                let mut parts: Vec<String> = vec![];
                if keeps_tags() {
                    for t in query.get_untracked().get_all("tag").unwrap_or_default() {
                        parts.push(format!("tag={}", crate::util::enc(&t)));
                    }
                }
                if !q.trim().is_empty() {
                    parts.push(format!("q={}", crate::util::enc(q.trim())));
                }
                if !parts.is_empty() {
                    url.push('?');
                    url.push_str(&parts.join("&"));
                }
                navigate(&url, leptos_router::NavigateOptions { replace: true, ..Default::default() });
            }
        }
        q
    });
    // `/` focuses the search.
    Effect::new(move |prev: Option<()>| {
        app.focus_search.track();
        if prev.is_none() {
            return;
        }
        if let Some(el) = input.get() {
            let _ = el.focus();
            el.select();
        }
    });
    let connected = ws_connected();
    let jobs = use_jobs();
    let live = Signal::derive(move || if connected.get() { "on" } else { "warn" });
    let live_tip = Signal::derive(move || {
        if connected.get() { "Live: connected to the bc server".to_string() } else { "Offline: reconnecting to the bc server".to_string() }
    });
    let dl = Memo::new(move |_| jobs.active_count(KIND_DOWNLOAD));
    let an = Memo::new(move |_| jobs.active_count(KIND_ANALYZE));
    let busy = |n: Memo<usize>| Signal::derive(move || if n.get() > 0 { "busy" } else { "off" });
    let count_tip = |n: Memo<usize>, what: &'static str| {
        Signal::derive(move || match n.get() {
            0 => format!("No {what} running"),
            k => format!("{k} {what} job{} running", if k == 1 { "" } else { "s" }),
        })
    };
    view! {
        <header class=move || if app.nav_collapsed.get() { "topbar rail" } else { "topbar" }>
            <Button variant=Variant::Ghost icon="menu" title="Open navigation" class="only-mobile" on_click=move |_| app.nav_open.set(true) />
            <div class="hb-gutter"></div>
            <span class="hb-div"></span>
            <div class="hb-hist">
                <HistoryButton dir=Dir::Back />
                <HistoryButton dir=Dir::Forward />
            </div>
            <div class="hb-leds">
                <Led label="LIVE" state=live tip=live_tip />
                <Led label="DL" state=busy(dl) tip=count_tip(dl, "download") />
                <Led label="AN" state=busy(an) tip=count_tip(an, "analysis") />
            </div>
            <Show when=move || !owns_search.get() fallback=|| view! { <div class="search"></div> }>
                <div class="search"><SearchInput value=text node_ref=input placeholder="Search tracks, artists, albums, tags..." /></div>
            </Show>
            <Clock />
            <span class="hb-div"></span>
            <Button variant=Variant::Ghost icon="sparkles" title="Command palette (Ctrl+K)" on_click=move |_| app.palette_open.set(true) />
            <a class="btn btn-ghost btn-icon" href="/settings" title="Settings" aria-label="Settings"><crate::ds::Icon name="sliders" /></a>
            <ToastCentre />
        </header>
    }
}
