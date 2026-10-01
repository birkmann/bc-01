//! Feed: new releases from everything you follow, for review. Sources rail, state tabs,
//! grouped page (label / artist walls) or a flat virtualised `CardGrid`, batch queue and
//! ignore, sweep + enrich state patched in place from WS topics (never polled).
use std::collections::HashSet;
use std::sync::Arc;

use bc_types::Page;
use bc_types::bandcamp::*;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};

use crate::api;
use crate::data::{QuerySpec, use_query, use_topic};
use crate::ds::{Button, EmptyState, ErrorPanel, Icon, MenuEntry, MenuItem, Meter, PageHeader, Select, Size, Skeleton, Variant};
use crate::logic::format::format_count;
use crate::player::use_player;
use crate::util::qs_pairs;
use crate::widgets::{CardGrid, PageFetcher, PageRes};

pub(super) mod card;
mod logic;
pub(super) mod ops;

use card::{CARD_META_H, InboxCard, StateOverrides};
use logic::{FeedSegment, build_feed_segments};

/// Matches the harvest inbox page size.
const PAGE_SIZE: usize = 200;
const STATE_TABS: [(&str, &str); 5] = [("new", "New"), ("queued", "Queued"), ("in_library", "In library"), ("ignored", "Ignored"), ("all", "All")];
const POLL_CHOICES: [(&str, &str); 5] = [("0", "off"), ("6", "every 6 h"), ("12", "every 12 h"), ("24", "every 24 h"), ("48", "every 2 days")];

/// Filters and paging live in the URL: a reload, a shared link or the back button land on the same slice.
#[derive(Clone, Debug, PartialEq, Default)]
struct Slice {
    state: String,
    source: String,
    tags: Vec<String>,
    /// `flat` = infinite virtual grid; anything else = grouped pages.
    flat: bool,
    page: usize,
}

impl Slice {
    fn from_map(m: &leptos_router::params::ParamsMap) -> Self {
        Slice {
            state: m.get("state").filter(|s| !s.is_empty()).unwrap_or_else(|| "new".into()),
            source: m.get("source").unwrap_or_default(),
            tags: m.get_all("tag").unwrap_or_default(),
            flat: m.get("view").as_deref() == Some("flat"),
            page: m.get("page").and_then(|p| p.parse().ok()).unwrap_or(0),
        }
    }
    fn url(&self) -> String {
        let mut pairs: Vec<(String, String)> = vec![];
        if self.state != "new" {
            pairs.push(("state".into(), self.state.clone()));
        }
        if !self.source.is_empty() {
            pairs.push(("source".into(), self.source.clone()));
        }
        for t in &self.tags {
            pairs.push(("tag".into(), t.clone()));
        }
        if self.flat {
            pairs.push(("view".into(), "flat".into()));
        }
        if self.page > 0 {
            pairs.push(("page".into(), self.page.to_string()));
        }
        format!("/feed{}", qs_pairs(&pairs))
    }
    /// Query pairs of the inbox listing for this slice (without paging).
    fn item_pairs(&self) -> Vec<(String, String)> {
        let mut p = vec![("state".to_string(), self.state.clone()), ("source_kind".into(), "follow".into())];
        if !self.source.is_empty() {
            p.push(("source_label".into(), self.source.clone()));
        }
        for t in &self.tags {
            p.push(("tags".into(), t.clone()));
        }
        p
    }
    /// Identity of what a selection / collapse state refers to.
    fn key(&self) -> String {
        format!("{}|{}|{:?}|{}|{}", self.state, self.source, self.tags, self.page, self.flat)
    }
}

fn items_url(slice: &Slice, offset: usize, limit: usize) -> String {
    let mut p = slice.item_pairs();
    p.push(("offset".into(), offset.to_string()));
    p.push(("limit".into(), limit.to_string()));
    format!("/harvest/items{}", qs_pairs(&p))
}

fn tags_url(slice: &Slice) -> String {
    let mut p = vec![("state".to_string(), slice.state.clone()), ("source_kind".into(), "follow".into()), ("limit".into(), "30".into())];
    if !slice.source.is_empty() {
        p.push(("source_label".into(), slice.source.clone()));
    }
    format!("/harvest/tags{}", qs_pairs(&p))
}

fn toggle(set: &mut HashSet<i64>, id: i64) {
    if !set.remove(&id) {
        set.insert(id);
    }
}

