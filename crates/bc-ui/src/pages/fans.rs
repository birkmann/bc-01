//! Fans: Bandcamp accounts whose lists are followed here, mine and other people's. A rail of
//! fans, one fan on the right: wishlist, collection or both. Walk it (progress patched in place
//! from `fans.walk`), play it through (server-built `QueueSource::Fan`, sequential or a seeded
//! shuffle), pick records or queue the lot onto their shelf or into my library, adopt, unfollow.
use std::collections::HashSet;
use std::sync::Arc;

use bc_types::Page;
use bc_types::bandcamp::*;
use bc_types::library::{AdoptRequest, AdoptResult};
use bc_types::player::{FanOrder, PlayerCommand, QueueSource};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};

use crate::api;
use crate::data::{QuerySpec, use_query, use_topic};
use crate::ds::{Button, EmptyState, ErrorPanel, Icon, Meter, PageHeader, SearchInput, Size, Skeleton, Variant, use_debounced};
use crate::logic::format::format_count;
use crate::player::use_player;
use crate::util::qs_pairs;
use crate::widgets::{CardGrid, PageFetcher, PageRes};

mod logic;

use super::feed::card::{CARD_META_H, InboxCard, StateOverrides};
use super::feed::ops;
use logic::*;

const FANS_KEY: &str = "/fans";

#[derive(Clone, PartialEq)]
enum PanelState {
    Loading,
    Error(String),
    Empty,
    Panel(i64),
}

/// A grid row: the item and its place in the listing (to play on from "here").
#[derive(Clone)]
struct FanRow {
    item: HarvestItemOut,
    index: usize,
}

fn patch_fans(f: impl FnOnce(&mut Vec<FanOut>)) {
    crate::data::patch::<Vec<FanOut>>(FANS_KEY, f);
}

#[component]
pub fn FansPage() -> impl IntoView {
    let params = use_params_map();
    let navigate = use_navigate();
    let fans_q = use_query::<Vec<FanOut>>(|| Some(QuerySpec::new(FANS_KEY, &["fan"])));
    let (fans, fans_err) = (fans_q.data, fans_q.error);
    let fq = fans_q.clone();
    let refetch_fans = Callback::new(move |_: ()| fq.refetch());
    let epoch = RwSignal::new(0u32);

    // The walk reports through the bus: patch the fan in place; only its end refetches (counts moved).
    use_topic::<WalkState>(TOPIC_FANS_WALK, move |w| {
        let Some(id) = w.fan_id else { return };
        let end = matches!(w.phase.as_str(), "done" | "failed") && !w.running;
        patch_fans(|v| {
            if let Some(f) = v.iter_mut().find(|f| f.id == id) {
                f.walk = Some(w.clone());
            }
        });
        if end {
            refetch_fans.run(());
            epoch.update(|e| *e += 1);
        }
    });

    let wanted = Memo::new(move |_| params.with(|p| p.get("id").and_then(|s| s.parse::<i64>().ok())));
    let selected = Memo::new(move |_| {
        let list = fans.get()?;
        wanted.get().and_then(|w| list.iter().find(|f| f.id == w).map(|f| f.id)).or_else(|| list.first().map(|f| f.id))
    });
    // A stale link to a fan that was removed falls back to the first one.
    {
        let navigate = navigate.clone();
        Effect::new(move |_| {
            if let (Some(w), Some(list)) = (wanted.get(), fans.get()) {
                if !list.iter().any(|f| f.id == w) {
                    navigate("/fans", NavigateOptions { replace: true, ..Default::default() });
                }
            }
        });
    }

    // Only a change of *which* panel to show re-renders it: walk patches must not rebuild the fan.
    let view_state = Memo::new(move |_| match (selected.get(), fans_err.get(), fans.with(|f| f.is_some())) {
        (Some(id), _, _) => PanelState::Panel(id),
        (None, Some(e), false) => PanelState::Error(e.message()),
        (None, _, false) => PanelState::Loading,
        (None, _, true) => PanelState::Empty,
    });
    let subtitle = Signal::derive(move || fans.get().map(|f| format!("{} followed", f.len())).unwrap_or_default());
    view! {
        <div class="page fn-page">
            <PageHeader title="Fans" subtitle=subtitle />
            <div class="fn-layout">
                <FanRail fans=fans selected=Signal::derive(move || selected.get()) loading=Signal::derive(move || fans_q.loading.get()) />
                <div class="fn-main">
                    {move || match view_state.get() {
                        PanelState::Panel(id) => view! { <FanPanel id=id fans=fans epoch=epoch refetch=refetch_fans /> }.into_any(),
                        PanelState::Error(e) => view! { <ErrorPanel message=e on_retry=refetch_fans /> }.into_any(),
                        PanelState::Loading => view! { <div class="fn-skel"><Skeleton height="120px" /><Skeleton height="40px" /></div> }.into_any(),
                        PanelState::Empty => view! {
                            <EmptyState title="Follow a fan to start" icon="users"
                                hint="Paste a Bandcamp profile, wishlist or collection link, or just a username. Your own goes first: walking it recognises what you already have and queues only the rest. Public lists need no Bandcamp cookie." />
                        }.into_any(),
                    }}
                </div>
            </div>
        </div>
    }
}

