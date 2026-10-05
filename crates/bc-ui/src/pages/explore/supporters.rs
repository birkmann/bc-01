//! "Supported by": who bought a record and what they wrote about it, and the quickest route
//! from a record you like to people who like it too. Picking an avatar opens their collection
//! and wishlist at a glance (one page fetch, nothing stored), scrollable a page at a time,
//! and one more press follows them on the Fans page.
use bc_types::bandcamp::{AddFanRequest, CollectorOut, CollectorsOut, FanOut, FanPeekOut, FanPeekPageOut, PeekItem, ReleaseCardOut};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;

use super::cards::{GridPlaybackBar, ReleaseGrid};
use crate::api;
use super::qh;
use crate::data::{self, QuerySpec};
use crate::ds::{self, Button, Icon, Size, Variant};
use crate::logic::format::format_count;
use crate::util::enc;

const FIRST: usize = 80;
const STEP: usize = 160;
const OPEN_KEY: &str = "bc:supporters:open:v1";
const PEEK_PAGE: usize = 60;

fn read_open() -> bool {
    crate::util::ls_get(OPEN_KEY).as_deref() != Some("0")
}

#[component]
fn Avatar(name: String, image: Option<String>) -> impl IntoView {
    match image.filter(|i| !i.is_empty()) {
        Some(src) => view! { <img src=src alt="" loading="lazy" class="sup-av-img" /> }.into_any(),
        None => view! { <span class="sup-av-ph">{name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()}</span> }.into_any(),
    }
}

#[component]
pub fn Supporters(#[prop(into)] url: String) -> impl IntoView {
    let open = RwSignal::new(read_open());
    let limit = RwSignal::new(FIRST);
    let peeking = RwSignal::new(None::<CollectorOut>);
    Effect::new(move |_| crate::util::ls_set(OPEN_KEY, if open.get() { "1" } else { "0" }));

    let u = url.clone();
    let collectors = qh::use_q::<CollectorsOut>(move || {
        open.get().then(|| {
            let l = limit.get();
            let spec_url = format!("/explore/collectors?url={}&limit={l}", enc(&u));
            QuerySpec::new(spec_url, &[])
        })
    });
    let supporters = Signal::derive(move || collectors.data.with(|d| d.as_ref().map(|c| c.supporters.clone()).unwrap_or_default()));
    let reviews = Signal::derive(move || collectors.data.with(|d| d.as_ref().map(|c| c.reviews.clone()).unwrap_or_default()));
    let more = Signal::derive(move || collectors.data.with(|d| d.as_ref().is_some_and(|c| c.more)));

    view! {
        <section class="sup card">
            <button type="button" class="sup-head" aria-expanded=move || open.get().to_string() on:click=move |_| open.update(|o| *o = !*o)>
                <Icon name=ds::dyn_icon(move || if open.get() { "chevron-down" } else { "chevron-right" }) />
                <Icon name="users" />
                <span class="display sup-title">"Supported by"</span>
                {move || collectors.data.with(|d| d.is_some()).then(|| view! {
                    <span class="mono faint sup-count">
                        {format!("{}{} fans", format_count(supporters.with(|s| s.len()) as i64), if more.get() { "+" } else { "" })}
                        {move || { let r = reviews.with(|r| r.len()); (r > 0).then(|| format!(" \u{b7} {} wrote something", format_count(r as i64))) }}
                    </span>
                })}
                {move || collectors.loading.get().then(|| view! { <span class="xg-spin"><Icon name="refresh" /></span> })}
                <span class="spacer"></span>
                <span class="faint sup-hint">{move || if open.get() { "Fold" } else { "Show who bought this" }}</span>
            </button>
            <Show when=move || open.get()>
                <div class="sup-body">
                    {move || collectors.error.get().map(|e| view! { <p class="sup-err">{e.message()}</p> })}
                    {move || (collectors.data.with(|d| d.is_some()) && supporters.with(|s| s.is_empty())).then(|| view! { <p class="faint">"Nobody has bought this on Bandcamp yet."</p> })}
                    <ul class="sup-reviews">
                        <For each=move || reviews.get() key=|f| f.username.clone() let:fan>
                            {
                                let (f1, f2) = (fan.clone(), fan.clone());
                                view! {
                                    <li>
                                        <button type="button" class="sup-av" title=format!("See what {} collects and wishes for", fan.name) on:click=move |_| peeking.set(Some(f1.clone()))>
                                            <Avatar name=fan.name.clone() image=fan.image_url.clone() />
                                        </button>
                                        <div class="sup-rv">
                                            <button type="button" class="sup-name" on:click=move |_| peeking.set(Some(f2.clone()))>{fan.name.clone()}</button>
                                            {fan.followed_id.map(|id| view! { <a class="sup-followed" href=format!("/fans/{id}")>"followed"</a> })}
                                            {fan.why.clone().map(|w| view! { <span class="muted">" "{w}</span> })}
                                            {fan.fav_track.clone().map(|t| view! { <span class="faint sup-fav">{format!("Favorite track: {t}")}</span> })}
                                        </div>
                                    </li>
                                }
                            }
                        </For>
                    </ul>
                    <div class="sup-avatars">
                        <For each=move || supporters.get() key=|f| f.username.clone() let:fan>
                            {
                                let f1 = fan.clone();
                                let uname = fan.username.clone();
                                let followed = fan.followed_id.is_some();
                                view! {
                                    <button type="button" aria-label=format!("Peek at {}", fan.name)
                                        class=move || format!("sup-av{}{}", if peeking.with(|p| p.as_ref().is_some_and(|p| p.username == uname)) { " on" } else { "" }, if followed { " followed" } else { "" })
                                        title=format!("{}{} \u{2014} see their collection and wishlist", fan.name, if followed { " \u{b7} followed" } else { "" })
                                        on:click=move |_| peeking.set(Some(f1.clone()))>
                                        <Avatar name=fan.name.clone() image=fan.image_url.clone() />
                                    </button>
                                }
                            }
                        </For>
                        {move || more.get().then(|| view! {
                            <Button size=Size::Sm icon="plus" busy=collectors.loading title=format!("Load {STEP} more buyers (a couple of polite requests to Bandcamp)")
                                on_click=move |_| limit.update(|l| *l += STEP)>"more"</Button>
                        })}
                    </div>
                    {move || peeking.get().map(|fan| view! { <FanPeekPanel fan=fan on_close=Callback::new(move |_| peeking.set(None)) /> })}
                </div>
            </Show>
        </section>
    }
}

