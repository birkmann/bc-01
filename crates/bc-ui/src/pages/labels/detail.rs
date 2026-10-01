//! One label: hero, library releases, roster and the Bandcamp catalogue.
use std::sync::Arc;

use bc_types::bandcamp::BandOut;
use bc_types::library::{LabelOut, Page, ReleaseOut, ReleaseQuery, ReleaseSort, SortDir};
use bc_types::player::LabelMode;
use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};

use super::bandcamp::{Actions, BandCatalogue, BandcampPicker, CatalogDownloadButton, Entity, FollowButton, use_band};
use super::edit::{EditDialog, EditTarget, remove_label};
use super::folder::play_label;
use super::logic as lg;
use super::shared::{Collage, FavButton, Kind, NoticeBar, Q, ReleaseCard, open_tab, use_favs, use_q};
use super::strays::StraysTrigger;
use crate::api;
use crate::data::QuerySpec;
use crate::ds::{Button, EmptyState, ErrorPanel, Icon, MenuButton, MenuEntry, MenuItem, Select, SelectOption, Size, Variant};
use crate::logic::format::{format_bytes, format_count};
use crate::util::qs;
use crate::widgets::card_grid::CardGrid;
use crate::widgets::{PageFetcher, PageRes};

fn releases_url(id: i64, sort: &str, offset: usize, limit: usize) -> String {
    let order = lg::release_sort_order(sort);
    format!("/releases{}", qs(&[("label_id", id.to_string()), ("sort", sort.to_string()), ("order", order.to_string()), ("offset", offset.to_string()), ("limit", limit.to_string())]))
}

/// The listing an album started from this label's page plays on into.
fn release_listing(id: i64, sort: &str) -> serde_json::Value {
    let sort = match sort {
        "year" => ReleaseSort::Year,
        "title" => ReleaseSort::Title,
        "artist" => ReleaseSort::Artist,
        _ => ReleaseSort::Added,
    };
    let order = if lg::release_sort_order(match sort { ReleaseSort::Year => "year", ReleaseSort::Title => "title", ReleaseSort::Artist => "artist", _ => "added" }) == "asc" { SortDir::Asc } else { SortDir::Desc };
    serde_json::to_value(ReleaseQuery { label_id: Some(id), sort: Some(sort), order: Some(order), ..Default::default() }).unwrap_or_default()
}