// ---------------------------------------------------------------------------------------------
// Rail
// ---------------------------------------------------------------------------------------------

#[component]
fn FanRail(fans: RwSignal<Option<Arc<Vec<FanOut>>>>, #[prop(into)] selected: Signal<Option<i64>>, #[prop(into)] loading: Signal<bool>) -> impl IntoView {
    let navigate = use_navigate();
    let url = RwSignal::new(String::new());
    let adding = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let form_open = RwSignal::new(false);
    let add = {
        let navigate = navigate.clone();
        Arc::new(move || {
            let u = url.get_untracked().trim().to_string();
            if u.is_empty() || adding.get_untracked() {
                return;
            }
            adding.set(true);
            error.set(None);
            let navigate = navigate.clone();
            spawn_local(async move {
                match api::post::<_, FanOut>("/fans", &AddFanRequest { url: u, walk: true }).await {
                    Ok(f) => {
                        url.set(String::new());
                        form_open.set(false);
                        let id = f.id;
                        patch_fans(|v| if !v.iter().any(|x| x.id == id) { v.push(f) });
                        navigate(&format!("/fans/{id}"), NavigateOptions::default());
                    }
                    Err(e) => error.set(Some(e.message())),
                }
                adding.set(false);
            });
        })
    };
    let add2 = add.clone();
    view! {
        <aside class="fn-rail">
            <form class=move || if form_open.get() { "fn-add open" } else { "fn-add" } on:submit=move |ev| { ev.prevent_default(); add() }>
                <input class="input mono" prop:value=move || url.get() on:input=move |ev| url.set(event_target_value(&ev))
                    placeholder="bandcamp.com/name or a username" spellcheck="false" autocomplete="off" aria-label="Profile link or username" />
                <Button variant=Variant::Primary icon="plus" busy=adding disabled=Signal::derive(move || url.get().trim().is_empty()) kind="submit" on_click=move |_| add2()>
                    {move || if adding.get() { "Checking\u{2026}" } else { "Follow fan" }}
                </Button>
                {move || error.get().map(|e| view! { <p class="small danger-text fn-add-err">{e}</p> })}
            </form>
            <button type="button" class="chip fn-add-toggle" aria-expanded=move || form_open.get().to_string() on:click=move |_| form_open.update(|o| *o = !*o)><Icon name="plus" />"Follow"</button>
            <ul class="fn-list" aria-label="Followed fans">
                {move || fans.get().map(|list| list.iter().map(|f| {
                    let id = f.id;
                    let is_self = f.is_self;
                    let name = fan_name(f);
                    let walking = f.walk.as_ref().map(|w| w.running).unwrap_or(false);
                    let waiting = f.walk.as_ref().map(|w| w.phase == "queued").unwrap_or(false);
                    let items = f.items;
                    view! {
                        <li>
                            <a href=format!("/fans/{id}") class="fn-fan" aria-current=move || (selected.get() == Some(id)).then_some("page")>
                                <Icon name=if is_self { "heart-fill" } else { "heart" } class=if is_self { "fn-self".to_string() } else { String::new() } />
                                <span class="truncate grow">{name}</span>
                                {if walking { view! { <Icon name="refresh" class="spin".to_string() /> }.into_any() }
                                 else if waiting { view! { <span class="faint small">"waiting"</span> }.into_any() }
                                 else { view! { <span class="mono faint small">{format_count(items)}</span> }.into_any() }}
                            </a>
                        </li>
                    }
                }).collect_view())}
                {move || (loading.get() && fans.with(|f| f.is_none())).then(|| view! { <li><Skeleton height="34px" /></li> })}
                {move || fans.with(|f| f.as_ref().map(|f| f.is_empty()).unwrap_or(false)).then(|| view! { <li class="faint small fn-none">"No fans followed yet."</li> })}
            </ul>
        </aside>
    }
}

