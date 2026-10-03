//! Home on an empty library: instead of a page of empty shelves, the ways music gets in.
//! A folder on disk (added and scanned right here, with live progress), the Bandcamp
//! account, or the old app's library, plus Bandcamp itself (genres, who's selling, search)
//! and the discovery pages that work with nothing in.
//! Signed in to Bandcamp, the page leads with the account's own collection and wishlist.
use bc_types::Accepted;
use bc_types::bandcamp::{FanOut, IdentityStatus, WalkRequest};
use bc_types::library::*;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;

use super::logic;
use super::starter::BandcampDig;
use crate::api;
use crate::data::{QuerySpec, use_query, use_topic};
use crate::ds::{BrandMark, Button, Icon, Meter, Variant};
use crate::logic::format::format_count;
use crate::widgets::FolderPicker;

#[component]
pub fn Welcome(
    /// Roots from the shelves snapshot (the list is refetched here once one is added).
    roots: Vec<RootOut>,
    /// The library has music now: redraw Home as shelves.
    on_filled: Callback<()>,
) -> impl IntoView {
    let id = use_query::<IdentityStatus>(|| Some(QuerySpec::new("/harvest/identity", &["identity"])));
    let fans = use_query::<Vec<FanOut>>(|| Some(QuerySpec::new("/fans", &["fan"])));
    let (id_data, fans_data) = (id.data, fans.data);
    // Signed in and linked as "me" (the server does that on sign-in).
    let me = Memo::new(move |_| {
        let signed_in = id_data.get().is_some_and(|s| s.configured && s.valid != Some(false));
        signed_in.then(|| fans_data.get().and_then(|f| f.iter().find(|x| x.is_self).cloned())).flatten()
    });
    move || match me.get() {
        Some(fan) => view! { <BandcampStart fan=fan roots=roots.clone() on_filled=on_filled /> }.into_any(),
        None => view! { <Ways roots=roots.clone() on_filled=on_filled /> }.into_any(),
    }
}

/// The ways in, for someone not signed in to Bandcamp.
#[component]
fn Ways(roots: Vec<RootOut>, on_filled: Callback<()>) -> impl IntoView {
    view! {
        <section class="hm-wel" aria-labelledby="hm-wel-title">
            <div class="hm-wel-hero">
                <div class="hm-wel-brand"><BrandMark size=28 /><span class="hm-eyebrow">"Welcome to bc"</span></div>
                <h2 id="hm-wel-title" class="hm-wel-title">"Let\u{2019}s fill the crate"</h2>
                <p class="hm-wel-lede">"bc plays, tags and analyses the music you keep on disk, and fetches what you buy on Bandcamp. Pick a way in: the shelves fill up as soon as the first tracks land."</p>
            </div>
            <div class="hm-wel-ways">
                <FolderWay roots=roots.clone() on_filled=on_filled />
                <div class="hm-wel-side">
                    <BandcampWay roots=roots />
                    <ImportWay />
                </div>
            </div>
            <DigLinks />
        </section>
    }
}

/// Signed in: start from the account's own collection or wishlist, the rest below.
#[component]
fn BandcampStart(fan: FanOut, roots: Vec<RootOut>, on_filled: Callback<()>) -> impl IntoView {
    let name = fan.display_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| fan.username.clone());
    view! {
        <section class="hm-wel" aria-labelledby="hm-wel-title">
            <div class="hm-wel-hero">
                <div class="hm-wel-brand"><BrandMark size=28 /><span class="hm-eyebrow">"Welcome to bc"</span></div>
                <h2 id="hm-wel-title" class="hm-wel-title">{format!("Hi {name}, where do you want to start?")}</h2>
                <p class="hm-wel-lede">"Your library is still empty. Start with the music you bought on Bandcamp, or with the records on your wishlist."</p>
            </div>
            <div class="hm-wel-start">
                <StartCard fan=fan.clone() tab="collection" icon="disc" title="Your collection" cta="Open collection"
                    count=fan.collection_count unit="release you bought" units="releases you bought"
                    text="Download them into your library in the format you picked, or stream them first." />
                <StartCard fan=fan.clone() tab="wishlist" icon="heart" title="Your wishlist" cta="Open wishlist"
                    count=fan.wishlist_count unit="release you wished for" units="releases you wished for"
                    text="Listen through them, keep what you love and see what is already on disk." />
            </div>
            <div class="hm-wel-dig">
                <h3 class="hm-wel-dig-title">"Other ways in"</h3>
                <div class="hm-wel-ways">
                    <FolderWay roots=roots on_filled=on_filled />
                    <div class="hm-wel-side"><ImportWay /></div>
                </div>
            </div>
            <DigLinks />
        </section>
    }
}