#[component]
pub fn FeedPage() -> impl IntoView {
    let query = use_query_map();
    let navigate = use_navigate();
    let player = use_player();
    let slice = Memo::new(move |_| query.with(Slice::from_map));
    let go = {
        let navigate = navigate.clone();
        Arc::new(move |s: Slice| navigate(&s.url(), NavigateOptions::default()))
    };

    let selected: RwSignal<HashSet<i64>> = RwSignal::new(HashSet::new());
    let collapsed: RwSignal<HashSet<String>> = RwSignal::new(HashSet::new());
    let overrides: StateOverrides = RwSignal::new(Default::default());
    let notice = RwSignal::new(None::<(bool, String)>);
    let busy_queue = RwSignal::new(false);
    let busy_ignore = RwSignal::new(false);
    let rail_open = RwSignal::new(false);
    let grid_epoch = RwSignal::new(0u32);

    // A selection names rows of the slice in view; any other slice makes it stale.
    Effect::new(move |_| {
        slice.with(|s| s.key());
        selected.set(HashSet::new());
        collapsed.set(HashSet::new());
        overrides.set(Default::default());
    });

    // -- data ---------------------------------------------------------------------
    let follows = use_query::<FollowsOut>(|| Some(QuerySpec::new("/follows", &["follow"])));
    let sweep = RwSignal::new(None::<FeedSweepStatus>);
    let sweep_q = use_query::<FeedSweepStatus>(|| Some(QuerySpec::new("/follows/sweep", &["follow"])));
    Effect::new(move |_| {
        if let Some(s) = sweep_q.data.get() {
            sweep.set(Some((*s).clone()));
        }
    });
    let enrich = RwSignal::new(None::<EnrichState>);
    let enrich_q = use_query::<EnrichState>(|| Some(QuerySpec::new("/harvest/enrich", &["harvest"])));
    Effect::new(move |_| {
        if let Some(s) = enrich_q.data.get() {
            enrich.set(Some((*s).clone()));
        }
    });
    let items_q = use_query::<Page<HarvestItemOut>>(move || {
        let s = slice.get();
        (!s.flat).then(|| QuerySpec::new(items_url(&s, s.page * PAGE_SIZE, PAGE_SIZE), &["harvest"]))
    });
    let tags_q = use_query::<Vec<TagCount>>(move || Some(QuerySpec::new(tags_url(&slice.get()), &["harvest"])));
    // `Query` is not Copy: keep Copy handles to its signals and refetch.
    let (follows_data, follows_err, items_data, items_err, tags_data) = (follows.data, follows.error, items_q.data, items_q.error, tags_q.data);
    let (fq, iq, tq) = (follows.clone(), items_q.clone(), tags_q.clone());
    let rf_follows = Callback::new(move |_: ()| fq.refetch());
    let rf_items = Callback::new(move |_: ()| iq.refetch());
    let rf_tags = Callback::new(move |_: ()| tq.refetch());

    // Patched in place from the bus; a finished sweep is exactly when the list is stale.
    use_topic::<FeedSweepStatus>(TOPIC_FEED_SWEEP, move |s| {
        let was_running = sweep.with_untracked(|p| p.as_ref().map(|p| p.running).unwrap_or(false));
        let done_now = s.phase == "done" && (was_running || !s.running);
        sweep.set(Some(s));
        if was_running && done_now {
            rf_follows.run(());
            rf_items.run(());
            rf_tags.run(());
            grid_epoch.update(|e| *e += 1);
        }
    });
    use_topic::<EnrichState>(TOPIC_HARVEST_ENRICH, move |e| {
        let finished = e.phase == "done" && enrich.with_untracked(|p| p.as_ref().map(|p| p.running).unwrap_or(false));
        enrich.set(Some(e));
        if finished {
            rf_items.run(());
            rf_tags.run(());
            grid_epoch.update(|e| *e += 1);
        }
    });

    let list: Signal<Vec<HarvestItemOut>> = Signal::derive(move || items_data.get().map(|p| p.items.clone()).unwrap_or_default());
    let total = Signal::derive(move || items_data.get().map(|p| p.total as usize).unwrap_or(0));
    let flat_total = RwSignal::new(None::<usize>);
    let running = Signal::derive(move || sweep.with(|s| s.as_ref().map(|s| s.running).unwrap_or(false)));

    // -- actions ------------------------------------------------------------------
    let start_sweep = Arc::new(move |ids: Vec<i64>| {
        spawn_local(async move {
            match api::post::<_, FeedSweepStatus>("/follows/sweep", &FeedSweepRequest { source_ids: ids }).await {
                Ok(s) => {
                    notice.set(None);
                    sweep.set(Some(s));
                }
                Err(e) => notice.set(Some((true, e.message()))),
            }
        });
    });
    let stop_sweep = move || {
        spawn_local(async move {
            if let Ok(s) = api::send::<(), FeedSweepStatus>("DELETE", "/follows/sweep", &()).await {
                sweep.set(Some(s));
            }
        });
    };
    let put_settings = Arc::new(move |body: FollowSettingsIn| {
        spawn_local(async move {
            match api::put::<_, FollowsOut>("/follows/settings", &body).await {
                Ok(f) => crate::data::patch::<FollowsOut>("/follows", |v| *v = f),
                Err(e) => notice.set(Some((true, e.message()))),
            }
        });
    });
    let start_enrich = move |ids: Vec<i64>| {
        spawn_local(async move {
            match api::post::<_, EnrichState>("/harvest/enrich", &EnrichRequest { item_ids: ids }).await {
                Ok(s) => enrich.set(Some(s)),
                Err(e) => notice.set(Some((true, e.message()))),
            }
        });
    };
    let stop_enrich = move || {
        spawn_local(async move {
            if let Ok(s) = api::send::<(), EnrichState>("DELETE", "/harvest/enrich", &()).await {
                enrich.set(Some(s));
            }
        });
    };

    let queue_selected = move || {
        let ids: Vec<i64> = selected.get_untracked().into_iter().collect();
        if ids.is_empty() || busy_queue.get_untracked() {
            return;
        }
        busy_queue.set(true);
        spawn_local(async move {
            // Same flags as the label sweep's queue: finding unowned releases is the point,
            // and a subfolder would duplicate artists already at the downloads root.
            let req = QueueRequest { item_ids: ids.clone(), allow_unowned: true, target_subdir: Some(String::new()), label: Some("Feed".into()), ..Default::default() };
            match ops::queue(&req).await {
                Ok(r) => {
                    overrides.update(|m| {
                        for id in &ids {
                            m.insert(*id, "queued".into());
                        }
                    });
                    selected.set(HashSet::new());
                    notice.set(Some((false, ops::queue_notice(&r, "for download"))));
                }
                Err(e) => notice.set(Some((true, e.message()))),
            }
            busy_queue.set(false);
        });
    };
    let ignore_ids = Arc::new(move |ids: Vec<i64>| {
        if ids.is_empty() || busy_ignore.get_untracked() {
            return;
        }
        busy_ignore.set(true);
        // Optimistic: flip now, correct from the answers.
        let before: Vec<(i64, Option<String>)> = ids.iter().map(|id| (*id, overrides.with_untracked(|m| m.get(id).cloned()))).collect();
        spawn_local(async move {
            let (ok, err) = ops::toggle_ignore(ids.clone()).await;
            overrides.update(|m| {
                for (id, st) in ok {
                    m.insert(id, st);
                }
                for (id, prev) in &before {
                    if !m.contains_key(id) {
                        if let Some(p) = prev {
                            m.insert(*id, p.clone());
                        }
                    }
                }
            });
            selected.update(|s| s.clear());
            if let Some(e) = err {
                notice.set(Some((true, e)));
            }
            busy_ignore.set(false);
        });
    });

    let toggle_tag = {
        let go = go.clone();
        Arc::new(move |tag: String| {
            let mut s = slice.get_untracked();
            if let Some(i) = s.tags.iter().position(|t| *t == tag) {
                s.tags.remove(i);
            } else {
                s.tags.push(tag);
            }
            s.page = 0;
            go(s);
        })
    };
    let active_tags = Signal::derive(move || slice.with(|s| s.tags.clone()));

    // -- header -------------------------------------------------------------------
    let subtitle = Signal::derive(move || {
        let n = if slice.with(|s| s.flat) { flat_total.get() } else { items_data.get().map(|p| p.total as usize) };
        n.map(|n| format!("{} {}", format_count(n as i64), slice.with(|s| s.state.replace('_', " ")))).unwrap_or_default()
    });
    let ss = start_sweep.clone();
    let overflow = Callback::new(move |_| -> Vec<MenuEntry> {
        let mut v: Vec<MenuEntry> = vec![];
        let s = slice.get_untracked();
        if s.tags.is_empty() && s.state == "new" {
            let n = if s.flat { flat_total.get_untracked() } else { Some(total.get_untracked()) }.unwrap_or(0);
            let source = s.source.clone();
            v.push(
                MenuItem::new(format!("Queue all {} new", format_count(n as i64)))
                    .icon("download")
                    .disabled(n == 0)
                    .on(move || {
                        let source = source.clone();
                        spawn_local(async move {
                            if !crate::ds::confirm("Queue everything new?", &format!("All {n} new finds{} go to the download queue.", if source.is_empty() { String::new() } else { format!(" from {source}") }), "Queue all", false).await {
                                return;
                            }
                            let req = QueueRequest {
                                all_matching: true,
                                state: "new".into(),
                                source_kind: Some("follow".into()),
                                source_label: (!source.is_empty()).then_some(source),
                                allow_unowned: true,
                                target_subdir: Some(String::new()),
                                label: Some("Feed".into()),
                                ..Default::default()
                            };
                            match ops::queue(&req).await {
                                Ok(r) => {
                                    notice.set(Some((false, ops::queue_notice(&r, "for download"))));
                                    grid_epoch.update(|e| *e += 1);
                                    rf_items.run(());
                                }
                                Err(e) => notice.set(Some((true, e.message()))),
                            }
                        });
                    })
                    .into(),
            );
        }
        let ss = ss.clone();
        v.push(MenuItem::new("Check every source now").icon("refresh").on(move || ss(vec![])).into());
        v
    });
    let ss2 = start_sweep.clone();

    // -- grid (flat mode) ---------------------------------------------------------
    let fetcher: PageFetcher<HarvestItemOut> = Arc::new(move |req| {
        let s = slice.get_untracked();
        let url = items_url(&s, req.offset, req.limit);
        Box::pin(async move {
            let p: Page<HarvestItemOut> = api::get(&url).await?;
            Ok(PageRes { rows: p.items, total: p.total as usize })
        })
    });
    let on_toggle = Callback::new(move |id: i64| selected.update(|s| toggle(s, id)));
    let ignore_cb = {
        let ig = ignore_ids.clone();
        Callback::new(move |id: i64| ig(vec![id]))
    };
    let play_one = Callback::new(move |id: i64| {
        let cur = list.get_untracked();
        let from = cur.iter().position(|i| i.id == id);
        let cards = match from {
            Some(i) => cur[i..].iter().map(ops::card_of).collect(),
            None => vec![],
        };
        ops::play_cards(player, cards, false);
    });
    let grid_key = Signal::derive(move || format!("{}#{}", slice.with(|s| s.key()), grid_epoch.get()));
    let render_flat: Callback<(HarvestItemOut, f64), AnyView> = {
        Callback::new(move |(item, _w): (HarvestItemOut, f64)| {
            let id = item.id;
            let card = ops::card_of(&item);
            view! {
                <InboxCard item=item selected=Signal::derive(move || selected.with(|s| s.contains(&id))) overrides=overrides
                    on_toggle=on_toggle on_ignore=ignore_cb
                    on_play=Callback::new(move |_| ops::play_cards(player, vec![card.clone()], false)) />
            }
            .into_any()
        })
    };

    // -- view ---------------------------------------------------------------------
    let go_tab = go.clone();
    let go_view = go.clone();
    let go_src = go.clone();
    let go_page = go.clone();
    let tt_bar = toggle_tag.clone();
    let tt_rail = toggle_tag.clone();
    let ig_bar = ignore_ids.clone();
    let sweep_rail = start_sweep.clone();
    let ps = put_settings.clone();

    view! {
        <div class="page fd-page">
            <PageHeader title="Feed" subtitle=subtitle overflow=overflow
                actions=crate::ds::children(move || {
                    let ss2 = ss2.clone();
                    view! {
                        {move || if running.get() {
                            view! { <Button icon="x" title="Stop checking" on_click=move |_| stop_sweep()><span class="hide-sm">"Stop checking"</span></Button> }.into_any()
                        } else {
                            let ss2 = ss2.clone();
                            view! { <Button variant=Variant::Primary icon="refresh" title="Check every followed source for new releases. Nothing is downloaded: finds land here for review."
                                on_click=move |_| ss2(vec![])><span class="hide-sm">"Check now"</span></Button> }.into_any()
                        }}
                    }
                }) />
            <div class="fd-layout">
                <aside class="fd-rail" data-open=move || rail_open.get().to_string()>
                    <button type="button" class="fd-rail-toggle" aria-expanded=move || rail_open.get().to_string() on:click=move |_| rail_open.update(|o| *o = !*o)>
                        <Icon name="rss" />
                        <span>"Sources"</span>
                        <span class="mono faint">{move || follows_data.get().map(|f| f.sources.len().to_string()).unwrap_or_default()}</span>
                        <span class="spacer"></span>
                        <Icon name=crate::ds::dyn_icon(move || if rail_open.get() { "chevron-up" } else { "chevron-down" }) />
                    </button>
                    <div class="fd-rail-body">
                        <SweepStatusView sweep=sweep />
                        <FeedSettings data=follows_data put=ps.clone() />
                        {move || {
                            let sweep_rail = sweep_rail.clone();
                            let go_src = go_src.clone();
                            match (follows_data.get(), follows_err.get()) {
                                (Some(f), _) => view! {
                                    <SourcesList sources=f.sources.clone() active=Signal::derive(move || slice.with(|s| s.source.clone()))
                                        running=running sweep=sweep_rail
                                        on_filter=Callback::new(move |label: String| {
                                            let mut s = slice.get_untracked();
                                            s.source = if s.source == label { String::new() } else { label };
                                            s.page = 0;
                                            go_src(s);
                                        }) notice=notice />
                                }.into_any(),
                                (None, Some(e)) => view! { <ErrorPanel message=e.message() on_retry=rf_follows /> }.into_any(),
                                (None, None) => view! { <div class="fd-skel"><Skeleton height="34px" /><Skeleton height="34px" /><Skeleton height="34px" /></div> }.into_any(),
                            }
                        }}
                    </div>
                </aside>

                <section class="fd-main">
                    <div class="fd-bar">
                        <div class="fd-tabs" role="group" aria-label="Inbox state">
                            {STATE_TABS.into_iter().map(|(id, label)| {
                                let go_tab = go_tab.clone();
                                view! {
                                    <button type="button" class="fd-tab" aria-pressed=move || slice.with(|s| s.state == id).to_string()
                                        on:click=move |_| {
                                            let mut s = slice.get_untracked();
                                            s.state = id.to_string();
                                            s.page = 0;
                                            go_tab(s);
                                        }>{label}</button>
                                }
                            }).collect_view()}
                        </div>
                        <span class="spacer"></span>
                        <div class="segmented" role="group" aria-label="Layout">
                            <button type="button" aria-pressed=move || (!slice.with(|s| s.flat)).to_string() title="Grouped by label and artist, 200 per page"
                                on:click={ let g = go_view.clone(); move |_| { let mut s = slice.get_untracked(); s.flat = false; s.page = 0; g(s) } }>
                                <Icon name="layers" /><span class="hide-sm">"Grouped"</span></button>
                            <button type="button" aria-pressed=move || slice.with(|s| s.flat).to_string() title="One continuous grid"
                                on:click={ let g = go_view.clone(); move |_| { let mut s = slice.get_untracked(); s.flat = true; s.page = 0; g(s) } }>
                                <Icon name="grid" /><span class="hide-sm">"Flat"</span></button>
                        </div>
                    </div>

                    <div class="fd-actions">
                        {move || (!slice.with(|s| s.flat) && !list.with(|l| l.is_empty())).then(|| view! {
                            <Button size=Size::Sm icon="play" title="Play these finds" on_click=move |_| ops::play_cards(player, list.get_untracked().iter().map(ops::card_of).collect(), false)><span class="hide-sm">"Play"</span></Button>
                            <Button size=Size::Sm icon="shuffle" title="Shuffle these finds" on_click=move |_| ops::play_cards(player, list.get_untracked().iter().map(ops::card_of).collect(), true)><span class="hide-sm">"Shuffle"</span></Button>
                        })}
                        {move || (!slice.with(|s| s.flat) && !list.with(|l| l.is_empty())).then(|| {
                            let all = list.with(|l| l.len());
                            let n = selected.with(|s| s.len());
                            view! {
                                <Button size=Size::Sm variant=Variant::Ghost on_click=move |_| {
                                    let ids: Vec<i64> = list.get_untracked().iter().map(|i| i.id).collect();
                                    selected.update(|s| if s.len() == ids.len() { s.clear() } else { *s = ids.into_iter().collect() });
                                }>{if n == all && n > 0 { "Clear selection".to_string() } else { format!("Select all {all}") }}</Button>
                            }
                        })}
                        <span class="spacer"></span>
                        {
                            let ig = ig_bar.clone();
                            view! {
                                <Button icon=crate::ds::dyn_icon(move || if slice.with(|s| s.state == "ignored") { "eye" } else { "eye-off" })
                                    title="Ignore the selected finds: they leave the new tab and are never suggested again"
                                    disabled=Signal::derive(move || selected.with(|s| s.is_empty())) busy=busy_ignore
                                    on_click=move |_| ig(selected.get_untracked().into_iter().collect())>
                                    <span class="hide-sm">{move || if slice.with(|s| s.state == "ignored") { "Restore " } else { "Ignore " }}</span>{move || { let n = selected.with(|s| s.len()); if n > 0 { n.to_string() } else { String::new() } }}
                                </Button>
                            }
                        }
                        <Button variant=Variant::Primary icon="download" disabled=Signal::derive(move || selected.with(|s| s.is_empty())) busy=busy_queue
                            title="Queue the selected finds for download" on_click=move |_| queue_selected()>
                            <span class="hide-sm">"Queue "</span>{move || { let n = selected.with(|s| s.len()); if n > 0 { n.to_string() } else { String::new() } }}
                        </Button>
                    </div>

                    {move || notice.get().map(|(is_err, msg)| view! {
                        <div class=if is_err { "banner danger" } else { "banner" } role="status">
                            <Icon name=if is_err { "alert" } else { "check-circle" } />
                            <span class="grow">{msg}</span>
                            <Button size=Size::Sm variant=Variant::Ghost icon="x" title="Dismiss" on_click=move |_| notice.set(None) />
                        </div>
                    })}

                    <div class="fd-tagbar">
                        <span class="section-title"><Icon name="tag" />"Tags"</span>
                        {
                            let tt = tt_bar.clone();
                            move || {
                                let tt = tt.clone();
                                let counts = tags_data.get().map(|t| (*t).clone()).unwrap_or_default();
                                let mut shown: Vec<TagCount> = counts.clone();
                                for t in active_tags.get() {
                                    if !shown.iter().any(|c| c.tag == t) {
                                        shown.insert(0, TagCount { tag: t, count: 0 });
                                    }
                                }
                                let empty = tags_data.get().map(|t| t.is_empty()).unwrap_or(false) && shown.is_empty();
                                view! {
                                    {shown.into_iter().map(|c| {
                                        let tag = c.tag.clone();
                                        let tag2 = c.tag.clone();
                                        let tt = tt.clone();
                                        view! {
                                            <button type="button" class="chip" aria-pressed=move || active_tags.with(|a| a.contains(&tag)).to_string()
                                                title="Filter the feed by this tag" on:click=move |_| tt(tag2.clone())>
                                                {c.tag.clone()}{(c.count > 0).then(|| view! { <span class="mono faint">{format_count(c.count)}</span> })}
                                            </button>
                                        }
                                    }).collect_view()}
                                    {empty.then(|| view! { <span class="faint small">"none known yet: band pages carry no tags, only release pages do"</span> })}
                                }
                            }
                        }
                        <EnrichControl enrich=enrich list=list start=Callback::new(start_enrich) stop=Callback::new(move |_| stop_enrich()) />
                    </div>

                    {move || {
                        let s = slice.get();
                        let (tt_rail, go_page, fetcher) = (tt_rail.clone(), go_page.clone(), fetcher.clone());
                        if s.flat {
                                            view! {
                                <CardGrid fetch=fetcher.clone() source_key=grid_key min_card_w=140.0 gap=10.0 meta_h=CARD_META_H render=render_flat
                                    total_out=flat_total entities=vec!["harvest"]
                                    empty=move || view! { <EmptyHint slice=slice /> } />
                            }.into_any()
                        } else {
                            view! {
                                <div class="fd-scroll">
                                    {move || match (items_data.get(), items_err.get()) {
                                        (None, Some(e)) => view! { <ErrorPanel message=e.message() on_retry=rf_items /> }.into_any(),
                                        (None, None) => view! { <SkeletonGrid /> }.into_any(),
                                        (Some(_), _) => {
                                            let l = list.get();
                                            if l.is_empty() {
                                                view! { <EmptyHint slice=slice /> }.into_any()
                                            } else {
                                                view! {
                                                    <GroupedList list=l selected=selected collapsed=collapsed overrides=overrides
                                                        on_toggle=on_toggle on_ignore=ignore_cb on_play=play_one
                                                        on_tag=Callback::new({ let tt = tt_rail.clone(); move |t: String| tt(t) })
                                                        active_tags=active_tags />
                                                }.into_any()
                                            }
                                        }
                                    }}
                                    {move || {
                                        let s = slice.get();
                                        let t = total.get();
                                        let pages = t.div_ceil(PAGE_SIZE).max(1);
                                        let go_page = go_page.clone();
                                        (pages > 1).then(|| {
                                            let (gp, gn) = (go_page.clone(), go_page.clone());
                                            let page = s.page;
                                            let (first, last) = (page == 0, page + 1 >= pages);
                                            let (s1, s2) = (s.clone(), s.clone());
                                            view! {
                                                <nav class="fd-pager" aria-label="Pages">
                                                    <Button size=Size::Sm icon="chevron-left" disabled=first on_click=move |_| { let mut n = s1.clone(); n.page = page.saturating_sub(1); gp(n) }>"Prev"</Button>
                                                    <span class="mono">{format!("{}\u{2013}{} of {}", page * PAGE_SIZE + 1, ((page + 1) * PAGE_SIZE).min(t), format_count(t as i64))}</span>
                                                    <Button size=Size::Sm disabled=last on_click=move |_| { let mut n = s2.clone(); n.page = (page + 1).min(pages - 1); gn(n) }>"Next"<Icon name="chevron-right" /></Button>
                                                </nav>
                                            }
                                        })
                                    }}
                                </div>
                            }.into_any()
                        }
                    }}
                </section>
            </div>
        </div>
    }
}

