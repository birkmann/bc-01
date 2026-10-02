//! Harvest: identify a Bandcamp source, sweep it into an inbox (a job; progress arrives over
//! `harvest.progress`), then pick from the inbox and queue the picks as one download job.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bc_types::bandcamp::{HarvestCompleted, HarvestItemOut, HarvestProgress, IdentityStatus, QueueRequest, QueueResult, ResolveRequest, ResolveResult, RunRequest, RunResult};
use bc_types::{Accepted, Page};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::data::{self, QuerySpec, use_jobs, use_topic};
use crate::ds::{self, Button, EmptyState, Icon, Meter, PageHeader, Size, Variant};
use crate::logic::format::format_count;
use crate::util::qs_pairs;
use crate::widgets::{CardGrid, PageFetcher, PageRes};
use inbox_card::InboxCard;
use logic::{FAN_TABS, QueueOutcome, STATE_TABS, empty_text, fan_tab, kind_label, queue_outcome, run_kind, run_notice, single_folder_default};

mod inbox_card;
mod logic;

#[derive(Clone, PartialEq)]
enum Note {
    Info(String),
    Warn(String),
    Err(String),
}

impl Note {
    fn parts(&self) -> (&'static str, &str) {
        match self {
            Note::Info(s) => ("hv-note", s),
            Note::Warn(s) => ("hv-note warn", s),
            Note::Err(s) => ("hv-note err", s),
        }
    }
}

fn inbox_fetcher(state: RwSignal<String>) -> PageFetcher<HarvestItemOut> {
    Arc::new(move |req| {
        let st = state.get_untracked();
        let mut pairs: Vec<(String, String)> = vec![];
        if st != "all" {
            pairs.push(("state".into(), st));
        }
        pairs.push(("offset".into(), req.offset.to_string()));
        pairs.push(("limit".into(), req.limit.to_string()));
        let url = format!("/harvest/items{}", qs_pairs(&pairs));
        Box::pin(async move {
            let p: Page<HarvestItemOut> = api::get(&url).await?;
            Ok(PageRes { rows: p.items, total: p.total.max(0) as usize })
        })
    })
}

