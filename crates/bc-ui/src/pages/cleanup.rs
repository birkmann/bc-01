//! Cleanup: albums that are not music you would ever play (sample packs, preview
//! stubs), single-track strays, and the blacklist. Everything here destroys data, so
//! it is its own workspace; deleting can also blacklist, which is the only thing that
//! makes a deletion stick (the wishlist would otherwise fetch it straight back).
use std::collections::BTreeSet;
use std::sync::Arc;

use bc_types::Page;
use bc_types::library::maint::*;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

use crate::api;
use crate::data::{QuerySpec, use_invalidation, use_query};
use crate::ds::tabs::TabDef;
use crate::pages::settings::qh;
use crate::ds::{Button, Dialog, EmptyState, ErrorPanel, Icon, Meter, PageHeader, Select, SelectOption, Size, Tabs, Variant, toast_err};
use crate::logic::format::{format_count, format_duration_ms};
use crate::widgets::{CardGrid, PageFetcher, PageRes};

mod blacklist;
mod logic;
mod strays;

pub use blacklist::BlacklistPanel;
use logic::*;
use strays::StraysPanel;

#[component]
pub fn CleanupPage() -> impl IntoView {
    let query = use_query_map();
    let navigate = use_navigate();
    let initial = query.get_untracked().get("tab").filter(|t| ["junk", "strays", "blacklist"].contains(&t.as_str())).unwrap_or_else(|| "junk".into());
    let tab = RwSignal::new(initial);
    Effect::new(move |prev: Option<String>| {
        let t = tab.get();
        if prev.is_some() && prev.as_deref() != Some(t.as_str()) {
            navigate(&format!("/cleanup?tab={t}"), NavigateOptions { replace: true, ..Default::default() });
        }
        t
    });

    let threshold = RwSignal::new(parse_threshold(crate::util::ls_get(THRESHOLD_KEY).as_deref()).to_string());
    Effect::new(move |_| crate::util::ls_set(THRESHOLD_KEY, &threshold.get()));
    let max_s = Signal::derive(move || threshold.get().parse::<i64>().unwrap_or(DEFAULT_THRESHOLD));

    let strays = qh(use_query::<StraysOut>(|| Some(QuerySpec::new("/releases/strays?limit=40", &["release", "strays"]))));
    let bl = qh(use_query::<Page<BlacklistOut>>(|| Some(QuerySpec::new("/blacklist?limit=500", &["blacklist"]))));
    let junk_total = RwSignal::new(None::<usize>);

    let tabs = Signal::derive(move || {
        let mut junk = TabDef::new("junk", "Junk");
        if let Some(n) = junk_total.get() {
            junk = junk.count(n as i64);
        }
        let mut st = TabDef::new("strays", "Strays");
        if let Some(s) = strays.data.get() {
            st = st.count(s.total);
        }
        let mut b = TabDef::new("blacklist", "Blacklist");
        if let Some(p) = bl.data.get() {
            b = b.count(p.total);
        }
        vec![junk, st, b]
    });
    let thresholds: Vec<SelectOption> = THRESHOLDS.iter().map(|t| SelectOption::new(t.to_string(), format!("{t} s"))).collect();
    let subtitle = Signal::derive(move || match (tab.get().as_str(), junk_total.get()) {
        ("junk", Some(n)) => format!("{} album{} look like sample packs or preview stubs", format_count(n as i64), if n == 1 { "" } else { "s" }),
        ("strays", _) => "Single tracks filed as albums of their own".to_string(),
        ("blacklist", _) => "Releases that are never downloaded again".to_string(),
        _ => String::new(),
    });

    view! {
        <div class="page">
            <PageHeader title="Cleanup" subtitle=subtitle
                actions=crate::ds::children(move || {
                    let thresholds = thresholds.clone();
                    view! {
                        <Show when=move || tab.get() == "junk">
                            <div class="row gap cleanup-threshold">
                                <span class="faint hide-sm">"Longest track under"</span>
                                <div style="width:96px"><Select options=thresholds.clone() value=threshold aria_label="Longest track under (seconds)" /></div>
                            </div>
                        </Show>
                    }
                }) />
            <div class="sys-tabs"><Tabs tabs=tabs value=tab /></div>
            <Show when=move || tab.get() == "junk">
                <JunkTab max_s=max_s total=junk_total />
            </Show>
            <Show when=move || tab.get() == "strays"><div class="page-scroll"><div class="sys-page"><StraysPanel /></div></div></Show>
            <Show when=move || tab.get() == "blacklist"><div class="page-scroll"><div class="sys-page"><BlacklistPanel /></div></div></Show>
        </div>
    }
}