#[component]
fn SkeletonGrid() -> impl IntoView {
    view! {
        <div class="ib-grid" aria-busy="true">
            {(0..12).map(|_| view! { <div><div class="art skeleton"></div><div style="padding-top:8px"><Skeleton width="80%" height="11px" /></div></div> }).collect_view()}
        </div>
    }
}

#[component]
fn EmptyHint(slice: Memo<Slice>) -> impl IntoView {
    let (title, hint) = {
        let s = slice.get_untracked();
        if !s.tags.is_empty() {
            ("Nothing carries these tags".to_string(), format!("Nothing here carries {}. Most items only learn their tags when their release page is fetched: use \u{201c}Fetch tags\u{201d}.", s.tags.join(" + ")))
        } else if !s.source.is_empty() {
            (format!("Nothing {} from {}", if s.state == "all" { String::new() } else { s.state.replace('_', " ") }, s.source), "Pick another source or state.".to_string())
        } else {
            ("Nothing here yet".to_string(), "Press \u{201c}Check now\u{201d} to sweep your follows.".to_string())
        }
    };
    view! { <EmptyState title=title hint=hint icon="rss" /> }
}

// ---------------------------------------------------------------------------------
// Grouped page
// ---------------------------------------------------------------------------------

#[component]
fn GroupedList(
    list: Vec<HarvestItemOut>,
    selected: RwSignal<HashSet<i64>>,
    collapsed: RwSignal<HashSet<String>>,
    overrides: StateOverrides,
    on_toggle: Callback<i64>,
    on_ignore: Callback<i64>,
    on_play: Callback<i64>,
    on_tag: Callback<String>,
    #[prop(into)] active_tags: Signal<Vec<String>>,
) -> impl IntoView {
    let segments = build_feed_segments(&list);
    let card = move |item: HarvestItemOut| {
        let id = item.id;
        view! {
            <InboxCard item=item selected=Signal::derive(move || selected.with(|s| s.contains(&id))) overrides=overrides
                on_toggle=on_toggle on_ignore=on_ignore on_play=on_play on_tag=on_tag active_tags=active_tags />
        }
    };
    let grid = move |items: Vec<HarvestItemOut>| view! { <div class="ib-grid">{items.into_iter().map(card).collect_view()}</div> };
    view! {
        <div class="fd-groups">
            {segments.into_iter().map(|seg| match seg {
                FeedSegment::Loose(items) => grid(items).into_any(),
                FeedSegment::Label(g) => {
                    let ids: Vec<i64> = g.items.iter().map(|i| i.id).collect();
                    view! {
                        <FeedGroup level_artist=false name=g.name.clone() key=g.key.clone() ids=ids selected=selected collapsed=collapsed>
                            <div class="fd-sub">
                                {g.artists.into_iter().map(|a| {
                                    let ids: Vec<i64> = a.items.iter().map(|i| i.id).collect();
                                    view! {
                                        <FeedGroup level_artist=true name=a.name.clone() key=a.key.clone() ids=ids selected=selected collapsed=collapsed>
                                            {grid(a.items)}
                                        </FeedGroup>
                                    }
                                }).collect_view()}
                                {(!g.rest.is_empty()).then(|| grid(g.rest))}
                            </div>
                        </FeedGroup>
                    }.into_any()
                }
            }).collect_view()}
        </div>
    }
}

