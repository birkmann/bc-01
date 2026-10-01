//! Badge, StatusBadge, Meter, Skeleton, EmptyState, PageHeader, Field, Switch, SearchInput.
use leptos::prelude::*;

use super::icon::Icon;
use super::menu::{MenuButton, MenuEntry};

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Tone {
    #[default]
    Neutral,
    Ok,
    Warn,
    Danger,
    Info,
    Accent,
}
impl Tone {
    pub fn badge_class(self) -> &'static str {
        match self {
            Tone::Neutral => "badge",
            Tone::Ok => "badge badge-ok",
            Tone::Warn => "badge badge-warn",
            Tone::Danger => "badge badge-danger",
            Tone::Info => "badge badge-info",
            Tone::Accent => "badge badge-accent",
        }
    }
    pub fn status_class(self) -> &'static str {
        match self {
            Tone::Neutral => "status idle",
            Tone::Ok => "status ok",
            Tone::Warn => "status warn",
            Tone::Danger => "status danger",
            Tone::Info => "status info",
            Tone::Accent => "status ok",
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            Tone::Neutral => "clock",
            Tone::Ok | Tone::Accent => "check-circle",
            Tone::Warn => "alert",
            Tone::Danger => "x-circle",
            Tone::Info => "info",
        }
    }
}

#[component]
pub fn Badge(#[prop(optional)] tone: Tone, #[prop(optional, into)] icon: Option<String>, children: Children) -> impl IntoView {
    view! { <span class=tone.badge_class()>{icon.map(|i| view! { <Icon name=i /> })}{children()}</span> }
}

/// Status colours are always paired with an icon and a label (never colour alone).
#[component]
pub fn StatusBadge(tone: Tone, #[prop(into)] label: Signal<String>) -> impl IntoView {
    view! { <span class=tone.status_class()><Icon name=tone.icon() /><span>{move || label.get()}</span></span> }
}

/// Progress meter. `value` in 0..=1; `None` = indeterminate.
#[component]
pub fn Meter(#[prop(into)] value: Signal<Option<f64>>, #[prop(optional)] tone: Tone, #[prop(optional, into)] label: Option<String>) -> impl IntoView {
    let cls = move || {
        let t = match tone {
            Tone::Warn => " warn",
            Tone::Danger => " danger",
            _ => "",
        };
        format!("meter{t}{}", if value.get().is_none() { " indeterminate" } else { "" })
    };
    view! {
        <div class=cls role="progressbar" aria-label=label
            aria-valuemin="0" aria-valuemax="100" aria-valuenow=move || value.get().map(|v| (v * 100.0).round().to_string())>
            <i style=move || format!("width:{:.1}%", value.get().unwrap_or(0.0).clamp(0.0, 1.0) * 100.0)></i>
        </div>
    }
}

#[component]
pub fn Skeleton(#[prop(optional, into)] width: Option<String>, #[prop(optional, into)] height: Option<String>, #[prop(optional, into)] class: String) -> impl IntoView {
    let style = format!("width:{};height:{}", width.unwrap_or_else(|| "100%".into()), height.unwrap_or_else(|| "14px".into()));
    view! { <div class=format!("skeleton {class}") style=style aria-hidden="true"></div> }
}

#[component]
pub fn EmptyState(
    #[prop(into)] title: String,
    #[prop(optional, into)] hint: Option<String>,
    #[prop(optional, into)] icon: Option<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    view! {
        <div class="empty">
            <Icon name=icon.unwrap_or_else(|| "music".into()) />
            <h3>{title}</h3>
            {hint.map(|h| view! { <p>{h}</p> })}
            {children.map(|c| view! { <div class="row" style="margin-top:8px">{c()}</div> })}
        </div>
    }
}

/// Error panel for failed queries and route-level failures.
#[component]
pub fn ErrorPanel(#[prop(into)] message: Signal<String>, #[prop(optional, into)] on_retry: Option<Callback<()>>) -> impl IntoView {
    view! {
        <div class="empty">
            <Icon name="alert-circle" />
            <h3>"Something went wrong"</h3>
            <p>{move || message.get()}</p>
            {on_retry.map(|cb| view! { <button class="btn btn-outline" on:click=move |_| cb.run(())>"Retry"</button> })}
        </div>
    }
}