fn peek_card(i: &PeekItem) -> ReleaseCardOut {
    ReleaseCardOut {
        url: i.url.clone(),
        title: i.title.clone(),
        artist_name: i.artist_name.clone(),
        item_type: i.item_type.clone(),
        art_url: i.art_url.clone(),
        release_date: None,
        is_free_download: false,
        in_library: i.in_library,
        blacklisted: false,
        library_release_id: i.library_release_id,
    }
}

/// One fan at a glance: their two lists, a tab each, with what the library already holds
/// marked, and the button that follows them properly.
#[component]
pub fn FanPeekPanel(fan: CollectorOut, on_close: Callback<()>) -> impl IntoView {
    let navigate = use_navigate();
    let which = RwSignal::new("collection".to_string());
    let fan_url = fan.url.clone();
    let peek = qh::use_q::<FanPeekOut>(move || Some(QuerySpec::new(format!("/fans/peek?url={}", enc(&fan_url)), &[])));
    let following = RwSignal::new(false);
    let followed_id = Signal::derive({
        let fid = fan.followed_id;
        move || peek.data.with(|d| d.as_ref().and_then(|p| p.followed_id)).or(fid)
    });
    let follow = {
        let url = fan.url.clone();
        move |_| {
            following.set(true);
            let (url, navigate) = (url.clone(), navigate.clone());
            spawn_local(async move {
                match api::post::<_, FanOut>("/fans", &AddFanRequest { url, walk: true }).await {
                    Ok(f) => {
                        data::invalidate_prefix("/fans");
                        navigate(&format!("/fans/{}", f.id), Default::default());
                    }
                    Err(e) => ds::toast_err(&e.message()),
                }
                let _ = following.try_set(false);
            });
        }
    };
    let (name, image) = (fan.name.clone(), fan.image_url.clone());
    let display = {
        let n = fan.name.clone();
        Signal::derive(move || peek.data.with(|d| d.as_ref().map(|p| p.display_name.clone())).filter(|d| !d.is_empty()).unwrap_or_else(|| n.clone()))
    };
    let counts = Signal::derive(move || peek.data.with(|d| d.as_ref().map(|p| (p.collection_count, p.wishlist_count))));
    view! {
        <div class="peek">
            <div class="peek-head">
                <span class="sup-av static"><Avatar name=name image=image /></span>
                <div class="grow">
                    <div class="peek-name">
                        {move || display.get()}
                        <a href=fan.url.clone() target="_blank" rel="noreferrer" title="Open their Bandcamp page" class="faint"><Icon name="external" /></a>
                    </div>
                    <div class="faint peek-user">
                        {move || match (peek.data.with(|d| d.as_ref().map(|p| p.username.clone())), peek.error.get()) {
                            (Some(u), _) => format!("@{u}"),
                            (None, Some(e)) => e.message(),
                            _ => "Reading their page\u{2026}".into(),
                        }}
                    </div>
                </div>
                {move || match followed_id.get() {
                    Some(id) => view! { <a class="btn btn-primary btn-sm" href=format!("/fans/{id}")><Icon name="users" />"Open in Fans"</a> }.into_any(),
                    None => view! {
                        <Button variant=Variant::Primary size=Size::Sm icon="plus" busy=following on_click=follow.clone()
                            title="Follow this fan: their collection and wishlist get walked in and appear on the Fans page.">"Follow & open"</Button>
                    }.into_any(),
                }}
                <Button variant=Variant::Ghost size=Size::Sm icon="x" title="Close" on_click=move |_| on_close.run(()) />
            </div>
            {move || counts.get().map(|(c, w)| view! {
                <div class="peek-tabs">
                    {[("collection", "Collection", c), ("wishlist", "Wishlist", w)].into_iter().map(|(id, label, n)| view! {
                        <button type="button" class="chip" aria-pressed=move || (which.get() == id).to_string() on:click=move |_| which.set(id.to_string())>
                            <Icon name=if id == "wishlist" { "heart" } else { "disc" } />
                            <span class="display">{label}</span>
                            <span class="mono faint">{format_count(n)}</span>
                        </button>
                    }).collect_view()}
                </div>
            })}
            {move || {
                let (c, w) = counts.get()?;
                let w_name = which.get();
                let total = if w_name == "collection" { c } else { w };
                let (fan, fid) = (fan.clone(), peek.data.with(|d| d.as_ref().and_then(|p| p.bc_fan_id)));
                Some(view! { <PeekList fan=fan fan_id=fid which=w_name total=total /> })
            }}
        </div>
    }
}

