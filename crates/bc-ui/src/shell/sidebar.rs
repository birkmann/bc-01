use bc_types::jobs::{KIND_ANALYZE, KIND_DOWNLOAD};
use bc_types::library::{LibraryStats, TagOut};
use leptos::prelude::*;
use leptos_router::hooks::use_location;

use crate::app::use_app;
use crate::data::{QuerySpec, use_jobs, use_query};
use crate::ds::{BrandLockup, Button, Icon, Variant, dyn_icon};
use crate::logic::format::{format_bytes, format_count, format_long_duration};
use crate::nav::{self, MORE, NavItem, PRIMARY, SETTINGS};
use crate::theme::use_theme;

fn none_str() -> Signal<Option<String>> {
    Signal::derive(|| None::<String>)
}

const MORE_KEY: &str = "bc:nav-more:v1";

#[component]
fn NavRow(item: NavItem, #[prop(into)] trailing: Signal<Option<String>>, #[prop(into)] active_dot: Signal<bool>) -> impl IntoView {
    let loc = use_location();
    let href = item.to;
    view! {
        <a href=href class="nav-item" title=item.label
            aria-current=move || nav::is_active(href, &loc.pathname.get()).then_some("page")>
            <Icon name=item.icon />
            <span class="nav-label">{item.label}</span>
            {move || trailing.get().map(|t| view! { <span class="nav-trail nav-label">{t}<span class="dot pulse"></span></span> })}
            <span class=move || if active_dot.get() { "nav-dot-rail on" } else { "nav-dot-rail" }></span>
        </a>
    }
}

/// The nav list shared by the desktop sidebar and the mobile drawer.
#[component]
pub fn NavLinks() -> impl IntoView {
    let jobs = use_jobs();
    let loc = use_location();
    let analysis = use_query::<bc_types::analysis::AnalysisStatus>(|| Some(QuerySpec::new("/analysis/status", &["analysis"])));
    // analysis.progress patches the cached status in place (no refetch).
    crate::data::ws::use_topic::<bc_types::analysis::AnalysisProgressEvent>("analysis.progress", move |e| {
        analysis.data.update(|d| {
            if let Some(a) = d {
                let mut s = (**a).clone();
                s.batch_total = e.total;
                s.batch_done = e.completed + e.failed;
                s.running_jobs = if e.status == "running" || e.status == "queued" { 1 } else { 0 };
                *d = Some(std::sync::Arc::new(s));
            }
        });
    });
    let more_open = RwSignal::new(crate::util::ls_get(MORE_KEY).map(|v| v == "1").unwrap_or(false));
    Effect::new(move |_| crate::util::ls_set(MORE_KEY, if more_open.get() { "1" } else { "0" }));
    // Open the fold when the current page lives inside it.
    Effect::new(move |_| {
        let p = loc.pathname.get();
        if MORE.iter().any(|(_, items)| items.iter().any(|i| nav::is_active(i.to, &p))) {
            more_open.set(true);
        }
    });
    let dl_trailing = Signal::derive(move || {
        let n = jobs.active_count(KIND_DOWNLOAD);
        (n > 0).then(|| match jobs.progress_of(KIND_DOWNLOAD) {
            Some(p) => format!("{}%", (p * 100.0).round() as i64),
            None => n.to_string(),
        })
    });
    let an_trailing = Signal::derive(move || {
        let a = analysis.data.get()?;
        if a.running_jobs > 0 && a.batch_total > 0 {
            Some(format!("{}%", (a.batch_done as f64 / a.batch_total as f64 * 100.0).round() as i64))
        } else if jobs.active_count(KIND_ANALYZE) > 0 {
            Some("..".into())
        } else {
            None
        }
    });
    view! {
        {PRIMARY.iter().map(|i| view! { <NavRow item=*i trailing=none_str() active_dot=Signal::derive(|| false) /> }).collect_view()}
        <button class="nav-item sb-more" type="button" aria-expanded=move || more_open.get().to_string() on:click=move |_| more_open.update(|v| *v = !*v)>
            <Icon name=dyn_icon(move || if more_open.get() { "chevron-down" } else { "chevron-right" }) />
            <span class="nav-label">"More"</span>
            <span class=move || if dl_trailing.get().is_some() || an_trailing.get().is_some() { "nav-dot-rail on" } else { "nav-dot-rail" }></span>
        </button>
        <Show when=move || more_open.get()>
            {MORE.iter().map(|(group, items)| view! {
                <div class="sb-section">{*group}</div>
                {items.iter().map(|i| {
                    let t = match i.to { "/downloads" => dl_trailing, "/analysis" => an_trailing, _ => none_str() };
                    view! { <NavRow item=*i trailing=t active_dot=Signal::derive(move || t.get().is_some()) /> }
                }).collect_view()}
            }).collect_view()}
        </Show>
        <div class="sb-section">"Tags"</div>
        <TopTags />
        <NavRow item=SETTINGS trailing=none_str() active_dot=Signal::derive(|| false) />
    }
}

