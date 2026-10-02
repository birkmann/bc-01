//! The Bandcamp side of an artist/label page: find-new-releases (locate, harvest, queue), the page
//! picker, the catalogue grid with Missing / In library / All, follow, bulk download, related.
use std::collections::HashSet;
use std::sync::Arc;

use bc_types::bandcamp::{BandOut, CatalogResult, FollowsOut, LocateOut, QueueResult, ReleaseCardOut, RelatedOut, RunResult, SearchHitOut};
use bc_types::player::{ExploreCard, PlayerCommand, QueueSource};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::logic::{self as lg, CatalogFilter, Tone};
use super::shared::{Cover, Kind, Notice, Q, use_q};
use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Button, Icon, Size, Variant, confirm, toast_err, toast_ok, use_debounced};
use crate::logic::format::format_count;
use crate::player::use_player;
use crate::util::{enc, qs};
use crate::widgets::card_grid::CardGrid;
use crate::widgets::{PageFetcher, PageRes};

/// The artist/label an action is about.
#[derive(Clone, Debug)]
pub struct Entity {
    pub kind: Kind,
    pub id: i64,
    pub name: String,
    pub url: Option<String>,
}

/// Shared state of the actions on an artist/label (find new, pin, queue) and the report they raise.
#[derive(Clone, Copy)]
pub struct Actions {
    pub notice: RwSignal<Option<Notice>>,
    pub find_busy: RwSignal<bool>,
    pub queue_busy: RwSignal<bool>,
    pub pinning: RwSignal<Option<String>>,
    pub pin_error: RwSignal<Option<String>>,
    /// A locate that "Find new releases" started is a harvest interrupted: a page picked by
    /// hand afterwards carries on into it.
    harvest_after_pin: RwSignal<bool>,
    pub on_changed: Callback<()>,
}

impl Actions {
    pub fn new(on_changed: Callback<()>) -> Self {
        Self {
            notice: RwSignal::new(None),
            find_busy: RwSignal::new(false),
            queue_busy: RwSignal::new(false),
            pinning: RwSignal::new(None),
            pin_error: RwSignal::new(None),
            harvest_after_pin: RwSignal::new(false),
            on_changed,
        }
    }

    pub fn busy(&self) -> Signal<bool> {
        let (a, b) = (self.find_busy, self.queue_busy);
        Signal::derive(move || a.get() || b.get())
    }

    /// "Find new releases": harvest the pinned page, or locate it first.
    pub fn find_new(&self, e: Entity) {
        if self.find_busy.get_untracked() {
            return;
        }
        let a = *self;
        match e.url.clone().filter(|u| !u.is_empty()) {
            Some(url) => {
                a.notice.set(Some(Notice::ok(format!("Checking {} on Bandcamp\u{2026}", e.name))));
                a.find_busy.set(true);
                spawn_local(async move {
                    a.harvest(&e, &url).await;
                    let _ = a.find_busy.try_set(false);
                });
            }
            None => {
                a.notice.set(Some(Notice::ok(format!("Looking for {} on Bandcamp\u{2026}", e.name))));
                a.find_busy.set(true);
                spawn_local(async move {
                    a.locate(e).await;
                    let _ = a.find_busy.try_set(false);
                });
            }
        }
    }

    async fn harvest(&self, e: &Entity, url: &str) {
        match run_harvest(e.kind, &e.name, url).await {
            Ok(res) => {
                let mut n = Notice::ok(lg::find_new_text(&e.name, &res));
                n.to_harvest = res.new > 0 || !res.pending_item_ids.is_empty();
                n.download_ids = res.pending_item_ids.clone();
                let _ = self.notice.try_set(Some(n));
            }
            Err(m) => {
                let _ = self.notice.try_set(Some(Notice::err(format!("{}: {m}", e.name))));
            }
        }
    }