/// A collapsible group with the header both levels share: name as a chevron toggle, the count,
/// and one pill that selects or clears the whole group (nested rows included).
#[component]
fn FeedGroup(
    level_artist: bool,
    name: String,
    key: String,
    ids: Vec<i64>,
    selected: RwSignal<HashSet<i64>>,
    collapsed: RwSignal<HashSet<String>>,
    children: Children,
) -> impl IntoView {
    let n = ids.len();
    let ids = StoredValue::new(ids);
    let shut = {
        let key = key.clone();
        Signal::derive(move || collapsed.with(|c| c.contains(&key)))
    };
    let picked = Signal::derive(move || ids.with_value(|v| selected.with(|s| v.iter().filter(|i| s.contains(i)).count())));
    let all = Signal::derive(move || picked.get() == n);
    let label_open = format!("Expand {name}");
    let label_shut = format!("Collapse {name}");
    let key2 = key.clone();
    view! {
        <section class=if level_artist { "fd-group sub" } else { "fd-group" }>
            <div class="fd-group-head">
                <button type="button" class="fd-group-toggle" aria-expanded=move || (!shut.get()).to_string()
                    aria-label=move || if shut.get() { label_open.clone() } else { label_shut.clone() }
                    on:click=move |_| { let k = key2.clone(); collapsed.update(|c| if !c.remove(&k) { c.insert(k); }) }>
                    <Icon name=crate::ds::dyn_icon(move || if shut.get() { "chevron-right" } else { "chevron-down" }) />
                    <span class="truncate">{name}</span>
                </button>
                <span class="faint small"><span class="mono">{n}</span>" releases"</span>
                <button type="button" class="chip" aria-pressed=move || all.get().to_string() on:click=move |_| {
                    let v = ids.get_value();
                    selected.update(|s| if v.iter().all(|i| s.contains(i)) { for i in &v { s.remove(i); } } else { for i in v { s.insert(i); } });
                }>{move || if all.get() { format!("Clear {n}") } else { format!("Select {n}") }}</button>
                // Visible even collapsed, so a folded group cannot ride along into a queue press unseen.
                {move || (picked.get() > 0 && !all.get()).then(|| view! { <span class="mono fd-sel">{format!("{} selected", picked.get())}</span> })}
                {move || (shut.get() && all.get()).then(|| view! { <span class="mono fd-sel">"all selected"</span> })}
            </div>
            <div class:hidden=move || shut.get()>{children()}</div>
        </section>
    }
}