#[component]
fn TopTags() -> impl IntoView {
    let tags = use_query::<Vec<TagOut>>(|| Some(QuerySpec::new("/tags?limit=14", &["tag"])));
    view! {
        <div class="sb-tags">
            {move || tags.data.get().map(|t| t.iter().take(14).map(|t| {
                let href = format!("/tracks?tag={}", crate::util::enc(&t.name));
                view! { <a class="chip" href=href style=format!("--tag-h:{}", crate::util::tag_hue(&t.name)) title=format!("{} tracks", t.track_count)>{t.name.clone()}</a> }
            }).collect_view())}
        </div>
    }
}

#[component]
fn Footer() -> impl IntoView {
    let theme = use_theme();
    let app = use_app();
    let stats = use_query::<LibraryStats>(|| Some(QuerySpec::new("/library/stats", &["stats", "track"])));
    view! {
        <div class="sb-foot">
            <div class="sb-foot-text">
                {move || stats.data.get().map(|s| view! {
                    <div class="stats">
                        <span>"tracks"</span><b>{format_count(s.tracks)}</b>
                        <span>"albums"</span><b>{format_count(s.releases)}</b>
                        <span>"artists"</span><b>{format_count(s.artists)}</b>
                        <span>"playtime"</span><b>{format_long_duration(s.total_duration_ms as f64)}</b>
                    </div>
                    {s.disk.clone().filter(|d| d.total_bytes > 0).map(|d| {
                        let used = d.used_bytes as f64 / d.total_bytes as f64;
                        let tone = if used >= 0.9 { " danger" } else if used >= 0.7 { " warn" } else { "" };
                        view! {
                            <div class="sb-foot-row"><span>"Disk"</span><span class="mono">{format_bytes(d.free_bytes as f64)}" free"</span></div>
                            <div class=format!("meter{tone}") role="meter" aria-label="Disk space used"
                                aria-valuenow=(used * 100.0).round().to_string() aria-valuemin="0" aria-valuemax="100"
                                title=format!("{} of {} used", format_bytes(d.used_bytes as f64), format_bytes(d.total_bytes as f64))>
                                <i style=format!("width:{:.0}%", used * 100.0)></i></div>
                        }
                    })}
                })}
            </div>
            <div class="sb-foot-row">
                <Button variant=Variant::Ghost size=crate::ds::Size::Sm
                    icon=dyn_icon(move || if theme.store.get().mode == bc_types::theme::Mode::Dark { "sun" } else { "moon" })
                    title="Toggle light / dark" on_click=move |_| theme.toggle_mode() />
                <Button variant=Variant::Ghost size=crate::ds::Size::Sm icon=dyn_icon(move || if app.nav_collapsed.get() { "chevron-right" } else { "panel-left" })
                    title="Collapse sidebar" on_click=move |_| app.nav_collapsed.update(|c| *c = !*c) />
            </div>
        </div>
    }
}

#[component]
pub fn Sidebar() -> impl IntoView {
    let app = use_app();
    view! {
        <nav class=move || if app.nav_collapsed.get() { "sidebar collapsed" } else { "sidebar" } aria-label="Main">
            <div class="sb-scroll"><NavLinks /></div>
            <Footer />
        </nav>
        <Show when=move || app.nav_open.get()>
            <div class="drawer-scrim" on:click=move |_| app.nav_open.set(false)></div>
            <nav class="drawer" role="dialog" aria-modal="true" aria-label="Navigation"
                on:keydown=move |ev| if ev.key() == "Escape" { app.nav_open.set(false) }>
                <div class="sb-head"><BrandLockup />
                    <span class="spacer"></span>
                    <Button variant=Variant::Ghost icon="x" title="Close navigation" on_click=move |_| app.nav_open.set(false) /></div>
                <div class="sb-scroll"><NavLinks /></div>
                <Footer />
            </nav>
        </Show>
    }
}
