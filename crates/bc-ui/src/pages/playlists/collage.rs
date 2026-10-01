//! Cover collage: one cover, or up to four in a 2x2 grid; placeholder when there are none.
use leptos::prelude::*;

use crate::ds::Icon;
use crate::widgets::common::Art;

#[component]
pub fn Collage(#[prop(into)] urls: Signal<Vec<String>>, #[prop(optional, into)] icon: Option<String>, #[prop(optional, into)] class: String) -> impl IntoView {
    let icon = icon.unwrap_or_else(|| "list-music".into());
    view! {
        <div class=move || format!("collage n{} {class}", urls.with(|u| u.len().clamp(1, 4)))>
            {move || {
                let u = urls.get();
                if u.is_empty() {
                    view! { <div class="collage-ph"><Icon name=icon.clone() /></div> }.into_any()
                } else {
                    let n = if u.len() >= 4 { 4 } else if u.len() >= 2 { 2 } else { 1 };
                    u.into_iter().take(n).map(|s| view! { <Art src=s class="collage-cell" /> }).collect_view().into_any()
                }
            }}
        </div>
    }
}
