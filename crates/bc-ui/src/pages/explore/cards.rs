//! Bandcamp releases as cards and shelves: the counterpart of the library's album cards.
//! A card is a URL and a cover, not something the app owns: playing it fetches the release
//! page first, keeping it queues a download.
use std::collections::BTreeSet;

use bc_types::bandcamp::{
    CatalogResult, DownloadCatalogRequest, DownloadReleasesRequest, FollowCreate, FollowOut, FollowPatch, FollowsOut, ReleaseCardOut,
};
use bc_types::jobs::JobOut;
use bc_types::player::QueueSource;
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::logic::{DOWNLOAD_CHUNK, catalog_label, catalog_result, origin_of, release_path};
use super::play;
use crate::api;
use super::qh;
use crate::data::{self, QuerySpec};
use crate::ds::{self, Button, Icon, Size, Variant};
use crate::logic::format::format_count;
use crate::player::use_player;
use crate::widgets::common::Art;

/// The record label a grid's downloads should be filed under (label pages only).
#[derive(Clone, Debug, PartialEq)]
pub struct FileUnder {
    pub name: String,
    pub url: String,
}

/// A grid in selection mode: which cards are ticked (by release URL).
#[derive(Clone, Copy)]
pub struct Selecting {
    pub on: RwSignal<bool>,
    pub picked: RwSignal<BTreeSet<String>>,
}

impl Selecting {
    pub fn new() -> Self {
        Self { on: RwSignal::new(false), picked: RwSignal::new(BTreeSet::new()) }
    }
    pub fn toggle(&self, url: &str) {
        self.picked.update(|p| {
            if !p.remove(url) {
                p.insert(url.to_string());
            }
        });
    }
    pub fn stop(&self) {
        self.on.set(false);
        self.picked.set(BTreeSet::new());
    }
}