// ---------------------------------------------------------------------------------------------
// One fan
// ---------------------------------------------------------------------------------------------

#[component]
fn FanPanel(id: i64, fans: RwSignal<Option<Arc<Vec<FanOut>>>>, epoch: RwSignal<u32>, refetch: Callback<()>) -> impl IntoView {
    let navigate = use_navigate();
    let player = use_player();
    let fan: Signal<FanOut> = Signal::derive(move || fans.with(|f| f.as_ref().and_then(|l| l.iter().find(|x| x.id == id).cloned()).unwrap_or_default()));
    let first = fan.get_untracked();
    // List, state and search live in the query string: a reload or shared link lands on the same slice.
    let url_q = use_query_map();
    let (init_list, init_state, init_q) = url_q.with_untracked(|m| (m.get("list").and_then(|s| ListTab::from_param(&s)), m.get("state"), m.get("q")));
    let list = RwSignal::new(init_list.unwrap_or_else(|| default_list(&first)));
    let state = RwSignal::new(init_state.filter(|s| STATE_TABS.iter().any(|(id, _)| id == s)).unwrap_or_else(|| default_state(&first).to_string()));
    let q = RwSignal::new(init_q.unwrap_or_default());
    let qd = use_debounced(q, 250);
    {
        let navigate = navigate.clone();
        let (dl, ds) = (default_list(&first), default_state(&first));
        Effect::new(move |prev: Option<()>| {
            let (l, st, qv) = (list.get(), state.get(), qd.get());
            if prev.is_none() {
                return;
            }
            let mut pairs: Vec<(String, String)> = vec![];
            if l != dl {
                pairs.push(("list".into(), l.param().into()));
            }
            if st != ds {
                pairs.push(("state".into(), st));
            }
            if !qv.trim().is_empty() {
                pairs.push(("q".into(), qv.trim().into()));
            }
            navigate(&format!("/fans/{id}{}", qs_pairs(&pairs)), NavigateOptions { replace: true, ..Default::default() });
        });
    }
    let selected: RwSignal<HashSet<i64>> = RwSignal::new(HashSet::new());
    let overrides: StateOverrides = RwSignal::new(Default::default());
    let notice = RwSignal::new(None::<(bool, String)>);
    // Where a foreign fan's downloads go: their own shelf, apart from my library, unless asked into it.
    let into_library = RwSignal::new(first.is_self);
    // One folder of flat files is the point of exporting my own wishlist; others read better as <artist>/<album>.
    let single_folder = RwSignal::new(first.is_self);
    let include_in_library = RwSignal::new(false);
    let needs_confirm = RwSignal::new(false);
    let busy_queue = RwSignal::new(false);
    let busy_play = RwSignal::new(None::<FanOrder>);
    let options_open = RwSignal::new(false);
    let total = RwSignal::new(None::<usize>);

    Effect::new(move |_| {
        (list.get(), state.get(), qd.get());
        selected.set(HashSet::new());
        overrides.set(Default::default());
        needs_confirm.set(false);
    });

    let counts = Signal::derive(move || counts_for(&fan.get(), list.get()));
    let count_new = Signal::derive(move || count_for_state(&counts.get(), "new"));
    let list_total = Signal::derive(move || list_items(&fan.get(), list.get()));
    let walking = Signal::derive(move || fan.with(|f| f.walk.as_ref().map(|w| w.running).unwrap_or(false)));
    let waiting = Signal::derive(move || fan.with(|f| f.walk.as_ref().map(|w| w.phase == "queued").unwrap_or(false)));

    // -- grid ------------------------------------------------------------------------------
    let base_pairs = move || -> Vec<(String, String)> {
        let mut p = vec![("fan_id".to_string(), id.to_string()), ("tab".into(), list.get_untracked().param().into()), ("state".into(), state.get_untracked()), ("order".into(), "position".into())];
        let qv = qd.get_untracked();
        if !qv.trim().is_empty() {
            p.push(("q".into(), qv.trim().to_string()));
        }
        p
    };
    let fetcher: PageFetcher<FanRow> = Arc::new(move |req| {
        let mut p = base_pairs();
        p.push(("offset".into(), req.offset.to_string()));
        p.push(("limit".into(), req.limit.to_string()));
        let url = format!("/harvest/items{}", qs_pairs(&p));
        Box::pin(async move {
            let page: Page<HarvestItemOut> = api::get(&url).await?;
            let rows = page.items.into_iter().enumerate().map(|(i, item)| FanRow { item, index: req.offset + i }).collect();
            Ok(PageRes { rows, total: page.total as usize })
        })
    });
    let grid_key = Signal::derive(move || format!("{id}|{}|{}|{}|{}", list.get().param(), state.get(), qd.get(), epoch.get()));

    // -- actions ---------------------------------------------------------------------------
    let walk = move || {
        let tabs = walk_tabs(list.get_untracked());
        spawn_local(async move {
            match api::post::<_, FanOut>(&format!("/fans/{id}/walk"), &WalkRequest { queue_new: None, tabs }).await {
                Ok(f) => patch_fans(|v| if let Some(x) = v.iter_mut().find(|x| x.id == id) { *x = f }),
                Err(e) => notice.set(Some((true, e.message()))),
            }
        });
    };
    let stop_walk = move || {
        spawn_local(async move {
            if let Ok(f) = api::send::<(), FanOut>("DELETE", &format!("/fans/{id}/walk"), &()).await {
                patch_fans(|v| if let Some(x) = v.iter_mut().find(|x| x.id == id) { *x = f });
            }
        });
    };
    let unfollow = {
        let navigate = navigate.clone();
        Arc::new(move || {
            let f = fan.get_untracked();
            let navigate = navigate.clone();
            spawn_local(async move {
                let body = if f.downloaded > 0 {
                    format!("{} album(s) downloaded from this fan will be moved into your library first: the files stay where they are. The fan is forgotten; the inbox rows remain.", format_count(f.downloaded))
                } else {
                    "The fan is forgotten; what their lists put in the inbox remains.".to_string()
                };
                let label = if f.downloaded > 0 { "Move albums in and unfollow" } else { "Unfollow" };
                if !crate::ds::confirm(&format!("Unfollow {}?", fan_name(&f)), &body, label, true).await {
                    return;
                }
                let path = if f.downloaded > 0 { format!("/fans/{id}?releases=adopt") } else { format!("/fans/{id}") };
                match api::call("DELETE", &path).await {
                    Ok(()) => {
                        patch_fans(|v| v.retain(|x| x.id != id));
                        if f.downloaded > 0 {
                            crate::data::invalidate_all();
                        }
                        navigate("/fans", NavigateOptions { replace: true, ..Default::default() });
                    }
                    Err(e) => crate::ds::toast_err(&e.message()),
                }
            });
        })
    };
    let adopt_all = move || {
        spawn_local(async move {
            match api::post::<_, AdoptResult>("/releases/adopt", &AdoptRequest { ids: vec![], fan_id: Some(id) }).await {
                Ok(r) => {
                    notice.set(Some((false, format!("Moved {} album(s) into your library.", r.adopted))));
                    crate::data::invalidate_all();
                }
                Err(e) => notice.set(Some((true, e.message()))),
            }
        });
    };

    let queue = move |all: bool| {
        if busy_queue.get_untracked() {
            return;
        }
        busy_queue.set(true);
        let f = fan.get_untracked();
        let (l, ids) = (list.get_untracked(), selected.get_untracked().into_iter().collect::<Vec<i64>>());
        let to_library = into_library.get_untracked();
        let allow = if f.is_self { needs_confirm.get_untracked() } else { true };
        spawn_local(async move {
            let mut req = QueueRequest {
                // Someone else's wishlist is by definition not owned: confirming each time would be the whole button.
                allow_unowned: allow,
                target_subdir: Some(if to_library { String::new() } else { f.shelf.clone() }),
                source_fan_id: (!to_library).then_some(f.id),
                include_in_library: include_in_library.get_untracked(),
                single_folder: single_folder.get_untracked(),
                label: Some(format!("{}'s {}", fan_name(&f), if l == ListTab::All { "lists" } else { l.param() })),
                ..Default::default()
            };
            if all {
                req.all_matching = true;
                req.state = "new".into();
                req.fan_id = Some(f.id);
                req.tab = Some(l.param().into());
            } else {
                req.item_ids = ids.clone();
            }
            match ops::queue(&req).await {
                Ok(r) => {
                    if !r.needs_confirmation.is_empty() && r.queued == 0 {
                        needs_confirm.set(true);
                        notice.set(Some((true, format!("{} item(s) are neither owned nor offered free: only the public preview stream would be fetched. Press the button again to queue anyway.", r.needs_confirmation.len()))));
                    } else {
                        needs_confirm.set(false);
                        overrides.update(|m| {
                            for i in &ids {
                                m.insert(*i, "queued".into());
                            }
                        });
                        selected.set(HashSet::new());
                        let to = if to_library || f.is_self { "into your library".to_string() } else { format!("onto {}'s shelf", fan_name(&f)) };
                        let mut msg = ops::queue_notice(&r, &to);
                        if r.skipped_in_library > 0 {
                            msg.push_str(" Tick \u{201c}include in library\u{201d} to queue those too.");
                        }
                        notice.set(Some((false, msg)));
                        refetch.run(());
                        if all {
                            epoch.update(|e| *e += 1);
                        }
                    }
                }
                Err(e) => notice.set(Some((true, e.message()))),
            }
            busy_queue.set(false);
        });
    };
    let queue = Arc::new(queue);

    let ignore_ids = Arc::new(move |ids: Vec<i64>| {
        spawn_local(async move {
            let (ok, err) = ops::toggle_ignore(ids).await;
            overrides.update(|m| {
                for (i, st) in ok {
                    m.insert(i, st);
                }
            });
            if let Some(e) = err {
                notice.set(Some((true, e)));
            }
            refetch.run(());
        });
    });

    // -- playback --------------------------------------------------------------------------
    let play = move |order: FanOrder| {
        if busy_play.get_untracked().is_some() {
            return;
        }
        let f = fan.get_untracked();
        let seed = new_seed(crate::util::entropy());
        let c = cursor(&f, list.get_untracked(), &state.get_untracked(), order, seed, None);
        // The order is the list's (or the seeded draw's); the player's own per-track re-roll would undo it.
        player.cmd(PlayerCommand::StartSource { source: QueueSource::Fan(c), shuffle: false });
        busy_play.set(Some(order));
        crate::util::after(900, move || {
            let _ = busy_play.try_set(None);
        });
    };
    let playing_order = Signal::derive(move || {
        player.state.with(|s| match (&s.source, s.status) {
            (Some(QueueSource::Fan(c)), st) if c.fan_id == id && st != bc_types::player::PlayerStatus::Idle => Some(c.order),
            _ => None,
        })
    });
    // Play one card and carry on after it: the item before it in list order is the cursor.
    let play_from = Callback::new(move |row: FanRow| {
        let f = fan.get_untracked();
        let (l, st, has_q) = (list.get_untracked(), state.get_untracked(), !qd.get_untracked().trim().is_empty());
        spawn_local(async move {
            let single = || ops::play_cards(player, vec![ops::card_of(&row.item)], false);
            if has_q {
                return single();
            }
            let after = match play_from_offset(row.index) {
                None => None,
                Some(off) => {
                    let p = vec![("fan_id".to_string(), id.to_string()), ("tab".into(), l.param().into()), ("state".into(), st.clone()), ("order".into(), "position".into()), ("offset".into(), off.to_string()), ("limit".into(), "1".into())];
                    match api::get::<Page<HarvestItemOut>>(&format!("/harvest/items{}", qs_pairs(&p))).await {
                        Ok(pg) => match pg.items.first() {
                            Some(prev) => Some(prev.id),
                            None => return single(),
                        },
                        Err(_) => return single(),
                    }
                }
            };
            let c = cursor(&f, l, &st, FanOrder::Seq, 0, after);
            player.cmd(PlayerCommand::StartSource { source: QueueSource::Fan(c), shuffle: false });
        });
    });

    let select_all = move || {
        // Everything on the list, up to a sane size: ids come from the same listing the grid shows.
        let n = total.get_untracked().unwrap_or(0);
        if selected.with_untracked(|s| s.len()) == n && n > 0 {
            selected.set(HashSet::new());
            return;
        }
        let mut p = base_pairs();
        p.push(("limit".into(), "200".into()));
        spawn_local(async move {
            let mut all = HashSet::new();
            let mut offset = 0;
            while offset < n.min(2000) {
                let mut pp = p.clone();
                pp.push(("offset".into(), offset.to_string()));
                match api::get::<Page<HarvestItemOut>>(&format!("/harvest/items{}", qs_pairs(&pp))).await {
                    Ok(pg) if !pg.items.is_empty() => {
                        offset += pg.items.len();
                        all.extend(pg.items.into_iter().map(|i| i.id));
                    }
                    _ => break,
                }
            }
            selected.set(all);
        });
    };

    let on_toggle = Callback::new(move |i: i64| selected.update(|s| if !s.remove(&i) { s.insert(i); }));
    let ignore_cb = {
        let ig = ignore_ids.clone();
        Callback::new(move |i: i64| ig(vec![i]))
    };
    let render: Callback<(FanRow, f64), AnyView> = Callback::new(move |(row, _w): (FanRow, f64)| {
        let i = row.item.id;
        let r2 = row.clone();
        view! {
            <InboxCard item=row.item.clone() selected=Signal::derive(move || selected.with(|s| s.contains(&i))) overrides=overrides
                on_toggle=on_toggle on_ignore=ignore_cb on_play=Callback::new(move |_| play_from.run(r2.clone())) show_tabs=true />
        }
        .into_any()
    });
    let queue_btn = queue.clone();
    let unfollow2 = unfollow.clone();
    let sel_count = Signal::derive(move || selected.with(|s| s.len()));

    view! {
        <div class="fn-panel">
            <section class="card fn-head">
                <div class="fn-head-row">
                    <Icon name=crate::ds::dyn_icon(move || if fan.with(|f| f.is_self) { "heart-fill" } else { "heart" }) class="fn-self".to_string() />
                    <a class="fn-name truncate" target="_blank" rel="noreferrer" title="Open on Bandcamp"
                        href=move || fan.with(|f| if list.get() == ListTab::Collection { f.url.clone() } else { f.wishlist_url.clone() })>
                        {move || fan_name(&fan.get())}<Icon name="external" />
                    </a>
                    {move || fan.with(|f| f.is_self).then(|| view! { <span class="badge">"me"</span> })}
                    {move || (!fan.with(|f| f.is_self)).then(|| view! {
                        <span class="badge" title=move || format!("Downloads from this fan's lists land in {}/ under your downloads root and stay out of your library until you move them in.", fan.with(|f| f.shelf.clone()))>
                            {move || format!("shelf {}", fan.with(|f| f.shelf.clone()))}</span>
                    })}
                    <span class="spacer"></span>
                    {move || if walking.get() || waiting.get() {
                        view! { <Button size=Size::Sm icon="x" title="Stop the walk" on_click=move |_| stop_walk()>"Stop"</Button> }.into_any()
                    } else {
                        view! {
                            <Button size=Size::Sm icon="refresh" on_click=move |_| walk()
title="Walk the list again to pick up what was added or removed: the list on screen, or both lists">
                                {move || if list_total.get() == 0 { format!("Walk {}", if list.get() == ListTab::All { "both lists" } else { list.get().param() }) } else { "Refresh".to_string() }}
                            </Button>
                        }.into_any()
                    }}
                    {let u = unfollow2.clone(); view! { <Button size=Size::Sm variant=Variant::Ghost icon="trash" title="Unfollow this fan" on_click=move |_| u()><span class="hide-sm">"Unfollow"</span></Button> }}
                </div>
                <div class="fn-counts mono faint small hide-sm">
                    {move || fan.with(|f| format!("{} items here{}", format_count(f.items),
                        if f.wishlist_count.is_some() || f.collection_count.is_some() { format!(" \u{b7} on Bandcamp: {} wished, {} owned", format_count(f.wishlist_count.unwrap_or(0)), format_count(f.collection_count.unwrap_or(0))) } else { String::new() }))}
                </div>
                <WalkLine fan=fan />
                <div class="fn-play">
                    <Button size=Size::Sm variant=Variant::Primary icon="play" busy=Signal::derive(move || busy_play.get() == Some(FanOrder::Seq)) pressed=Signal::derive(move || playing_order.get() == Some(FanOrder::Seq))
                        disabled=Signal::derive(move || count_for_state(&counts.get(), &state.get()) == 0) title="Stream the list album by album, in its own order (newest first). Records already in the library play from your files." on_click=move |_| play(FanOrder::Seq)>"Play all"</Button>
                    <Button size=Size::Sm icon="shuffle" busy=Signal::derive(move || busy_play.get() == Some(FanOrder::Shuffle)) pressed=Signal::derive(move || playing_order.get() == Some(FanOrder::Shuffle))
                        disabled=Signal::derive(move || count_for_state(&counts.get(), &state.get()) == 0) title="A shuffle across the whole list: one random track from one random record after another." on_click=move |_| play(FanOrder::Shuffle)>"Shuffle"</Button>
                    <span class="faint small hide-sm">{move || format!("{} \u{b7} streams from Bandcamp at 128 kbps", if state.get() == "all" { "Everything but ignored".to_string() } else { format!("Only \u{201c}{}\u{201d}", state.get().replace('_', " ")) })}</span>
                    <span class="spacer"></span>
                    {move || {
                        let f = fan.get();
                        (f.downloaded > 0).then(|| view! {
                            <a class="fn-shelf small" href=format!("/albums?fan={}", f.id)><Icon name="folder" /><span class="mono">{format_count(f.downloaded)}</span>" on their shelf"</a>
                            <Button size=Size::Sm variant=Variant::Ghost title="Move every album downloaded from this fan into your library, for good." on_click=move |_| adopt_all()>"Move all into my library"</Button>
                        })
                    }}
                </div>
            </section>

            <div class="fn-filters">
                <div class="segmented" role="group" aria-label="List">
                    {ListTab::ALL.into_iter().map(|t| view! {
                        <button type="button" aria-pressed=move || (list.get() == t).to_string() on:click=move |_| list.set(t)>
                            {t.label()}<span class="mono faint fn-cnt">{move || format_count(list_items(&fan.get(), t))}</span>
                        </button>
                    }).collect_view()}
                </div>
                <SearchInput value=q placeholder="Search this list" class="fn-search" />
            </div>
            <div class="fn-states" role="group" aria-label="Inbox state">
                {STATE_TABS.into_iter().map(|(sid, label)| view! {
                    <button type="button" class="fd-tab" aria-pressed=move || (state.get() == sid).to_string() on:click=move |_| state.set(sid.to_string())>
                        {label}<span class="mono faint">{move || count_for_state(&counts.get(), sid)}</span>
                    </button>
                }).collect_view()}
            </div>

            <div class="fn-bar">
                <Button size=Size::Sm variant=Variant::Ghost icon="sliders" pressed=options_open title="Download options" on_click=move |_| options_open.update(|o| *o = !*o)><span>"Options"</span></Button>
                <Button size=Size::Sm variant=Variant::Ghost on_click=move |_| select_all() disabled=Signal::derive(move || total.get().unwrap_or(0) == 0)>
                    {move || if sel_count.get() > 0 && Some(sel_count.get()) == total.get() { "Clear selection".to_string() } else { format!("Select all {}", total.get().map(|t| t.to_string()).unwrap_or_default()) }}
                </Button>
                {move || (sel_count.get() > 0).then(|| {
                    let ig = ignore_ids.clone();
                    view! { <Button size=Size::Sm icon="eye-off" title="Ignore the selected" on_click=move |_| ig(selected.get_untracked().into_iter().collect())><span class="hide-sm">"Ignore "</span>{sel_count.get()}</Button> }
                })}
                <span class="spacer"></span>
                {let queue = queue_btn.clone(); view! {
                    <Button variant=Variant::Primary icon="download" busy=busy_queue
                        disabled=Signal::derive(move || sel_count.get() == 0 && count_new.get() == 0)
                        on_click=move |_| queue(sel_count.get_untracked() == 0)>
                        {move || { let any = if needs_confirm.get() { " anyway" } else { "" }; if sel_count.get() > 0 { format!("Queue {}{any}", sel_count.get()) } else { format!("Queue {} new{any}", format_count(count_new.get())) } }}
                    </Button>
                }}
            </div>
            {move || options_open.get().then(|| view! {
                <div class="fn-options">
                    {move || (!fan.with(|f| f.is_self)).then(|| view! {
                        <span class="fn-opt-group">
                            <span class="faint small">"Download to"</span>
                            <label class="check small"><input type="radio" name=format!("target-{id}") prop:checked=move || !into_library.get() on:change=move |_| into_library.set(false) />{move || format!("{}'s shelf", fan_name(&fan.get()))}</label>
                            <label class="check small"><input type="radio" name=format!("target-{id}") prop:checked=move || into_library.get() on:change=move |_| into_library.set(true) />"my library"</label>
                        </span>
                    })}
                    <label class="check small" title="Everything lands directly in one folder: no artist/album subfolders."><input type="checkbox" prop:checked=move || single_folder.get() on:change=move |ev| single_folder.set(event_target_checked(&ev)) />"Single folder"</label>
                    <label class="check small" title="Also download items you already have in the library, instead of skipping them."><input type="checkbox" prop:checked=move || include_in_library.get() on:change=move |ev| include_in_library.set(event_target_checked(&ev)) />"Include in library"</label>
                </div>
            })}
            {move || notice.get().map(|(is_err, msg)| view! {
                <div class=if is_err { "banner danger fn-note" } else { "banner fn-note" } role="status">
                    <Icon name=if is_err { "alert" } else { "check-circle" } /><span class="grow">{msg}</span>
                    <Button size=Size::Sm variant=Variant::Ghost icon="x" title="Dismiss" on_click=move |_| notice.set(None) />
                </div>
            })}

            <CardGrid fetch=fetcher source_key=grid_key min_card_w=140.0 gap=10.0 meta_h=CARD_META_H render=render total_out=total entities=vec!["harvest"]
                empty=move || {
                    let walking_now = walking.get_untracked();
                    let items = list_total.get_untracked();
                    let (t, h) = if items == 0 {
                        if walking_now { ("Walking\u{2026}".to_string(), "The list is being brought in; cards appear as it goes.".to_string()) }
                        else { ("Nothing here yet".to_string(), format!("Walk the {} to bring it in.", if list.get_untracked() == ListTab::All { "lists" } else { list.get_untracked().param() })) }
                    } else { ("Nothing matches".to_string(), "Try another state or clear the search.".to_string()) };
                    view! { <EmptyState title=t hint=h icon="heart" /> }
                } />
        </div>
    }
}

