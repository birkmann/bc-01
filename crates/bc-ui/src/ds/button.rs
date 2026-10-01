use leptos::prelude::*;

use super::icon::Icon;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Variant {
    Primary,
    #[default]
    Outline,
    Ghost,
    Danger,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Size {
    Sm,
    #[default]
    Md,
    Lg,
}

/// Button / icon button. With no children and an `icon` it renders square.
#[component]
pub fn Button(
    #[prop(optional)] variant: Variant,
    #[prop(optional)] size: Size,
    #[prop(optional, into)] icon: MaybeProp<String>,
    #[prop(optional, into)] on_click: Option<Callback<()>>,
    #[prop(optional, into)] disabled: MaybeProp<bool>,
    #[prop(optional, into)] busy: MaybeProp<bool>,
    #[prop(optional, into)] pressed: MaybeProp<bool>,
    #[prop(optional, into)] title: Option<String>,
    #[prop(optional, into)] class: String,
    #[prop(optional, into)] kind: Option<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let has_children = children.is_some();
    let cls = move || {
        let v = match variant {
            Variant::Primary => "btn-primary",
            Variant::Outline => "btn-outline",
            Variant::Ghost => "btn-ghost",
            Variant::Danger => "btn-danger",
        };
        let s = match size {
            Size::Sm => " btn-sm",
            Size::Md => "",
            Size::Lg => " btn-lg",
        };
        let sq = if !has_children { " btn-icon" } else { "" };
        let busy = if busy.get().unwrap_or(false) { " is-busy" } else { "" };
        format!("btn {v}{s}{sq}{busy} {class}")
    };
    let label = if has_children { None } else { title.clone() };
    view! {
        <button
            class=cls
            type=kind.unwrap_or_else(|| "button".into())
            title=title
            aria-label=label
            aria-pressed=move || pressed.get().map(|p| if p { "true" } else { "false" })
            disabled=move || disabled.get().unwrap_or(false) || busy.get().unwrap_or(false)
            on:click=move |ev| {
                ev.stop_propagation();
                if let Some(cb) = on_click { cb.run(()); }
            }
        >
            {move || icon.get().map(|i| view! { <Icon name=i /> })}
            {children.map(|c| c())}
        </button>
    }
}
