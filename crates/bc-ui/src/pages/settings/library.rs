//! Settings > Library: stats, roots (add / enable / watch / scan / move), library
//! scope, snippet filter. Scan and move progress arrive over WS and are patched in
//! place (no refetch on progress ticks).
use std::collections::BTreeMap;

use bc_types::Accepted;
use bc_types::library::*;
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::common::{Notice, Stat, SysCard, QueryError, qh};
use super::logic::split_warnings;
use crate::api;
use crate::data::{QuerySpec, use_query, use_topic};
use crate::ds::{Badge, Button, Dialog, Icon, Meter, Select, SelectOption, Size, Skeleton, Switch, Tone, Variant, toast_err, toast_ok};
use crate::logic::format::{format_bytes, format_count, format_long_duration};
use crate::widgets::FolderPicker;

#[component]
pub fn LibrarySection() -> impl IntoView {
    let stats = qh(use_query::<LibraryStats>(|| Some(QuerySpec::new("/library/stats", &["stats", "track"]))));
    let progress = RwSignal::new(BTreeMap::<i64, ScanProgress>::new());
    let last_scan = RwSignal::new(Vec::<ScanResult>::new());
    let scanning = RwSignal::new(false);
    let move_root = RwSignal::new(None::<RootOut>);
    let move_open = RwSignal::new(false);
    let stopping = RwSignal::new(false);

    use_topic::<ScanProgress>("library.scan.progress", move |p| {
        progress.update(|m| {
            m.insert(p.root_id, p);
        });
    });
    use_topic::<serde_json::Value>("library.scan.done", move |v| {
        scanning.set(false);
        stopping.set(false);
        progress.set(BTreeMap::new());
        if let Some(id) = v.get("job_id").and_then(|j| j.as_str()).map(str::to_string) {
            spawn_local(async move {
                if let Ok(s) = api::get::<ScanStatus>(&format!("/library/scan/{id}")).await {
                    last_scan.set(s.results);
                }
            });
        }
        stats.refetch();
    });

    let start_scan = move |root: Option<i64>| {
        scanning.set(true);
        last_scan.set(vec![]);
        let url = match root {
            Some(id) => format!("/library/scan?root_id={id}"),
            None => "/library/scan".to_string(),
        };
        spawn_local(async move {
            if let Err(e) = api::post::<_, Accepted>(&url, &serde_json::json!({})).await {
                scanning.set(false);
                toast_err(&e.message());
            }
        });
    };

    // the running scan, whoever started it
    let job = Memo::new(move |_| progress.with(|m| m.values().next().map(|p| p.job_id.clone())));
    let stop_scan = move |_| {
        let Some(id) = job.get_untracked() else { return };
        stopping.set(true);
        spawn_local(async move {
            if let Err(e) = api::post::<_, Accepted>(&format!("/library/scan/{id}/cancel"), &serde_json::json!({})).await {
                stopping.set(false);
                toast_err(&e.message());
            }
        });
    };

    let tracks = Signal::derive(move || stats.data.get().map(|s| format_count(s.tracks)).unwrap_or_else(|| "-".into()));
    let albums = Signal::derive(move || stats.data.get().map(|s| format_count(s.releases)).unwrap_or_else(|| "-".into()));
    let artists = Signal::derive(move || stats.data.get().map(|s| format_count(s.artists)).unwrap_or_else(|| "-".into()));
    let tags = Signal::derive(move || stats.data.get().map(|s| format_count(s.tags)).unwrap_or_else(|| "-".into()));
    let size = Signal::derive(move || stats.data.get().map(|s| format_bytes(s.total_bytes as f64)).unwrap_or_else(|| "-".into()));
    let dur = Signal::derive(move || stats.data.get().map(|s| format_long_duration(s.total_duration_ms as f64)).unwrap_or_else(|| "-".into()));
    let analysed = Signal::derive(move || stats.data.get().map(|s| format_count(s.analyzed)).unwrap_or_else(|| "-".into()));
    let missing = Signal::derive(move || stats.data.get().map(|s| format_count(s.missing_files)).unwrap_or_else(|| "-".into()));
    let disk = Signal::derive(move || stats.data.get().and_then(|s| s.disk.clone()));

    view! {
        <SysCard title="Library" icon="layers">
            <QueryError q=stats />
            <div class="sys-stats">
                <Stat label="Tracks" value=tracks />
                <Stat label="Albums" value=albums />
                <Stat label="Artists" value=artists />
                <Stat label="Tags" value=tags />
                <Stat label="Size" value=size />
                <Stat label="Duration" value=dur />
                <Stat label="Analysed" value=analysed />
                <Stat label="Missing files" value=missing />
            </div>
            {move || disk.get().map(|d| {
                let used = d.used_bytes as f64 / (d.total_bytes.max(1)) as f64;
                view! {
                    <div class="disk-usage">
                        <div class="row"><Icon name="hdd" />
                            <span class="mono">{format_bytes(d.free_bytes as f64)}</span><span class="muted">" free of "</span>
                            <span class="mono">{format_bytes(d.total_bytes as f64)}</span></div>
                        <Meter value=Some(used) tone={if used > 0.92 { Tone::Danger } else if used > 0.8 { Tone::Warn } else { Tone::Neutral }} label="Disk used" />
                    </div>
                }
            })}
        </SysCard>

        <SysCard title="Library roots" icon="folder"
            hint="Folders Crate indexes. Files are read in place and never modified by a scan."
            actions=crate::ds::children(move || view! {
                <Show when=move || job.get().is_some()>
                    <Button size=Size::Sm icon="x" busy=stopping title="Stop the scan; tracks already added stay in the library"
                        on_click=stop_scan>"Stop"</Button>
                </Show>
                <Button size=Size::Sm icon="refresh" busy=scanning on_click=move |_| start_scan(None)>"Scan all"</Button>
            })>
            <Show when=move || stats.data.get().map(|s| s.roots.is_empty()).unwrap_or(false)>
                <p class="sys-empty">"No roots yet. Add the folder holding your music below."</p>
            </Show>
            <Show when=move || stats.data.get().is_none() && stats.error.get().is_none()><Skeleton height="64px" /></Show>
            <div class="root-list">
                <For each=move || stats.data.get().map(|s| s.roots.clone()).unwrap_or_default() key=|r| (r.id, r.track_count, r.last_scan_at.clone()) let:root>
                    <RootRow root=root progress=progress scanning=scanning
                        on_scan=Callback::new(move |id| start_scan(Some(id)))
                        on_move=Callback::new(move |r: RootOut| { move_root.set(Some(r)); move_open.set(true); }) />
                </For>
            </div>
            <AddRoot on_added=Callback::new(move |_| stats.refetch()) />
            <Show when=move || !last_scan.get().is_empty()>
                <div class="scan-results" aria-live="polite">
                    <For each=move || last_scan.get() key=|r| r.root_id let:r>
                        <div class="scan-result">
                            <div class="mono faint truncate">{r.root_path.clone()}</div>
                            <div class="mono">
                                {format!("{} seen, {} added, {} updated, {} unchanged", r.files_seen, r.files_added, r.files_updated, r.files_unchanged)}
                                {(r.files_missing > 0).then(|| format!(", {} missing", r.files_missing))}
                                {format!(" ({} ms)", r.duration_ms)}
                            </div>
                            {r.errors.iter().take(5).map(|e| view! { <div class="sys-notice danger"><Icon name="alert" /><span class="truncate">{e.clone()}</span></div> }).collect_view()}
                        </div>
                    </For>
                </div>
            </Show>
        </SysCard>

        <MoveRootDialog root=move_root open=move_open on_done=Callback::new(move |_| stats.refetch()) />
        <ScopeCards />
    }
}