#[component]
fn WalkLine(fan: Signal<FanOut>) -> impl IntoView {
    view! {
        {move || fan.get().walk.filter(|w| w.phase != "idle").map(|w| {
            let progress = w.total.filter(|t| *t > 0).map(|t| (w.seen as f64 / t as f64).clamp(0.0, 1.0));
            view! {
                <div class="fn-walk" role="status" aria-live="polite">
                    {match w.phase.as_str() {
                        "queued" => view! { <span class="small"><Icon name="clock" />"Waiting for the walk before it to finish\u{2026}"</span> }.into_any(),
                        "harvesting" => view! {
                            <span class="small"><Icon name="refresh" class="spin".to_string() />{format!("Walking the {} ", w.tab.clone().unwrap_or_else(|| "list".into()))}<span class="mono">{format_count(w.seen)}</span>
                                {w.total.map(|t| view! { " / "<span class="mono">{format_count(t)}</span> })}</span>
                            <Meter value=Signal::derive(move || progress) label="Walk progress" />
                        }.into_any(),
                        "queueing" => view! { <span class="small"><Icon name="download" />"Queueing what is missing\u{2026}"</span> }.into_any(),
                        "done" => view! {
                            <span class="small"><Icon name="check-circle" />"Saw "<span class="mono">{format_count(w.seen)}</span>", new "<span class="mono">{format_count(w.new)}</span>", already had "<span class="mono">{format_count(w.in_library)}</span>
                                {(w.queued > 0).then(|| view! { ", queued "<span class="mono">{format_count(w.queued)}</span> })}"."</span>
                        }.into_any(),
                        "failed" => {
                            let stopped = w.error.as_deref() == Some("Stopped");
                            view! { <span class=if stopped { "small" } else { "small danger-text" }><Icon name=if stopped { "x" } else { "alert" } />{w.error.clone().unwrap_or_else(|| "The walk failed.".into())}</span> }.into_any()
                        }
                        _ => ().into_any(),
                    }}
                    {(!w.running && !w.errors.is_empty()).then(|| view! {
                        <ul class="faint small fn-walk-notes">{w.errors.iter().take(3).map(|e| view! { <li>{e.clone()}</li> }).collect_view()}</ul>
                    })}
                </div>
            }
        })}
    }
}
