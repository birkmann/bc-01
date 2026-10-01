//! Albums: the infinite virtual grid (`CardGrid`, 200-row sparse pages) with place
//! restore, "N new" merge, selection + bulk delete with blacklist, fan shelf view
//! with adopt, fill missing and sort. Plus the album detail page (`detail`).
use std::collections::HashMap;
use std::sync::Arc;

use bc_types::library::*;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;

use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Button, EmptyState, Icon, MenuEntry, MenuItem, PageHeader, Size, Variant, toast_err, toast_ok};
use crate::logic::format::format_count;
use crate::logic::grid_place::{self, GridPlace, ListingWords, PLACE_KEY};
use crate::player::use_player;
use crate::util::{ls_get, ls_set, qs_pairs};
use crate::widgets::card_grid::{CardGrid, grid_metrics};
use crate::widgets::{PageFetcher, PageRes};

pub mod card;
pub mod detail;
pub mod filters;
pub mod host;
pub mod logic;
pub mod related;
pub mod supporters;

pub use detail::AlbumDetailPage;

use card::{AlbumCard, AlbumSel, META_H, META_H_DETAILS};
use filters::{AddedFilter, AddedFilterBar, FillMissingBar, ReleaseSortBar, ReleaseSortState, TagFilterBar, TagScope, UrlState, describe_tags};
use host::{LibraryHost, provide_library_host, release_track_ids};

const DETAILS_KEY: &str = "bc:albums:details:v1";
const PLAY_PAGES: usize = 5;

/// Everything that names the listing, resolved from the URL.
#[derive(Clone, PartialEq, Debug)]
struct Listing {
    tags: Vec<String>,
    added: logic::AddedBounds,
    sort: String,
    order: SortDir,
    fan: Option<i64>,
    missing: bool,
}

impl Listing {
    fn query(&self) -> ReleaseQuery {
        ReleaseQuery {
            tags: self.tags.clone(),
            added_after: self.added.after.clone(),
            added_before: self.added.before.clone(),
            sort: Some(logic::release_sort_of(&self.sort)),
            order: Some(self.order),
            source_fan_id: self.fan,
            missing: self.missing.then_some(true),
            ..Default::default()
        }
    }
    fn track_query(&self) -> TrackQuery {
        TrackQuery { tags: self.tags.clone(), added_after: self.added.after.clone(), added_before: self.added.before.clone(), source_fan_id: self.fan, ..Default::default() }
    }
    fn filtered(&self) -> bool {
        !self.tags.is_empty() || self.added != logic::AddedBounds::default() || self.fan.is_some() || self.missing
    }
}

fn read_details() -> bool {
    ls_get(DETAILS_KEY).map(|v| v == "1").unwrap_or(false)
}