#[component]
fn RootRow(root: RootOut, progress: RwSignal<BTreeMap<i64, ScanProgress>>, scanning: RwSignal<bool>, on_scan: Callback<i64>, on_move: Callback<RootOut>) -> impl IntoView {
    let id = root.id;
    let enabled = RwSignal::new(root.enabled);
    let watch = RwSignal::new(root.watch);
    let patch = move |p: RootPatch| {
        spawn_local(async move {
            if let Err(e) = api::patch::<_, RootOut>(&format!("/library/roots/{id}"), &p).await {
                toast_err(&e.message());
            }
        });
    };
    let prog = Signal::derive(move || progress.with(|m| m.get(&id).cloned()));
    let meter = Signal::derive(move || prog.get().and_then(|p| p.total.filter(|t| *t > 0).map(|t| p.seen as f64 / t as f64)));
    let r2 = root.clone();
    let sub = format!(
        "{} files{}",
        format_count(root.track_count),
        root.last_scan_ms.map(|ms| format!(", last scan {ms} ms")).unwrap_or_default()
    );
    view! {
        <div class="root-row">
            <div class="root-main">
                <div class="mono truncate" title=root.path.clone()>{root.path.clone()}</div>
                <div class="faint root-sub">
                    <Badge icon={if root.kind == "downloads" { "download" } else { "folder" }}>{root.kind.clone()}</Badge>
                    <span class="mono">{sub}</span>
                </div>
                {move || prog.get().map(|p| view! {
                    <div class="root-progress" role="status">
                        <Meter value=meter label="Scan progress" />
                        <span class="mono faint">{format!("{} {} {}", p.phase, format_count(p.seen), p.total.map(|t| format!("/ {}", format_count(t))).unwrap_or_default())}</span>
                    </div>
                })}
            </div>
            <div class="root-ctrls">
                <label class="switch-label" title="Include in listings and scans">
                    <Switch value=enabled label="Enabled" on_change=Callback::new(move |v: bool| patch(RootPatch { enabled: Some(v), ..Default::default() })) />
                    <span>"On"</span>
                </label>
                <label class="switch-label" title="Watch the folder for new files">
                    <Switch value=watch label="Watch for changes" on_change=Callback::new(move |v: bool| patch(RootPatch { watch: Some(v), ..Default::default() })) />
                    <span>"Watch"</span>
                </label>
                <Button size=Size::Sm icon="refresh" disabled=scanning on_click=move |_| on_scan.run(id)>"Scan"</Button>
                <Button size=Size::Sm icon="hdd" title="Move this folder to another drive" on_click=move |_| on_move.run(r2.clone())>"Move"</Button>
            </div>
        </div>
    }
}