/// One of the account's lists. Opening a list that was never walked walks it first, without
/// queueing anything: the list page shows it filling in and downloading stays a choice.
#[component]
fn StartCard(
    fan: FanOut,
    tab: &'static str,
    icon: &'static str,
    title: &'static str,
    cta: &'static str,
    count: Option<i64>,
    unit: &'static str,
    units: &'static str,
    text: &'static str,
) -> impl IntoView {
    let navigate = use_navigate();
    let busy = RwSignal::new(false);
    let walked = fan.tabs.get(tab).is_some_and(|t| t.items > 0) || fan.walk.as_ref().is_some_and(|w| w.running);
    let id = fan.id;
    let open = move |_| {
        let navigate = navigate.clone();
        busy.set(true);
        spawn_local(async move {
            if !walked {
                let body = WalkRequest { queue_new: Some(false), tabs: Some(vec![tab.to_string()]) };
                if let Err(e) = api::post::<_, FanOut>(&format!("/fans/{id}/walk"), &body).await {
                    crate::ds::toast_err(&e.message());
                }
            }
            let _ = busy.try_set(false);
            navigate(&format!("/fans/{id}?list={tab}"), Default::default());
        });
    };
    let sub = match count {
        Some(n) => format!("{} {}. {text}", format_count(n), if n == 1 { unit } else { units }),
        None => text.to_string(),
    };
    view! {
        <div class="hm-wel-card primary hm-wel-start-card">
            <div class="hm-wel-card-head">
                <span class="hm-wel-icon"><Icon name=icon /></span>
                <div>
                    <h3 class="hm-wel-card-title">{title}</h3>
                    <p class="faint hm-wel-card-sub">{sub}</p>
                </div>
            </div>
            <div class="hm-wel-actions">
                <Button variant=Variant::Primary icon="arrow-right" busy=busy on_click=open>{cta}</Button>
            </div>
        </div>
    }
}

#[component]
fn DigLinks() -> impl IntoView {
    view! {
        <BandcampDig />
        <div class="hm-wel-dig">
            <h3 class="hm-wel-dig-title">"More ways to dig"</h3>
            <div class="hm-wel-links">
                <DigLink to="/explore" icon="compass" title="Explore" text="Browse Bandcamp by genre, place and what\u{2019}s selling" />
                <DigLink to="/feed" icon="rss" title="Feed" text="New releases from the artists and labels you follow" />
                <DigLink to="/fans" icon="users" title="Fans" text="Follow collectors whose taste you trust" />
            </div>
        </div>
    }
}

#[component]
fn DigLink(to: &'static str, icon: &'static str, title: &'static str, text: &'static str) -> impl IntoView {
    view! {
        <a class="hm-wel-link" href=to>
            <span class="hm-wel-link-icon"><Icon name=icon /></span>
            <span class="hm-wel-link-text"><span class="hm-wel-link-title">{title}</span><span class="faint">{text}</span></span>
            <Icon name="arrow-right" size=14 />
        </a>
    }
}

// ---- a folder on disk ------------------------------------------------------------------------------