#[component]
pub fn LabelDetailPage() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    let navigate = use_navigate();
    let id = Memo::new(move |_| params.get().get("id").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0));
    let favs = use_favs();

    let label = use_q::<LabelOut>(move || {
        let id = id.get();
        (id > 0).then(|| QuerySpec::new(format!("/labels/{id}"), &["label"]))
    });
    let band_url = Signal::derive(move || label.data.get().and_then(|l| l.bandcamp_url.clone()));
    let band = use_band(band_url);
    let tab = Memo::new(move |_| query.get().get("tab").unwrap_or_else(|| "library".into()));
    let set_tab = {
        let navigate = navigate.clone();
        move |t: &str| {
            let url = if t == "library" { format!("/labels/{}", id.get_untracked()) } else { format!("/labels/{}?tab={t}", id.get_untracked()) };
            navigate(&url, NavigateOptions { replace: true, ..Default::default() });
        }
    };
    let set_tab = Arc::new(set_tab);
    let sort = RwSignal::new("added".to_string());
    let edit: RwSignal<Option<EditTarget>> = RwSignal::new(None);
    let actions = Actions::new(Callback::new(move |_| {
        crate::data::invalidate_entity("label", &[]);
        label.refetch();
    }));
    let bio_open = RwSignal::new(false);

    let name = Signal::derive(move || label.data.get().map(|l| l.name.clone()).unwrap_or_default());
    let entity = move || label.data.get_untracked().map(|l| Entity { kind: Kind::Label, id: l.id, name: l.name.clone(), url: l.bandcamp_url.clone() });
    let band_data = Signal::derive(move || band.data.get());
    let catalogue = Signal::derive(move || band_data.get().map(|b| lg::catalogue_counts(&b.releases)));
    let missing = Signal::derive(move || catalogue.get().map(|c| c.0).unwrap_or(0));
    let exact = Signal::derive(move || band_data.get().map(|b| !b.truncated).unwrap_or(false));
    let busy_any = actions.busy();

    let play = move |mode: LabelMode| {
        let Some(l) = label.data.get_untracked() else { return };
        let lj = serde_json::json!({ "sort": "releases", "order": "desc" });
        play_label(l.id, lj, mode);
    };

    // Tracks the library holds for this label: Play/Shuffle disabled when none.
    let no_tracks = Signal::derive(move || label.data.get().map(|l| l.track_count == 0).unwrap_or(true));

    let nav_remove = navigate.clone();
    let menu = Callback::new(move |_| -> Vec<MenuEntry> {
        let Some(l) = label.data.get_untracked() else { return vec![] };
        let (a, b, c) = (l.clone(), l.clone(), l.clone());
        let nav = nav_remove.clone();
        let mut v: Vec<MenuEntry> = vec![
            MenuItem::new("Edit label\u{2026}").icon("edit").on(move || edit.set(Some(EditTarget { kind: Kind::Label, id: a.id, name: a.name.clone(), url: a.bandcamp_url.clone() }))).into(),
            MenuItem::new("Download as ZIP").icon("download").disabled(b.track_count == 0).on(move || open_tab(&format!("/api/tracks/export{}", qs(&[("label_id", b.id.to_string()), ("format", "zip".into())])))).into(),
        ];
        if let Some(u) = l.bandcamp_url.clone().filter(|u| !u.is_empty()) {
            v.push(MenuItem::new("Open on Bandcamp").icon("external").on(move || open_tab(&u)).into());
        }
        v.push(MenuEntry::Sep);
        v.push(MenuItem::new("Remove label\u{2026}").icon("trash").danger().on(move || {
            let nav = nav.clone();
            remove_label((*c).clone(), Callback::new(move |_| { nav("/labels", Default::default()); }));
        }).into());
        v
    });

    let nav_saved = navigate.clone();
    let on_saved = Callback::new(move |survivor: i64| {
        crate::data::invalidate_entity("label", &[]);
        if survivor != id.get_untracked() {
            nav_saved(&format!("/labels/{survivor}"), NavigateOptions { replace: true, ..Default::default() });
        } else {
            label.refetch();
        }
    });

    let fetcher: PageFetcher<ReleaseOut> = Arc::new(move |req| {
        let url = releases_url(id.get_untracked(), &sort.get_untracked(), req.offset, req.limit);
        Box::pin(async move {
            let p: Page<ReleaseOut> = api::get(&url).await?;
            Ok(PageRes { rows: p.items, total: p.total as usize })
        })
    });
    let source_key = Signal::derive(move || format!("{}|{}", id.get(), sort.get()));
    let listing = Signal::derive(move || release_listing(id.get(), &sort.get()));
    let sort_options = Signal::derive(|| lg::RELEASE_SORTS.iter().map(|s| SelectOption::new(s.0, s.1)).collect::<Vec<_>>());
    let roster_n = Signal::derive(move || band_data.get().map(|b| b.roster.len()).unwrap_or(0));

    let set_tab_lib = set_tab.clone();
    let set_tab_bc = set_tab.clone();
    let set_tab_ro = set_tab.clone();

    view! {
        <div class="page pp-page pp-detail">
            <header class="pp-hero">
                <a class="pp-back" href="/labels" aria-label="All labels"><Icon name="arrow-left" /><span class="hide-sm">"All labels"</span></a>
                <div class="pp-hero-art pp-hero-square">
                    {move || match label.data.get() {
                        Some(l) => {
                            let urls: Vec<String> = l.art_urls.iter().map(|u| lg::thumb(u)).collect();
                            view! { <Collage urls=urls /> }.into_any()
                        }
                        None => view! { <div class="skeleton pp-fill-box"></div> }.into_any(),
                    }}
                </div>
                <div class="pp-hero-body">
                    <div class="pp-eyebrow hide-sm">"Label"</div>
                    <h1 class="pp-title">{move || if name.get().is_empty() { "\u{2026}".to_string() } else { name.get() }}</h1>
                    <div class="pp-meta num">
                        {move || label.data.get().map(|l| view! {
                            <span>{format!("{} \u{b7} {}", lg::count_of(l.release_count, "release"), lg::count_of(l.track_count, "track"))}{(l.size_bytes > 0).then(|| format!(" \u{b7} {}", format_bytes(l.size_bytes as f64)))}</span>
                        })}
                        {move || band_data.get().map(|b| {
                            let (m, _, all) = lg::catalogue_counts(&b.releases);
                            view! {
                                <span>{format!("\u{b7} {}{} on Bandcamp", format_count(all as i64), if b.truncated { "+" } else { "" })}
                                    {(m > 0).then(|| view! { " (" <span class="pp-accent">{format!("{} missing", format_count(m as i64))}</span> ")" })}
                                </span>
                                {b.location.clone().map(|l| view! { <span>{format!("\u{b7} {l}")}</span> })}
                            }
                        })}
                    </div>
                    {move || band_data.get().and_then(|b| b.bio.clone()).filter(|b| !b.is_empty()).map(|bio| view! {
                        <button type="button" class=move || if bio_open.get() { "pp-bio open" } else { "pp-bio" } aria-expanded=move || bio_open.get().to_string()
                            title="Show the whole bio" on:click=move |_| bio_open.update(|o| *o = !*o)>{bio}</button>
                    })}
                    <div class="pp-actions">
                        <Button variant=Variant::Primary icon="play" disabled=no_tracks on_click=move |_| play(LabelMode::All)>"Play all"</Button>
                        <Button icon="shuffle" disabled=no_tracks on_click=move |_| play(LabelMode::Shuffle) title="Shuffle"><span class="hide-sm">"Shuffle"</span></Button>
                        <Button icon="rss" busy=actions.find_busy disabled=Signal::derive(move || label.data.get().is_none())
                            on_click=move |_| if let Some(e) = entity() { actions.find_new(e) }
                            title=move_title(band_url)><span class="hide-sm">"Find new releases"</span></Button>
                        {move || band_url.get().filter(|u| !u.is_empty()).map(|u| view! { <CatalogDownloadButton url=u missing=missing exact=exact /> })}
                        {move || label.data.get().map(|l| view! { <FavButton kind=Kind::Label id=l.id favs=favs name=l.name.clone() /> })}
                        <MenuButton entries=menu title="Label menu" />
                    </div>
                    <div class="pp-actions pp-actions-sub">
                        {move || match (band_url.get().filter(|u| !u.is_empty()), label.data.get()) {
                            (Some(u), Some(l)) => view! {
                                <FollowButton kind=Kind::Label name=l.name.clone() url=u.clone() />
                                <a class="btn btn-ghost btn-sm" href=u.clone() target="_blank" rel="noreferrer noopener" title=u.clone()><Icon name="external" /><span class="hide-sm">"Open on Bandcamp"</span></a>
                            }.into_any(),
                            (None, Some(_)) => view! {
                                <Button size=Size::Sm variant=Variant::Outline icon="compass" busy=actions.find_busy on_click=move |_| if let Some(e) = entity() { actions.find_new(e) }
                                    title="Search Bandcamp for this label's page and prove it against the library">"Find on Bandcamp"</Button>
                            }.into_any(),
                            _ => ().into_any(),
                        }}
                    </div>
                </div>
            </header>
            <div class="pp-notices"><NoticeBar notice=actions.notice busy=busy_any on_download=Callback::new(move |ids| actions.queue(ids)) /></div>
            <div class="tabs pp-tabs" role="tablist">
                {move || {
                    let t = tab.get();
                    let cnt = label.data.get().map(|l| l.release_count);
                    let (sl, sb, sr) = (set_tab_lib.clone(), set_tab_bc.clone(), set_tab_ro.clone());
                    view! {
                        <button class="tab" role="tab" type="button" aria-selected=(t == "library").to_string() on:click=move |_| sl("library")>
                            <Icon name="disc" />"In your library"{cnt.map(|c| view! { <span class="count">{format_count(c)}</span> })}
                        </button>
                        <button class="tab" role="tab" type="button" aria-selected=(t == "bandcamp").to_string() on:click=move |_| sb("bandcamp")>
                            <Icon name="compass" />"On Bandcamp"
                            {catalogue.get().map(|c| view! { <span class="count">{format_count(c.2 as i64)}</span> })}
                            {(missing.get() > 0).then(|| view! { <span class="badge badge-accent">{format!("{} missing", format_count(missing.get() as i64))}</span> })}
                        </button>
                        {(roster_n.get() > 0).then(|| view! {
                            <button class="tab" role="tab" type="button" aria-selected=(t == "roster").to_string() on:click=move |_| sr("roster")>
                                <Icon name="users" />"Roster"<span class="count">{roster_n.get()}</span>
                            </button>
                        })}
                    }
                }}
            </div>
            <div class="pp-tabbody">
                {move || {
                    if let Some(e) = label.error.get() {
                        if label.data.get().is_none() {
                            return view! { <ErrorPanel message=e.message() on_retry=Callback::new(move |_| label.refetch()) /> }.into_any();
                        }
                    }
                    match tab.get().as_str() {
                        "bandcamp" => view! { <LabelBandcamp label=label.data band=band actions=actions edit=edit /> }.into_any(),
                        "roster" => view! { <Roster band=band_data /> }.into_any(),
                        _ => view! {
                            <div class="pp-lib">
                                <div class="pp-subbar">
                                    {move || (id.get() > 0).then(|| view! { <StraysTrigger label_id=id.get() /> })}
                                    <span class="spacer"></span>
                                    <Select options=sort_options value=sort aria_label="Sort releases" class="pp-sort" />
                                </div>
                                <div class="pp-fill">
                                    <CardGrid fetch=fetcher.clone() source_key=source_key min_card_w=144.0 meta_h=48.0 gap=12.0 entities=vec!["release"]
                                        render=Callback::new(move |(r, _w): (ReleaseOut, f64)| view! { <ReleaseCard r=r listing=listing show_artist=true /> }.into_any())
                                        empty=move || view! { <EmptyState icon="disc" title="Nothing on this label yet" /> } />
                                </div>
                            </div>
                        }.into_any(),
                    }
                }}
            </div>
            <EditDialog target=edit on_saved=on_saved />
        </div>
    }
}