#[component]
fn AddRoot(on_added: Callback<()>) -> impl IntoView {
    let path = RwSignal::new(String::new());
    let kind = RwSignal::new("library".to_string());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let add = move || {
        let p = path.get_untracked().trim().to_string();
        if p.is_empty() {
            return;
        }
        busy.set(true);
        error.set(None);
        let k = kind.get_untracked();
        spawn_local(async move {
            match api::post::<_, RootOut>("/library/roots", &AddRootRequest { path: p, kind: k }).await {
                Ok(_) => {
                    path.set(String::new());
                    toast_ok("Root added");
                    on_added.run(());
                }
                Err(e) => error.set(Some(e.message())),
            }
            busy.set(false);
        });
    };
    let add2 = add.clone();
    let picking = RwSignal::new(false);
    view! {
        <div class="add-root">
            <input class="input mono grow" placeholder="/home/you/Music" spellcheck="false" aria-label="New root path"
                prop:value=move || path.get() on:input=move |ev| path.set(event_target_value(&ev))
                on:keydown=move |ev| if ev.key() == "Enter" { add2() } />
            <div style="width:140px"><Select options=vec![SelectOption::new("library", "Library"), SelectOption::new("downloads", "Downloads")] value=kind aria_label="Root kind" /></div>
            <Button icon="folder" title="Pick a folder on the machine running bc" on_click=move |_| picking.set(true)>"Browse"</Button>
            <Button variant=Variant::Primary icon="plus" busy=busy disabled=Signal::derive(move || path.get().trim().is_empty()) on_click=move |_| add()>"Add"</Button>
        </div>
        <Notice text=error />
        <FolderPicker open=picking start=Signal::derive(move || path.get()) on_pick=Callback::new(move |p| path.set(p)) />
    }
}