/// One of a fan's lists, newest first, as far down as the reader scrolls. It scrolls in its
/// own box so opening someone with three hundred records does not bury the rest of the page.
/// Every cover plays on its own, and Play all / Shuffle sweep what has been read so far,
/// the library's own copy standing in wherever the shelf already has the record.
#[component]
fn PeekList(fan: CollectorOut, fan_id: Option<i64>, which: String, total: i64) -> impl IntoView {
    let items = RwSignal::new(Vec::<ReleaseCardOut>::new());
    let cursor = RwSignal::new(None::<String>);
    let done = RwSignal::new(total == 0);
    let loading = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let box_ref = NodeRef::<leptos::html::Div>::new();
    let load = {
        let (url, which) = (fan.url.clone(), which.clone());
        move || {
            if loading.get_untracked() || done.get_untracked() {
                return;
            }
            loading.set(true);
            error.set(None);
            let mut q = format!("/fans/peek/items?url={}&which={}&count={PEEK_PAGE}", enc(&url), enc(&which));
            if let Some(c) = cursor.get_untracked() {
                q.push_str(&format!("&cursor={}", enc(&c)));
            }
            if let Some(f) = fan_id {
                q.push_str(&format!("&fan_id={f}"));
            }
            spawn_local(async move {
                match api::get::<FanPeekPageOut>(&q).await {
                    Ok(p) => {
                        // Deduped by URL: a page without a cursor re-serves the newest.
                        let _ = items.try_update(|v| {
                            for i in p.items.iter().map(peek_card) {
                                if !v.iter().any(|x| x.url == i.url) {
                                    v.push(i);
                                }
                            }
                        });
                        let _ = done.try_set(p.cursor.is_none() || !p.more && p.items.is_empty());
                        let _ = cursor.try_set(p.cursor);
                    }
                    Err(e) => {
                        let _ = error.try_set(Some(e.message()));
                    }
                }
                let _ = loading.try_set(false);
            });
        }
    };
    let load = std::sync::Arc::new(load);
    {
        let load = load.clone();
        Effect::new(move |_| {
            load();
        });
    }
    let on_scroll = {
        let load = load.clone();
        move |_| {
            if let Some(el) = box_ref.get_untracked() {
                if (el.scroll_height() - el.scroll_top() - el.client_height()) < 400 {
                    load();
                }
            }
        }
    };
    let load2 = load.clone();
    let sweep = Signal::derive(move || items.with(|v| v.iter().map(|c| (c.url.clone(), c.library_release_id)).collect::<Vec<_>>()));
    view! {
        {(total > 0).then(|| view! { <GridPlaybackBar items=sweep noun="records" /> })}
        <div class="peek-list" node_ref=box_ref on:scroll=on_scroll>
            {(total == 0).then(|| view! { <p class="faint">"Nothing on this list."</p> })}
            <ReleaseGrid items=items min=130 />
            <div class="peek-foot faint">
                {move || {
                    if let Some(e) = error.get() {
                        let load = load2.clone();
                        view! { <span class="sup-err">{e}</span><Button size=Size::Sm on_click=move |_| load()>"Retry"</Button> }.into_any()
                    } else if loading.get() {
                        view! { <span class="xg-spin"><Icon name="refresh" /></span>"Reading their list\u{2026}" }.into_any()
                    } else if !done.get() {
                        let load = load2.clone();
                        view! { <Button size=Size::Sm on_click=move |_| load()>"Load more"</Button> }.into_any()
                    } else {
                        view! { <span class="mono">{format!("{} of {} shown", format_count(items.with(|i| i.len()) as i64), format_count(total))}</span> }.into_any()
                    }
                }}
            </div>
        </div>
    }
}
