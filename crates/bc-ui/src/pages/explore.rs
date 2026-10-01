//! Browsing Bandcamp from inside the app: a browse screen (search + discover feed with
//! infinite scroll), a band page (artist or label) and a release page. Everything playable
//! goes through the same player queue as the library: a Bandcamp track is a queue item with
//! a negative id and `origin: bandcamp`, so the transport and the queue need no special case.
use std::collections::BTreeMap;
use std::sync::Arc;

use bc_types::bandcamp::{BandOut, DiscoverOut, ExploreReleaseOut, FollowCreate, FollowOut, FollowPatch, FollowsOut, ReleaseCardOut, SearchHitOut};
use bc_types::library::{TrackOut, TrackPage};
use bc_types::player::{PlayerCommand, QueueItem};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_navigate, use_query_map};
use serde_json::Value;

use crate::api;
use crate::data::{self, QuerySpec};
use crate::ds::{self, Button, EmptyState, ErrorPanel, Icon, SelectOption, Size, Variant};
use crate::logic::format::{format_count, format_duration_ms};
use crate::player::use_player;
use crate::util::{enc, qs_pairs};
use crate::widgets::common::{Art, LoveButton, queue_item};

pub(crate) mod cards;
pub(crate) mod logic;
mod play;
#[allow(dead_code)]
pub(crate) mod qh;
mod related;
/// `BandcampSearchShelf` / `BandcampPagePicker` are exported for the library screens (Tracks search miss, artist/label pinning).
#[allow(dead_code)]
pub(crate) mod search;
mod supporters;
mod vgrid;

use cards::{CatalogDownloadButton, FileUnder, FollowBandButton, GridPlaybackBar, ReleaseCardView, Selecting, SelectionBar};
use logic::{Browse, Facets, band_path, group_hits, local_match, parse_facets, price_label, query_key, saved_query_path, stream_item, tag_path};
use related::{BandDiscography, BandcampRelated, Loading};
use search::{HitSections, SectionHeading};
use supporters::Supporters;
use vgrid::VGrid;