#[component]
pub fn HarvestPage() -> impl IntoView {
    let jobs = use_jobs();
    crate::pages::explore::qh::use_title(true, || "Harvest".to_string());
    let input = RwSignal::new(String::new());
    let resolved = RwSignal::new(None::<ResolveResult>);
    let resolving = RwSignal::new(false);
    let limit = RwSignal::new(200i64);
    let tab = RwSignal::new(None::<String>);
    let note = RwSignal::new(None::<Note>);
    let single_folder = RwSignal::new(false);
    let include_in_library = RwSignal::new(false);

    let state = RwSignal::new("new".to_string());
    let selected = RwSignal::new(BTreeSet::<i64>::new());
    let all_matching = RwSignal::new(false);
    let needs_confirm = RwSignal::new(0usize);
    let queue_busy = RwSignal::new(false);
    let refresh = RwSignal::new(0u64);
    let flipped = RwSignal::new(BTreeSet::<i64>::new());

    let run_job = RwSignal::new(None::<String>);
    let progress = RwSignal::new(None::<HarvestProgress>);

    let stats = data::use_query::<BTreeMap<String, i64>>(|| Some(QuerySpec::new("/harvest/stats", &["harvest"])));
    let identity = data::use_query::<IdentityStatus>(|| Some(QuerySpec::new("/harvest/identity", &[])));
    let count_of = move |s: &str| stats.data.with(|d| d.as_ref().and_then(|m| m.get(s).copied()).unwrap_or(0));
    let all_count = Signal::derive(move || stats.data.with(|d| d.as_ref().map(|m| m.values().sum::<i64>()).unwrap_or(0)));
    let tab_total = Signal::derive(move || {
        let s = state.get();
        if s == "all" { all_count.get() } else { stats.data.with(|d| d.as_ref().and_then(|m| m.get(&s).copied()).unwrap_or(0)) }
    });

    let reload = move || {
        data::invalidate_prefix("/harvest/stats");
        refresh.update(|r| *r += 1);
    };

    // New items landing (from this page's run or from another source): keep the counts honest.
    use_topic::<HarvestCompleted>("harvest.completed", move |_| {
        data::invalidate_prefix("/harvest/stats");
        refresh.update(|r| *r += 1);
    });
    use_topic::<HarvestProgress>("harvest.progress", move |p| {
        if run_job.with_untracked(|j| j.as_deref() == Some(p.job_id.as_str())) {
            progress.set(Some(p));
        }
    });

    // ---- 1. identify ----
    let resolve = move || {
        let text = input.get_untracked();
        if text.trim().is_empty() || resolving.get_untracked() {
            return;
        }
        resolving.set(true);
        note.set(None);
        spawn_local(async move {
            match api::post::<_, ResolveResult>("/harvest/resolve", &ResolveRequest { input: text }).await {
                Ok(r) => {
                    tab.set(fan_tab(&r.kind));
                    single_folder.set(single_folder_default(&r.kind));
                    resolved.set(Some(r));
                }
                Err(e) => note.set(Some(Note::Err(crate::pages::explore::qh::err_text(&e)))),
            }
            let _ = resolving.try_set(false);
        });
    };
    let resolve = Arc::new(resolve);
    // `/harvest?url=...`: arrive from a band page with the source already filled in and identified.
    let preset = leptos_router::hooks::use_query_map().get_untracked().get("url").unwrap_or_default();
    if !preset.is_empty() {
        input.set(preset);
        let r = resolve.clone();
        crate::util::raf(move || r());
    }

    // ---- 2. harvest (a job) ----
    let finish = move |r: RunResult| {
        run_job.set(None);
        progress.set(None);
        note.set(Some(Note::Info(run_notice(&r))));
        reload();
    };
    let run = move || {
        let Some(r) = resolved.get_untracked() else { return };
        if run_job.get_untracked().is_some() {
            return;
        }
        let kind = run_kind(tab.get_untracked().as_deref(), Some(&r.kind));
        let body = RunRequest {
            label_name: (r.kind == "label").then(|| r.label.clone()),
            kind,
            url: r.url.clone(),
            // A label page names the imprint for everything on it: passing it files the releases under it.
            text: (r.kind == "url_list").then(|| input.get_untracked()),
            tags: vec![],
            slice: "new".into(),
            genre: None,
            geoname_id: 0,
            limit: limit.get_untracked().clamp(1, 25_000),
            depth: "shallow".into(),
        };
        note.set(None);
        run_job.set(Some(String::new()));
        spawn_local(async move {
            let id = match api::post::<_, Accepted>("/harvest/run", &body).await {
                Ok(a) => a.job_id,
                Err(e) => {
                    let _ = run_job.try_set(None);
                    let _ = note.try_set(Some(Note::Err(e.message())));
                    return;
                }
            };
            let _ = run_job.try_set(Some(id.clone()));
            // The result arrives in `harvest.completed` and at GET /harvest/runs/{id}. Poll that as
            // well, so a dropped socket cannot leave the page waiting forever.
            loop {
                gloo_timers::future::TimeoutFuture::new(1500).await;
                if run_job.try_get_untracked().flatten().as_deref() != Some(id.as_str()) {
                    return;
                }
                match api::get::<RunResult>(&format!("/harvest/runs/{id}")).await {
                    Ok(r) => {
                        let _ = finish(r);
                        return;
                    }
                    Err(e) if e.status == 404 => {
                        let failed = jobs.jobs.with_untracked(|j| j.iter().find(|x| x.id == id).filter(|x| matches!(x.status.as_str(), "failed" | "cancelled")).map(|x| x.error.clone().unwrap_or_else(|| format!("The harvest {}", x.status))));
                        if let Some(msg) = failed {
                            let _ = run_job.try_set(None);
                            let _ = progress.try_set(None);
                            let _ = note.try_set(Some(Note::Err(msg)));
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = run_job.try_set(None);
                        let _ = progress.try_set(None);
                        let _ = note.try_set(Some(Note::Err(e.message())));
                        return;
                    }
                }
            }
        });
    };
    let run = Arc::new(run);

    // ---- queue ----
    let queue = move |allow_unowned: bool| {
        let all = all_matching.get_untracked();
        let ids: Vec<i64> = selected.get_untracked().into_iter().collect();
        if !all && ids.is_empty() {
            return;
        }
        queue_busy.set(true);
        let body = QueueRequest {
            item_ids: if all { vec![] } else { ids },
            all_matching: all,
            state: state.get_untracked(),
            target_subdir: resolved.get_untracked().map(|r| r.label),
            allow_unowned,
            include_in_library: include_in_library.get_untracked(),
            single_folder: single_folder.get_untracked(),
            ..Default::default()
        };
        spawn_local(async move {
            match api::post::<_, QueueResult>("/harvest/items/queue", &body).await {
                Ok(r) => match queue_outcome(&r) {
                    QueueOutcome::NeedsConfirmation(n) => {
                        // Keep the selection: the button turns into "Queue anyway" and resends exactly these items.
                        let _ = needs_confirm.try_set(n);
                        let _ = note.try_set(Some(Note::Warn(format!(
                            "{n} item(s) are neither owned nor offered free. bandcamp-dl would fetch only the public preview stream. Press \u{201c}Queue anyway\u{201d} to take that."
                        ))));
                    }
                    QueueOutcome::Queued(msg) => {
                        let _ = selected.try_set(BTreeSet::new());
                        let _ = all_matching.try_set(false);
                        let _ = needs_confirm.try_set(0);
                        let _ = note.try_set(Some(Note::Info(msg)));
                        let _ = flipped.try_set(BTreeSet::new());
                        data::invalidate_prefix("/harvest/stats");
                        let _ = refresh.try_update(|r| *r += 1);
                    }
                },
                Err(e) => {
                    let _ = note.try_set(Some(Note::Err(e.message())));
                }
            }
            let _ = queue_busy.try_set(false);
        });
    };
    let queue = Arc::new(queue);

    let clear_selection = move || {
        selected.set(BTreeSet::new());
        all_matching.set(false);
        needs_confirm.set(0);
    };
    let on_toggle = Callback::new(move |id: i64| {
        needs_confirm.set(0);
        selected.update(|s| {
            if !s.remove(&id) {
                s.insert(id);
            }
        });
    });
    let sel_count = Signal::derive(move || if all_matching.get() { tab_total.get() } else { selected.with(|s| s.len()) as i64 });

    // CardGrid reserves 40px of gutter on top of the page padding, so a phone needs a smaller minimum to get two columns.
    let min_card = if crate::util::is_mobile() { 128.0 } else { 160.0 };
    let fetcher = inbox_fetcher(state);
    let source_key = Signal::derive(move || format!("{}|{}", state.get(), refresh.get()));
    let render = Callback::new(move |(item, _w): (HarvestItemOut, f64)| {
        let id = item.id;
        let sel = Signal::derive(move || all_matching.get() || selected.with(|s| s.contains(&id)));
        let locked = Signal::derive(move || all_matching.get());
        view! { <InboxCard item=item selected=sel locked=locked on_toggle=on_toggle flipped=flipped on_changed=Callback::new(move |_| data::invalidate_prefix("/harvest/stats")) /> }.into_any()
    });

    let (res1, res2) = (resolve.clone(), resolve.clone());
    let (run1, queue1, queue2) = (run.clone(), queue.clone(), queue.clone());
    let header = ds::children(move || {
        let (res1, res2, run1, queue1, queue2) = (res1.clone(), res2.clone(), run1.clone(), queue1.clone(), queue2.clone());
        view! {
            <section class="hv-step card">
                <p class="hv-lead"><b>"1 \u{b7} Find a source."</b>" Paste a Bandcamp page \u{2014} an artist or label, a genre\u{2019}s discover feed, your collection or wishlist, or a plain list of album URLs \u{2014} and press Identify to see what it is."</p>
                <div class="hv-find">
                    <input class="input mono hv-input" prop:value=move || input.get() spellcheck="false" autocomplete="off" aria-label="Bandcamp source"
                        placeholder="bandcamp.com/yourname/wishlist \u{b7} hyperdub.bandcamp.com \u{b7} bandcamp.com/discover/techno \u{b7} or paste album URLs"
                        on:input=move |ev| { input.set(event_target_value(&ev)); resolved.set(None); }
                        on:keydown={ let r = res1.clone(); move |ev| if ev.key() == "Enter" { r() } } />
                    <Button icon="search" busy=resolving disabled=Signal::derive(move || input.with(|i| i.trim().is_empty()))
                        on_click=move |_| res2()>{move || if resolving.get() { "Checking\u{2026}" } else { "Identify" }}</Button>
                </div>
                {move || resolved.get().map(|r| {
                    let run1 = run1.clone();
                    let kind = r.kind.clone();
                    view! {
                        <div class="hv-resolved">
                            <span class="badge badge-accent">{kind_label(&kind).to_string()}</span>
                            <span class="hv-rlabel">{r.label.clone()}</span>
                            <span class="mono muted hv-rdetail">{r.detail.clone()}</span>
                            {move || tab.get().is_some().then(|| view! {
                                <div class="segmented" role="group" aria-label="Which list">
                                    {FAN_TABS.iter().map(|t| { let t = t.to_string(); let t2 = t.clone(); let t3 = t.clone(); view! {
                                        <button type="button" aria-pressed=move || (tab.get().as_deref() == Some(t.as_str())).to_string()
                                            on:click=move |_| { tab.set(Some(t2.clone())); single_folder.set(single_folder_default(&t2)); }>{t3}</button>
                                    } }).collect_view()}
                                </div>
                            })}
                            <span class="spacer"></span>
                            <label class="hv-limit faint" title="Stop after this many releases. A big label or feed can run to thousands.">
                                "fetch at most"
                                <input class="input mono" type="number" min="1" max="25000" prop:value=move || limit.get().to_string()
                                    on:input=move |ev| limit.set(event_target_value(&ev).parse().unwrap_or(200)) />
                            </label>
                            <Button variant=Variant::Primary icon="sparkles" busy=Signal::derive(move || run_job.get().is_some()) on_click=move |_| run1()>
                                {move || if run_job.get().is_some() { "Harvesting\u{2026}" } else { "Harvest" }}
                            </Button>
                        </div>
                    }
                })}
                {move || run_job.get().map(|_| {
                    let frac = Signal::derive(move || progress.get().and_then(|p| p.total.filter(|t| *t > 0).map(|t| p.seen as f64 / t as f64)));
                    view! {
                        <div class="hv-progress" role="status">
                            <Meter value=frac label="Harvest progress" />
                            <span class="mono muted">
                                {move || match progress.get() {
                                    Some(p) => format!("{} seen{} \u{b7} {} new", format_count(p.seen), p.total.map(|t| format!(" of {}", format_count(t))).unwrap_or_default(), format_count(p.new)),
                                    None => "starting\u{2026}".into(),
                                }}
                            </span>
                        </div>
                        <p class="faint hv-hint">"Sweeping the source into the inbox. Nothing is downloaded yet \u{2014} this only collects what exists. Bandcamp is fetched at a polite rate, so a large source takes a few minutes."</p>
                    }
                })}
                {move || note.get().map(|n| { let (cls, text) = n.parts(); view! { <p class=cls role=if cls == "hv-note err" { "alert" } else { "status" }>{text.to_string()}</p> } })}
                {move || identity.data.with(|d| d.as_ref().is_some_and(|i| !i.configured)).then(|| view! {
                    <p class="faint hv-hint">"Your own collection and wishlist need you to sign in to Bandcamp \u{2014} do it in "<a href="/settings?tab=downloads" class="hv-a">"Settings"</a>". Public artist, label, tag and discover pages work without it."</p>
                })}
            </section>
            <p class="hv-lead hv-lead2"><b>"2 \u{b7} Pick from the inbox."</b>" Everything harvested lands here first; nothing downloads by itself. Tap covers to select, then queue them as one download job."</p>
            <div class="hv-tabrow">
                <div class="hv-tabs" role="tablist">
                    {STATE_TABS.iter().map(|(s, label)| {
                        let (s1, s2) = (s.to_string(), s.to_string());
                        view! {
                            <button type="button" role="tab" class="hv-tab" aria-selected=move || (state.get() == s1).to_string()
                                on:click=move |_| { state.set(s2.clone()); selected.set(BTreeSet::new()); all_matching.set(false); needs_confirm.set(0); flipped.set(BTreeSet::new()); }>
                                {*label}
                                <span class="mono faint hv-cnt">{
                                    let s3 = s.to_string();
                                    move || format_count(if s3 == "all" { all_count.get() } else { count_of(&s3) })
                                }</span>
                            </button>
                        }
                    }).collect_view()}
                </div>
                {move || {
                    let t = tab_total.get();
                    (t > 0 && state.get() != "all" && !all_matching.get()).then(|| view! {
                        <Button size=Size::Sm variant=Variant::Ghost title="Select every item in this tab, not only the ones loaded"
                            on_click=move |_| { selected.set(BTreeSet::new()); all_matching.set(true) }>
                            {move || format!("Select all {}", format_count(tab_total.get()))}
                        </Button>
                    })
                }}
            </div>
            {move || {
                let (q1, q2) = (queue1.clone(), queue2.clone());
                let variant = if needs_confirm.get() > 0 { Variant::Danger } else { Variant::Primary };
                let hint = if needs_confirm.get() > 0 { "These items are neither owned nor free: only the public preview stream can be fetched." } else { "Create one download job for the selected items." };
                (sel_count.get() > 0).then(|| view! {
                    <div class="hv-sel" role="region" aria-label="Selection">
                        <span class="mono hv-selcount" role="status">
                            {move || if all_matching.get() { format!("All {} in \u{201c}{}\u{201d} selected", format_count(tab_total.get()), state.get().replace('_', " ")) } else { format!("{} selected", format_count(sel_count.get())) }}
                        </span>
                        <Button size=Size::Sm variant=Variant::Ghost on_click=move |_| clear_selection()>"Clear"</Button>
                        <span class="spacer"></span>
                        <button type="button" class="chip" aria-pressed=move || single_folder.get().to_string()
                            title="Everything lands directly in one folder \u{2014} no artist/album subfolders." on:click=move |_| single_folder.update(|v| *v = !*v)>
                            <Icon name="folder" />"single folder"</button>
                        <button type="button" class="chip" aria-pressed=move || include_in_library.get().to_string()
                            title="Also download items you already have in the library, instead of skipping them." on:click=move |_| include_in_library.update(|v| *v = !*v)>
                            <Icon name="check" />"include in library"</button>
                        <Button variant=variant size=Size::Sm icon="download" busy=queue_busy title=hint
                            on_click=move |_| { if needs_confirm.get_untracked() > 0 { q1(true) } else { q2(false) } }>
                            {move || format!("Queue {}{}", format_count(sel_count.get()), if needs_confirm.get() > 0 { " anyway" } else { "" })}
                        </Button>
                    </div>
                })
            }}
        }
    });

    view! {
        <div class="page hv-page">
            <PageHeader title="Harvest" subtitle="Sweep a whole Bandcamp source into an inbox, then pick what to download" />
            <CardGrid fetch=fetcher source_key=source_key min_card_w=min_card meta_h=76.0 render=render header=header
                empty=move || view! { <EmptyState icon="sparkles" title="Inbox is empty here" hint=empty_text(&state.get()) /> } />
        </div>
    }
}