// ---------------------------------------------------------------------------------
// Rail
// ---------------------------------------------------------------------------------

#[component]
fn SweepStatusView(sweep: RwSignal<Option<FeedSweepStatus>>) -> impl IntoView {
    view! {
        {move || sweep.get().filter(|s| s.phase != "idle").map(|s| {
            let progress = s.total.filter(|t| *t > 0).map(|t| (s.done as f64 / t as f64).clamp(0.0, 1.0));
            view! {
                <div class="fd-sweep" role="status" aria-live="polite">
                    {match s.phase.as_str() {
                        "harvesting" => view! {
                            <div class="fd-sweep-line"><Icon name="refresh" class="spin".to_string() />
                                <span>"Checking "<span class="mono">{s.done}</span>{s.total.map(|t| view! { " / "<span class="mono">{t}</span> })}</span></div>
                            <Meter value=Signal::derive(move || progress) label="Sweep progress" />
                            {s.current.clone().map(|c| view! { <div class="faint small truncate">{c}</div> })}
                        }.into_any(),
                        "done" => view! {
                            <div class="fd-sweep-line"><Icon name="check-circle" />
                                <span>"Found "<span class="mono">{format_count(s.new)}</span>" new of "<span class="mono">{format_count(s.seen)}</span>" seen"
                                {(s.no_url > 0).then(|| view! { <span class="faint">{format!(" \u{b7} {} without a Bandcamp page skipped", s.no_url)}</span> })}</span></div>
                            {s.error.clone().map(|e| view! { <div class="small danger-text">{e}</div> })}
                        }.into_any(),
                        "failed" => view! { <div class="fd-sweep-line danger-text"><Icon name="alert" /><span>{s.error.clone().unwrap_or_else(|| "Sweep failed.".into())}</span></div> }.into_any(),
                        _ => view! { <span class="faint small">{s.phase.clone()}</span> }.into_any(),
                    }}
                </div>
            }
        })}
    }
}