    async fn locate(&self, e: Entity) {
        let r = api::post::<_, LocateOut>(&format!("/{}/{}/locate", e.kind.path(), e.id), &serde_json::json!({})).await;
        match r {
            Ok(res) => match res.url {
                Some(url) => {
                    self.on_changed.run(());
                    let _ = self.notice.try_set(Some(Notice::ok(format!(
                        "{}: found {} \u{2014} confirmed by {} of your releases. Checking for new ones\u{2026}",
                        e.name,
                        lg::strip_scheme(&url),
                        format_count(res.matched)
                    ))));
                    self.harvest(&e, &url).await;
                }
                None => {
                    let _ = self.harvest_after_pin.try_set(true);
                    let mut n = Notice::err(format!("{}: {} \u{2014} pick it from the search yourself.", e.name, res.detail));
                    n.pick_href = Some(format!("/{}/{}?tab=bandcamp", e.kind.path(), e.id));
                    let _ = self.notice.try_set(Some(n));
                }
            },
            Err(err) => {
                let _ = self.notice.try_set(Some(Notice::err(format!("{}: {}", e.name, err.message()))));
            }
        }
    }

    /// Pin a Bandcamp page picked by hand.
    pub fn pin(&self, e: Entity, url: String) {
        let a = *self;
        a.pinning.set(Some(url.clone()));
        a.pin_error.set(None);
        spawn_local(async move {
            let r = api::patch::<_, serde_json::Value>(&format!("/{}/{}", e.kind.path(), e.id), &serde_json::json!({ "bandcamp_url": url })).await;
            let _ = a.pinning.try_set(None);
            match r {
                Ok(_) => {
                    a.on_changed.run(());
                    let where_ = lg::strip_scheme(&url);
                    if a.harvest_after_pin.get_untracked() {
                        let _ = a.harvest_after_pin.try_set(false);
                        a.notice.set(Some(Notice::ok(format!("{}: pinned {where_}. Checking for new releases\u{2026}", e.name))));
                        let e2 = Entity { url: Some(url.clone()), ..e };
                        let _ = a.find_busy.try_set(true);
                        a.harvest(&e2, &url).await;
                        let _ = a.find_busy.try_set(false);
                    } else {
                        let _ = a.notice.try_set(Some(Notice::ok(format!("{}: pinned {where_}.", e.name))));
                    }
                }
                Err(err) => {
                    let _ = a.pin_error.try_set(Some(err.message()));
                }
            }
        });
    }

    /// Queue inbox items found by a harvest.
    pub fn queue(&self, ids: Vec<i64>) {
        let a = *self;
        a.queue_busy.set(true);
        spawn_local(async move {
            let r = api::post::<_, QueueResult>("/harvest/items/queue", &serde_json::json!({ "item_ids": ids, "allow_unowned": true, "target_subdir": "" })).await;
            let _ = a.queue_busy.try_set(false);
            let n = match r {
                Ok(res) => {
                    let mut n = Notice::ok(format!("Queued {}.", lg::count_of(res.queued, "download")));
                    n.to_downloads = true;
                    n
                }
                Err(e) => Notice::err(e.message()),
            };
            let _ = a.notice.try_set(Some(n));
        });
    }
}

/// Kick off a harvest of one artist/label page and wait for its result (job + poll).
async fn run_harvest(kind: Kind, name: &str, url: &str) -> Result<RunResult, String> {
    let mut body = serde_json::json!({ "kind": kind.noun(), "url": url, "limit": 2000 });
    if kind == Kind::Label {
        body["label_name"] = serde_json::json!(name);
    }
    let v: serde_json::Value = api::post("/harvest/run", &body).await.map_err(|e| e.message())?;
    if v.get("seen").is_some() {
        return serde_json::from_value(v).map_err(|e| e.to_string());
    }
    let job = v.get("job_id").and_then(|j| j.as_str()).ok_or("The server did not start the harvest")?.to_string();
    for _ in 0..240 {
        gloo_timers::future::TimeoutFuture::new(1500).await;
        match api::get::<RunResult>(&format!("/harvest/runs/{job}")).await {
            Ok(r) => return Ok(r),
            Err(e) if e.status == 404 => continue,
            Err(e) => return Err(e.message()),
        }
    }
    Err("Timed out waiting for Bandcamp".into())
}

// ---- the band page (bio, roster, catalogue) --------------------------------------------------------