fn move_title(band_url: Signal<Option<String>>) -> String {
    match band_url.get_untracked() {
        Some(u) if !u.is_empty() => format!("Check {u} for releases not yet harvested"),
        _ => "Search Bandcamp for this label, verify the page against your releases, then check it".into(),
    }
}

#[component]
fn Roster(band: Signal<Option<Arc<BandOut>>>) -> impl IntoView {
    view! {
        <div class="pp-scroll">
            <div class="pp-chiprow pp-roster">
                {move || band.get().map(|b| b.roster.iter().map(|a| view! {
                    <a class="chip pp-roster-chip" href=lg::band_path(&a.url) title=a.location.clone().unwrap_or_default()>{a.name.clone()}</a>
                }).collect_view())}
            </div>
        </div>
    }
}

#[component]
fn LabelBandcamp(label: RwSignal<Option<Arc<LabelOut>>>, band: Q<BandOut>, actions: Actions, edit: RwSignal<Option<EditTarget>>) -> impl IntoView {
    view! {
        {move || {
            let Some(l) = label.get() else { return view! { <div class="pp-scroll"><p class="pp-hint"><span class="pp-spin"></span></p></div> }.into_any() };
            let Some(url) = l.bandcamp_url.clone().filter(|u| !u.is_empty()) else {
                let (l2, l3) = (l.clone(), l.clone());
                return view! {
                    <div class="pp-scroll">
                        <p class="pp-hint pp-hint-wide">
                            {format!("No Bandcamp page is pinned for {} yet \u{2014} pick theirs from the search below and the catalogue, \u{201c}Find new releases\u{201d} and the bulk download all read from it. Nothing here? ", l.name)}
                            <button type="button" class="pp-linkbtn" on:click=move |_| edit.set(Some(EditTarget { kind: Kind::Label, id: l2.id, name: l2.name.clone(), url: None }))>"Paste the URL yourself"</button>"."
                        </p>
                        <BandcampPicker name=l.name.clone() kind=Kind::Label current_url=Signal::derive(|| None::<String>)
                            on_pick=Callback::new(move |u| actions.pin(Entity { kind: Kind::Label, id: l3.id, name: l3.name.clone(), url: None }, u))
                            picking=actions.pinning error=actions.pin_error />
                    </div>
                }.into_any();
            };
            if let Some(b) = band.data.get() {
                let fu = Some((l.name.clone(), url.clone()));
                return view! { <BandCatalogue band=(*b).clone() url=url file_under=fu /> }.into_any();
            }
            if let Some(e) = band.error.get() {
                return view! { <div class="pp-scroll"><p class="pp-hint danger">{format!("Could not read the label's Bandcamp page: {} ", e.message())}
                    <button type="button" class="pp-linkbtn" on:click=move |_| band.refetch()>"Try again"</button></p></div> }.into_any();
            }
            view! { <div class="pp-scroll"><p class="pp-hint"><span class="pp-spin"></span>{format!(" Reading {url}\u{2026}")}</p></div> }.into_any()
        }}
    }
}