#[component]
fn FeedSettings(data: RwSignal<Option<Arc<FollowsOut>>>, put: Arc<dyn Fn(FollowSettingsIn) + Send + Sync>) -> impl IntoView {
    let labels = Signal::derive(move || data.get().map(|f| f.include_library_labels).unwrap_or(true));
    let artists = Signal::derive(move || data.get().map(|f| f.include_library_artists).unwrap_or(true));
    let poll = RwSignal::new("12".to_string());
    Effect::new(move |_| {
        if let Some(f) = data.get() {
            poll.set(format!("{}", f.poll_hours.round() as i64));
        }
    });
    let (p1, p2, p3) = (put.clone(), put.clone(), put);
    let options: Vec<crate::ds::SelectOption> = POLL_CHOICES.iter().map(|(v, l)| crate::ds::SelectOption::new(*v, *l)).collect();
    view! {
        <div class="fd-settings">
            <label class="check" title="Sweep every label in your library that has a Bandcamp page pinned.">
                <input type="checkbox" prop:checked=move || labels.get() on:change=move |ev| p1(FollowSettingsIn { include_library_labels: Some(event_target_checked(&ev)), ..Default::default() }) />
                "Library labels"
            </label>
            <label class="check" title="Sweep every artist in your library that has a Bandcamp page pinned.">
                <input type="checkbox" prop:checked=move || artists.get() on:change=move |ev| p2(FollowSettingsIn { include_library_artists: Some(event_target_checked(&ev)), ..Default::default() }) />
                "Library artists"
            </label>
            <div class="fd-poll">
                <span class="faint small">"Check automatically"</span>
                <Select options=options value=poll aria_label="Automatic check interval"
                    on_change=Callback::new(move |v: String| p3(FollowSettingsIn { poll_hours: v.parse().ok(), ..Default::default() })) />
            </div>
        </div>
    }
}