/// `GET /explore/band` for a Bandcamp page (cached; shared by the hero and the catalogue tab).
pub fn use_band(url: Signal<Option<String>>) -> Q<BandOut> {
    use_q::<BandOut>(move || {
        let u = url.get().filter(|u| !u.is_empty())?;
        Some(QuerySpec::new(format!("/explore/band{}", qs(&[("url", u)])), &[]))
    })
}

// ---- page picker -----------------------------------------------------------------------------------

/// Search Bandcamp for the page of an artist/label and pin the one the person picks.
#[component]
pub fn BandcampPicker(
    name: String,
    kind: Kind,
    #[prop(into)] current_url: Signal<Option<String>>,
    on_pick: Callback<String>,
    #[prop(into)] picking: Signal<Option<String>>,
    #[prop(into)] error: Signal<Option<String>>,
) -> impl IntoView {
    let q = RwSignal::new(name.clone());
    let settled = use_debounced(q, 600);
    let noun = kind.noun();
    let search = use_query::<Vec<SearchHitOut>>(move || {
        let s = settled.get();
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        Some(QuerySpec::new(format!("/explore/search{}", qs(&[("q", s.to_string()), ("kind", "artist".into()), ("limit", "20".into())])), &[]))
    });
    let bands = Memo::new(move |_| {
        search.data.get().map(|d| d.iter().filter(|h| h.kind == "artist" || h.kind == "label").cloned().collect::<Vec<_>>()).unwrap_or_default()
    });
    let ph = format!("Search Bandcamp for {name}");
    view! {
        <div class="pp-picker">
            <div class="input-wrap pp-picker-input">
                <Icon name="search" />
                <input class="input" type="search" aria-label=format!("Search Bandcamp for the {noun}'s page") placeholder=ph spellcheck="false"
                    prop:value=move || q.get() on:input=move |ev| q.set(event_target_value(&ev)) />
            </div>
            {move || error.get().map(|e| view! { <div class="pp-notice err" role="alert"><span class="pp-notice-text">{e}</span></div> })}
            {move || {
                let s = settled.get();
                if s.trim().is_empty() {
                    return view! { <p class="pp-hint">"Type the name Bandcamp knows them by."</p> }.into_any();
                }
                if let Some(e) = search.error.get() {
                    return view! { <div class="pp-notice err" role="alert"><span class="pp-notice-text">{e.message()}</span></div> }.into_any();
                }
                if search.data.get().is_none() {
                    return view! { <p class="pp-hint"><span class="pp-spin"></span>" Searching Bandcamp\u{2026}"</p> }.into_any();
                }
                let hits = bands.get();
                if hits.is_empty() {
                    return view! { <p class="pp-hint">{format!("Bandcamp has no {noun} page named \u{201c}{}\u{201d}. Try another spelling.", s.trim())}</p> }.into_any();
                }
                view! {
                    <ul class="pp-hits">
                        {hits.into_iter().map(|h| {
                            let pinned = lg::same_url(current_url.get().as_deref(), &h.url);
                            let url = h.url.clone();
                            let url2 = h.url.clone();
                            let busy = move || lg::same_url(picking.get().as_deref(), &url2);
                            let sub = [h.subtitle.clone(), lg::strip_scheme(&h.url)].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" \u{b7} ");
                            view! {
                                <li class="pp-hit">
                                    <Cover src=h.art_url.clone() class="pp-hit-art" icon="users" />
                                    <div class="pp-hit-text">
                                        <div class="truncate pp-hit-name">{h.name.clone()}{(h.kind == "label").then(|| view! { <span class="badge">"Label"</span> })}</div>
                                        <div class="truncate faint pp-hit-sub">{sub}</div>
                                    </div>
                                    <a class="btn btn-ghost btn-sm" href=lg::band_path(&h.url) title="Look at the page first">"View"</a>
                                    {if pinned {
                                        view! { <span class="badge badge-accent"><Icon name="check" />"Pinned"</span> }.into_any()
                                    } else {
                                        view! {
                                            <Button size=Size::Sm variant=Variant::Outline busy=Signal::derive(busy) on_click=move |_| on_pick.run(url.clone())>"Use this page"</Button>
                                        }.into_any()
                                    }}
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                }.into_any()
            }}
        </div>
    }
}

// ---- follow ------------------------------------------------------------------------------------------

/// Follow/unfollow a band in the Feed (a follow source on its URL).
#[component]
pub fn FollowButton(kind: Kind, name: String, url: String) -> impl IntoView {
    let follows = use_query::<FollowsOut>(|| Some(QuerySpec::keyed("follows", "/follows", &["follow"])));
    let busy = RwSignal::new(false);
    let url_c = url.clone();
    let followed = Memo::new(move |_| {
        follows.data.get().and_then(|f| f.sources.iter().find(|s| lg::same_url(s.url.as_deref(), &url_c)).map(|s| s.id))
    });
    let toggle = {
        let name = name.clone();
        move |_| {
            busy.set(true);
            let (name, url) = (name.clone(), url.clone());
            let cur = followed.get_untracked();
            spawn_local(async move {
                let r = match cur {
                    Some(id) => api::call("DELETE", &format!("/follows/{id}")).await.map(|_| false),
                    None => api::post::<_, serde_json::Value>("/follows", &serde_json::json!({ "kind": kind.noun(), "label": name, "url": url })).await.map(|_| true),
                };
                let _ = busy.try_set(false);
                match r {
                    Ok(now) => {
                        crate::data::invalidate_entity("follow", &[]);
                        crate::data::invalidate_prefix("/follows");
                        toast_ok(if now { "Following in Feed" } else { "Unfollowed" });
                    }
                    Err(e) => toast_err(&e.message()),
                }
            });
        }
    };
    view! {
        <Button size=Size::Sm variant=Variant::Outline icon=crate::ds::dyn_icon(move || if followed.get().is_some() { "check" } else { "rss" })
            pressed=Signal::derive(move || followed.get().is_some()) busy=busy on_click=toggle
            title="Follow this page in the Feed">
            {move || if followed.get().is_some() { "Following" } else { "Follow in Feed" }}
        </Button>
    }
}

// ---- bulk catalogue download ---------------------------------------------------------------------------

/// "Download N missing": queue everything on the page that the library does not hold.
#[component]
pub fn CatalogDownloadButton(url: String, #[prop(into)] missing: Signal<usize>, #[prop(into)] exact: Signal<bool>) -> impl IntoView {
    let busy = RwSignal::new(false);
    // Releases this press put in the download queue; pressing again would only queue duplicates.
    let queued = RwSignal::new(None::<i64>);
    let nothing_left = move || exact.get() && missing.get() == 0;
    let label = move || {
        let m = missing.get();
        if let Some(n) = queued.get() {
            format!("{} queued", format_count(n))
        } else if nothing_left() {
            "All in your library".to_string()
        } else if exact.get() {
            format!("Download {} missing", format_count(m as i64))
        } else if m > 0 {
            format!("Download {}+ missing", format_count(m as i64))
        } else {
            "Download missing".into()
        }
    };
    let run = move |_| {
        let url = url.clone();
        busy.set(true);
        spawn_local(async move {
            let r = api::post::<_, CatalogResult>("/explore/download/catalog", &serde_json::json!({ "url": url })).await;
            let _ = busy.try_set(false);
            match r {
                Ok(r) if r.queued > 0 => {
                    let _ = queued.try_set(Some(r.queued));
                    let mut t = format!("Queued {}", lg::count_of(r.queued, "release"));
                    if r.skipped_in_library > 0 {
                        t.push_str(&format!(", skipped {} already in library", format_count(r.skipped_in_library)));
                    }
                    toast_ok(&t);
                }
                Ok(r) => toast_ok(if r.detail.is_empty() { "Nothing to queue." } else { &r.detail }),
                Err(e) => toast_err(&e.message()),
            }
        });
    };
    view! {
        <Button variant=Variant::Primary icon=crate::ds::dyn_icon(move || if queued.get().is_some() { "check" } else { "download" })
            busy=busy disabled=Signal::derive(move || nothing_left() || queued.get().is_some()) on_click=run
            title="Queue everything from this catalogue that you do not already have">
            <span class="hide-sm">{label}</span>
        </Button>
    }
}

// ---- catalogue ---------------------------------------------------------------------------------------------

fn queue_urls(urls: Vec<String>, file_under: Option<(String, String)>, done: Callback<()>) {
    if urls.is_empty() {
        return;
    }
    let n = urls.len() as i64;
    let mut body = serde_json::json!({ "urls": urls });
    if let Some((name, url)) = file_under {
        body["label_name"] = serde_json::json!(name);
        body["label_url"] = serde_json::json!(url);
    }
    spawn_local(async move {
        match api::post::<_, serde_json::Value>("/explore/download", &body).await {
            Ok(_) => {
                toast_ok(&format!("Queued {}", lg::count_of(n, "release")));
                done.run(());
            }
            Err(e) => toast_err(&e.message()),
        }
    });
}

fn cards_of(items: &[ReleaseCardOut]) -> Vec<ExploreCard> {
    items.iter().map(|r| ExploreCard { url: r.url.clone(), library_release_id: r.library_release_id }).collect()
}

/// One Bandcamp release card.
#[component]
fn BcCard(
    r: ReleaseCardOut,
    selecting: Signal<bool>,
    picked: RwSignal<HashSet<String>>,
    file_under: Option<(String, String)>,
) -> impl IntoView {
    let url = r.url.clone();
    let url_sel = r.url.clone();
    let url_dl = r.url.clone();
    let missing = lg::is_missing(&r);
    let href = lg::release_path(&r.url);
    let is_picked = {
        let u = url.clone();
        move || picked.with(|p| p.contains(&u))
    };
    let status = if r.in_library {
        view! { <span class="badge badge-ok"><Icon name="check" />"In library"</span> }.into_any()
    } else if r.blacklisted {
        view! { <span class="badge"><Icon name="x" />"Blacklisted"</span> }.into_any()
    } else {
        view! { <span class="badge badge-accent">"Missing"</span> }.into_any()
    };
    let sub = [r.artist_name.clone(), r.release_date.clone().unwrap_or_default().chars().take(10).collect::<String>()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" \u{b7} ");
    let ic = is_picked.clone();
    view! {
        <div class="pp-card pp-bc" class:sel=is_picked>
            <div class="pp-card-art">
                <Cover src=r.art_url.clone() />
                <div class="pp-card-badges">{status}{r.is_free_download.then(|| view! { <span class="badge">"free"</span> })}</div>
                {move || (!selecting.get() && missing).then(|| {
                    let (u, fu) = (url_dl.clone(), file_under.clone());
                    view! {
                        <button type="button" class="pp-chipbtn pp-card-act" title="Download this release" aria-label=format!("Download {}", "release")
                            on:click=move |ev| { ev.stop_propagation(); ev.prevent_default(); queue_urls(vec![u.clone()], fu.clone(), Callback::new(|_| {})); }>
                            <Icon name="download" />
                        </button>
                    }
                })}
            </div>
            <div class="pp-card-meta">
                <div class="pp-card-title truncate" title=r.title.clone()>{r.title.clone()}</div>
                <div class="pp-card-sub truncate">{sub}</div>
            </div>
            <a class="pp-card-link" href=href aria-label=r.title.clone()
                on:click=move |ev| {
                    if selecting.get_untracked() {
                        ev.prevent_default();
                        let u = url.clone();
                        picked.update(|p| { if !p.remove(&u) { p.insert(u); } });
                    }
                }></a>
            {move || selecting.get().then(|| {
                let u = url_sel.clone();
                let on = ic.clone();
                view! {
                    <span class=move || if on() { "pp-check on" } else { "pp-check" } aria-hidden="true" data-u=u><Icon name="check" /></span>
                }
            })}
        </div>
    }
}

/// The Bandcamp catalogue of a band with the Missing / In library / All filter, selection and
/// bulk download. Virtualised through `CardGrid` (an in-memory fetcher).
#[component]
pub fn BandCatalogue(band: BandOut, url: String, file_under: Option<(String, String)>) -> impl IntoView {
    let filter = RwSignal::new(CatalogFilter::Missing);
    let selecting = RwSignal::new(false);
    let picked: RwSignal<HashSet<String>> = RwSignal::new(HashSet::new());
    let player = use_player();
    let all = Arc::new(band.releases.clone());
    let (n_missing, n_owned, n_all) = lg::catalogue_counts(&all);
    let items: Memo<Arc<Vec<ReleaseCardOut>>> = {
        let all = all.clone();
        Memo::new(move |_| Arc::new(lg::filter_catalogue(&all, filter.get())))
    };
    let items_for_fetch = items;
    let fetch: PageFetcher<ReleaseCardOut> = Arc::new(move |req| {
        let rows = items_for_fetch.get_untracked();
        Box::pin(async move {
            let total = rows.len();
            Ok(PageRes { rows: rows.iter().skip(req.offset).take(req.limit).cloned().collect(), total })
        })
    });
    let key_url = url.clone();
    let source_key = Signal::derive(move || format!("{key_url}|{}", filter.get().key()));
    let fu = file_under.clone();
    let render = Callback::new(move |(r, _w): (ReleaseCardOut, f64)| {
        view! { <BcCard r=r selecting=selecting.into() picked=picked file_under=fu.clone() /> }.into_any()
    });
    let play = move |shuffle: bool| {
        let cards = cards_of(&items.get_untracked());
        if cards.is_empty() {
            return;
        }
        player.cmd(PlayerCommand::StartSource { source: QueueSource::Explore { cards, shuffle, next: 0 }, shuffle });
    };
    let filters = [(CatalogFilter::Missing, "Missing", n_missing), (CatalogFilter::Library, "In library", n_owned), (CatalogFilter::All, "All", n_all)];
    let truncated = band.truncated;
    let fu_dl = file_under.clone();
    let download_picked = move |_| {
        let urls: Vec<String> = picked.get_untracked().into_iter().collect();
        queue_urls(urls, fu_dl.clone(), Callback::new(move |_| picked.set(HashSet::new())));
    };
    let all_for_sel = all.clone();
    let select_missing = move |_| {
        picked.set(all_for_sel.iter().filter(|r| lg::is_missing(r)).map(|r| r.url.clone()).collect());
    };
    let empty_all = n_all;
    view! {
        <div class="pp-cat">
            <div class="pp-cat-bar">
                <div class="segmented" role="group" aria-label="Catalogue filter">
                    {filters.into_iter().map(|(f, label, n)| view! {
                        <button type="button" aria-pressed=move || (filter.get() == f).to_string() on:click=move |_| filter.set(f)>
                            {label}" "<span class="num faint">{format_count(n as i64)}</span>
                        </button>
                    }).collect_view()}
                </div>
                <Button size=Size::Sm variant=Variant::Outline icon="check" pressed=selecting on_click=move |_| { selecting.update(|s| *s = !*s); if !selecting.get_untracked() { picked.set(HashSet::new()); } }
                    title="Select releases to download">"Select"</Button>
                <span class="spacer"></span>
                <Button size=Size::Sm variant=Variant::Ghost icon="play" on_click=move |_| play(false) title="Play these releases in order"><span class="hide-sm">"Play"</span></Button>
                <Button size=Size::Sm variant=Variant::Ghost icon="shuffle" on_click=move |_| play(true) title="Shuffle these releases"><span class="hide-sm">"Shuffle"</span></Button>
            </div>
            {truncated.then(|| view! { <p class="pp-hint">"Bandcamp shows only part of a long catalogue here; \u{201c}Download missing\u{201d} still checks all of it."</p> })}
            {move || selecting.get().then(|| {
                let (dl, sm) = (download_picked.clone(), select_missing.clone());
                view! {
                    <div class="pp-selbar" role="status">
                        <span class="num">{move || format_count(picked.with(|p| p.len()) as i64)}</span><span>" selected"</span>
                        <span class="spacer"></span>
                        <Button size=Size::Sm variant=Variant::Ghost on_click=sm>"Select all missing"</Button>
                        <Button size=Size::Sm variant=Variant::Primary icon="download" disabled=Signal::derive(move || picked.with(|p| p.is_empty())) on_click=dl>"Download"</Button>
                        <Button size=Size::Sm variant=Variant::Ghost on_click=move |_| picked.set(HashSet::new())>"Clear"</Button>
                    </div>
                }
            })}
            <div class="pp-fill">
                <CardGrid fetch=fetch source_key=source_key min_card_w=144.0 meta_h=48.0 gap=12.0 render=render
                    empty=move || {
                        let f = filter.get();
                        view! {
                            <div class="empty">
                                <Icon name="disc" />
                                <h3>{match f {
                                    CatalogFilter::Missing => "Everything on this page is already in your library",
                                    CatalogFilter::Library => "None of these releases are in your library yet",
                                    CatalogFilter::All => "Bandcamp lists nothing on this page",
                                }}</h3>
                                {(f == CatalogFilter::Missing && empty_all > 0).then(|| view! {
                                    <button type="button" class="btn btn-outline" on:click=move |_| filter.set(CatalogFilter::All)>{format!("Show all {}", format_count(empty_all as i64))}</button>
                                })}
                            </div>
                        }
                    } />
            </div>
        </div>
    }
}

/// What Bandcamp files near an artist (their newest record's neighbours): feeds by tag.
#[component]
pub fn BandcampRelated(url: String, tags: Vec<String>) -> impl IntoView {
    let chosen = RwSignal::new(tags.iter().take(3).cloned().collect::<Vec<_>>());
    let u = url.clone();
    let rel = use_query::<RelatedOut>(move || {
        let mut pairs: Vec<(String, String)> = vec![("url".into(), u.clone()), ("size".into(), "48".into()), ("tag_limit".into(), "3".into()), ("include_band".into(), "false".into())];
        for t in chosen.get() {
            pairs.push(("tags".into(), t));
        }
        Some(QuerySpec::new(format!("/explore/related{}", crate::util::qs_pairs(&pairs)), &[]))
    });
    let seed = tags.clone();
    view! {
        <section class="pp-section">
            <h2 class="section-title"><Icon name="compass" />"More like this on Bandcamp"</h2>
            {(seed.len() > 1).then(|| view! {
                <div class="pp-chiprow">
                    <span class="pp-eyebrow">"Feeds"</span>
                    {seed.into_iter().map(|t| {
                        let t2 = t.clone();
                        view! {
                            <button type="button" class="chip" aria-pressed=move || chosen.with(|c| c.contains(&t)).to_string()
                                on:click=move |_| chosen.update(|c| { if let Some(i) = c.iter().position(|x| *x == t2) { c.remove(i); } else { c.push(t2.clone()); if c.len() > 3 { c.remove(0); } } })>
                                {t2.clone()}
                            </button>
                        }
                    }).collect_view()}
                </div>
            })}
            {move || {
                if let Some(e) = rel.error.get() {
                    return view! { <p class="pp-hint danger">{format!("Could not read Bandcamp's neighbours: {}", e.message())}</p> }.into_any();
                }
                let Some(d) = rel.data.get() else {
                    return view! { <p class="pp-hint"><span class="pp-spin"></span>" Looking for neighbours\u{2026}"</p> }.into_any();
                };
                if d.sections.iter().all(|s| s.items.is_empty()) {
                    return view! { <p class="pp-hint">"Bandcamp has nothing near this page."</p> }.into_any();
                }
                d.sections.iter().filter(|s| !s.items.is_empty()).map(|s| {
                    let items = s.items.clone();
                    view! {
                        <div class="pp-shelf">
                            <h3 class="pp-shelf-title">{s.title.clone()}</h3>
                            <div class="pp-strip">
                                {items.into_iter().map(|r| {
                                    let sel = RwSignal::new(false);
                                    let picked: RwSignal<HashSet<String>> = RwSignal::new(HashSet::new());
                                    let _ = sel;
                                    view! { <div class="pp-strip-item"><BcCard r=r selecting=Signal::derive(|| false) picked=picked file_under=None /></div> }
                                }).collect_view()}
                            </div>
                        </div>
                    }
                }).collect_view().into_any()
            }}
        </section>
    }
}

#[allow(dead_code)]
fn _keep(_: Tone) {
    let _ = enc("");
    let _ = confirm;
}