/// Plan, then run, a move of a root to another folder or drive.
#[component]
fn MoveRootDialog(root: RwSignal<Option<RootOut>>, open: RwSignal<bool>, on_done: Callback<()>) -> impl IntoView {
    let target = RwSignal::new(String::new());
    let plan = RwSignal::new(None::<MovePlanOut>);
    let checking = RwSignal::new(false);
    let moving = RwSignal::new(false);
    let prog = RwSignal::new(None::<MoveProgress>);
    let error = RwSignal::new(None::<String>);

    Effect::new(move |_| {
        if !open.get() && !moving.get_untracked() {
            plan.set(None);
            error.set(None);
            prog.set(None);
        }
    });
    use_topic::<MoveProgress>("library.move.progress", move |p| {
        let mine = root.with_untracked(|r| r.as_ref().map(|r| r.id)) == Some(p.root_id);
        if !mine {
            return;
        }
        let done = p.done;
        prog.set(Some(p));
        if done {
            moving.set(false);
            toast_ok("Root moved");
            open.set(false);
            on_done.run(());
        }
    });

    let check = move || {
        let Some(id) = root.get_untracked().map(|r| r.id) else { return };
        let t = target.get_untracked().trim().to_string();
        if t.is_empty() {
            return;
        }
        checking.set(true);
        error.set(None);
        spawn_local(async move {
            match api::post::<_, MovePlanOut>(&format!("/library/roots/{id}/move/plan"), &MoveRequest { target_path: t, dry_run: true }).await {
                Ok(p) => plan.set(Some(p)),
                Err(e) => error.set(Some(e.message())),
            }
            checking.set(false);
        });
    };
    let check2 = check.clone();
    let run = move |_| {
        let Some(id) = root.get_untracked().map(|r| r.id) else { return };
        let t = target.get_untracked().trim().to_string();
        moving.set(true);
        error.set(None);
        spawn_local(async move {
            if let Err(e) = api::post::<_, serde_json::Value>(&format!("/library/roots/{id}/move"), &MoveRequest { target_path: t, dry_run: false }).await {
                moving.set(false);
                error.set(Some(e.message()));
            }
        });
    };
    let can_move = Signal::derive(move || plan.get().map(|p| p.ok).unwrap_or(false) && !moving.get());
    let title = Signal::derive(move || "Move to another folder or drive".to_string());
    let footer = crate::ds::children(move || view! {
        <Button variant=Variant::Ghost disabled=moving on_click=move |_| open.set(false)>"Close"</Button>
        <Button variant=Variant::Primary icon="hdd" busy=moving disabled=Signal::derive(move || !can_move.get()) on_click=run.clone()>
            {move || plan.get().map(|p| format!("Move {} files", format_count(p.file_count))).unwrap_or_else(|| "Move".into())}
        </Button>
    });
    view! {
        <Dialog open=open title=title footer=footer>
            <div class="move-dialog">
                <div class="mono faint truncate">{move || root.get().map(|r| r.path).unwrap_or_default()}</div>
                <div class="row gap">
                    <input class="input mono grow" placeholder="/mnt/new-drive/music" spellcheck="false" aria-label="Target folder"
                        prop:value=move || target.get()
                        on:input=move |ev| { target.set(event_target_value(&ev)); plan.set(None); }
                        on:keydown={ let c = check2.clone(); move |ev| if ev.key() == "Enter" { c() } } />
                    <Button busy=checking disabled=Signal::derive(move || target.get().trim().is_empty() || moving.get()) on_click=move |_| check()>"Check"</Button>
                </div>
                <Notice text=error />
                {move || plan.get().map(|p| {
                    let (blockers, notes) = split_warnings(&p.warnings);
                    view! {
                        <div class="plan">
                            <div class="mono plan-facts">
                                <span>{format!("{} files", format_count(p.file_count))}</span>
                                <span>{format_bytes(p.total_bytes as f64)}</span>
                                <span>{format!("{} free on target", format_bytes(p.free_bytes as f64))}</span>
                            </div>
                            <Badge tone={if p.same_filesystem { Tone::Ok } else { Tone::Info }} icon={if p.same_filesystem { "check-circle" } else { "info" }}>
                                {if p.same_filesystem { "Same drive: instant rename" } else { "Different drive: copy, verify, then delete" }}
                            </Badge>
                            {blockers.into_iter().map(|w| view! { <p class="sys-notice danger"><Icon name="x-circle" />{w}</p> }).collect_view()}
                            {notes.into_iter().map(|w| view! { <p class="sys-notice warn"><Icon name="alert" />{w}</p> }).collect_view()}
                            {(!p.same_filesystem && p.ok).then(|| view! { <p class="sys-hint faint">"Each file is copied, verified, then removed, so an interruption never leaves a half-written track. Safe to stop and re-run."</p> })}
                        </div>
                    }
                })}
                {move || prog.get().filter(|_| moving.get()).map(|p| {
                    let v = (p.total > 0).then(|| p.moved as f64 / p.total as f64);
                    view! {
                        <div class="root-progress" role="status">
                            <Meter value=v label="Move progress" />
                            <span class="mono faint">{format!("{} / {} files, {}", format_count(p.moved), format_count(p.total), format_bytes(p.bytes_moved as f64))}</span>
                            {p.current.map(|c| view! { <span class="mono faint truncate">{c}</span> })}
                        </div>
                    }
                })}
            </div>
        </Dialog>
    }
}