#[component]
fn FolderWay(roots: Vec<RootOut>, on_filled: Callback<()>) -> impl IntoView {
    let fetched = use_query::<Vec<RootOut>>(|| Some(QuerySpec::new("/library/roots", &["stats"])));
    let fetched_data = fetched.data;
    // `refetch` alone keeps a fresh cache entry; the list has to be marked stale
    let reload_roots = || crate::data::invalidate_prefix("/library/roots");
    // the downloads folder is registered on its own; this card is about the user's music
    let roots = Memo::new(move |_| {
        let all = fetched_data.get().map(|r| (*r).clone()).unwrap_or_else(|| roots.clone());
        all.into_iter().filter(|r| r.kind == "library").collect::<Vec<_>>()
    });

    let path = RwSignal::new("~/Music".to_string());
    let adding = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let scanning = RwSignal::new(false);
    let progress = RwSignal::new(None::<ScanProgress>);
    let note = RwSignal::new(None::<String>);
    // the running scan, for Stop (from the start response, or from progress if it began elsewhere)
    let job = RwSignal::new(None::<String>);
    let stopping = RwSignal::new(false);

    use_topic::<ScanProgress>("library.scan.progress", move |p| {
        scanning.set(true);
        job.set(Some(p.job_id.clone()));
        progress.set(Some(p));
    });
    use_topic::<serde_json::Value>("library.scan.done", move |v| {
        let id = v.get("job_id").and_then(|j| j.as_str()).map(str::to_string);
        let cancelled = v.get("cancelled").and_then(|c| c.as_bool()).unwrap_or(false);
        spawn_local(async move {
            let status = match id {
                Some(id) => api::get::<ScanStatus>(&format!("/library/scan/{id}")).await.ok(),
                None => None,
            };
            let _ = scanning.try_set(false);
            let _ = progress.try_set(None);
            let _ = job.try_set(None);
            let _ = stopping.try_set(false);
            reload_roots();
            match status.map(|s| logic::scan_outcome(&s.results, cancelled || s.state == "cancelled")) {
                Some(logic::ScanOutcome::Empty { seen }) => {
                    let _ = note.try_set(Some(logic::empty_scan_note(seen)));
                }
                Some(logic::ScanOutcome::Errors(e)) => {
                    let _ = note.try_set(Some(e));
                }
                Some(logic::ScanOutcome::Stopped) => {
                    let _ = note.try_set(Some(logic::STOPPED_SCAN_NOTE.into()));
                }
                _ => on_filled.run(()),
            }
        });
    });

    let scan = move |root: Option<i64>| {
        scanning.set(true);
        note.set(None);
        let url = match root {
            Some(id) => format!("/library/scan?root_id={id}"),
            None => "/library/scan".to_string(),
        };
        spawn_local(async move {
            match api::post::<_, Accepted>(&url, &serde_json::json!({})).await {
                Ok(a) => job.set(Some(a.job_id)),
                Err(e) => {
                    scanning.set(false);
                    error.set(Some(e.message()));
                }
            }
        });
    };
    // Stops between files: what is in stays in, and `library.scan.done` settles the card.
    let stop = move |_| {
        let Some(id) = job.get_untracked() else { return };
        stopping.set(true);
        spawn_local(async move {
            if let Err(e) = api::post::<_, Accepted>(&format!("/library/scan/{id}/cancel"), &serde_json::json!({})).await {
                let _ = stopping.try_set(false);
                crate::ds::toast_err(&e.message());
            }
        });
    };
    let add = move || {
        let p = path.get_untracked().trim().to_string();
        if p.is_empty() {
            return;
        }
        adding.set(true);
        error.set(None);
        note.set(None);
        spawn_local(async move {
            match api::post::<_, RootOut>("/library/roots", &AddRootRequest { path: p, kind: "library".into() }).await {
                Ok(r) => {
                    path.set(String::new());
                    reload_roots();
                    scan(Some(r.id));
                }
                Err(e) => error.set(Some(e.message())),
            }
            adding.set(false);
        });
    };
    let add2 = add.clone();
    let has_roots = Memo::new(move |_| roots.with(|r| !r.is_empty()));
    let picking = RwSignal::new(false);

    view! {
        <div class="hm-wel-card primary">
            <div class="hm-wel-card-head">
                <span class="hm-wel-icon"><Icon name="folder" /></span>
                <div>
                    <h3 class="hm-wel-card-title">{move || if has_roots.get() { "Your music folders" } else { "Add your music folder" }}</h3>
                    <p class="faint hm-wel-card-sub">"Point bc at the folder that holds your music. Files are read in place and never changed."</p>
                </div>
            </div>

            {move || has_roots.get().then(|| view! {
                <ul class="hm-wel-roots">
                    {roots.get().into_iter().map(|r| {
                        let id = r.id;
                        view! {
                            <li class="hm-wel-root">
                                <Icon name="hdd" size=14 />
                                <span class="mono truncate" title=r.path.clone()>{r.path.clone()}</span>
                                <span class="faint mono hm-wel-root-n">{if r.last_scan_at.is_some() { format!("{} tracks", format_count(r.track_count)) } else { "not scanned".into() }}</span>
                                <button type="button" class="hm-pill" disabled=move || scanning.get() on:click=move |_| scan(Some(id))>
                                    <Icon name="refresh" size=11 />"Scan"
                                </button>
                            </li>
                        }
                    }).collect_view()}
                </ul>
            })}

            {move || scanning.get().then(|| {
                let p = progress.get();
                let value = p.as_ref().and_then(|p| p.total.filter(|t| *t > 0).map(|t| p.seen as f64 / t as f64));
                let phase = logic::scan_phase_label(p.as_ref().map(|p| p.phase.as_str()));
                view! {
                    <div class="hm-wel-scan" role="status" aria-live="polite">
                        <div class="hm-wel-scan-row">
                            <span>{move || if stopping.get() { "Stopping\u{2026}" } else { phase }}</span>
                            <span class="hm-wel-scan-end">
                                <span class="mono faint">{p.as_ref().map(|p| logic::scan_count(p.seen, p.total)).unwrap_or_default()}</span>
                                <button type="button" class="hm-pill" title="Stop the scan; tracks already added stay in the library"
                                    disabled=move || stopping.get() || job.get().is_none() on:click=stop>
                                    <Icon name="x" size=11 />"Stop"
                                </button>
                            </span>
                        </div>
                        <Meter value=value label="Scan progress" />
                    </div>
                }
            })}

            {move || note.get().map(|n| view! { <p class="sys-notice warn" role="status"><Icon name="info" />{n}</p> })}

            {move || (!has_roots.get() && !scanning.get()).then(|| view! {
                <ol class="hm-wel-next" aria-label="What happens after a scan">
                    <li><Icon name="music" size=14 /><span><b>"Tags and artwork"</b>" are read from every file: albums, artists and labels sort themselves out."</span></li>
                    <li><Icon name="home" size=14 /><span><b>"Home fills up"</b>" with the newest additions, a crate to dig through and what you play most."</span></li>
                    <li><Icon name="waveform" size=14 /><span><b>"Analysis"</b>" (BPM, key, energy, waveforms) runs in the background for mixing and DJ sets."</span></li>
                </ol>
            })}

            <div class="hm-wel-add">
                <label class="sr-only" for="hm-wel-path">"Music folder path"</label>
                <input id="hm-wel-path" class="input mono grow" spellcheck="false" autocomplete="off"
                    placeholder=move || if has_roots.get() { "Add another folder, e.g. /Volumes/Music" } else { "~/Music" }
                    prop:value=move || path.get() on:input=move |ev| path.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" { add2() } />
                <Button icon="folder" title="Pick a folder on the machine running bc" disabled=Signal::derive(move || scanning.get())
                    on_click=move |_| picking.set(true)>"Browse\u{2026}"</Button>
                <Button variant=Variant::Primary icon="plus" busy=adding
                    disabled=Signal::derive(move || path.get().trim().is_empty() || scanning.get())
                    on_click=move |_| add()>"Add and scan"</Button>
            </div>
            <FolderPicker open=picking start=Signal::derive(move || path.get()) title="Choose your music folder"
                on_pick=Callback::new(move |p| path.set(p)) />
            {move || error.get().map(|e| view! { <p class="sys-notice danger" role="alert"><Icon name="alert" />{e}</p> })}
            <p class="faint hm-wel-fine">"Paths are on the machine running bc; "<code class="mono">"~"</code>" is its home folder. Watch folders and rescans live in "<a class="lib-link" href="/settings?tab=library">"Settings \u{203a} Library"</a>"."</p>
        </div>
    }
}

