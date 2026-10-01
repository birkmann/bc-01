//! Set status pill (draft / ready / performed / archived). Status is icon + word, never colour alone.
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::ds::popover::Rect;
use crate::ds::{Icon, MenuCtx, MenuEntry, MenuItem};

pub const STATUSES: [&str; 4] = ["draft", "ready", "performed", "archived"];

fn tone(status: &str) -> (&'static str, &'static str) {
    match status {
        "ready" => ("spill ok", "check-circle"),
        "performed" => ("spill accent", "star"),
        "archived" => ("spill idle", "folder"),
        _ => ("spill", "edit"),
    }
}

#[component]
pub fn StatusPill(#[prop(into)] status: Signal<String>, #[prop(optional, into)] on_change: Option<Callback<String>>) -> impl IntoView {
    let menu = expect_context::<MenuCtx>();
    let btn = NodeRef::<leptos::html::Button>::new();
    let inner = move || {
        let s = status.get();
        let (cls, icon) = tone(&s);
        (cls, icon, s)
    };
    match on_change {
        None => view! { <span class=move || inner().0><Icon name=move || inner().1.to_string() size=11 />{move || inner().2}</span> }.into_any(),
        Some(cb) => view! {
            <button node_ref=btn type="button" class=move || format!("{} btn-like", inner().0) title="Change status" aria-haspopup="menu"
                on:click=move |ev| {
                    ev.stop_propagation();
                    let Some(el) = btn.get_untracked() else { return };
                    let cur = status.get_untracked();
                    let entries: Vec<MenuEntry> = STATUSES.iter().map(|s| {
                        let s = s.to_string();
                        MenuItem::new(s.clone()).checked(s == cur).on(move || cb.run(s.clone())).into()
                    }).collect();
                    menu.open(Rect::of(el.unchecked_ref()), entries);
                }>
                <Icon name=move || inner().1.to_string() size=11 />{move || inner().2}<Icon name="chevron-down" size=10 />
            </button>
        }.into_any(),
    }
}