#[component]
fn JunkTab(max_s: Signal<i64>, total: RwSignal<Option<usize>>) -> impl IntoView {
    let items = RwSignal::new(Arc::new(Vec::<CleanupCandidate>::new()));
    let selected = RwSignal::new(BTreeSet::<i64>::new());
    let rev = RwSignal::new(0u64);
    let result = RwSignal::new(None::<String>);
    let confirming = RwSignal::new(false);
    let load_error = RwSignal::new(None::<String>);

    // A different threshold or a deletion elsewhere is a different list: refetch, never carry stale ids.
    use_invalidation(&["release"], move |_| rev.update(|r| *r += 1));
    Effect::new(move |_| {
        let _ = items.get();
        selected.update(|s| *s = retain_listed(s, &items.get_untracked()));
    });

    let fetcher: PageFetcher<CleanupCandidate> = Arc::new(move |req| {
        let s = max_s.get_untracked();
        Box::pin(async move {
            let out = api::get::<CleanupOut>(&format!("/cleanup/candidates?max_track_s={s}")).await;
            match out {
                Ok(out) => {
                    load_error.set(None);
                    let all = out.items;
                    let total_n = all.len();
                    items.set(Arc::new(all.clone()));
                    let rows = all.into_iter().skip(req.offset).take(req.limit).collect();
                    Ok(PageRes { rows, total: total_n })
                }
                Err(e) => {
                    load_error.set(Some(e.message()));
                    Err(e)
                }
            }
        })
    });
    let source_key = Signal::derive(move || format!("junk|{}|{}", max_s.get(), rev.get()));
    let render = Callback::new(move |(item, _w): (CleanupCandidate, f64)| view! { <JunkCard item=item selected=selected /> }.into_any());

    let n_sel = Signal::derive(move || selected.with(|s| s.len()));
    let sel_tracks = Signal::derive(move || selected_tracks(&items.get(), &selected.get()));
    let all_selected = Signal::derive(move || {
        let n = total.get().unwrap_or(0);
        n > 0 && n_sel.get() == n
    });

    view! {
        <div class="junk-bar">
            <Show when=move || total.get().unwrap_or(0) != 0>
                <Button size=Size::Sm variant=Variant::Ghost on_click=move |_| {
                    if all_selected.get_untracked() { selected.set(BTreeSet::new()) } else { selected.set(all_ids(&items.get_untracked())) }
                }>{move || if all_selected.get() { "Clear selection".to_string() } else { format!("Select all {}", total.get().unwrap_or(0)) }}</Button>
            </Show>
            <Show when=move || n_sel.get() != 0>
                <Button size=Size::Sm variant=Variant::Danger icon="trash" on_click=move |_| confirming.set(true)>
                    {move || format!("Delete {} ({} tracks)", format_count(n_sel.get() as i64), format_count(sel_tracks.get()))}
                </Button>
            </Show>
            <span class="spacer"></span>
            <span class="faint junk-note"><Icon name="info" />"Nothing is selected for you: a title match alone is a hint, not a verdict."</span>
        </div>
        {move || result.get().map(|r| view! {
            <div class="banner" role="status"><Icon name="check-circle" /><span class="grow">{r}</span>
                <Button size=Size::Sm variant=Variant::Ghost on_click=move |_| result.set(None)>"Dismiss"</Button></div>
        })}
        {move || load_error.get().map(|e| view! { <ErrorPanel message=e on_retry=Callback::new(move |_| rev.update(|r| *r += 1)) /> })}
        <div class="junk-grid">
            <CardGrid fetch=fetcher source_key=source_key render=render min_card_w=150.0 meta_h=96.0 total_out=total
                empty=move || view! { <EmptyState icon="check-circle" title="Nothing looks like junk at this threshold" hint="Try a longer threshold in the header." /> } />
        </div>
        <DeleteDialog open=confirming count=n_sel tracks=sel_tracks selected=selected result=result rev=rev />
    }
}

