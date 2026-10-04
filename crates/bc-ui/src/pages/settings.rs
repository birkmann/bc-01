//! Settings: Appearance (theme editor), Audio, Library, Downloads and Bandcamp,
//! LAN and pairing, Import. Sections live in `settings/`; the active tab is part of
//! the URL (`?tab=audio`).
use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

use crate::ds::tabs::TabDef;
use crate::ds::{PageHeader, Tabs};

mod appearance;
mod audio;
mod bandcamp;
mod common;
mod import;
pub(crate) mod lan;
mod library;
pub(crate) mod logic;
mod prefs;

pub use common::{QH, QueryError, SysCard, qh};

const TABS: [(&str, &str); 6] = [
    ("appearance", "Appearance"),
    ("audio", "Audio"),
    ("library", "Library"),
    ("downloads", "Downloads"),
    ("lan", "LAN"),
    ("import", "Import"),
];

#[component]
pub fn SettingsPage() -> impl IntoView {
    let query = use_query_map();
    let navigate = use_navigate();
    let initial = query.get_untracked().get("tab").filter(|t| TABS.iter().any(|(id, _)| id == t)).unwrap_or_else(|| "appearance".into());
    let tab = RwSignal::new(initial);
    // Keep the URL in step with the tab (replace: tabs are not history entries).
    Effect::new(move |prev: Option<String>| {
        let t = tab.get();
        if prev.is_some() && prev.as_deref() != Some(t.as_str()) {
            navigate(&format!("/settings?tab={t}"), NavigateOptions { replace: true, ..Default::default() });
        }
        t
    });
    // Back/forward or a link changing ?tab=
    Effect::new(move |_| {
        if let Some(t) = query.get().get("tab").filter(|t| TABS.iter().any(|(id, _)| id == t)) {
            if tab.get_untracked() != t {
                tab.set(t);
            }
        }
    });
    let tabs = Signal::derive(|| TABS.iter().map(|(id, label)| TabDef::new(id, label)).collect::<Vec<_>>());
    let prefs = prefs::use_prefs();
    view! {
        <div class="page">
            <PageHeader title="Settings" subtitle="Appearance, audio, library and devices" />
            <div class="sys-tabs"><Tabs tabs=tabs value=tab /></div>
            <div class="page-scroll">
                <div class="sys-page" role="tabpanel">
                    {move || match tab.get().as_str() {
                        "audio" => view! { <audio::AudioSection prefs=prefs /> }.into_any(),
                        "library" => view! { <library::LibrarySection /> <crate::pages::BlacklistPanel /> }.into_any(),
                        "downloads" => view! { <bandcamp::BandcampSection /> }.into_any(),
                        "lan" => view! { <lan::LanSection /> }.into_any(),
                        "import" => view! { <import::ImportSection /> }.into_any(),
                        _ => view! { <appearance::AppearanceSection prefs=prefs /> }.into_any(),
                    }}
                </div>
            </div>
        </div>
    }
}