/// Page header: title, optional subtitle, primary actions, and overflow actions in a "..." menu.
#[component]
pub fn PageHeader(
    #[prop(into)] title: Signal<String>,
    #[prop(optional, into)] subtitle: MaybeProp<String>,
    #[prop(optional)] actions: Option<ChildrenFn>,
    #[prop(optional, into)] overflow: Option<Callback<(), Vec<MenuEntry>>>,
    #[prop(optional)] leading: Option<ChildrenFn>,
) -> impl IntoView {
    view! {
        <header class="page-header">
            {leading.map(|l| l())}
            <div class="titles">
                <h1 class="truncate">{move || title.get()}</h1>
                {move || subtitle.get().filter(|s| !s.is_empty()).map(|s| view! { <div class="sub truncate">{s}</div> })}
            </div>
            <div class="actions">
                {actions.map(|a| a())}
                {overflow.map(|o| view! { <MenuButton entries=o title="More" /> })}
            </div>
        </header>
    }
}

#[component]
pub fn Switch(value: RwSignal<bool>, #[prop(optional, into)] label: Option<String>, #[prop(optional, into)] on_change: Option<Callback<bool>>) -> impl IntoView {
    view! {
        <button type="button" role="switch" class="switch" aria-label=label aria-checked=move || value.get().to_string()
            on:click=move |_| { value.update(|v| *v = !*v); if let Some(cb) = on_change { cb.run(value.get_untracked()); } }></button>
    }
}

/// Search box with a clear button; `value` updates on every keystroke (debounce at the call site with `use_debounced`).
#[component]
pub fn SearchInput(
    value: RwSignal<String>,
    #[prop(optional, into)] placeholder: Option<String>,
    #[prop(optional)] node_ref: Option<NodeRef<leptos::html::Input>>,
    #[prop(optional, into)] class: String,
) -> impl IntoView {
    let nr = node_ref.unwrap_or_default();
    view! {
        <div class=format!("input-wrap {class}")>
            <Icon name="search" />
            <input node_ref=nr class="input" type="search" placeholder=placeholder.unwrap_or_else(|| "Search".into()) autocomplete="off"
                prop:value=move || value.get()
                on:input=move |ev| value.set(event_target_value(&ev))
                on:keydown=move |ev| if ev.key() == "Escape" { value.set(String::new()); } />
            {move || (!value.get().is_empty()).then(|| view! {
                <button type="button" class="btn btn-ghost btn-sm btn-icon clear" aria-label="Clear" on:click=move |_| value.set(String::new())><Icon name="x" /></button>
            })}
        </div>
    }
}

/// A signal that follows `source` after `ms` of quiet (search debounce: 120 ms),
/// so the UI keeps showing previous results while the user types.
pub fn use_debounced<T: Clone + Send + Sync + PartialEq + 'static>(source: RwSignal<T>, ms: i32) -> RwSignal<T> {
    use wasm_bindgen::JsCast;
    let out = RwSignal::new(source.get_untracked());
    let timer = StoredValue::new(None::<i32>);
    Effect::new(move |_| {
        let v = source.get();
        let w = crate::util::window();
        if let Some(id) = timer.get_value() {
            w.clear_timeout_with_handle(id);
        }
        if out.get_untracked() == v {
            return;
        }
        let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
            let _ = out.try_set(v);
        });
        timer.set_value(w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), ms).ok());
    });
    out
}

#[component]
pub fn Field(#[prop(into)] label: String, children: Children) -> impl IntoView {
    view! { <div class="field"><label>{label}</label>{children()}</div> }
}

/// Camelot keys render as text, never colour-encoded.
#[component]
pub fn Camelot(#[prop(into)] key: Signal<Option<String>>) -> impl IntoView {
    view! { <span class="camelot">{move || key.get().unwrap_or_default()}</span> }
}

/// Keyboard hint chip.
#[component]
pub fn Kbd(children: Children) -> impl IntoView {
    view! { <span class="kbd">{children()}</span> }
}