type Nav = Arc<dyn Fn(Vec<(&'static str, Option<String>)>) + Send + Sync>;

fn sweep_of(items: Signal<Vec<ReleaseCardOut>>) -> Signal<Vec<(String, Option<i64>)>> {
    Signal::derive(move || items.with(|v| v.iter().map(|c| (c.url.clone(), c.library_release_id)).collect()))
}

// ---------------------------------------------------------------------------
// Saved queries
// ---------------------------------------------------------------------------

/// The current query as a saveable thing, plus every already-saved one. A saved row keeps the
/// query twice: `explore_params` is this page's own URL state (recall is navigation),
/// `api_params` is the discover call the feed sweep replays.
#[component]
fn SavedQueries(
    #[prop(into)] explore: Signal<BTreeMap<String, Value>>,
    #[prop(into)] api_params: Signal<BTreeMap<String, Value>>,
    #[prop(into)] kind: Signal<&'static str>,
    #[prop(into)] label: Signal<String>,
) -> impl IntoView {
    let follows = qh::use_q::<FollowsOut>(|| Some(QuerySpec::keyed("/follows", "/follows", &["follow"])));
    let busy = RwSignal::new(false);
    let saved = Signal::derive(move || -> Vec<FollowOut> {
        follows.data.with(|d| d.as_ref().map(|f| f.sources.iter().filter(|s| s.kind == "discover" || s.kind == "search").cloned().collect()).unwrap_or_default())
    });
    let current = Signal::derive(move || query_key(&explore.get()));
    let is_saved = Signal::derive(move || saved.with(|s| s.iter().any(|f| query_key(&f.explore_params) == current.get())));
    let run = move |fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), api::ApiErr>>>>| {
        busy.set(true);
        spawn_local(async move {
            if let Err(e) = fut.await {
                ds::toast_err(&e.message());
            }
            data::invalidate_prefix("/follows");
            let _ = busy.try_set(false);
        });
    };
    let save = move |_| {
        let body = FollowCreate { kind: kind.get_untracked().into(), label: label.get_untracked(), explore_params: explore.get_untracked(), api_params: api_params.get_untracked(), ..Default::default() };
        run(Box::pin(async move { api::post::<_, FollowOut>("/follows", &body).await.map(|_| ()) }));
    };
    view! {
        <div class="xg-saved">
            <Button size=Size::Sm variant=Variant::Outline icon=ds::dyn_icon(move || if is_saved.get() { "check" } else { "bookmark" })
                disabled=Signal::derive(move || is_saved.get() || busy.get())
                title="Save this query for fast recall"
                on_click=save>
                {move || if is_saved.get() { "Saved" } else { "Save query" }}
            </Button>
            <For each=move || saved.get() key=|f| (f.id, f.enabled, f.label.clone()) let:f>
                {
                    let path = saved_query_path(&f.explore_params);
                    let key = query_key(&f.explore_params);
                    let (id, enabled, is_discover) = (f.id, f.enabled, f.kind == "discover");
                    view! {
                        <span class=move || if key == current.get() { "chip saved on" } else { "chip saved" }>
                            <a href=path>{f.label.clone()}</a>
                            {is_discover.then(|| view! {
                                <button type="button" class=if enabled { "sq-btn on" } else { "sq-btn" }
                                    aria-label=if enabled { "Followed. Click to unfollow" } else { "Follow this query" }
                                    title=if enabled { "Followed: the feed checks it for new releases. Click to unfollow." } else { "Follow: check this query for new releases" }
                                    on:click=move |_| run(Box::pin(async move { api::patch::<_, FollowOut>(&format!("/follows/{id}"), &FollowPatch { label: None, enabled: Some(!enabled) }).await.map(|_| ()) }))>
                                    <Icon name="rss" />
                                </button>
                            })}
                            <button type="button" class="sq-btn del" aria-label="Forget this saved query" title="Forget this saved query"
                                on:click=move |_| run(Box::pin(async move { api::call("DELETE", &format!("/follows/{id}")).await }))>
                                <Icon name="x" />
                            </button>
                        </span>
                    }
                }
            </For>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Browse feed (discover, infinite)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct Feed {
    items: RwSignal<Vec<ReleaseCardOut>>,
    has_more: RwSignal<bool>,
    loading: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    total: RwSignal<Option<i64>>,
    epoch: RwSignal<u64>,
    more: Callback<()>,
    reset: Callback<()>,
}

fn use_feed(browse: Memo<Browse>, active: Signal<bool>, facets: Signal<Facets>) -> Feed {
    let items = RwSignal::new(Vec::<ReleaseCardOut>::new());
    let has_more = RwSignal::new(true);
    let loading = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let total = RwSignal::new(None::<i64>);
    let epoch = RwSignal::new(0u64);
    let cursor = RwSignal::new(None::<String>);
    let generation = StoredValue::new(0u64);

    let more = Callback::new(move |_: ()| {
        if loading.get_untracked() || !has_more.get_untracked() || !active.get_untracked() {
            return;
        }
        loading.set(true);
        let g = generation.get_value();
        let pairs = browse.get_untracked().discover_pairs(&facets.get_untracked(), &cursor.get_untracked().unwrap_or_else(|| "*".into()), 48);
        spawn_local(async move {
            let res = api::get::<DiscoverOut>(&format!("/explore/discover{}", qs_pairs(&pairs))).await;
            if generation.try_get_value() != Some(g) {
                return;
            }
            match res {
                Ok(d) => {
                    let end = d.cursor.is_none() || d.items.is_empty();
                    let _ = items.try_update(|v| {
                        let have: std::collections::HashSet<String> = v.iter().map(|c| c.url.clone()).collect();
                        v.extend(logic::dedupe_cards(d.items).into_iter().filter(|c| !have.contains(&c.url)));
                    });
                    if d.total.is_some() {
                        let _ = total.try_set(d.total);
                    }
                    let _ = cursor.try_set(d.cursor);
                    let _ = has_more.try_set(!end);
                }
                Err(e) => {
                    // No retry loop: the error panel offers one.
                    let _ = error.try_set(Some(qh::err_text(&e)));
                    let _ = has_more.try_set(false);
                }
            }
            let _ = loading.try_set(false);
        });
    });
    let reset = Callback::new(move |_: ()| {
        generation.update_value(|g| *g += 1);
        items.set(vec![]);
        cursor.set(None);
        total.set(None);
        error.set(None);
        loading.set(false);
        has_more.set(true);
        epoch.update(|e| *e += 1);
        more.run(());
    });
    Effect::new(move |_| {
        let _ = browse.get();
        if active.get() {
            reset.run(());
        }
    });
    Feed { items, has_more, loading, error, total, epoch, more, reset }
}

#[component]
fn Notice(#[prop(into)] text: String, #[prop(optional)] error: bool) -> impl IntoView {
    view! { <div class=if error { "xg-notice err" } else { "xg-notice" } role=if error { "alert" } else { "status" }>{text}</div> }
}

#[component]
fn SearchResults(hits: Vec<SearchHitOut>, tag: String, tag_items: Vec<ReleaseCardOut>, tag_total: Option<i64>) -> impl IntoView {
    let grouped = group_hits(&hits);
    let playable: Vec<(String, Option<i64>)> = grouped.playable().iter().map(|h| (h.url.clone(), h.library_release_id)).collect();
    let feed_leads = tag_total.unwrap_or(0) >= logic::TAG_IS_A_GENRE;
    // A genre goes above the name matches (it is what the word was asking for); a tag only a
    // few records carry goes under them, where it reads as the footnote it is.
    let tag_feed = (!tag_items.is_empty()).then(|| {
        let (items, tag2) = (tag_items.clone(), tag.clone());
        view! {
            <section class="xg-section">
                <h2 class="xg-h">
                    <span class="display xg-h-l"><Icon name="tag" />{format!("Tagged {tag2}")}</span>
                    {tag_total.map(|t| view! { <span class="mono faint">{format!("{} releases", format_count(t))}</span> })}
                    <a class="xg-more" href=tag_path(&tag2)>"browse the whole feed \u{2192}"</a>
                </h2>
                <cards::ReleaseGrid items=items min=140 />
            </section>
        }
    });
    let (lead, trail) = if feed_leads { (tag_feed, None) } else { (None, tag_feed) };
    let playable = Signal::derive(move || playable.clone());
    view! {
        <div class="xg-results">
            {(!playable.with(|p| p.is_empty())).then(|| view! { <GridPlaybackBar items=playable noun="results" /> })}
            {lead}
            <HitSections grouped=grouped />
            {trail}
        </div>
    }
}

#[component]
pub fn ExplorePage() -> impl IntoView {
    let query = use_query_map();
    let navigate = use_navigate();
    let q = Memo::new(move |_| query.get().get("q").unwrap_or_default());
    let browse = Memo::new(move |_| {
        let m = query.get();
        Browse::from_params(m.get("genre"), m.get("slice"), m.get("tag"), m.get("place"))
    });
    let searching = Signal::derive(move || !q.get().trim().is_empty());
    let browsing = Signal::derive(move || !searching.get());
    qh::use_title(true, move || {
        let (q, b) = (q.get(), browse.get());
        let what = if !q.is_empty() { q } else { b.tag };
        if what.is_empty() { "Explore".to_string() } else { format!("Explore \u{2014} {what}") }
    });

    let nav: Nav = Arc::new(move |updates| {
        let m = query.get_untracked();
        let mut pairs: Vec<(String, String)> = vec![];
        for k in ["q", "genre", "slice", "tag", "place"] {
            let v = match updates.iter().find(|(uk, _)| *uk == k) {
                Some((_, v)) => v.clone(),
                None => m.get(k),
            };
            if let Some(v) = v.filter(|v| !v.is_empty()) {
                pairs.push((k.into(), v));
            }
        }
        navigate(&format!("/explore{}", qs_pairs(&pairs)), NavigateOptions { replace: true, ..Default::default() });
    });

    // Bandcamp's own filter vocabulary, so the dropdowns offer slugs discover actually matches.
    let genres_q = qh::use_q::<Value>(|| Some(QuerySpec::new("/explore/genres", &[])));
    let facets = Signal::derive(move || genres_q.data.with(|d| parse_facets(d.as_deref())));

    let input = RwSignal::new(q.get_untracked());
    Effect::new(move |_| input.set(q.get()));

    // Search: name matches, plus the same words read as a tag (best-selling, like bandcamp.com/discover/<tag>).
    let search = qh::use_q::<Vec<SearchHitOut>>(move || {
        let q = q.get();
        (!q.trim().is_empty()).then(|| QuerySpec::new(format!("/explore/search?q={}&limit=30", enc(q.trim())), &[]))
    });
    let tag_preview = qh::use_q::<DiscoverOut>(move || {
        let q = q.get();
        (!q.trim().is_empty()).then(|| QuerySpec::new(format!("/explore/discover?tags={}&slice=top&size=24", enc(q.trim())), &[]))
    });

    let feed = use_feed(browse, browsing, facets);

    // Saved-query shape (what "Save query" stores).
    let explore_params = Signal::derive(move || browse.get().explore_params(&q.get()));
    let api_params = Signal::derive(move || browse.get().api_params(&q.get(), &facets.get()));
    let save_label = Signal::derive(move || browse.get().save_label(&q.get(), &facets.get()));
    let save_kind = Signal::derive(move || if searching.get() { "search" } else { "discover" });

    // ---- facets row ----
    let genre = RwSignal::new(browse.get_untracked().genre);
    let slice = RwSignal::new(browse.get_untracked().slice);
    let tag = RwSignal::new(browse.get_untracked().tag);
    let place = RwSignal::new(browse.get_untracked().place);
    Effect::new(move |_| {
        let b = browse.get();
        genre.set(b.genre);
        slice.set(b.slice);
        tag.set(b.tag);
        place.set(b.place);
    });
    let genre_opts = Signal::derive(move || facets.with(|f| f.genres.iter().map(|g| SelectOption::new(g.slug.clone(), g.label.clone())).collect::<Vec<_>>()));
    let subgenres = Signal::derive(move || facets.with(|f| f.subgenres_of(&genre.get())));
    let sub_opts = Signal::derive(move || {
        let mut v = vec![SelectOption::new("", format!("all {}", genre.get().replace('-', " ")))];
        v.extend(subgenres.get().into_iter().map(|s| SelectOption::new(s.slug, s.label)));
        v
    });
    let slice_opts = Signal::derive(move || facets.with(|f| f.slices.iter().map(|g| SelectOption::new(g.slug.clone(), g.label.clone())).collect::<Vec<_>>()));
    let place_opts = Signal::derive(move || {
        facets.with(|f| {
            let mut v = vec![SelectOption::new("0", "from anywhere")];
            v.extend(f.locations.iter().filter(|l| l.slug != "0").map(|l| SelectOption::new(l.slug.clone(), l.label.clone())));
            v
        })
    });
    let has_places = Signal::derive(move || facets.with(|f| !f.locations.is_empty()));
    let free_tag = Signal::derive(move || {
        let t = tag.get();
        (!t.is_empty() && !subgenres.with(|s| s.iter().any(|x| x.slug == t))).then_some(t)
    });

    let submit = {
        let nav = nav.clone();
        move |ev: leptos::ev::SubmitEvent| {
            ev.prevent_default();
            nav(vec![("q", Some(input.get_untracked().trim().to_string()))]);
        }
    };
    let (n1, n2, n3, n4, n5, n6) = (nav.clone(), nav.clone(), nav.clone(), nav.clone(), nav.clone(), nav.clone());
    let items_sig: Signal<Vec<ReleaseCardOut>> = feed.items.into();

    let sel = Selecting::new();
    let header_feed = ds::children(move || {
        view! {
            <div class="xg-head">
                <SavedQueries explore=explore_params api_params=api_params kind=save_kind label=save_label />
                <GridPlaybackBar items=sweep_of(items_sig) />
                <Button size=Size::Sm icon="check-circle" pressed=sel.on title="Pick releases to download together"
                    on_click=move |_| { if sel.on.get_untracked() { sel.stop() } else { sel.on.set(true) } }>"Select"</Button>
            </div>
            {move || sel.on.get().then(|| view! { <SelectionBar sel=sel items=items_sig /> })}
        }
    });
    let footer_feed = ds::children(move || {
        view! {
            {move || feed.error.get().map(|e| view! { <ErrorPanel message=e on_retry=Callback::new(move |_| feed.reset.run(())) /> })}
            {move || (feed.items.with(|i| i.is_empty()) && feed.loading.get()).then(|| view! { <div class="xg-skeletons">{(0..12).map(|_| view! { <div class="xc-skel skeleton"></div> }).collect_view()}</div> })}
            {move || (!feed.items.with(|i| i.is_empty()) && feed.loading.get()).then(|| view! { <Loading text="Loading more\u{2026}" /> })}
            {move || (!feed.has_more.get() && feed.error.get().is_none() && !feed.items.with(|i| i.is_empty())).then(|| view! { <div class="xg-end faint">"That\u{2019}s the end of the feed."</div> })}
        }
    });
    let render = Callback::new(move |(item, _w): (ReleaseCardOut, f64)| view! { <ReleaseCardView item=item sel=Some(sel) /> }.into_any());
    let empty_feed = move || {
        view! {
            <Show when=move || feed.error.get().is_none() && !feed.has_more.get()>
                <EmptyState icon="compass" title="Nothing in this feed" hint="Try another genre, a wider location, or a different sort." />
            </Show>
        }
    };

    view! {
        <div class="page xg-page">
            <ds::PageHeader title="Explore" subtitle="Search Bandcamp, browse by tag, listen, and download what you want" />
            <div class="xg-top">
                <form class="xg-search" role="search" on:submit=submit>
                    <ds::SearchInput value=input placeholder="Search Bandcamp for an artist, label or album\u{2026}" />
                </form>
                {move || if searching.get() {
                    let n = n1.clone();
                    view! { <div><Button variant=Variant::Ghost size=Size::Sm icon="arrow-left" on_click=move |_| n(vec![("q", None)])>"Back to browsing"</Button></div> }.into_any()
                } else {
                    let (a, b, c, d, e) = (n2.clone(), n3.clone(), n4.clone(), n5.clone(), n6.clone());
                    view! {
                        <div class="xg-facets">
                            <ds::Select options=genre_opts value=genre aria_label="Genre" class="xg-sel"
                                on_change=Callback::new(move |v: String| a(vec![("genre", Some(v)), ("tag", None)])) />
                            {move || (!subgenres.with(|s| s.is_empty())).then(|| {
                                let b = b.clone();
                                view! { <ds::Select options=sub_opts value=tag aria_label="Subgenre" class="xg-sel"
                                    on_change=Callback::new(move |v: String| b(vec![("tag", Some(v))])) /> }
                            })}
                            <ds::Select options=slice_opts value=slice aria_label="Sort" class="xg-sel"
                                on_change=Callback::new(move |v: String| c(vec![("slice", Some(v))])) />
                            {move || has_places.get().then(|| {
                                let d = d.clone();
                                view! { <ds::Select options=place_opts value=place aria_label="Location" class="xg-sel"
                                    on_change=Callback::new(move |v: String| d(vec![("place", Some(v))])) /> }
                            })}
                            {move || free_tag.get().map(|t| {
                                let e = e.clone();
                                view! { <button type="button" class="chip on" title="Remove tag filter" on:click=move |_| e(vec![("tag", None)])><Icon name="tag" />{t}<Icon name="x" /></button> }
                            })}
                            {move || feed.total.get().map(|t| view! { <span class="mono faint xg-total">{format!("{} releases", format_count(t))}</span> })}
                        </div>
                    }.into_any()
                }}
            </div>
            {move || if searching.get() {
                let qv = q.get();
                let body = if search.first_load() || (search.loading.get() && search.data.with(|d| d.is_none())) {
                    view! { <Loading text="Searching\u{2026}" /> }.into_any()
                } else if let Some(e) = search.failure() {
                    view! { <ErrorPanel message=qh::err_text(&e) on_retry=Callback::new(move |_| search.refetch()) /> }.into_any()
                } else {
                    let hits = search.data.with(|d| d.as_ref().map(|h| (**h).clone()).unwrap_or_default());
                    let (tag_items, tag_total) = tag_preview.data.with(|d| d.as_ref().map(|d| (d.items.clone(), d.total)).unwrap_or_default());
                    if hits.is_empty() && tag_items.is_empty() {
                        view! { <EmptyState icon="search" title=format!("Nothing on Bandcamp matches \u{201c}{qv}\u{201d}") hint="Check the spelling, or search for an artist, label or album title." /> }.into_any()
                    } else {
                        view! { <SearchResults hits=hits tag=qv tag_items=tag_items tag_total=tag_total /> }.into_any()
                    }
                };
                view! {
                    <div class="page-scroll">
                        <SavedQueries explore=explore_params api_params=api_params kind=save_kind label=save_label />
                        {body}
                    </div>
                }.into_any()
            } else {
                view! {
                    <VGrid items=items_sig epoch=feed.epoch has_more=feed.has_more loading=feed.loading on_more=feed.more render=render
                        header=header_feed.clone() footer=footer_feed.clone() empty=empty_feed.clone() min_card_w=150.0 meta_h=44.0 />
                }.into_any()
            }}
        </div>
    }
}

// ---------------------------------------------------------------------------
// Band (artist or label)
// ---------------------------------------------------------------------------

#[component]
pub fn ExploreBandPage() -> impl IntoView {
    let query = use_query_map();
    let url = Memo::new(move |_| query.get().get("url").unwrap_or_default());
    let band = qh::use_q::<BandOut>(move || {
        let u = url.get();
        (!u.is_empty()).then(|| QuerySpec::new(format!("/explore/band?url={}", enc(&u)), &[]))
    });
    qh::use_title(false, move || band.data.with(|d| d.as_ref().map(|b| b.name.clone()).unwrap_or_default()));
    view! {
        <div class="page xg-page">
            {move || {
                if url.get().is_empty() {
                    return view! { <EmptyState icon="compass" title="No artist or label selected" hint="Open one from a search or a release.">
                        <a class="btn btn-outline" href="/explore">"Back to Explore"</a></EmptyState> }.into_any();
                }
                if band.first_load() || (band.loading.get() && band.data.with(|d| d.is_none()) && band.error.get().is_none()) {
                    return view! { <div class="page-scroll"><Back /><Hero loading=true /></div> }.into_any();
                }
                if let Some(e) = band.failure() {
                    return view! { <div class="page-scroll"><Back /><ErrorPanel message=qh::err_text(&e) on_retry=Callback::new(move |_| band.refetch()) /></div> }.into_any();
                }
                match band.data.get() {
                    Some(b) => view! { <BandView b=b /> }.into_any(),
                    None => ().into_any(),
                }
            }}
        </div>
    }
}

#[component]
fn Back() -> impl IntoView {
    view! { <a class="xg-back" href="/explore"><Icon name="arrow-left" />"Explore"</a> }
}

#[component]
fn Hero(#[prop(optional)] loading: bool) -> impl IntoView {
    loading.then(|| view! {
        <div class="xg-hero" aria-busy="true">
            <div class="xg-hero-art skeleton"></div>
            <div class="grow xg-hero-sk"><div class="skeleton" style="height:28px;width:50%"></div><div class="skeleton" style="height:14px;width:30%"></div><div class="skeleton" style="height:14px;width:70%"></div></div>
        </div>
    })
}

#[component]
fn BandView(b: Arc<BandOut>) -> impl IntoView {
    let releases = RwSignal::new(b.releases.clone());
    let sel = Selecting::new();
    let under = (b.kind == "label").then(|| FileUnder { name: b.name.clone(), url: b.url.clone() });
    let missing = Signal::derive(move || releases.with(|r| r.iter().filter(|c| !c.in_library).count()));
    // `truncated`: Bandcamp served a partial discography, so what the page counted is a floor.
    let exact = !b.truncated;
    let items_sig: Signal<Vec<ReleaseCardOut>> = releases.into();
    let bandb = b.clone();
    let under2 = under.clone();
    let header = ds::children(move || {
        let b = bandb.clone();
        let under = under2.clone();
        view! {
            <Back />
            <div class="xg-hero">
                {match b.image_url.clone().filter(|i| !i.is_empty()) {
                    Some(src) => view! { <img class="xg-hero-face" src=src alt="" /> }.into_any(),
                    None => view! { <span class="xg-hero-face ph"><Icon name="users" /></span> }.into_any(),
                }}
                <div class="grow xg-hero-main">
                    <div class="xg-title-row">
                        <h1 class="xg-title">{b.name.clone()}</h1>
                        <span class="badge">{b.kind.clone()}</span>
                    </div>
                    <div class="mono faint xg-meta">
                        {format!("{} releases", b.releases.len())}
                        {(!b.roster.is_empty()).then(|| format!(" \u{b7} {} artists", b.roster.len()))}
                        {b.location.clone().map(|l| format!(" \u{b7} {l}"))}
                    </div>
                    {b.bio.clone().filter(|s| !s.is_empty()).map(|bio| view! { <Bio text=bio /> })}
                    <div class="xg-actions">
                        <FollowBandButton url=b.url.clone() name=b.name.clone() kind=b.kind.clone() />
                        <a class="btn btn-ghost btn-sm" href=b.url.clone() target="_blank" rel="noreferrer"><Icon name="external" />"Open on Bandcamp"</a>
                        <a class="btn btn-ghost btn-sm" href=format!("/harvest?url={}", logic::pct_encode(&b.url)) title="Sweep this page into the Harvest inbox"><Icon name="sparkles" />"Harvest"</a>
                    </div>
                </div>
                <CatalogDownloadButton url=b.url.clone() missing=missing exact=exact />
            </div>
            {b.truncated.then(|| view! { <Notice text="Bandcamp did not serve the full discography for this page \u{2014} some releases may be missing." /> })}
            {(!b.roster.is_empty()).then(|| view! {
                <section class="xg-section">
                    <SectionHeading icon="users" title="Roster" count=b.roster.len() />
                    <div class="xg-chips">
                        {b.roster.iter().map(|a| view! { <a class="chip" href=band_path(&a.url)>{a.name.clone()}</a> }).collect_view()}
                    </div>
                </section>
            })}
            <h2 class="xg-h xg-disco">
                <span class="display xg-h-l"><Icon name="disc" />"Discography"</span>
                <GridPlaybackBar items=sweep_of(items_sig) />
                {(!b.releases.is_empty()).then(|| view! {
                    <Button size=Size::Sm icon="check-circle" pressed=sel.on title="Pick releases to download together" on_click=move |_| { if sel.on.get_untracked() { sel.stop() } else { sel.on.set(true) } }>"Select"</Button>
                })}
            </h2>
            {move || sel.on.get().then({
                let under = under.clone();
                move || view! { <SelectionBar sel=sel items=items_sig under=under.clone() /> }
            })}
        }
    });
    let under3 = under.clone();
    let render = Callback::new(move |(item, _w): (ReleaseCardOut, f64)| view! { <ReleaseCardView item=item under=under3.clone() sel=Some(sel) /> }.into_any());
    view! {
        <VGrid items=items_sig epoch=Signal::derive(|| 0u64) has_more=Signal::derive(|| false) loading=Signal::derive(|| false) on_more=Callback::new(|_| ()) render=render
            header=header min_card_w=150.0 meta_h=44.0
            empty=move || view! { <EmptyState icon="disc" title="No releases on this page" /> } />
    }
}

/// A bio that clamps to a few lines until asked for the rest.
#[component]
fn Bio(text: String) -> impl IntoView {
    let open = RwSignal::new(false);
    let long = text.len() > 280;
    view! {
        <div class="xg-bio">
            <p class=move || if open.get() || !long { "muted" } else { "muted clamp" }>{text}</p>
            {long.then(|| view! { <button type="button" class="xg-link" on:click=move |_| open.update(|o| *o = !*o)>{move || if open.get() { "Show less" } else { "Read more" }}</button> })}
        </div>
    }
}

// ---------------------------------------------------------------------------
// Release
// ---------------------------------------------------------------------------

/// What the page shows and plays for one track row.
#[derive(Clone)]
struct TrackRow {
    num: Option<i64>,
    title: String,
    artist: String,
    duration_ms: Option<i64>,
    local: Option<TrackOut>,
    /// The queue entry (a library file or the stream); `None` when Bandcamp does not stream it.
    item: Option<QueueItem>,
}

fn build_rows(r: &ExploreReleaseOut, local: &[TrackOut]) -> Vec<TrackRow> {
    let nums: Vec<Option<i64>> = local.iter().map(|t| t.track_no).collect();
    r.tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let lt = local_match(t.track_num, i, &nums, r.tracks.len()).and_then(|j| local.get(j).cloned());
            let item = match &lt {
                Some(l) => Some(queue_item(l)),
                None => t.stream_url.as_deref().filter(|s| !s.is_empty()).map(|_| stream_item(r, t, i)),
            };
            TrackRow {
                num: t.track_num.or(Some(i as i64 + 1)),
                title: t.title.clone(),
                artist: t.artist.clone().filter(|a| !a.is_empty() && *a != r.artist_name).unwrap_or_default(),
                duration_ms: lt.as_ref().and_then(|l| l.duration_ms).or(t.duration_sec.map(|s| (s * 1000.0) as i64)),
                local: lt,
                item,
            }
        })
        .collect()
}

#[component]
pub fn ExploreReleasePage() -> impl IntoView {
    let query = use_query_map();
    let url = Memo::new(move |_| query.get().get("url").unwrap_or_default());
    let release = qh::use_q::<ExploreReleaseOut>(move || {
        let u = url.get();
        (!u.is_empty()).then(|| QuerySpec::new(format!("/explore/release?url={}", enc(&u)), &[]))
    });
    qh::use_title(false, move || release.data.with(|d| d.as_ref().map(|r| r.title.clone()).unwrap_or_default()));
    view! {
        <div class="page xg-page">
            {move || {
                if url.get().is_empty() {
                    return view! { <EmptyState icon="compass" title="No release selected" hint="Open one from a search, a feed or a band page.">
                        <a class="btn btn-outline" href="/explore">"Back to Explore"</a></EmptyState> }.into_any();
                }
                if release.first_load() || (release.loading.get() && release.data.with(|d| d.is_none()) && release.error.get().is_none()) {
                    return view! { <div class="page-scroll"><Back /><Hero loading=true /></div> }.into_any();
                }
                if let Some(e) = release.failure() {
                    return view! { <div class="page-scroll"><Back /><ErrorPanel message=qh::err_text(&e) on_retry=Callback::new(move |_| release.refetch()) /></div> }.into_any();
                }
                match release.data.get() {
                    Some(r) => view! { <ReleaseView r=r /> }.into_any(),
                    None => ().into_any(),
                }
            }}
        </div>
    }
}

#[component]
fn ReleaseView(r: Arc<ExploreReleaseOut>) -> impl IntoView {
    let player = use_player();
    // The shelf's own copy, when the page is badged in-library: those rows play the files on disk.
    let local_id = r.library_release_id;
    let local = qh::use_q::<TrackPage>(move || local_id.map(|id| QuerySpec::new(format!("/tracks?release_id={id}&sort=album&order=asc&limit=500"), &["track"])));
    let r2 = r.clone();
    let rows = Signal::derive(move || {
        let lt = local.data.with(|d| d.as_ref().map(|p| p.page.items.clone()).unwrap_or_default());
        build_rows(&r2, &lt)
    });
    let playable = Signal::derive(move || rows.with(|rs| rs.iter().filter_map(|r| r.item.clone()).collect::<Vec<_>>()));
    let play_from = move |start: usize| {
        let items = playable.get_untracked();
        if items.is_empty() {
            ds::toast_warn("Bandcamp streams nothing from this release");
            return;
        }
        let start_index = start.min(items.len() - 1);
        player.cmd(PlayerCommand::PlayQueue { items, start_index, source: None });
    };
    let current_id = Signal::derive(move || player.current_track_id());
    let download_state = RwSignal::new(0u8);
    let download = {
        let r = r.clone();
        move |_| {
            download_state.set(1);
            let body = bc_types::bandcamp::DownloadReleasesRequest { urls: vec![r.url.clone()], label: Some(format!("{} \u{2014} {}", r.artist_name, r.title)), ..Default::default() };
            spawn_local(async move {
                match api::post::<_, bc_types::jobs::JobOut>("/explore/download", &body).await {
                    Ok(_) => {
                        let _ = download_state.try_set(2);
                        ds::toast_ok("Queued for download");
                    }
                    Err(e) => {
                        let _ = download_state.try_set(0);
                        ds::toast_err(&e.message());
                    }
                }
            });
        }
    };
    let art = r.art_url.clone();
    let hero_style = art.as_ref().filter(|a| a.starts_with("http") || a.starts_with('/')).map(|a| format!("--hero-art:url('{}')", a.replace('\'', "%27")));
    let url_for_related = r.url.clone();
    let tags = r.tags.clone();
    let band_url = r.band_url.clone();
    let meta = format!(
        "{} \u{b7} {} tracks \u{b7} {}{}{}",
        r.release_date.clone().unwrap_or_else(|| "\u{2014}".into()),
        r.tracks.len(),
        price_label(&r),
        r.label_name.as_ref().map(|l| format!(" \u{b7} {l}")).unwrap_or_default(),
        if r.is_preorder { " \u{b7} pre-order" } else { "" }
    );
    view! {
        <div class="page-scroll xg-release">
            <Back />
            <div class="xg-hero rel" style=hero_style>
                <div class="xg-hero-cover"><Art src=art /></div>
                <div class="grow xg-hero-main">
                    <h1 class="xg-title">{r.title.clone()}</h1>
                    {match band_url.clone() {
                        Some(b) => view! { <a class="xg-artist" href=band_path(&b)>{r.artist_name.clone()}</a> }.into_any(),
                        None => view! { <div class="xg-artist">{r.artist_name.clone()}</div> }.into_any(),
                    }}
                    <div class="mono faint xg-meta">{meta}</div>
                    <div class="xg-actions">
                        <Button variant=Variant::Primary icon="play" disabled=Signal::derive(move || playable.with(|p| p.is_empty())) on_click=move |_| play_from(0)>"Play"</Button>
                        {if r.in_library {
                            view! { <span class="badge badge-ok"><Icon name="check" />"Already in your library"</span> }.into_any()
                        } else {
                            view! {
                                <Button icon=ds::dyn_icon(move || if download_state.get() == 2 { "check" } else { "download" })
                                    busy=Signal::derive(move || download_state.get() == 1) disabled=Signal::derive(move || download_state.get() == 2)
                                    on_click=download>{move || if download_state.get() == 2 { "Queued" } else { "Download" }}</Button>
                            }.into_any()
                        }}
                        {r.library_release_id.map(|id| view! { <a class="btn btn-ghost" href=format!("/albums/{id}") title="Open in library" aria-label="Open in library"><Icon name="disc" /><span class="xg-lbl">"Open in library"</span></a> })}
                        <a class="btn btn-ghost" href=r.url.clone() target="_blank" rel="noreferrer" title="Open on Bandcamp" aria-label="Open on Bandcamp"><Icon name="external" /><span class="xg-lbl">"Bandcamp"</span></a>
                    </div>
                    {(!r.is_free_download && r.is_purchasable && !r.in_library).then(|| view! {
                        <p class="faint xg-paid">"This release is paid. Downloading fetches only what Bandcamp offers; buy it there, or add your account cookie in Settings, to get the real files."</p>
                    })}
                </div>
            </div>
            {(!tags.is_empty()).then(|| view! {
                <div class="xg-chips tags">
                    {tags.iter().map(|t| view! { <a class="chip" href=tag_path(t)>{t.clone()}</a> }).collect_view()}
                </div>
            })}
            <section class="xg-section">
                <SectionHeading icon="list-music" title="Tracks" count=r.tracks.len() />
                {move || rows.with(|rs| rs.is_empty()).then(|| view! { <p class="faint">"Bandcamp lists no tracks for this release."</p> })}
                <ol class="tr-list">
                    {move || {
                        let items = playable.get();
                        rows.get().into_iter().map(|row| {
                            let pos = row.item.as_ref().and_then(|it| items.iter().position(|x| x.track_id == it.track_id));
                            let tid = row.item.as_ref().map(|i| i.track_id);
                            let on = Signal::derive(move || tid.is_some() && current_id.get() == tid);
                            view! {
                                <li class=move || if on.get() { "tr on" } else if pos.is_none() { "tr off" } else { "tr" }>
                                    <button type="button" class="tr-play" disabled=pos.is_none() aria-label=format!("Play {}", row.title)
                                        title=if pos.is_none() { "Bandcamp does not stream this track".to_string() } else { format!("Play {}", row.title) }
                                        on:click=move |_| if let Some(p) = pos { play_from(p) }>
                                        <span class="tr-n mono">{row.num.map(|n| n.to_string()).unwrap_or_default()}</span>
                                        <span class="tr-i"><Icon name=ds::dyn_icon(move || if on.get() { "pause" } else { "play" }) /></span>
                                    </button>
                                    <div class="grow">
                                        <div class="truncate tr-t">{row.title.clone()}</div>
                                        {(!row.artist.is_empty()).then(|| view! { <div class="truncate faint tr-a">{row.artist.clone()}</div> })}
                                    </div>
                                    {row.local.as_ref().map(|_| view! { <span class="badge badge-ok" title="Plays the file in your library"><Icon name="check" />"library"</span> })}
                                    <span class="mono faint tr-d">{format_duration_ms(row.duration_ms.map(|d| d as f64))}</span>
                                    {row.local.as_ref().map(|l| view! { <LoveButton track_id=l.id loved=l.loved /> })}
                                </li>
                            }
                        }).collect_view()
                    }}
                </ol>
            </section>
            <Supporters url=url_for_related.clone() />
            <div class="xg-rel-wrap">
                {band_url.clone().map(|b| view! { <BandDiscography url=b exclude=url_for_related.clone() /> })}
                <BandcampRelated url=url_for_related.clone() tags=tags.clone() include_band=false />
            </div>
            {r.about.clone().filter(|s| !s.is_empty()).map(|a| view! {
                <section class="xg-prose"><h2 class="display xg-h-l">"About"</h2><p class="muted">{a}</p></section>
            })}
            {r.credits.clone().filter(|s| !s.is_empty()).map(|a| view! {
                <section class="xg-prose"><h2 class="display xg-h-l">"Credits"</h2><p class="muted">{a}</p></section>
            })}
        </div>
    }
}