#[component]
fn JunkCard(item: CleanupCandidate, selected: RwSignal<BTreeSet<i64>>) -> impl IntoView {
    let id = item.release.id;
    let is_sel = Signal::derive(move || selected.with(|s| s.contains(&id)));
    let title = item.release.title.clone();
    let artist = item.release.artist.as_ref().map(|a| a.name.clone()).unwrap_or_else(|| "Unknown artist".into());
    let art = item.release.art_url.clone();
    let longest = item.longest_ms;
    let short = item.reasons.iter().any(|r| r == "short-tracks");
    view! {
        <div class="junk-card" class:selected=move || is_sel.get()>
            <button type="button" class="junk-art" aria-pressed=move || is_sel.get().to_string() aria-label=format!("Select {title}")
                on:click=move |_| selected.update(|s| toggle(s, id))>
                <crate::widgets::common::Art src=art />
                <span class="junk-check" aria-hidden="true"><Icon name="check" /></span>
            </button>
            <div class="junk-meta">
                <div class="truncate strong" title=item.release.title.clone()>{item.release.title.clone()}</div>
                <div class="truncate faint">{artist}<span class="mono">{format!(" - {} tracks", item.track_count)}</span></div>
                <div class="reasons">
                    {short.then(|| view! { <span class="badge badge-warn"><Icon name="clock" />{format!("longest {}", format_duration_ms(longest.map(|l| l as f64)))}</span> })}
                    {item.matched_phrases.iter().map(|p| view! { <span class="badge">{format!("\"{p}\"")}</span> }).collect_view()}
                    <a class="open" href=format!("/albums/{id}")>"Open"</a>
                </div>
            </div>
        </div>
    }
}

/// Confirmation + chunked delete with progress and Stop.
#[component]
fn DeleteDialog(
    open: RwSignal<bool>,
    count: Signal<usize>,
    tracks: Signal<i64>,
    selected: RwSignal<BTreeSet<i64>>,
    result: RwSignal<Option<String>>,
    rev: RwSignal<u64>,
) -> impl IntoView {
    let blacklist = RwSignal::new(true);
    let busy = RwSignal::new(false);
    let stop = StoredValue::new(false);
    let done = RwSignal::new(0usize);
    let total = RwSignal::new(0usize);
    let error = RwSignal::new(None::<String>);

    let run = move |_| {
        let ids: Vec<i64> = selected.get_untracked().into_iter().collect();
        if ids.is_empty() {
            return;
        }
        let bl = blacklist.get_untracked();
        busy.set(true);
        stop.set_value(false);
        done.set(0);
        total.set(ids.len());
        error.set(None);
        spawn_local(async move {
            let mut parts = vec![];
            let mut failed = None;
            for chunk in ids.chunks(CHUNK) {
                if stop.get_value() {
                    break;
                }
                let req = DeleteReleasesRequest { ids: chunk.to_vec(), blacklist: bl, reason: Some("cleanup".into()) };
                match api::post::<_, DeleteReleasesResult>("/releases/delete", &req).await {
                    Ok(r) => {
                        parts.push(r);
                        done.update(|d| *d += chunk.len());
                        selected.update(|s| {
                            for i in chunk {
                                s.remove(i);
                            }
                        });
                    }
                    Err(e) => {
                        failed = Some(e.message());
                        break;
                    }
                }
            }
            busy.set(false);
            if !parts.is_empty() {
                result.set(Some(result_sentence(&merge_results(&parts))));
                crate::data::invalidate_all();
                rev.update(|r| *r += 1);
            }
            match failed {
                Some(e) => {
                    toast_err(&e);
                    error.set(Some(e));
                }
                None => open.set(false),
            }
        });
    };
    let progress = Signal::derive(move || (total.get() > 0).then(|| done.get() as f64 / total.get() as f64));
    let footer = crate::ds::children(move || view! {
        <Show when=move || busy.get() fallback=move || view! {
            <Button variant=Variant::Ghost on_click=move |_| open.set(false)>"Cancel"</Button>
            <Button variant=Variant::Danger icon="trash" on_click=run>{move || format!("Delete {}", format_count(count.get() as i64))}</Button>
        }>
            <Button variant=Variant::Outline on_click=move |_| stop.set_value(true)>"Stop after this batch"</Button>
        </Show>
    });
    view! {
        <Dialog open=open title=Signal::derive(move || format!("Delete {} album{}?", format_count(count.get() as i64), if count.get() == 1 { "" } else { "s" })) footer=footer>
            <p>{move || format!("{} track{} and their files are erased from disk. This cannot be undone.", format_count(tracks.get()), if tracks.get() == 1 { "" } else { "s" })}</p>
            <label class="check bl-check">
                <input type="checkbox" prop:checked=move || blacklist.get() disabled=move || busy.get()
                    on:change=move |ev| blacklist.set(event_target_checked(&ev)) />
                <span><b>"Also blacklist these"</b>
                    <span class="faint block">"Never download or queue them again. Without this the wishlist they came from will fetch them straight back."</span></span>
            </label>
            <Show when=move || busy.get()>
                <div role="status" aria-live="polite" class="del-progress">
                    <Meter value=progress label="Delete progress" />
                    <span class="mono faint">{move || format!("{} / {}", format_count(done.get() as i64), format_count(total.get() as i64))}</span>
                </div>
            </Show>
            {move || error.get().map(|e| view! { <p class="sys-notice danger" role="alert"><Icon name="alert" />{e}</p> })}
        </Dialog>
    }
}