#[component]
fn ScopeCards() -> impl IntoView {
    let scope = qh(use_query::<LibraryScopeOut>(|| Some(QuerySpec::new("/library/scope", &["scope"]))));
    let snip = qh(use_query::<SnippetSettingOut>(|| Some(QuerySpec::new("/library/snippets", &["scope"]))));
    let unified = RwSignal::new(false);
    let hidden = RwSignal::new(false);
    Effect::new(move |_| {
        if let Some(s) = scope.data.get() {
            unified.set(s.unified)
        }
    });
    Effect::new(move |_| {
        if let Some(s) = snip.data.get() {
            hidden.set(s.hidden)
        }
    });
    // Both settings are predicates every listing applies: refetch everything.
    let set_scope = move |v: bool| {
        spawn_local(async move {
            match api::put::<_, LibraryScopeOut>("/library/scope", &LibraryScopeIn { unified: v }).await {
                Ok(s) => {
                    crate::data::cache::patch::<LibraryScopeOut>("/library/scope", |x| *x = s);
                    crate::data::invalidate_all();
                }
                Err(e) => {
                    unified.set(!v);
                    toast_err(&e.message());
                }
            }
        });
    };
    let set_snip = move |v: bool| {
        spawn_local(async move {
            match api::put::<_, SnippetSettingOut>("/library/snippets", &SnippetSettingIn { hidden: v }).await {
                Ok(s) => {
                    crate::data::cache::patch::<SnippetSettingOut>("/library/snippets", |x| *x = s);
                    crate::data::invalidate_all();
                }
                Err(e) => {
                    hidden.set(!v);
                    toast_err(&e.message());
                }
            }
        });
    };
    view! {
        <SysCard title="Library scope" icon="users"
            hint="Records downloaded from other people's wishlists sit on that person's shelf: out of Albums, Artists, Tracks, Home, search and the crate until you say otherwise. To keep one for good, move it into your library from its menu.">
            <div class="pref-row">
                <div class="grow"><div class="name">"Show other people's downloads in my library"</div>
                    <div class="desc faint mono">{move || scope.data.get().map(|s| format!("{} releases on shelves", format_count(s.foreign_releases))).unwrap_or_default()}</div></div>
                <Switch value=unified label="Show other people's downloads" on_change=Callback::new(set_scope) />
            </div>
        </SysCard>
        <SysCard title="Snippets" icon="scissors"
            hint="A record that is not for sale as a download still has audio on its page: a ninety-second cut per side, or one montage. They land in your library as ordinary tracks (named [SNIPPET], (clip only), Preview Snippets) and stop two minutes in when played in a set. This keeps them out of listings, search, the crate and everything playlists or automix draw from.">
            <div class="pref-row">
                <div class="grow"><div class="name">"Keep preview clips out of my library and playlists"</div>
                    <div class="desc faint mono">{move || snip.data.get().map(|s| format!("{} clips, {} clip-only records", format_count(s.snippet_tracks), format_count(s.snippet_releases))).unwrap_or_default()}</div></div>
                <Switch value=hidden label="Hide snippets" on_change=Callback::new(set_snip) />
            </div>
            <p class="sys-hint faint">"Nothing is deleted. An album page always lists its own tracks, and a playlist or set that already contains a clip keeps it, badged SNIPPET. Detection reads the title, so it errs toward leaving music alone."</p>
        </SysCard>
    }
}
