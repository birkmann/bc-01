//! App shell: full-width header, sidebar / rail / drawer, route host, right panels, player bar.
mod sidebar;
mod topbar;

use leptos::prelude::*;
use leptos_router::hooks::use_location;

use crate::app::{Panel, use_app};
use crate::data::ws_connected;
use crate::ds::{Button, Icon, Variant};
use crate::player::bar::PlayerBar;
use crate::player::plan::PlanPanel;
use crate::player::similar::SimilarPanel;
pub use sidebar::{NavLinks, Sidebar};
pub use topbar::TopBar;

#[component]
pub fn Shell(children: Children) -> impl IntoView {
    let app = use_app();
    crate::data::provide_jobs();
    crate::shortcuts::install();
    let loc = use_location();
    // Any navigation closes the drawer.
    Effect::new(move |_| {
        loc.pathname.track();
        app.nav_open.set(false);
    });
    let connected = ws_connected();
    // Debounce the offline banner so a quick reconnect never flashes it.
    let show_offline = RwSignal::new(false);
    Effect::new(move |_| {
        if connected.get() {
            show_offline.set(false);
        } else {
            crate::util::after(2500, move || {
                if !ws_connected().get_untracked() {
                    show_offline.set(true);
                }
            });
        }
    });
    view! {
        <div class="shell">
            <TopBar />
            <div class="shell-body">
                <Sidebar />
                <main class="shell-main">
                    <Show when=move || show_offline.get()>
                        <div class="offline-banner" role="status"><Icon name="wifi-off" />
                            <span>"Offline - reconnecting. Showing the last data."</span></div>
                    </Show>
                    <div class="route-host">{children()}</div>
                </main>
                <aside class=move || if app.panel.get() == Some(Panel::Plan) { "right-panel open" } else { "right-panel" } aria-label="Planner">
                    <Show when=move || app.panel.get() == Some(Panel::Plan)>
                        <div class="rp-head"><span class="grow">"Planner"</span>
                            <Button variant=Variant::Ghost icon="x" title="Close (q)" on_click=move |_| app.panel.set(None) /></div>
                        <PlanPanel />
                    </Show>
                </aside>
                <aside class=move || if app.panel.get() == Some(Panel::Similar) { "right-panel open" } else { "right-panel" } aria-label="Similar">
                    <Show when=move || app.panel.get() == Some(Panel::Similar)>
                        <div class="rp-head"><span class="grow">"Similar"</span>
                            <Button variant=Variant::Ghost icon="x" title="Close (s)" on_click=move |_| app.panel.set(None) /></div>
                        <SimilarPanel />
                    </Show>
                </aside>
            </div>
            <PlayerBar />
        </div>
        <crate::palette::CommandPalette />
        <crate::player::deck::DeckView />
    }
}
