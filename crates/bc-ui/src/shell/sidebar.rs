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
        <TopTags />
        <NavRow item=SETTINGS trailing=none_str() active_dot=Signal::derive(|| false) />
    }
}

#[component]
fn TopTags() -> impl IntoView {
    let tags = use_query::<Vec<TagOut>>(|| Some(QuerySpec::new("/tags?limit=14", &["tag"])));
    // no heading over an empty library
    view! {
        {move || tags.data.get().filter(|t| !t.is_empty()).map(|t| view! {
            <div class="sb-section">"Tags"</div>
            <div class="sb-tags">
                {t.iter().take(14).map(|t| {
                    let href = format!("/tracks?tag={}", crate::util::enc(&t.name));
                    view! { <a class="chip" href=href style=format!("--tag-h:{}", crate::util::tag_hue(&t.name)) title=format!("{} tracks", t.track_count)>{t.name.clone()}</a> }
                }).collect_view()}
            </div>
        })}
    }
}

const STATS_METRIC_KEY: &str = "bc:sb-stats-metric:v1";
const STATS_SIZES: [&str; 4] = ["off", "s", "m", "l"];

/// Library stats in the sidebar footer, sized by `UiPrefs::sidebar_stats`. Small shows one
/// figure at a time (click for the next), medium a compact grid, large labelled tiles.
#[component]
fn LibraryInfo(#[prop(into)] size: Signal<String>) -> impl IntoView {
    let stats = use_query::<LibraryStats>(|| Some(QuerySpec::new("/library/stats", &["stats", "track"])));
    let metric = RwSignal::new(crate::util::ls_get(STATS_METRIC_KEY).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0));
    Effect::new(move |_| crate::util::ls_set(STATS_METRIC_KEY, &metric.get().to_string()));
    view! {
        {move || {
            let size = size.get();
            let s = stats.data.get()?;
            let disk = s.disk.clone().filter(|d| d.total_bytes > 0);
            let used = disk.as_ref().map(|d| d.used_bytes as f64 / d.total_bytes as f64);
            let tone = match used { Some(u) if u >= 0.9 => " danger", Some(u) if u >= 0.7 => " warn", _ => "" };
            let disk_title = disk.as_ref().map(|d| format!("{} of {} used", format_bytes(d.used_bytes as f64), format_bytes(d.total_bytes as f64))).unwrap_or_default();
            let meter = used.map(|u| view! {
                <div class=format!("meter sbi-meter{tone}") role="meter" aria-label="Disk space used"
                    aria-valuenow=(u * 100.0).round().to_string() aria-valuemin="0" aria-valuemax="100" title=disk_title.clone()>
                    <i style=format!("width:{:.0}%", u * 100.0)></i></div>
            });
            let mut figures = vec![
                (format_count(s.tracks), "tracks"),
                (format_count(s.releases), "albums"),
                (format_count(s.artists), "artists"),
                (format_long_duration(s.total_duration_ms as f64), "playtime"),
            ];
            let free = disk.as_ref().map(|d| format_bytes(d.free_bytes as f64));
            Some(match size.as_str() {
                "s" => {
                    if let Some(f) = &free {
                        figures.push((f.clone(), "free"));
                    }
                    let n = figures.len();
                    let i = metric.get() % n;
                    let (value, label) = figures[i].clone();
                    view! {
                        <button type="button" class="sbi sbi-s sb-foot-text" title="Library info: click for the next figure"
                            on:click=move |_| metric.set((i + 1) % n)>
                            <span class="sbi-line"><b>{value}</b><span>{label}</span>
                                <span class="sbi-dots" aria-hidden="true">
                                    {(0..n).map(|k| view! { <i class:on=k == i></i> }).collect_view()}
                                </span></span>
                            {meter}
                        </button>
                    }.into_any()
                }
                "l" => view! {
                    <div class="sbi sbi-l sb-foot-text">
                        <div class="sbi-tiles">
                            {figures.into_iter().map(|(v, l)| view! { <div><span>{l}</span><b>{v}</b></div> }).collect_view()}
                        </div>
                        {free.map(|f| view! {
                            <div class="sbi-disk"><span>"disk"</span><span class="mono" title=disk_title.clone()>{f}" free"</span></div>
                        })}
                        {meter}
                    </div>
                }.into_any(),
                _ => view! {
                    <div class="sbi sbi-m sb-foot-text">
                        <div class="stats">
                            {figures.into_iter().map(|(v, l)| view! { <span>{l}</span><b>{v}</b> }).collect_view()}
                        </div>
                        {meter}
                    </div>
                }.into_any(),
            })
        }}
    }
}

#[component]
fn Footer() -> impl IntoView {
    let theme = use_theme();
    let app = use_app();
    let prefs = crate::prefs::use_prefs().prefs;
    let size = Signal::derive(move || prefs.with(|p| p.sidebar_stats.clone()));
    let cycle = move |_| prefs.update(|p| {
        let i = STATS_SIZES.iter().position(|s| *s == p.sidebar_stats).unwrap_or(0);
        p.sidebar_stats = STATS_SIZES[(i + 1) % STATS_SIZES.len()].into();
    });
    view! {
        <div class="sb-foot">
            <Show when=move || size.with(|s| s != "off")>
                <LibraryInfo size=size />
            </Show>
            <div class="sb-foot-row">
                <Button variant=Variant::Ghost size=crate::ds::Size::Sm
                    icon=dyn_icon(move || if theme.store.get().mode == bc_types::theme::Mode::Dark { "sun" } else { "moon" })
                    title="Toggle light / dark" on_click=move |_| theme.toggle_mode() />
                <span class="spacer"></span>
                <Button variant=Variant::Ghost size=crate::ds::Size::Sm icon="info" class="sbi-toggle"
                    pressed=Signal::derive(move || size.with(|s| s != "off"))
                    title="Library info: hidden / S / M / L" on_click=cycle />
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
