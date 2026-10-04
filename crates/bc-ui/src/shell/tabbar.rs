use bc_types::jobs::{KIND_ANALYZE, KIND_DOWNLOAD};
use leptos::prelude::*;
use leptos_router::hooks::use_location;

use crate::app::use_app;
use crate::data::use_jobs;
use crate::ds::Icon;
use crate::nav::{self, TABS};

/// Phone navigation, pinned under the player: the main library routes as tabs, and "More",
/// which opens the drawer with the full nav list. Hidden above the phone breakpoint (CSS).
#[component]
pub fn TabBar() -> impl IntoView {
    let app = use_app();
    let loc = use_location();
    let jobs = use_jobs();
    // Stands in for the header's activity lights, which phones don't show.
    let busy = move || jobs.active_count(KIND_DOWNLOAD) + jobs.active_count(KIND_ANALYZE) > 0;
    view! {
        <nav class="tabbar" aria-label="Primary">
            {TABS.iter().map(|t| {
                let href = t.to;
                view! {
                    <a href=href class="tab-item" aria-current=move || nav::is_active(href, &loc.pathname.get()).then_some("page")>
                        <Icon name=t.icon />
                        <span>{t.label}</span>
                    </a>
                }
            }).collect_view()}
            <button type="button" class="tab-item" aria-haspopup="dialog"
                aria-expanded=move || app.nav_open.get().to_string()
                aria-current=move || (!nav::in_tabs(&loc.pathname.get())).then_some("page")
                on:click=move |_| app.nav_open.set(true)>
                <Icon name="more" />
                <span>"More"</span>
                <Show when=busy><i class="tab-dot" aria-hidden="true"></i></Show>
            </button>
        </nav>
    }
}