#[component]
pub fn AlbumsPage() -> impl IntoView {
    provide_library_host();
    let player = use_player();
    let navigate = use_navigate();
    let url = UrlState::new();
    let added = AddedFilter::new(&url);
    let sorting = ReleaseSortState::new(&url);

    let details = RwSignal::new(read_details());
    Effect::new(move |_| ls_set(DETAILS_KEY, if details.get() { "1" } else { "0" }));

    let listing = {
        let url = url.clone();
        let sorting = sorting.clone();
        Memo::new(move |_| {
            let (spec, order, _) = sorting.current();
            Listing {
                tags: url.all("tag"),
                added: added.resolved.with(|r| r.as_ref().map(|r| r.bounds.clone()).unwrap_or_default()),
                sort: spec.value.to_string(),
                order,
                fan: url.get("fan").and_then(|f| f.parse().ok()),
                missing: url.get("missing").as_deref() == Some("1"),
            }
        })
    };
    let reload = RwSignal::new(0u64);
    let prepend = RwSignal::new(0usize);
    let place_key = Memo::new(move |_| {
        let l = listing.get();
        grid_place::listing_key(&ListingWords {
            tags: l.tags.clone(),
            added: added.url_value.get(),
            sort: l.sort.clone(),
            order: logic::order_name(l.order).to_string(),
            fan: l.fan,
            missing: l.missing,
        })
    });
    let source_key = Memo::new(move |_| format!("{}|{}", place_key.get(), reload.get()));

    let total = RwSignal::new(None::<usize>);
    let newest = RwSignal::new(None::<i64>);

    // ---- fetcher -----------------------------------------------------------------------------
    let fetcher: PageFetcher<ReleaseOut> = Arc::new(move |req| {
        let mut q = listing.get_untracked().query();
        q.offset = Some(req.offset as i64);
        q.limit = Some(req.limit as i64);
        let url = format!("/releases{}", qs_pairs(&q.to_pairs()));
        let first = req.offset == 0;
        Box::pin(async move {
            let page: Page<ReleaseOut> = api::get(&url).await?;
            if first {
                newest.set(page.items.first().map(|r| r.id));
            }
            Ok(PageRes { total: page.total as usize, rows: page.items })
        })
    });

    // ---- "N new": a one-row probe of the same listing -----------------------------------------
    let probe = use_query::<Page<ReleaseOut>>(move || {
        let mut q = listing.get().query();
        q.offset = Some(0);
        q.limit = Some(1);
        let url = format!("/releases{}", qs_pairs(&q.to_pairs()));
        Some(QuerySpec::new(url, &["release"]))
    });
    let probe_data = probe.data;
    let added_count = Memo::new(move |_| match (total.get(), probe_data.get()) {
        (Some(t), Some(p)) => (p.total as usize).saturating_sub(t),
        _ => 0,
    });
    let stale = Memo::new(move |_| {
        let (Some(t), Some(p)) = (total.get(), probe_data.get()) else { return false };
        p.total as usize != t || p.items.first().map(|r| r.id) != newest.get()
    });
    let scroller = NodeRef::<leptos::html::Div>::new();
    let do_reload = {
        move || {
            // a relative window ("last 24 hours") is measured from when it was set; start over from now
            added.refresh();
            // new releases land on top of a newest-first listing: keep the reader's place
            let l = listing.get_untracked();
            if added_count.get_untracked() > 0 && l.sort == "added" && l.order == bc_types::library::SortDir::Desc {
                prepend.set(added_count.get_untracked());
            }
            reload.update(|r| *r += 1);
            probe.refetch();
        }
    };
    let do_reload = Arc::new(do_reload);

    // ---- selection ---------------------------------------------------------------------------
    let selecting = RwSignal::new(false);
    let picked = RwSignal::new(HashMap::<i64, i64>::new());
    let anchor = StoredValue::new(None::<i64>);
    let id_cache = StoredValue::new_local(None::<(String, Vec<ReleaseStub>)>);
    Effect::new(move |_| {
        source_key.track();
        picked.set(HashMap::new());
        anchor.set_value(None);
    });
    let toggle = Callback::new(move |(id, tc, shift): (i64, i64, bool)| {
        let from = anchor.get_value();
        if shift {
            if let Some(from) = from {
                let key = place_key.get_untracked();
                let q = listing.get_untracked().query();
                spawn_local(async move {
                    let cached = id_cache.with_value(|c| c.as_ref().filter(|(k, _)| *k == key).map(|(_, v)| v.clone()));
                    let stubs = match cached {
                        Some(v) => v,
                        None => match api::get::<Vec<ReleaseStub>>(&format!("/releases/ids{}", qs_pairs(&q.to_pairs()))).await {
                            Ok(v) => {
                                id_cache.set_value(Some((key, v.clone())));
                                v
                            }
                            Err(e) => {
                                toast_err(&e.message());
                                return;
                            }
                        },
                    };
                    if let Some(run) = crate::logic::range_select::run_between(&stubs, |s| s.id, &from, &id) {
                        picked.update(|m| {
                            for s in run {
                                m.insert(s.id, s.track_count);
                            }
                        });
                    }
                });
                return;
            }
        }
        picked.update(|m| {
            if m.remove(&id).is_none() {
                m.insert(id, tc);
            }
        });
        anchor.set_value(Some(id));
    });
    provide_context(AlbumSel { selecting, picked, toggle });

    // ---- place restore & save ------------------------------------------------------------------
    let restore = StoredValue::new(
        grid_place::parse_place(ls_get(PLACE_KEY).as_deref()).filter(|p| p.key == place_key.get_untracked() && grid_place::away_from_top(p)),
    );
    let phone = crate::util::is_mobile();
    let min_card_w = if phone { 140.0 } else { 168.0 };
    let gap = if phone { 12.0 } else { 16.0 };
    let metrics = move |el: &web_sys::HtmlElement| {
        // `.lib-grid-wrap .cg-scroll` has no padding (see the css), CardGrid reserves 40px for the gutters
        let w = el.client_width() as f64;
        let (cols, cw) = grid_metrics((w - 40.0).max(120.0), min_card_w, gap);
        let meta = if details.get_untracked() { META_H_DETAILS } else { META_H };
        (cols, cw + meta + gap)
    };
    Effect::new(move |_| {
        if total.get().unwrap_or(0) == 0 {
            return;
        }
        let Some(place) = restore.get_value() else { return };
        restore.set_value(None);
        crate::util::raf(move || {
            use wasm_bindgen::JsCast;
            if let Some(Some(el)) = scroller.try_get_untracked() {
                let el: web_sys::HtmlElement = el.unchecked_into();
                let (cols, row_h) = metrics(&el);
                el.set_scroll_top(logic::scroll_for_place(place.index, place.offset, cols, row_h) as i32);
            }
        });
    });
    // Another listing starts at the top (CardGrid resets on its own); forget the old place.
    Effect::new(move |prev: Option<String>| {
        let k = place_key.get();
        if prev.as_ref().is_some_and(|p| *p != k) {
            restore.set_value(None);
        }
        k
    });
    let last_save = StoredValue::new(0.0f64);
    let save_place = move |_: web_sys::Event| {
        use wasm_bindgen::JsCast;
        let now = crate::util::unix_ms();
        if now - last_save.get_value() < 500.0 {
            return;
        }
        last_save.set_value(now);
        let Some(el) = scroller.get_untracked() else { return };
        let el: web_sys::HtmlElement = el.unchecked_into();
        let (cols, row_h) = metrics(&el);
        let (index, offset) = logic::place_of(el.scroll_top() as f64, cols, row_h);
        // the release at the top-left of the viewport
        let top = el.get_bounding_client_rect().top();
        let mut anchor_id = 0i64;
        let mut best = f64::MAX;
        if let Ok(list) = el.query_selector_all(".alb") {
            for i in 0..list.length() {
                if let Some(c) = list.item(i).and_then(|n| n.dyn_into::<web_sys::Element>().ok()) {
                    let d = (c.get_bounding_client_rect().top() - top).abs();
                    if d < best {
                        best = d;
                        anchor_id = c.get_attribute("data-release-id").and_then(|v| v.parse().ok()).unwrap_or(0);
                    }
                }
            }
        }
        let place = GridPlace { key: place_key.get_untracked(), anchor_id, index, offset };
        if let Ok(s) = serde_json::to_string(&place) {
            ls_set(PLACE_KEY, &s);
        }
    };

    // ---- play all / shuffle all -----------------------------------------------------------------
    let run_id = StoredValue::new(0u64);
    let starting = RwSignal::new(None::<&'static str>);
    let note = RwSignal::new(String::new());
    on_cleanup(move || run_id.update_value(|r| *r += 1));
    let play_everything = move |shuffle: bool| {
        run_id.update_value(|r| *r += 1);
        let run = run_id.get_value();
        starting.set(Some(if shuffle { "shuffle" } else { "all" }));
        note.set(String::new());
        let mut base = listing.get_untracked().track_query();
        if shuffle {
            base.sort = Some(TrackSort::Random);
            base.seed = Some((crate::util::entropy() % 1_000_000 + 1) as i64);
        } else {
            base.sort = Some(TrackSort::Album);
            base.order = Some(SortDir::Asc);
        }
        spawn_local(async move {
            let mut queued = 0usize;
            for page in 0..PLAY_PAGES {
                if run_id.try_get_value() != Some(run) {
                    break;
                }
                let mut q = base.clone();
                q.offset = Some((page * 500) as i64);
                q.limit = Some(500);
                let p: TrackPage = match api::get(&format!("/tracks{}", qs_pairs(&q.to_pairs()))).await {
                    Ok(p) => p,
                    Err(e) => {
                        toast_err(&e.message());
                        break;
                    }
                };
                if run_id.try_get_value() != Some(run) || p.page.items.is_empty() {
                    break;
                }
                if page == 0 {
                    host::play_items(player, &p.page.items, 0, None, shuffle);
                    starting.set(None);
                } else {
                    player.cmd(bc_types::player::PlayerCommand::AddToQueue { items: p.page.items.iter().map(crate::widgets::common::queue_item).collect() });
                }
                queued += p.page.items.len();
                let tot = p.page.total as usize;
                note.set(if queued < tot {
                    format!("{} {} of {} tracks", if shuffle { "shuffling" } else { "queued" }, format_count(queued as i64), format_count(tot as i64))
                } else {
                    String::new()
                });
                if queued >= tot {
                    break;
                }
            }
            starting.set(None);
        });
    };
    let play_everything = Arc::new(play_everything);

    // ---- fan shelf ------------------------------------------------------------------------------
    let fans = use_query::<Vec<bc_types::bandcamp::FanOut>>(move || listing.with(|l| l.fan).map(|_| QuerySpec::new("/fans", &["fan"])));
    let shelf_name = Memo::new(move |_| {
        let f = listing.with(|l| l.fan)?;
        fans.data.get().and_then(|d| d.iter().find(|x| x.id == f).map(|x| x.display_name.clone().unwrap_or_else(|| x.username.clone())))
    });
    let nav2 = navigate.clone();
    let adopt = move || {
        let Some(fan) = listing.with_untracked(|l| l.fan) else { return };
        let nav = nav2.clone();
        spawn_local(async move {
            let r: Result<AdoptResult, _> = api::post("/releases/adopt", &AdoptRequest { ids: vec![], fan_id: Some(fan) }).await;
            match r {
                Ok(a) => {
                    toast_ok(&format!("Moved {} album{} into your library", format_count(a.adopted), if a.adopted == 1 { "" } else { "s" }));
                    crate::data::invalidate_all();
                    crate::data::invalidate_prefix("home");
                    nav("/albums", Default::default());
                }
                Err(e) => toast_err(&e.message()),
            }
        });
    };
    let adopt = Arc::new(adopt);

    // ---- header -----------------------------------------------------------------------------------
    let sorting_sub = sorting.clone();
    let subtitle = Signal::derive(move || {
        let t = total.get()?;
        let l = listing.get();
        let mut s = format!("{} release{}", format_count(t as i64), if t == 1 { "" } else { "s" });
        if l.missing {
            s.push_str(" missing tracks");
        }
        if l.fan.is_some() {
            s.push_str(&format!(" on {}'s shelf (downloaded from their wishlist, kept out of your library)", shelf_name.get().unwrap_or_else(|| "this".into())));
        }
        if added.active() {
            s.push_str(&format!(" {}", added.description()));
        }
        if !l.tags.is_empty() {
            s.push_str(&format!(" tagged {}", l.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ")));
        }
        if let Some(d) = sorting_sub.describe() {
            s.push_str(&format!(", {d}"));
        }
        let n = note.get();
        if !n.is_empty() {
            s.push_str(&format!(" · {n}"));
        }
        Some(s)
    });
    let (pa, pb) = (play_everything.clone(), play_everything.clone());
    let (rl1, rl2) = (do_reload.clone(), do_reload.clone());
    let adopt_menu = adopt.clone();
    let overflow = Callback::new(move |_| -> Vec<MenuEntry> {
        let mut v: Vec<MenuEntry> = vec![];
        if listing.with_untracked(|l| l.fan.is_some()) && total.get_untracked().unwrap_or(0) > 0 {
            let a = adopt_menu.clone();
            v.push(MenuItem::new("Move all into my library").icon("folder").on(move || a()).into());
        }
        let rl = rl2.clone();
        v.push(MenuItem::new("Reload albums").icon("refresh").on(move || rl()).into());
        v
    });

    let tag_scope = Signal::derive(move || TagScope { q: None, loved: None, added: added.bounds() });
    let (url_f, url_g, url_h, url_i) = (url.clone(), url.clone(), url.clone(), url.clone());

    let empty_view = move || {
        let l = listing.get_untracked();
        let msg = if l.filtered() {
            let mut s = String::from("No albums");
            if l.missing {
                s.push_str(" missing tracks");
            }
            if !l.tags.is_empty() {
                s.push_str(&format!(" tagged {}", describe_tags(&l.tags)));
            }
            if added.active() {
                s.push_str(&format!(" {}", added.description()));
            }
            s.push('.');
            s
        } else {
            "No albums yet. Scan a folder in Settings.".to_string()
        };
        view! { <EmptyState title=msg hint="Try removing a filter." icon="disc" /> }
    };

    let grid = move || {
        let det = details.get();
        let render = Callback::new(move |(r, _w): (ReleaseOut, f64)| {
            let lv = serde_json::to_value(listing.get_untracked().query()).unwrap_or_default();
            view! { <AlbumCard release=r details=det listing=lv /> }.into_any()
        });
        view! {
            <CardGrid prepend=prepend fetch=fetcher.clone() source_key=Signal::derive(move || source_key.get())
                min_card_w=min_card_w gap=gap meta_h=if det { META_H_DETAILS } else { META_H }
                render=render total_out=total node_ref=scroller empty=empty_view.clone() />
        }
    };

    view! {
        <div class="page">
            <PageHeader title="Albums" subtitle=subtitle overflow=overflow
                actions=crate::ds::children(move || {
                    let (pa, pb, rl1) = (pa.clone(), pb.clone(), rl1.clone());
                    view! {
                        {move || stale.get().then(|| {
                            let rl = rl1.clone();
                            let new_label = move || if added_count.get() > 0 { format!("Reload albums, {} new", format_count(added_count.get() as i64)) } else { "Reload albums, the library changed".to_string() };
                            view! {
                                <button type="button" class="lib-pill on strong" on:click=move |_| rl()
                                    aria-label=new_label
                                    title="New albums are available">
                                    <Icon name="refresh" size=13 />
                                    <span class="mono">{move || if added_count.get() > 0 { format!("{} new", format_count(added_count.get() as i64)) } else { "Updated".to_string() }}</span>
                                </button>
                            }
                        })}
                        <Button variant=Variant::Ghost icon="rows" title="Album details" pressed=details on_click=move |_| details.update(|d| *d = !*d) />
                        <Button variant=Variant::Ghost icon="check" title="Select albums" pressed=selecting on_click=move |_| selecting.update(|s| *s = !*s) />
                        {move || (total.get().unwrap_or(0) > 0).then(|| {
                            let (pa, pb) = (pa.clone(), pb.clone());
                            view! {
                                <Button variant=Variant::Primary icon="play" busy=Signal::derive(move || starting.get() == Some("all")) on_click=move |_| pa(false)>"Play"</Button>
                                <Button icon="shuffle" title="Shuffle all" busy=Signal::derive(move || starting.get() == Some("shuffle")) on_click=move |_| pb(true)><span class="hide-sm">"Shuffle"</span></Button>
                            }
                        })}
                    }
                }) />
            <div class="lib-filters">
                <TagFilterBar url=url_f scope=tag_scope />
                <AddedFilterBar url=url_g filter=added />
                <FillMissingBar url=url_h />
                <span class="spacer"></span>
                <ReleaseSortBar state=ReleaseSortState::new(&url_i) />
            </div>
            {move || selecting.get().then(|| view! { <ReleaseSelectionBar total=total selecting=selecting picked=picked listing=listing reload=reload /> })}
            <div class="lib-grid-wrap" on:scroll:capture=save_place>
                {grid}
            </div>
            <LibraryHost />
        </div>
    }
}

/// Bulk bar above the grid while selecting: select all, add to set, delete (blacklist optional).
#[component]
fn ReleaseSelectionBar(
    total: RwSignal<Option<usize>>,
    selecting: RwSignal<bool>,
    picked: RwSignal<HashMap<i64, i64>>,
    listing: Memo<Listing>,
    reload: RwSignal<u64>,
) -> impl IntoView {
    let host = host::use_library_host();
    let busy = RwSignal::new(false);
    let count = Memo::new(move |_| picked.with(|p| p.len()));
    let track_count = Memo::new(move |_| picked.with(|p| p.values().sum::<i64>()));
    let all_picked = Memo::new(move |_| total.get().map(|t| t > 0 && count.get() >= t).unwrap_or(true));
    let select_all = move |_| {
        busy.set(true);
        let q = listing.get_untracked().query();
        spawn_local(async move {
            match api::get::<Vec<ReleaseStub>>(&format!("/releases/ids{}", qs_pairs(&q.to_pairs()))).await {
                Ok(v) => picked.set(v.into_iter().map(|s| (s.id, s.track_count)).collect()),
                Err(e) => toast_err(&e.message()),
            }
            busy.set(false);
        });
    };
    let delete = move |_| {
        let ids: Vec<i64> = picked.with_untracked(|p| p.keys().copied().collect());
        let tracks = track_count.get_untracked();
        host.delete.set(Some(host::DeleteReq {
            release_ids: ids,
            tracks: Some(tracks),
            on_done: Some(Callback::new(move |gone: Vec<i64>| {
                picked.update(|p| {
                    for g in gone {
                        p.remove(&g);
                    }
                });
                reload.update(|r| *r += 1);
            })),
        }));
    };
    let add_to = move |_| {
        let ids: Vec<i64> = picked.with_untracked(|p| p.keys().copied().collect());
        host.picker.set(Some(host::PickerReq { ids: release_track_ids(ids) }));
    };
    view! {
        <div class="lib-selbar" role="status">
            {move || (total.get().unwrap_or(0) > 0).then(|| view! {
                <Button size=Size::Sm variant=Variant::Ghost busy=busy disabled=all_picked on_click=select_all>{move || format!("Select all {}", format_count(total.get().unwrap_or(0) as i64))}</Button>
            })}
            {move || (count.get() > 0).then(|| view! { <Button size=Size::Sm variant=Variant::Ghost on_click=move |_| picked.set(HashMap::new())>"Clear"</Button> })}
            <span class="mono muted lib-selcount">
                {move || if count.get() == 0 { "Nothing selected. Click covers to pick them, shift-click for a range.".to_string() }
                    else { format!("{} selected · {} track{}", format_count(count.get() as i64), format_count(track_count.get()), if track_count.get() == 1 { "" } else { "s" }) }}
            </span>
            <span class="spacer"></span>
            {move || (count.get() > 0).then(|| view! {
                <Button size=Size::Sm icon="list" on_click=add_to>"Add to set"</Button>
                <Button size=Size::Sm variant=Variant::Danger icon="trash" on_click=delete>{move || format!("Delete {}", format_count(count.get() as i64))}</Button>
            })}
            <Button size=Size::Sm on_click=move |_| { picked.set(HashMap::new()); selecting.set(false); }>"Done"</Button>
        </div>
    }
}
