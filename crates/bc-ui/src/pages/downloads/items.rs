//! A job's items, clustered by the account that hosts them (on Bandcamp the subdomain is the
//! artist or label). Groups are paged from `/jobs/{id}/groups`; their items load on expand.
//! Item events patch rows and group counts in place (no refetch on progress); pointer-event DnD
//! (works on touch) and move-to-top/bottom buttons reorder the queue; the worker claims by `seq`.
use std::collections::HashMap;
use std::sync::Arc;

use bc_types::Page;
use bc_types::jobs::*;
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::logic::*;
use crate::api;
use crate::data::{use_jobs, use_topic};
use crate::ds::{Button, Icon, Meter, Size, Variant};
use crate::widgets::dnd::{self, DragPayload, begin_drag, register_target};

const GROUPS_PAGE: usize = 100;
const ITEMS_PAGE: usize = 50;

/// Fields of an item that events patch; overlays the fetched row.
#[derive(Clone, Debug, Default, PartialEq)]
struct ItemLive {
    status: Option<String>,
    progress: Option<f64>,
    message: Option<String>,
    last_error: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct MoveReq {
    item_ids: Vec<i64>,
    groups: Vec<String>,
    place: &'static str,
    anchor_item: Option<i64>,
    anchor_group: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct RemoveReq {
    item_ids: Vec<i64>,
    groups: Vec<String>,
    label: String,
    count: i64,
}

/// Everything rows share. All fields are `Copy`, so rows capture it freely.
#[derive(Clone, Copy)]
struct Ctx {
    job_id: StoredValue<String>,
    filter: RwSignal<Filter>,
    /// Whether items can still change places (the job is not finished).
    sortable: Signal<bool>,
    busy: RwSignal<bool>,
    live: RwSignal<HashMap<i64, ItemLive>>,
    /// Last known status per item (for moving group counts on events).
    item_status: StoredValue<HashMap<i64, String>>,
    /// Item -> group key.
    item_group: StoredValue<HashMap<i64, String>>,
    epoch: RwSignal<u32>,
    on_move: Callback<MoveReq>,
    on_remove: Callback<RemoveReq>,
}

#[derive(Clone)]
struct GroupRef {
    key: String,
    data: RwSignal<JobItemGroupOut>,
}

fn kind_for(job: &str, group: bool) -> String {
    format!("job-{}:{job}", if group { "group" } else { "item" })
}

#[component]
pub fn JobItems(job_id: String) -> impl IntoView {
    let store = use_jobs();
    let status = {
        let id = job_id.clone();
        Memo::new(move |_| store.jobs.with(|v| v.iter().find(|j| j.id == id).map(|j| j.status.clone()).unwrap_or_default()))
    };
    let filter = RwSignal::new(default_filter(&status.get_untracked()));
    let groups: RwSignal<Vec<GroupRef>> = RwSignal::new(vec![]);
    let total = RwSignal::new(0usize);
    let loading = RwSignal::new(true);
    let more_loading = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let epoch = RwSignal::new(0u32);
    let generation = StoredValue::new(0u64);
    let reload_pending = StoredValue::new(false);
    let ctx = Ctx {
        job_id: StoredValue::new(job_id.clone()),
        filter,
        sortable: Signal::derive(move || is_open(&status.get())),
        busy,
        live: RwSignal::new(HashMap::new()),
        item_status: StoredValue::new(HashMap::new()),
        item_group: StoredValue::new(HashMap::new()),
        epoch,
        on_move: Callback::new(|_| {}),
        on_remove: Callback::new(|_| {}),
    };

    let fetch_groups = move |reset: bool| {
        // Timers and socket events can land after the panel is gone.
        let g = if reset {
            generation.try_update_value(|g| {
                *g += 1;
                *g
            })
        } else {
            generation.try_get_value()
        };
        let (Some(g), Some(job_id)) = (g, ctx.job_id.try_get_value()) else { return };
        let offset = if reset { 0 } else { groups.with_untracked(|v| v.len()) };
        let mut url = format!("/jobs/{job_id}/groups?offset={offset}&limit={GROUPS_PAGE}");
        if let Some(s) = filter.get_untracked().status() {
            url.push_str(&format!("&status={s}"));
        }
        if reset {
            loading.set(true);
        } else {
            more_loading.set(true);
        }
        spawn_local(async move {
            let res = api::get::<Page<JobItemGroupOut>>(&url).await;
            if generation.try_get_value() != Some(g) {
                return;
            }
            match res {
                Ok(p) => {
                    error.set(None);
                    total.set(p.total as usize);
                    // Keep the signal of a group that is still there: rows are keyed by group, so a
                    // fresh signal would never reach the row already on screen.
                    let existing: HashMap<String, RwSignal<JobItemGroupOut>> = groups.with_untracked(|v| v.iter().map(|g| (g.key.clone(), g.data)).collect());
                    let mk = |x: JobItemGroupOut| {
                        if let Some(it) = &x.item {
                            ctx.item_status.update_value(|m| {
                                m.insert(it.id, it.status.clone());
                            });
                            ctx.item_group.update_value(|m| {
                                m.insert(it.id, x.key.clone());
                            });
                        }
                        match existing.get(&x.key) {
                            Some(sig) => {
                                let key = x.key.clone();
                                sig.set(x);
                                GroupRef { key, data: *sig }
                            }
                            None => GroupRef { key: x.key.clone(), data: RwSignal::new(x) },
                        }
                    };
                    if reset {
                        groups.set(p.items.into_iter().map(mk).collect());
                    } else {
                        groups.update(|v| {
                            for x in p.items {
                                if !v.iter().any(|g| g.key == x.key) {
                                    v.push(mk(x));
                                }
                            }
                        });
                    }
                    ctx.live.update(|m| m.clear());
                }
                Err(e) => error.set(Some(e.message())),
            }
            loading.set(false);
            more_loading.set(false);
        });
    };
    let fetch_groups = Arc::new(fetch_groups);

    // First load, and again whenever the filter changes.
    {
        let f = fetch_groups.clone();
        Effect::new(move |_| {
            filter.track();
            f(true);
            epoch.update(|e| *e += 1);
        });
    }
    // Structural changes (reorder, retry, remove) are the only reasons to refetch; debounced.
    let schedule_reload: Arc<dyn Fn() + Send + Sync> = {
        let f = fetch_groups.clone();
        Arc::new(move || {
            if reload_pending.try_get_value() != Some(false) {
                return;
            }
            reload_pending.set_value(true);
            let f = f.clone();
            crate::util::after(900, move || {
                // `None` means the write landed, i.e. the panel is still mounted.
                if reload_pending.try_set_value(false).is_none() {
                    f(true);
                    epoch.update(|e| *e += 1);
                }
            });
        })
    };

    // -- live events: patch rows and group counts in place ---------------------------------
    let jid = job_id.clone();
    let patch_item = move |item_id: i64, to: &str, upd: Box<dyn FnOnce(&mut ItemLive)>, group_hint: Option<String>, assumed_from: &str, schedule: &Arc<dyn Fn() + Send + Sync>| {
        ctx.live.update(|m| {
            let e = m.entry(item_id).or_default();
            e.status = Some(to.to_string());
            upd(e);
        });
        let prev = ctx.item_status.with_value(|m| m.get(&item_id).cloned()).unwrap_or_else(|| assumed_from.to_string());
        ctx.item_status.update_value(|m| {
            m.insert(item_id, to.to_string());
        });
        if let Some(h) = group_hint {
            ctx.item_group.update_value(|m| {
                m.insert(item_id, h);
            });
        }
        let key = ctx.item_group.with_value(|m| m.get(&item_id).cloned());
        match key.and_then(|k| groups.with_untracked(|v| v.iter().find(|g| g.key == k).map(|g| g.data))) {
            Some(g) => g.update(|g| move_between(g, &prev, to)),
            None => schedule(),
        }
    };
    let patch_item = Arc::new(patch_item);
    {
        let (jid, p, sch) = (jid.clone(), patch_item.clone(), schedule_reload.clone());
        use_topic::<JobItemStarted>(TOPIC_JOB_ITEM_STARTED, move |e| {
            if e.job_id == jid {
                let host = e.url.as_deref().map(host_of);
                p(e.item_id, ITEM_RUNNING, Box::new(|l| l.progress = Some(0.0)), host, ITEM_PENDING, &sch);
            }
        });
    }
    {
        let (jid, p, sch) = (jid.clone(), patch_item.clone(), schedule_reload.clone());
        use_topic::<JobItemStarted>(TOPIC_JOB_ITEM_SKIPPED, move |e| {
            if e.job_id == jid {
                let host = e.url.as_deref().map(host_of);
                p(e.item_id, ITEM_SKIPPED, Box::new(|_| {}), host, ITEM_PENDING, &sch);
            }
        });
    }
    {
        let jid = jid.clone();
        use_topic::<JobItemProgress>(TOPIC_JOB_ITEM_PROGRESS, move |e| {
            if e.job_id == jid {
                ctx.live.update(|m| {
                    let l = m.entry(e.item_id).or_default();
                    if l.status.is_none() {
                        l.status = Some(ITEM_RUNNING.into());
                    }
                    l.progress = Some(e.progress);
                    l.message = Some(e.message);
                });
            }
        });
    }
    {
        let (jid, p, sch) = (jid.clone(), patch_item.clone(), schedule_reload.clone());
        use_topic::<JobItemCompleted>(TOPIC_JOB_ITEM_COMPLETED, move |e| {
            if e.job_id == jid {
                let d = e.detail;
                p(e.item_id, ITEM_DONE, Box::new(move |l| { l.progress = Some(1.0); l.message = Some(d); l.last_error = None; }), None, ITEM_RUNNING, &sch);
            }
        });
    }
    {
        let (jid, p, sch) = (jid.clone(), patch_item.clone(), schedule_reload.clone());
        use_topic::<JobItemFailed>(TOPIC_JOB_ITEM_FAILED, move |e| {
            if e.job_id == jid {
                let (d, retry) = (e.detail, e.will_retry);
                let to = if retry { ITEM_PENDING } else { ITEM_FAILED };
                p(e.item_id, to, Box::new(move |l| { l.progress = Some(0.0); l.last_error = Some(d); if retry { l.message = Some("will retry".into()); } }), None, ITEM_RUNNING, &sch);
            }
        });
    }
    for topic in [TOPIC_JOB_REORDERED, TOPIC_JOB_RETRIED] {
        let (jid, sch) = (jid.clone(), schedule_reload.clone());
        use_topic::<JobRef>(topic, move |e| {
            if e.job_id == jid {
                sch();
            }
        });
    }

    // -- actions ----------------------------------------------------------------------------
    let ctx = Ctx {
        on_move: {
            let sch = fetch_groups.clone();
            Callback::new(move |r: MoveReq| {
                if busy.get_untracked() {
                    return;
                }
                busy.set(true);
                let sch = sch.clone();
                spawn_local(async move {
                    let req = MoveItemsRequest {
                        selection: ItemSelection { item_ids: r.item_ids, groups: r.groups, status: ctx.filter.get_untracked().status().map(str::to_string) },
                        place: r.place.into(),
                        anchor_item_id: r.anchor_item,
                        anchor_group: r.anchor_group,
                    };
                    match api::post::<_, MovedOut>(&format!("/jobs/{}/items/move", ctx.job_id.get_value()), &req).await {
                        Ok(out) => {
                            store.upsert(out.job);
                            sch(true);
                            ctx.epoch.update(|e| *e += 1);
                        }
                        Err(e) => crate::ds::toast_err(&e.message()),
                    }
                    busy.set(false);
                });
            })
        },
        on_remove: {
            let sch = fetch_groups.clone();
            Callback::new(move |r: RemoveReq| {
                if busy.get_untracked() {
                    return;
                }
                let sch = sch.clone();
                spawn_local(async move {
                    // One item goes on a click; a whole cluster gets a second look first.
                    if r.count > 1 {
                        let body = format!("{} items under {} will be taken out of this job. Anything not yet downloaded goes back to the inbox.", crate::logic::format::format_count(r.count), r.label);
                        if !crate::ds::confirm("Remove from queue?", &body, &format!("Remove {}", crate::logic::format::format_count(r.count)), true).await {
                            return;
                        }
                    }
                    // After the confirm dialog: the page may be gone.
                    let (Some(filter), Some(job_id)) = (ctx.filter.try_get_untracked(), ctx.job_id.try_get_value()) else { return };
                    busy.set(true);
                    let req = ItemSelection { item_ids: r.item_ids, groups: r.groups, status: filter.status().map(str::to_string) };
                    match api::post::<_, RemovedOut>(&format!("/jobs/{}/items/remove", job_id), &req).await {
                        Ok(out) => {
                            match out.job {
                                Some(j) => store.upsert(j),
                                None => {
                                    let id = job_id;
                                    store.jobs.update(|v| v.retain(|j| j.id != id));
                                }
                            }
                            if out.kept_running > 0 {
                                crate::ds::toast_info(&format!("{} item(s) are downloading right now and were kept.", out.kept_running));
                            }
                            sch(true);
                            ctx.epoch.update(|e| *e += 1);
                        }
                        Err(e) => crate::ds::toast_err(&e.message()),
                    }
                    busy.set(false);
                });
            })
        },
        ..ctx
    };

    // Chip counts come from the live job counters.
    let chip_count = move |f: Filter| -> Option<i64> {
        store.jobs.with(|v| {
            let id = ctx.job_id.get_value();
            v.iter().find(|j| j.id == id).and_then(|j| match f {
                Filter::All => Some(j.total),
                Filter::Queued => is_open(&j.status).then(|| (j.total - j.completed - j.failed - j.skipped).max(0)),
                Filter::Failed => Some(j.failed),
                Filter::Done => Some(j.completed + j.skipped),
            })
        })
    };

    let f_more = fetch_groups.clone();
    let f_retry = fetch_groups.clone();
    view! {
        <div class="dl-items-panel">
            <div class="dl-itembar">
                <div role="group" aria-label="Filter items" class="dl-filter">
                    {Filter::ALL.into_iter().map(|f| view! {
                        <button type="button" class="dl-chip" aria-pressed=move || (filter.get() == f).to_string() on:click=move |_| filter.set(f)>
                            {f.label()}
                            {move || chip_count(f).filter(|n| *n > 0).map(|n| view! { <span class="mono faint">{n}</span> })}
                        </button>
                    }).collect_view()}
                </div>
                <span class="faint small">
                    {move || { let t = total.get(); (t > 0).then(|| format!("{t} {}", if t == 1 { "artist / label" } else { "artists / labels" })) }}
                    {move || (ctx.sortable.get() && total.get() > 0).then(|| view! { <span class="hide-sm">" \u{b7} drag the grip to reorder the queue"</span> })}
                </span>
            </div>
            {move || error.get().map(|e| { let f = f_retry.clone(); view! {
                <div class="dl-note danger-text"><Icon name="alert" /><span>{e}</span><Button size=Size::Sm variant=Variant::Ghost on_click=move |_| f(true)>"Retry"</Button></div>
            } })}
            <div class="dl-groups">
                <For each=move || groups.get() key=|g| g.key.clone() let:g>
                    <GroupRow g=g ctx=ctx />
                </For>
            </div>
            {move || loading.get().then(|| view! { <div class="dl-note faint">"Loading\u{2026}"</div> })}
            {move || (!loading.get() && error.get().is_none() && groups.with(|g| g.is_empty())).then(|| view! { <div class="dl-note faint">{filter.get().empty()}</div> })}
            {move || {
                let rest = total.get().saturating_sub(groups.with(|g| g.len()));
                let f = f_more.clone();
                (rest > 0).then(|| view! {
                    <button type="button" class="dl-more" disabled=move || more_loading.get() on:click=move |_| f(false)>
                        {move || if more_loading.get() { "Loading\u{2026}".to_string() } else { "Show more".to_string() }}
                        <span class="mono faint">{format!("({} more)", crate::logic::format::format_count(rest as i64))}</span>
                    </button>
                })
            }}
        </div>
    }
}

// ---------------------------------------------------------------------------------------------

#[component]
fn HostLabel(#[prop(into)] host: String) -> impl IntoView {
    if host.is_empty() {
        return view! { <span class="faint">"\u{2014}"</span> }.into_any();
    }
    let (name, suffix) = split_host(&host);
    let (name, suffix) = (name.to_string(), suffix.to_string());
    view! { <>{name}<span class="faint">{suffix}</span></> }.into_any()
}

#[component]
fn Grip(sortable: Signal<bool>, kind: String, ids: Vec<i64>, label: String) -> impl IntoView {
    view! {
        <span class=move || if sortable.get() { "dl-grip" } else { "dl-grip off" } aria-hidden="true"
            on:pointerdown=move |ev| {
                if sortable.get_untracked() {
                    // no text selection while dragging with a mouse (touch is held back by `touch-action`)
                    if ev.pointer_type() != "touch" {
                        ev.prevent_default();
                    }
                    begin_drag(&ev, DragPayload { kind: kind.clone(), ids: ids.clone(), label: label.clone(), index: None });
                }
            }>
            <svg viewBox="0 0 24 24" width="14" height="14" fill="currentColor"><circle cx="9" cy="6" r="1.6"/><circle cx="15" cy="6" r="1.6"/><circle cx="9" cy="12" r="1.6"/><circle cx="15" cy="12" r="1.6"/><circle cx="9" cy="18" r="1.6"/><circle cx="15" cy="18" r="1.6"/></svg>
        </span>
    }
}

/// Move-to-top / move-to-bottom / remove, shared by group and item rows.
#[component]
fn RowActions(ctx: Ctx, make_move: Callback<&'static str>, on_remove: Callback<()>, sortable: Signal<bool>, removable: Signal<bool>) -> impl IntoView {
    view! {
        <span class="dl-acts">
            {move || sortable.get().then(|| view! {
                <button type="button" class="dl-act" title="Move to top of queue" aria-label="Move to top of queue" disabled=move || ctx.busy.get()
                    on:click=move |ev| { ev.stop_propagation(); make_move.run("top") }><Icon name="arrow-up" /></button>
                <button type="button" class="dl-act" title="Move to bottom of queue" aria-label="Move to bottom of queue" disabled=move || ctx.busy.get()
                    on:click=move |ev| { ev.stop_propagation(); make_move.run("bottom") }><Icon name="arrow-down" /></button>
            })}
            {move || removable.get().then(|| view! {
                <button type="button" class="dl-act danger" title="Remove from queue" aria-label="Remove from queue" disabled=move || ctx.busy.get()
                    on:click=move |ev| { ev.stop_propagation(); on_remove.run(()) }><Icon name="trash" /></button>
            })}
        </span>
    }
}

fn over_class(target: &str) -> Signal<&'static str> {
    let target = target.to_string();
    Signal::derive(move || match dnd::over().with(|o| o.as_ref().filter(|(id, _)| *id == target).map(|(_, i)| i.before)) {
        Some(true) => "over-before",
        Some(false) => "over-after",
        None => "",
    })
}

fn is_held(kind: String, label: String, id: Option<i64>) -> Signal<bool> {
    Signal::derive(move || dnd::active().with(|a| a.as_ref().map(|p| p.kind == kind && p.label == label && (id.is_none() || p.ids.first().copied() == id)).unwrap_or(false)))
}

#[component]
fn GroupRow(g: GroupRef, ctx: Ctx) -> impl IntoView {
    let GroupRef { key, data } = g;
    let open = RwSignal::new(false);
    let items: RwSignal<Vec<JobItemOut>> = RwSignal::new(vec![]);
    let items_loading = RwSignal::new(false);
    let items_more = RwSignal::new(false);
    let items_err = RwSignal::new(None::<String>);
    let kind_g = kind_for(&ctx.job_id.get_value(), true);
    let kind_i = kind_for(&ctx.job_id.get_value(), false);
    let target = format!("jr:{}:g:{key}", ctx.job_id.get_value());

    // A group is sortable while it still has something to run, under All / Queued only.
    let sortable = Signal::derive({
        let base = ctx.sortable;
        move || base.get() && matches!(ctx.filter.get(), Filter::All | Filter::Queued) && data.with(|d| d.pending + d.running > 0)
    });
    let k2 = key.clone();
    register_target(&target, &[&kind_g, &kind_i], {
        let (key, kind_g) = (key.clone(), kind_g.clone());
        move |p, info| {
            let place = if info.before { "before" } else { "after" };
            let mut req = MoveReq { place, anchor_group: Some(key.clone()), ..Default::default() };
            if p.kind == kind_g {
                if p.label == key {
                    return;
                }
                req.groups = vec![p.label];
            } else {
                req.item_ids = p.ids;
            }
            ctx.on_move.run(req);
        }
    });

    // Items of an expanded group load on demand (and again after a structural change).
    let load = Arc::new(move |reset: bool| {
        let offset = if reset { 0 } else { items.with_untracked(|v| v.len()) };
        let mut url = format!("/jobs/{}/items?group={}&offset={offset}&limit={ITEMS_PAGE}", ctx.job_id.get_value(), crate::util::enc(&k2));
        if let Some(s) = ctx.filter.get_untracked().status() {
            url.push_str(&format!("&status={s}"));
        }
        if reset {
            items_loading.set(true);
        } else {
            items_more.set(true);
        }
        let gk = k2.clone();
        spawn_local(async move {
            match api::get::<Vec<JobItemOut>>(&url).await {
                Ok(v) => {
                    items_err.set(None);
                    ctx.item_status.update_value(|m| {
                        for i in &v {
                            m.insert(i.id, i.status.clone());
                        }
                    });
                    ctx.item_group.update_value(|m| {
                        for i in &v {
                            m.insert(i.id, gk.clone());
                        }
                    });
                    // Dedupe by id: a move renumbers, and pages fetched either side can overlap.
                    items.update(|cur| {
                        if reset {
                            cur.clear();
                        }
                        for i in v {
                            if !cur.iter().any(|x| x.id == i.id) {
                                cur.push(i);
                            }
                        }
                    });
                }
                Err(e) => items_err.set(Some(e.message())),
            }
            items_loading.set(false);
            items_more.set(false);
        });
    });
    {
        let load = load.clone();
        Effect::new(move |_| {
            ctx.epoch.track();
            if open.get() {
                load(true);
            }
        });
    }

    let selection_move = {
        let key = key.clone();
        Callback::new(move |place: &'static str| ctx.on_move.run(MoveReq { groups: vec![key.clone()], place, ..Default::default() }))
    };
    let remove = {
        let key = key.clone();
        Callback::new(move |_: ()| {
            let (count, label) = (visible_count(&data.get_untracked(), ctx.filter.get_untracked()), key.clone());
            ctx.on_remove.run(RemoveReq { groups: vec![key.clone()], label, count, ..Default::default() })
        })
    };
    let removable = Signal::derive(move || data.with(|d| visible_count(d, ctx.filter.get()) > d.running));
    let over = over_class(&target);
    let held = is_held(kind_g.clone(), key.clone(), None);
    let toggle = move || open.update(|o| *o = !*o);
    let key_label = key.clone();
    let key_title = key.clone();
    let tid = target.clone();

    // A group with exactly one visible item reads as the item itself.
    let single = Memo::new(move |_| data.with(|d| (d.visible == 1).then(|| d.item.clone()).flatten()));
    let key_single = key.clone();
    let key_grp = key.clone();

    view! {
        {move || match single.get() {
            Some(item) => view! { <ItemRow item=item host=key_single.clone() ctx=ctx hide_host=false /> }.into_any(),
            None => {
                let (key, k_title, k_label, tid, kind_g) = (key_grp.clone(), key_title.clone(), key_label.clone(), tid.clone(), kind_g.clone());
                let load = load.clone();
                view! {
                    <div class="dl-grp" class:held=move || held.get()>
                        <div class=move || format!("dl-row dl-grp-head {}", over.get()) data-dnd-target=tid>
                            <Grip sortable=sortable kind=kind_g ids=vec![] label=key.clone() />
                            <button type="button" class="dl-grp-toggle" aria-expanded=move || open.get().to_string() title=k_title on:click=move |_| toggle()>
                                <span class="dl-chev"><Icon name=crate::ds::dyn_icon(move || if open.get() { "chevron-down" } else { "chevron-right" }) /></span>
                                <span class="dl-host truncate mono"><HostLabel host=k_label /></span>
                                <span class="dl-counts mono faint">
                                    {move || data.with(|d| group_counts_view(d, ctx.filter.get()))}
                                </span>
                            </button>
                            <RowActions ctx=ctx make_move=selection_move on_remove=remove sortable=sortable removable=removable />
                        </div>
                        {move || open.get().then(|| {
                            let load = load.clone();
                            let k = key.clone();
                            view! {
                                <div class="dl-items">
                                    <For each=move || items.get() key=|i| i.id let:item>
                                        <ItemRow item=item host=k.clone() ctx=ctx hide_host=true />
                                    </For>
                                    {move || items_loading.get().then(|| view! { <div class="dl-note faint">"Loading\u{2026}"</div> })}
                                    {move || items_err.get().map(|e| view! { <div class="dl-note danger-text">{e}</div> })}
                                    {move || {
                                        let rest = data.with(|d| visible_count(d, ctx.filter.get())).max(0) as usize;
                                        let have = items.with(|v| v.len());
                                        let load = load.clone();
                                        (rest > have && !items_loading.get()).then(|| view! {
                                            <button type="button" class="dl-more" disabled=move || items_more.get() on:click=move |_| load(false)>
                                                {format!("Show {} more", (rest - have).min(ITEMS_PAGE))}
                                            </button>
                                        })
                                    }}
                                </div>
                            }
                        })}
                    </div>
                }.into_any()
            }
        }}
    }
}

fn group_counts_view(d: &JobItemGroupOut, f: Filter) -> impl IntoView + use<> {
    let mut parts: Vec<(String, &'static str)> = vec![];
    parts.push((format!("{}{}", if f == Filter::All { d.total } else { visible_count(d, f) }, if f == Filter::All { String::new() } else { format!(" {}", f.label().to_lowercase()) }), ""));
    if d.running > 0 {
        parts.push((format!("{} running", d.running), "info-text"));
    }
    if f != Filter::Queued && d.pending > 0 {
        parts.push((format!("{} queued", d.pending), ""));
    }
    if f != Filter::Done && d.done > 0 {
        parts.push((format!("{} ok", d.done), ""));
    }
    if f != Filter::Failed && d.failed > 0 {
        parts.push((format!("{} failed", d.failed), "danger-text"));
    }
    if f != Filter::Done && d.skipped > 0 {
        parts.push((format!("{} skipped", d.skipped), ""));
    }
    if d.cancelled > 0 {
        parts.push((format!("{} cancelled", d.cancelled), ""));
    }
    parts.into_iter().enumerate().map(|(i, (t, c))| view! { <span class=c>{if i > 0 { " \u{b7} " } else { "" }}{t}</span> }).collect_view()
}

#[component]
fn ItemRow(item: JobItemOut, host: String, hide_host: bool, ctx: Ctx) -> impl IntoView {
    let id = item.id;
    let live = Memo::new(move |_| ctx.live.with(|m| m.get(&id).cloned()));
    let base = StoredValue::new(item.clone());
    let status = Signal::derive(move || live.get().and_then(|l| l.status).unwrap_or_else(|| base.with_value(|b| b.status.clone())));
    let progress = Signal::derive(move || live.get().and_then(|l| l.progress).unwrap_or_else(|| base.with_value(|b| b.progress)));
    let message = Signal::derive(move || live.get().and_then(|l| l.message).or_else(|| base.with_value(|b| b.message.clone())));
    let last_error = Signal::derive(move || live.get().and_then(|l| l.last_error).or_else(|| base.with_value(|b| b.last_error.clone())));
    let bare = item.url.clone().map(|u| u.trim_start_matches("https://").trim_start_matches("http://").to_string()).unwrap_or_else(|| format!("item {}", item.seq));
    let path = item.url.as_deref().map(bare_path).unwrap_or_default();
    let kind_i = kind_for(&ctx.job_id.get_value(), false);
    let kind_g = kind_for(&ctx.job_id.get_value(), true);
    let target = format!("jr:{}:i:{id}", ctx.job_id.get_value());
    let sortable = Signal::derive(move || ctx.sortable.get() && matches!(status.get().as_str(), ITEM_PENDING | ITEM_RUNNING));
    register_target(&target, &[&kind_g, &kind_i], {
        move |p, info| {
            let place = if info.before { "before" } else { "after" };
            let mut req = MoveReq { place, anchor_item: Some(id), ..Default::default() };
            if p.kind.starts_with("job-group") {
                req.groups = vec![p.label];
            } else {
                if p.ids.first() == Some(&id) {
                    return;
                }
                req.item_ids = p.ids;
            }
            ctx.on_move.run(req);
        }
    });
    let over = over_class(&target);
    let held = is_held(kind_i.clone(), String::new(), Some(id));
    let make_move = Callback::new(move |place: &'static str| ctx.on_move.run(MoveReq { item_ids: vec![id], place, ..Default::default() }));
    // Nothing here can interrupt a download mid-flight; a running row offers no remove.
    let removable = Signal::derive(move || status.get() != ITEM_RUNNING);
    let remove = {
        let bare = bare.clone();
        Callback::new(move |_: ()| ctx.on_remove.run(RemoveReq { item_ids: vec![id], label: bare.clone(), count: 1, ..Default::default() }))
    };
    let title = item.url.clone().unwrap_or_default();
    let name = if hide_host { if path.is_empty() { bare.clone() } else { path.clone() } } else { String::new() };
    let host_view = (!hide_host).then(|| view! { <HostLabel host=host.clone() /><span>{path.clone()}</span> });
    let p_hide = hide_host;
    view! {
        <div class=move || format!("dl-row dl-item {} {}", over.get(), if p_hide { "nested" } else { "" }) class:held=move || held.get() data-dnd-target=target>
            <Grip sortable=sortable kind=kind_i ids=vec![id] label=bare.clone() />
            <span class="dl-st"><ItemStatus status=status /></span>
            <span class="dl-url truncate mono" title=title>{name}{host_view}</span>
            {move || (status.get() == ITEM_RUNNING).then(|| view! { <span class="dl-prog"><Meter value=Signal::derive(move || Some(progress.get())) label="Item progress" /></span> })}
            {move || message.get().filter(|m| !m.is_empty()).map(|m| { let t = m.clone(); view! { <span class="dl-msg faint truncate" title=t>{m}</span> } })}
            {move || (status.get() == ITEM_FAILED).then(|| last_error.get()).flatten().map(|e| { let t = e.clone(); view! { <span class="dl-msg danger-text truncate" title=t>{e}</span> } })}
            <RowActions ctx=ctx make_move=make_move on_remove=remove sortable=sortable removable=removable />
        </div>
    }
}

#[component]
pub fn ItemStatus(#[prop(into)] status: Signal<String>) -> impl IntoView {
    view! {
        <span class=move || format!("status {}", status_look(&status.get()).0)>
            <Icon name=crate::ds::dyn_icon(move || status_look(&status.get()).1) />
            <span>{move || status.get()}</span>
        </span>
    }
}