// ---- Bandcamp -------------------------------------------------------------------------------------

#[component]
fn BandcampWay(roots: Vec<RootOut>) -> impl IntoView {
    let downloads = roots.into_iter().find(|r| r.kind == "downloads").map(|r| r.path);
    let id = use_query::<IdentityStatus>(|| Some(QuerySpec::new("/harvest/identity", &["identity"])));
    let data = id.data;
    let connected = Memo::new(move |_| data.get().is_some_and(|s| s.configured && s.valid != Some(false)));
    view! {
        <div class="hm-wel-card">
            <div class="hm-wel-card-head">
                <span class="hm-wel-icon"><Icon name="download" /></span>
                <div>
                    <h3 class="hm-wel-card-title">{move || if connected.get() { "Download your Bandcamp collection" } else { "Connect Bandcamp" }}</h3>
                    <p class="faint hm-wel-card-sub">{move || match data.get() {
                        Some(s) if s.configured && s.valid != Some(false) => format!(
                            "Signed in{}. Sweep your collection into the inbox and queue what you want on disk.",
                            s.username.as_ref().map(|u| format!(" as {u}")).unwrap_or_default()
                        ),
                        Some(s) if s.configured => "Your Bandcamp sign-in has expired. Sign in again to reach your purchases.".to_string(),
                        _ => "Sign in to Bandcamp once and bc can download everything you\u{2019}ve bought, in the format you pick.".to_string(),
                    }}</p>
                </div>
            </div>
            <div class="hm-wel-actions">
                {move || if connected.get() {
                    view! {
                        <a class="btn btn-outline btn-sm" href="/harvest"><Icon name="sparkles" />"Open Harvest"</a>
                        <a class="lib-link faint" href="/settings?tab=downloads">"Download quality"</a>
                    }.into_any()
                } else {
                    view! {
                        <a class="btn btn-outline btn-sm" href="/settings?tab=downloads"><Icon name="key" />"Sign in"</a>
                        <span class="faint hm-wel-fine">"Optional: Explore and Feed work without it."</span>
                    }.into_any()
                }}
            </div>
            {downloads.map(|p| view! { <p class="faint hm-wel-fine hm-wel-dl">"Downloads land in "<span class="mono truncate" title=p.clone()>{p.clone()}</span>" and join the library on their own."</p> })}
        </div>
    }
}

// ---- the old app ------------------------------------------------------------------------------------

#[component]
fn ImportWay() -> impl IntoView {
    view! {
        <div class="hm-wel-card">
            <div class="hm-wel-card-head">
                <span class="hm-wel-icon"><Icon name="upload" /></span>
                <div>
                    <h3 class="hm-wel-card-title">"Coming from the old app?"</h3>
                    <p class="faint hm-wel-card-sub">"Bring over the library, playlists, sets, loved tracks and analysis from the Python Bandcamp manager."</p>
                </div>
            </div>
            <div class="hm-wel-actions">
                <a class="btn btn-outline btn-sm" href="/settings?tab=import"><Icon name="upload" />"Import"</a>
            </div>
        </div>
    }
}
