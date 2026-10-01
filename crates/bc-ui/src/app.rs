//! App root: contexts, router, global hosts. Routes mirror the legacy app (PLAN §10.2).
use leptos::prelude::*;
use leptos_router::components::{FlatRoutes, Redirect, Route, Router};
use leptos_router::path;

use crate::ds::{ConfirmHost, MenuHost, ToastHost, provide_menu};
use crate::pages::*;
use crate::shell::Shell;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panel {
    Plan,
    Similar,
}

/// UI-wide signals shared by shell, palette and shortcuts.
#[derive(Clone, Copy)]
pub struct AppCtx {
    pub nav_open: RwSignal<bool>,
    pub nav_collapsed: RwSignal<bool>,
    pub panel: RwSignal<Option<Panel>>,
    pub palette_open: RwSignal<bool>,
    pub deck_open: RwSignal<bool>,
    pub queue_open: RwSignal<bool>,
    /// Set by the `/` shortcut; the top-bar search focuses on change.
    pub focus_search: RwSignal<u64>,
}

impl AppCtx {
    pub fn toggle_panel(&self, p: Panel) {
        self.panel.update(|cur| *cur = if *cur == Some(p) { None } else { Some(p) });
    }
}

thread_local! {
    static APP: std::cell::Cell<Option<AppCtx>> = const { std::cell::Cell::new(None) };
}

/// UI-wide signals; usable inside async tasks too (session-wide fallback copy).
pub fn use_app() -> AppCtx {
    use_context::<AppCtx>().or_else(|| APP.with(|c| c.get())).expect("AppCtx not provided")
}

#[component]
pub fn App() -> impl IntoView {
    let collapsed = crate::util::ls_get("bc:nav-collapsed:v1").map(|v| v == "1").unwrap_or(false);
    let ctx = AppCtx {
        nav_open: RwSignal::new(false),
        nav_collapsed: RwSignal::new(collapsed),
        panel: RwSignal::new(None),
        palette_open: RwSignal::new(false),
        deck_open: RwSignal::new(false),
        queue_open: RwSignal::new(false),
        focus_search: RwSignal::new(0),
    };
    provide_context(ctx);
    APP.with(|c| c.set(Some(ctx)));
    Effect::new(move |_| crate::util::ls_set("bc:nav-collapsed:v1", if ctx.nav_collapsed.get() { "1" } else { "0" }));
    provide_menu();
    crate::data::ws::start();
    crate::theme::provide_theme();
    crate::prefs::provide_prefs();
    crate::player::provide_player();
    crate::player::accent::install();

    view! {
        <Router>
            <Shell>
                <FlatRoutes transition=true fallback=|| view! { <Redirect path="/tracks" /> }>
                    <Route path=path!("/") view=HomePage />
                    <Route path=path!("/tracks") view=TracksPage />
                    <Route path=path!("/loved") view=LovedPage />
                    <Route path=path!("/albums") view=AlbumsPage />
                    <Route path=path!("/albums/:id") view=AlbumDetailPage />
                    <Route path=path!("/artists") view=ArtistsPage />
                    <Route path=path!("/artists/:id") view=ArtistDetailPage />
                    <Route path=path!("/labels") view=LabelsPage />
                    <Route path=path!("/labels/:id") view=LabelDetailPage />
                    <Route path=path!("/tags") view=TagsPage />
                    <Route path=path!("/explore") view=ExplorePage />
                    <Route path=path!("/explore/band") view=ExploreBandPage />
                    <Route path=path!("/explore/release") view=ExploreReleasePage />
                    <Route path=path!("/feed") view=FeedPage />
                    <Route path=path!("/fans") view=FansPage />
                    <Route path=path!("/fans/:id") view=FansPage />
                    <Route path=path!("/wishlist") view=|| view! { <Redirect path="/fans" /> } />
                    <Route path=path!("/harvest") view=HarvestPage />
                    <Route path=path!("/downloads") view=DownloadsPage />
                    <Route path=path!("/tracklists") view=TracklistsPage />
                    <Route path=path!("/playlists") view=PlaylistsPage />
                    <Route path=path!("/playlists/:id") view=PlaylistDetailPage />
                    <Route path=path!("/sets") view=SetsPage />
                    <Route path=path!("/sets/:id") view=SetDetailPage />
                    <Route path=path!("/analysis") view=AnalysisPage />
                    <Route path=path!("/cleanup") view=CleanupPage />
                    <Route path=path!("/settings") view=SettingsPage />
                </FlatRoutes>
            </Shell>
        </Router>
        <MenuHost />
        <ConfirmHost />
        <ToastHost />
    }
}