impl Default for Selecting {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Card
// ---------------------------------------------------------------------------

/// Play control over a card's cover. Quiet until pointed at, except while it is the thing
/// playing; a failed start says so in the label and a toast (a title is unreachable by touch).
#[component]
pub fn CardPlay(
    #[prop(into)] url: String,
    #[prop(into)] title: String,
    #[prop(default = None)] library_id: Option<i64>,
    #[prop(optional, into)] class: String,
) -> impl IntoView {
    let player = use_player();
    let busy = RwSignal::new(false);
    let err = RwSignal::new(None::<String>);
    let current = play::is_current(player, url.clone(), library_id);
    let playing = play::is_playing(player, current);
    Effect::new(move |_| {
        if let Some(e) = err.get() {
            ds::toast_err(&e);
        }
    });
    let t1 = title.clone();
    let label = move || match err.get() {
        Some(e) => format!("Play {t1}: {e}"),
        None if playing.get() => format!("Pause {t1}"),
        None => format!("Play {t1}"),
    };
    let label2 = label.clone();
    let url2 = url.clone();
    view! {
        <button type="button" class=move || format!("xc-play {class}{}{}{}", if current.get() { " on" } else { "" }, if busy.get() { " busy" } else { "" }, if err.get().is_some() { " err" } else { "" })
            aria-label=label title=label2
            disabled=move || busy.get()
            on:click=move |ev| {
                ev.prevent_default();
                ev.stop_propagation();
                if current.get_untracked() { player.toggle() } else { play::play_release(player, url2.clone(), library_id, busy, err) }
            }>
            <Icon name=ds::dyn_icon(move || if busy.get() { "refresh" } else if playing.get() { "pause" } else { "play" }) />
        </button>
    }
}

/// Queue a release for download without opening it, so sweeping forty covers stays a sweep.
#[component]
pub fn CardQueue(#[prop(into)] url: String, #[prop(into)] title: String, #[prop(into)] artist: String, #[prop(default = None)] under: Option<FileUnder>) -> impl IntoView {
    let state = RwSignal::new(0u8); // 0 idle, 1 busy, 2 queued, 3 error
    let msg = RwSignal::new(String::new());
    let t = title.clone();
    let label = move || match state.get() {
        2 => format!("{t} queued"),
        3 => format!("Queue {t} for download: {}", msg.get()),
        _ => format!("Queue {t} for download"),
    };
    let label2 = label.clone();
    let on_click = move |ev: leptos::ev::MouseEvent| {
        ev.prevent_default();
        ev.stop_propagation();
        state.set(1);
        let body = DownloadReleasesRequest {
            urls: vec![url.clone()],
            label: Some(format!("{artist} \u{2014} {title}")),
            label_name: under.as_ref().map(|u| u.name.clone()),
            label_url: under.as_ref().map(|u| u.url.clone()),
            ..Default::default()
        };
        spawn_local(async move {
            match api::post::<_, JobOut>("/explore/download", &body).await {
                Ok(_) => {
                    let _ = state.try_set(2);
                    ds::toast_ok("Queued for download");
                }
                Err(e) => {
                    let _ = msg.try_set(e.message());
                    let _ = state.try_set(3);
                    ds::toast_err(&e.message());
                }
            }
        });
    };
    view! {
        <button type="button" class=move || format!("xc-q s{}", state.get()) aria-label=label title=label2
            disabled=move || matches!(state.get(), 1 | 2) on:click=on_click>
            <Icon name=ds::dyn_icon(move || match state.get() { 1 => "refresh", 2 => "check", _ => "download" }) />
        </button>
    }
}

#[component]
pub fn ReleaseCardView(
    item: ReleaseCardOut,
    #[prop(default = None)] under: Option<FileUnder>,
    #[prop(default = None)] sel: Option<Selecting>,
) -> impl IntoView {
    let href = release_path(&item.url);
    let aria = if item.artist_name.is_empty() { item.title.clone() } else { format!("{} by {}", item.title, item.artist_name) };
    let u_pick = item.url.clone();
    let picked = Signal::derive(move || sel.is_some_and(|s| s.picked.with(|p| p.contains(&u_pick))));
    let selecting = Signal::derive(move || sel.is_some_and(|s| s.on.get()));
    let (u_play, t_play, lib) = (item.url.clone(), item.title.clone(), item.library_release_id);
    let (u_q, t_q, a_q) = (item.url.clone(), item.title.clone(), item.artist_name.clone());
    let u_tog = item.url.clone();
    let can_queue = !item.in_library;
    let aria2 = aria.clone();
    view! {
        <div class=move || format!("xc{}{}", if picked.get() { " picked" } else { "" }, if selecting.get() { " selecting" } else { "" })>
            <div class="xc-art">
                <Art src=item.art_url.clone() />
                {item.in_library.then(|| view! { <span class="xc-badge"><Icon name="check" />"In library"</span> })}
                {(!item.in_library && item.is_free_download).then(|| view! { <span class="xc-badge free">"Free"</span> })}
                {move || if selecting.get() {
                    view! { <span class="xc-tick" aria-hidden="true"><Icon name="check" /></span> }.into_any()
                } else {
                    let (u, t, a, up, tp) = (u_q.clone(), t_q.clone(), a_q.clone(), u_play.clone(), t_play.clone());
                    let under = under.clone();
                    view! {
                        <CardPlay url=up title=tp library_id=lib />
                        {can_queue.then(|| view! { <CardQueue url=u title=t artist=a under=under /> })}
                    }.into_any()
                }}
            </div>
            <div class="xc-t truncate">{item.title.clone()}</div>
            <div class="xc-a truncate">{item.artist_name.clone()}</div>
            {move || if selecting.get() {
                let u = u_tog.clone();
                view! {
                    <button type="button" class="xc-link" aria-pressed=move || picked.get().to_string() aria-label=format!("Select {}", aria2.clone())
                        on:click=move |_| if let Some(s) = sel { s.toggle(&u) }></button>
                }.into_any()
            } else {
                view! { <a class="xc-link" href=href.clone() aria-label=aria.clone()></a> }.into_any()
            }}
        </div>
    }
}

/// A wrapped (non-virtual) grid for the bounded shelves: a discography, related releases.
#[component]
pub fn ReleaseGrid(
    #[prop(into)] items: Signal<Vec<ReleaseCardOut>>,
    /// Minimum card width in px.
    #[prop(default = 150)] min: u32,
    #[prop(default = None)] under: Option<FileUnder>,
    #[prop(default = None)] sel: Option<Selecting>,
) -> impl IntoView {
    view! {
        <div class="xg-shelf" style=format!("--xc-min:{min}px")>
            <For each=move || items.get() key=|c| c.url.clone() let:c>
                <ReleaseCardView item=c under=under.clone() sel=sel />
            </For>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Grid actions
// ---------------------------------------------------------------------------

/// Play all / Shuffle over whatever a grid holds. The sweep is a server-side queue source,
/// so progress comes from the player's own state and survives leaving the page.
#[component]
pub fn GridPlaybackBar(#[prop(into)] items: Signal<Vec<(String, Option<i64>)>>, #[prop(default = "releases")] noun: &'static str) -> impl IntoView {
    let player = use_player();
    let empty = move || items.with(|i| i.is_empty());
    // Only when the running sweep is of (the start of) THIS grid, not some other one.
    let sweep = Signal::derive(move || {
        player.state.with(|s| match &s.source {
            Some(QueueSource::Explore { cards, next, .. }) => {
                let first = cards.first().map(|c| c.url.as_str())?;
                items.with(|i| i.iter().any(|(u, _)| u == first)).then_some((cards.len(), *next))
            }
            _ => None,
        })
    });
    let go = move |shuffle: bool| start_sweep_now(player, items, shuffle);
    view! {
        <div class="xg-bar">
            <Button variant=Variant::Primary size=Size::Sm icon="play" disabled=Signal::derive(empty)
                title=format!("Play all {noun} below") on_click=move |_| go(false)>"Play all"</Button>
            <Button size=Size::Sm icon="shuffle" disabled=Signal::derive(empty)
                title=format!("Shuffle all {noun} below") on_click=move |_| go(true)>"Shuffle"</Button>
            {move || sweep.get().map(|(n, next)| view! {
                <span class="mono faint xg-note" role="status">{format!("sweeping \u{b7} {}/{n} {noun} fetched", next.min(n))}</span>
            })}
        </div>
    }
}

fn start_sweep_now(player: crate::player::PlayerCtx, items: Signal<Vec<(String, Option<i64>)>>, shuffle: bool) {
    let v = items.get_untracked();
    if v.is_empty() {
        return;
    }
    let n = v.len().min(super::logic::MAX_SWEEP);
    play::start_sweep(player, v, shuffle);
    ds::toast_info(&format!("{} {n} {}", if shuffle { "Shuffling" } else { "Playing" }, if n == 1 { "release" } else { "releases" }));
}

/// Fill in the gaps in a catalogue in one press. The server decides: it re-reads the
/// discography and drops what the library already holds, so `missing` is only a hint.
#[component]
pub fn CatalogDownloadButton(
    #[prop(into)] url: String,
    #[prop(into)] missing: Signal<usize>,
    #[prop(into)] exact: Signal<bool>,
    #[prop(optional)] small: bool,
) -> impl IntoView {
    let busy = RwSignal::new(false);
    let result = RwSignal::new(None::<String>);
    let nothing_left = move || exact.get() && missing.get() == 0;
    let run = {
        let url = url.clone();
        move |_| {
            busy.set(true);
            let body = DownloadCatalogRequest { url: url.clone(), limit: 500, free_only: false, target_subdir: None };
            spawn_local(async move {
                match api::post::<_, CatalogResult>("/explore/download/catalog", &body).await {
                    Ok(r) => {
                        let _ = result.try_set(Some(catalog_result(r.queued, r.skipped_in_library, &r.detail)));
                    }
                    Err(e) => {
                        let _ = result.try_set(Some(e.message()));
                        ds::toast_err(&e.message());
                    }
                }
                let _ = busy.try_set(false);
            });
        }
    };
    view! {
        <span class="xg-catalog">
            <Button variant=if small { Variant::Outline } else { Variant::Primary } size=if small { Size::Sm } else { Size::Md } icon="download"
                busy=busy disabled=Signal::derive(nothing_left)
                title="Queue everything from this catalogue that you do not already have" on_click=run>
                {move || catalog_label(missing.get(), exact.get())}
            </Button>
            {move || result.get().map(|r| view! { <span class="xg-note muted" role="status">{r}</span> })}
        </span>
    }
}

/// Follow an artist or label page: its new releases land in the feed.
#[component]
pub fn FollowBandButton(#[prop(into)] url: String, #[prop(into)] name: String, #[prop(into)] kind: String) -> impl IntoView {
    let follows = qh::use_q::<FollowsOut>(|| Some(QuerySpec::keyed("/follows", "/follows", &["follow"])));
    let busy = RwSignal::new(false);
    let root = origin_of(&url).unwrap_or_else(|| url.clone());
    let existing = {
        let root = root.clone();
        Signal::derive(move || -> Option<FollowOut> {
            follows.data.with(|d| {
                d.as_ref().and_then(|f| {
                    f.sources
                        .iter()
                        .find(|s| matches!(s.kind.as_str(), "artist" | "label") && s.url.as_deref().and_then(origin_of).as_deref() == Some(root.as_str()))
                        .cloned()
                })
            })
        })
    };
    let followed = move || existing.get().is_some_and(|e| e.enabled);
    let toggle = move |_| {
        busy.set(true);
        let (existing, url, name, kind) = (existing.get_untracked(), url.clone(), name.clone(), kind.clone());
        spawn_local(async move {
            let res = match existing {
                Some(e) => api::patch::<_, FollowOut>(&format!("/follows/{}", e.id), &FollowPatch { label: None, enabled: Some(!e.enabled) }).await.map(|_| ()),
                None => api::post::<_, FollowOut>(
                    "/follows",
                    &FollowCreate { kind: if kind == "label" { "label".into() } else { "artist".into() }, label: name, url: Some(url), enabled: true, ..Default::default() },
                )
                .await
                .map(|_| ()),
            };
            match res {
                Ok(()) => data::invalidate_prefix("/follows"),
                Err(e) => ds::toast_err(&e.message()),
            }
            let _ = busy.try_set(false);
        });
    };
    view! {
        <Button size=Size::Sm icon="rss" pressed=Signal::derive(followed) busy=busy
            disabled=Signal::derive(move || follows.first_load())
            title="Follow: new releases from this page land in your feed" on_click=toggle>
            {move || if followed() { "Following" } else { "Follow" }}
        </Button>
    }
}

/// Bulk bar for a grid in selection mode: select all missing, clear, download N, done.
#[component]
pub fn SelectionBar(
    sel: Selecting,
    #[prop(into)] items: Signal<Vec<ReleaseCardOut>>,
    #[prop(default = None)] under: Option<FileUnder>,
) -> impl IntoView {
    let busy = RwSignal::new(false);
    let progress = RwSignal::new((0usize, 0usize));
    let result = RwSignal::new(None::<String>);
    let missing = Signal::derive(move || items.with(|v| v.iter().filter(|c| !c.in_library && !c.blacklisted).map(|c| c.url.clone()).collect::<Vec<_>>()));
    let all_missing_picked = Signal::derive(move || sel.picked.with(|p| missing.with(|m| m.iter().all(|u| p.contains(u)))));
    let queue = move |_| {
        let urls: Vec<String> = sel.picked.get_untracked().into_iter().collect();
        if urls.is_empty() {
            return;
        }
        busy.set(true);
        result.set(None);
        progress.set((0, urls.len()));
        let under = under.clone();
        spawn_local(async move {
            let total = urls.len();
            let mut sent = 0usize;
            let mut jobs = 0;
            let mut failure = None;
            for chunk in urls.chunks(DOWNLOAD_CHUNK) {
                let body = DownloadReleasesRequest {
                    urls: chunk.to_vec(),
                    label: Some(match &under {
                        Some(u) => format!("{} from {}", format_count(total as i64), u.name),
                        None => format!("{} selected on Bandcamp", format_count(total as i64)),
                    }),
                    label_name: under.as_ref().map(|u| u.name.clone()),
                    label_url: under.as_ref().map(|u| u.url.clone()),
                    ..Default::default()
                };
                match api::post::<_, JobOut>("/explore/download", &body).await {
                    Ok(_) => {
                        sent += chunk.len();
                        jobs += 1;
                        let _ = progress.try_set((sent, total));
                        let chunk: Vec<String> = chunk.to_vec();
                        let _ = sel.picked.try_update(|p| {
                            for u in &chunk {
                                p.remove(u);
                            }
                        });
                    }
                    Err(e) => {
                        failure = Some(e.message());
                        break;
                    }
                }
            }
            let _ = result.try_set(Some(match failure {
                Some(f) => f,
                None => format!("Queued {} release{}{} \u{2014} see Downloads.", format_count(sent as i64), if sent == 1 { "" } else { "s" }, if jobs > 1 { format!(" in {jobs} jobs") } else { String::new() }),
            }));
            let _ = busy.try_set(false);
        });
    };
    view! {
        <div class="xg-selbar" role="region" aria-label="Selection">
            {move || (!missing.with(|m| m.is_empty())).then(|| view! {
                <Button size=Size::Sm variant=Variant::Ghost disabled=Signal::derive(move || busy.get() || all_missing_picked.get())
                    on_click=move |_| { let m = missing.get_untracked(); sel.picked.update(|p| p.extend(m)); }>
                    {move || format!("Select all {} missing", format_count(missing.with(|m| m.len()) as i64))}
                </Button>
            })}
            {move || (sel.picked.with(|p| !p.is_empty())).then(|| view! {
                <Button size=Size::Sm variant=Variant::Ghost disabled=busy on_click=move |_| sel.picked.set(BTreeSet::new())>"Clear"</Button>
            })}
            <span class="mono muted xg-note" role="status">
                {move || {
                    let n = sel.picked.with(|p| p.len());
                    if busy.get() { let (d, t) = progress.get(); format!("Queuing {} / {}\u{2026}", format_count(d as i64), format_count(t as i64)) }
                    else if n == 0 { "Nothing selected \u{2014} tap covers to pick them.".to_string() }
                    else { format!("{} selected", format_count(n as i64)) }
                }}
            </span>
            {move || result.get().map(|r| view! { <span class="muted xg-note truncate">{r}</span> })}
            <span class="spacer"></span>
            {move || (sel.picked.with(|p| !p.is_empty())).then(|| view! {
                <Button variant=Variant::Primary size=Size::Sm icon="download" busy=busy on_click=queue.clone()>
                    {move || format!("Download {}", format_count(sel.picked.with(|p| p.len()) as i64))}
                </Button>
            })}
            <Button size=Size::Sm disabled=busy on_click=move |_| sel.stop()>"Done"</Button>
        </div>
    }
}