#[component]
fn SourcesList(
    sources: Vec<FollowOut>,
    #[prop(into)] active: Signal<String>,
    #[prop(into)] running: Signal<bool>,
    sweep: Arc<dyn Fn(Vec<i64>) + Send + Sync>,
    on_filter: Callback<String>,
    notice: RwSignal<Option<(bool, String)>>,
) -> impl IntoView {
    if sources.is_empty() {
        return view! {
            <p class="faint small fd-none">"Nothing followed yet. Save a query or follow an artist or label from Explore; the library toggles above cover your own shelves."</p>
        }.into_any();
    }
    view! {
        <h2 class="section-title fd-rail-title"><Icon name="rss" />"Sources"</h2>
        <ul class="fd-sources">
            {sources.into_iter().map(|s| {
                let (id, label) = (s.id, s.label.clone());
                let (l1, l2, l3) = (label.clone(), label.clone(), label.clone());
                let is_active = Signal::derive(move || active.get() == l1);
                let sweep = sweep.clone();
                let ago = s.last_run_at.as_deref().and_then(ops::ago_of);
                let kind = s.kind.clone();
                let can_follow = kind != "search";
                let enabled = s.enabled;
                view! {
                    <li class="fd-source" data-active=move || is_active.get().to_string()>
                        <div class="fd-src-top">
                            {can_follow.then(|| view! {
                                <button type="button" class="fd-src-btn" aria-pressed=enabled.to_string()
                                    title=if enabled { "Followed: the sweep checks it. Click to pause." } else { "Paused: the sweep skips it. Click to follow." }
                                    aria-label=if enabled { "Pause this follow" } else { "Resume this follow" }
                                    on:click=move |_| {
                                        spawn_local(async move {
                                            match api::patch::<_, FollowOut>(&format!("/follows/{id}"), &FollowPatch { enabled: Some(!enabled), label: None }).await {
                                                Ok(f) => crate::data::patch::<FollowsOut>("/follows", |v| if let Some(x) = v.sources.iter_mut().find(|x| x.id == id) { *x = f }),
                                                Err(e) => notice.set(Some((true, e.message()))),
                                            }
                                        });
                                    }><Icon name="rss" /></button>
                            })}
                            <button type="button" class="fd-src-name truncate" title=l3 aria-pressed=move || is_active.get().to_string()
                                on:click=move |_| on_filter.run(l2.clone())>{label}</button>
                            {can_follow.then(|| view! {
                                <button type="button" class="fd-src-btn" title="Check this source now" aria-label="Check this source now" disabled=move || running.get()
                                    on:click=move |_| sweep(vec![id])><Icon name="refresh" /></button>
                            })}
                            <button type="button" class="fd-src-btn danger" title="Forget this follow" aria-label="Forget this follow" on:click=move |_| {
                                spawn_local(async move {
                                    if !crate::ds::confirm("Forget this follow?", "Its finds stay in the inbox; the sweep stops checking it.", "Forget", true).await {
                                        return;
                                    }
                                    match api::call("DELETE", &format!("/follows/{id}")).await {
                                        Ok(()) => crate::data::patch::<FollowsOut>("/follows", |v| v.sources.retain(|x| x.id != id)),
                                        Err(e) => notice.set(Some((true, e.message()))),
                                    }
                                });
                            }><Icon name="trash" /></button>
                        </div>
                        <div class="fd-src-meta">
                            <span class="badge">{kind.clone()}</span>
                            {(s.items_new > 0).then(|| view! { <span class="mono faint small" title="New finds, all time">{format!("{} new", format_count(s.items_new))}</span> })}
                            {match (s.last_error.clone(), ago) {
                                (Some(e), _) => view! { <span class="danger-text small" title=e><Icon name="alert" />"check failed"</span> }.into_any(),
                                (None, Some(a)) => view! { <span class="faint small" title="Last checked">{format!("checked {a}")}</span> }.into_any(),
                                _ => view! { <span class="faint small">"never checked"</span> }.into_any(),
                            }}
                            {(!enabled && can_follow).then(|| view! { <span class="faint small">"paused"</span> })}
                        </div>
                    </li>
                }
            }).collect_view()}
        </ul>
    }.into_any()
}

