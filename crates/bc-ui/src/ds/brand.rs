//! The bc mark (a record: ink ring, accent label) and wordmark. Same geometry as
//! `assets/brand/bc-mark.svg`, drawn heavier at small sizes so it holds up in the header.
use leptos::prelude::*;

#[component]
pub fn BrandMark(#[prop(default = 20)] size: u32) -> impl IntoView {
    let (stroke, disc) = if size <= 24 { ("11", "17") } else { ("6", "15") };
    view! {
        <svg class="brand-mark" viewBox="0 0 100 100" width=size height=size aria-hidden="true">
            <circle class="ring" cx="50" cy="50" r="36" fill="none" stroke-width=stroke />
            <circle class="disc" cx="50" cy="50" r=disc />
        </svg>
    }
}

/// Mark + "bc", linking home.
#[component]
pub fn BrandLockup(#[prop(optional, into)] class: Option<String>) -> impl IntoView {
    view! {
        <a href="/" class=format!("brand {}", class.unwrap_or_default()) title="bc - home">
            <BrandMark size=20 /><span class="brand-word">"bc"</span>
        </a>
    }
}