#[component]
fn EnrichControl(
    enrich: RwSignal<Option<EnrichState>>,
    #[prop(into)] list: Signal<Vec<HarvestItemOut>>,
    start: Callback<Vec<i64>>,
    stop: Callback<()>,
) -> impl IntoView {
    view! {
        {move || {
            let e = enrich.get();
            if e.as_ref().map(|e| e.running).unwrap_or(false) {
                let e = e.unwrap();
                view! {
                    <span class="mono faint small"><Icon name="refresh" class="spin".to_string() />{format!(" fetching tags {}/{}", e.done, e.total)}</span>
                    <button type="button" class="chip" on:click=move |_| stop.run(())>"stop"</button>
                }.into_any()
            } else {
                let n = list.with(|l| l.len());
                view! {
                    {(n > 0).then(|| view! {
                        <button type="button" class="chip" title="Fetch each shown release's page and record its tags: a few minutes per page, politely rate-limited"
                            on:click=move |_| start.run(list.get_untracked().iter().map(|i| i.id).collect())>{format!("Fetch tags for these {n}")}</button>
                    })}
                    {e.filter(|e| e.phase == "done").map(|e| view! { <span class="faint small">{format!("tagged {} of {}{}", e.tagged, e.total, if e.error.as_deref() == Some("Stopped") { " (stopped)" } else { "" })}</span> })}
                }.into_any()
            }
        }}
    }
}
