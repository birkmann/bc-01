//! "Supported by": who else bought this record on Bandcamp, straight off the
//! record's own page. A fan can be peeked at (collection / wishlist) and followed.
use bc_types::bandcamp::{AddFanRequest, CollectorOut, CollectorsOut, FanOut, FanPeekOut, FanPeekPageOut, PeekItem};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;

use super::related::release_path;
use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Button, Icon, Variant, toast_err};
use crate::logic::format::format_count;
use crate::util::{enc, ls_get, ls_set};
use crate::widgets::common::Art;

const FIRST: i64 = 80;
const STEP: i64 = 160;
const OPEN_KEY: &str = "bc:supporters:open:v1";
const PEEK_PAGE: i64 = 60;

#[component]
fn Avatar(name: String, image: Option<String>) -> impl IntoView {
    match image {
        Some(i) => view! { <img src=i alt="" loading="lazy" class="lib-avatar-img" /> }.into_any(),
        None => view! { <span class="lib-avatar-ph">{name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()}</span> }.into_any(),
    }
}

#[component]
pub fn Supporters(url: String) -> impl IntoView {
    let open = RwSignal::new(ls_get(OPEN_KEY).as_deref() != Some("0"));
    Effect::new(move |_| ls_set(OPEN_KEY, if open.get() { "1" } else { "0" }));
    let limit = RwSignal::new(FIRST);
    let peeking = RwSignal::new(None::<CollectorOut>);
    let u = url.clone();
    let collectors = use_query::<CollectorsOut>(move || {
        open.get().then(|| QuerySpec::new(format!("/explore/collectors?url={}&limit={}", enc(&u), limit.get()), &["fan"]))
    });
    let data = move || collectors.data.get();
    view! {
        <section class="lib-supporters card">
            <button type="button" class="lib-sup-head" aria-expanded=move || open.get().to_string() on:click=move |_| open.update(|o| *o = !*o)>
                <Icon name=move || if open.get() { "chevron-down" } else { "chevron-right" } />
                <Icon name="users" />
                <span class="section-title">"Supported by"</span>
                {move || data().map(|d| view! {
                    <span class="mono faint lib-sup-count">
                        {format!("{}{} fans", format_count(d.supporters.len() as i64), if d.more { "+" } else { "" })}
                        {(!d.reviews.is_empty()).then(|| format!(" · {} wrote something", format_count(d.reviews.len() as i64)))}
                    </span>
                })}
                <span class="spacer"></span>
                <span class="faint lib-sup-fold">{move || if open.get() { "Fold" } else { "Show who bought this" }}</span>
            </button>
            {move || open.get().then(|| view! {
                <div class="lib-sup-body">
                    {move || collectors.error.get().map(|e| if e.status == 404 { view! { <p class="faint">"Supporters are not available from this server."</p> }.into_any() } else { view! { <p class="danger-text">{e.message()}</p> }.into_any() })}
                    {move || data().filter(|d| d.supporters.is_empty()).map(|_| view! { <p class="faint">"Nobody has bought this on Bandcamp yet."</p> })}
                    {move || data().map(|d| {
                        let reviews = d.reviews.clone();
                        let supporters = d.supporters.clone();
                        let more = d.more;
                        view! {
                            {(!reviews.is_empty()).then(|| view! {
                                <ul class="lib-reviews">
                                    {reviews.into_iter().map(|f| {
                                        let (f1, f2) = (f.clone(), f.clone());
                                        view! {
                                            <li>
                                                <button type="button" class="lib-avatar" title=format!("See what {} collects and wishes for", f.name) on:click=move |_| peeking.set(Some(f1.clone()))>
                                                    <Avatar name=f.name.clone() image=f.image_url.clone() />
                                                </button>
                                                <div class="grow">
                                                    <button type="button" class="lib-link strong" on:click=move |_| peeking.set(Some(f2.clone()))>{f.name.clone()}</button>
                                                    {f.followed_id.map(|id| view! { <a class="lib-badge compact" href=format!("/fans/{id}")>"followed"</a> })}
                                                    {f.why.clone().map(|w| view! { <span class="muted">{format!(" {w}")}</span> })}
                                                    {f.fav_track.clone().map(|t| view! { <span class="faint lib-fav">{format!("Favorite track: {t}")}</span> })}
                                                </div>
                                            </li>
                                        }
                                    }).collect_view()}
                                </ul>
                            })}
                            {(!supporters.is_empty()).then(|| view! {
                                <div class="lib-avatars">
                                    {supporters.into_iter().map(|f| {
                                        let f1 = f.clone();
                                        let name = f.name.clone();
                                        let sel = { let n = f.username.clone(); move || peeking.with(|p| p.as_ref().map(|p| p.username == n).unwrap_or(false)) };
                                        view! {
                                            <button type="button" class="lib-avatar" class:sel=sel class:followed=f.followed_id.is_some()
                                                title=format!("{}{}: see their collection and wishlist", f.name, if f.followed_id.is_some() { " (followed)" } else { "" })
                                                aria-label=format!("Peek at {name}") on:click=move |_| peeking.set(Some(f1.clone()))>
                                                <Avatar name=f.name.clone() image=f.image_url.clone() />
                                            </button>
                                        }
                                    }).collect_view()}
                                    {more.then(|| view! {
                                        <button type="button" class="lib-pill" disabled=move || collectors.loading.get() title="Load more buyers (a couple of polite requests to Bandcamp)"
                                            on:click=move |_| limit.update(|l| *l += STEP)>
                                            <Icon name="plus" size=11 />"more"
                                        </button>
                                    })}
                                </div>
                            })}
                        }
                    })}
                    {move || peeking.get().map(|f| view! { <FanPeek fan=f on_close=Callback::new(move |_| peeking.set(None)) /> })}
                </div>
            })}
        </section>
    }
}

#[component]
fn FanPeek(fan: CollectorOut, on_close: Callback<()>) -> impl IntoView {
    let navigate = use_navigate();
    let which = RwSignal::new("collection".to_string());
    let furl = fan.url.clone();
    let peek = use_query::<FanPeekOut>(move || Some(QuerySpec::new(format!("/fans/peek?url={}", enc(&furl)), &[])));
    let busy = RwSignal::new(false);
    let followed = Memo::new({
        let base = fan.followed_id;
        move |_| peek.data.get().and_then(|p| p.followed_id).or(base)
    });
    let follow = {
        let url = fan.url.clone();
        move || {
            busy.set(true);
            let (url, nav) = (url.clone(), navigate.clone());
            spawn_local(async move {
                match api::post::<_, FanOut>("/fans", &AddFanRequest { url, walk: true }).await {
                    Ok(f) => {
                        crate::data::invalidate_entity("fan", &[]);
                        nav(&format!("/fans/{}", f.id), Default::default());
                    }
                    Err(e) => toast_err(&e.message()),
                }
                busy.set(false);
            });
        }
    };
    let follow = std::sync::Arc::new(follow);
    let name = fan.name.clone();
    let img = fan.image_url.clone();
    let page_url = fan.url.clone();
    view! {
        <div class="lib-peek">
            <div class="row lib-peek-head">
                <span class="lib-avatar big"><Avatar name=name.clone() image=img /></span>
                <div class="grow">
                    <div class="lib-peek-name">
                        {move || peek.data.get().map(|p| p.display_name.clone()).unwrap_or_else(|| name.clone())}
                        <a href=page_url.clone() target="_blank" rel="noreferrer" class="faint" title="Open their Bandcamp page"><Icon name="external" size=12 /></a>
                    </div>
                    <div class="faint lib-peek-sub">
                        {move || match (peek.data.get(), peek.error.get()) {
                            (Some(p), _) => format!("@{}", p.username),
                            (None, Some(e)) => e.message(),
                            _ => "Reading their page…".into(),
                        }}
                    </div>
                </div>
                {move || match followed.get() {
                    Some(id) => view! { <a class="btn btn-primary btn-sm" href=format!("/fans/{id}")><Icon name="users" size=12 />"Open in Fans"</a> }.into_any(),
                    None => { let f = follow.clone(); view! {
                        <Button variant=Variant::Primary icon="plus" busy=busy title="Follow this fan: their collection and wishlist get walked in and appear on the Fans page." on_click=move |_| f()>"Follow & open"</Button>
                    }.into_any() }
                }}
                <Button variant=Variant::Ghost icon="x" title="Close" on_click=move |_| on_close.run(()) />
            </div>
            {move || peek.data.get().map(|p| {
                let (cc, wc) = (p.collection_count, p.wishlist_count);
                let (url, id) = (fan.url.clone(), p.bc_fan_id);
                view! {
                    <div class="lib-peek-tabs">
                        <button type="button" class="lib-pill" class:on=move || which.get() == "collection" on:click=move |_| which.set("collection".into())><Icon name="disc" size=11 />"Collection "<span class="mono">{format_count(cc)}</span></button>
                        <button type="button" class="lib-pill" class:on=move || which.get() == "wishlist" on:click=move |_| which.set("wishlist".into())><Icon name="heart" size=11 />"Wishlist "<span class="mono">{format_count(wc)}</span></button>
                    </div>
                    {move || {
                        let w = which.get();
                        let total = if w == "collection" { cc } else { wc };
                        view! { <PeekList url=url.clone() fan_id=id which=w total=total /> }
                    }}
                }
            })}
        </div>
    }
}

#[component]
fn PeekList(url: String, fan_id: Option<i64>, which: String, total: i64) -> impl IntoView {
    let items = RwSignal::new(Vec::<PeekItem>::new());
    let cursor = RwSignal::new(None::<String>);
    let more = RwSignal::new(total > 0);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let load = {
        let (url, which) = (url.clone(), which.clone());
        move || {
            if busy.get_untracked() || !more.get_untracked() {
                return;
            }
            busy.set(true);
            let mut q = format!("/fans/peek/items?url={}&which={}&count={PEEK_PAGE}", enc(&url), which);
            if let Some(c) = cursor.get_untracked() {
                q.push_str(&format!("&cursor={}", enc(&c)));
            }
            if let Some(f) = fan_id {
                q.push_str(&format!("&fan_id={f}"));
            }
            spawn_local(async move {
                match api::get::<FanPeekPageOut>(&q).await {
                    Ok(p) => {
                        items.update(|v| {
                            for i in p.items {
                                if !v.iter().any(|x| x.url == i.url) {
                                    v.push(i);
                                }
                            }
                        });
                        more.set(p.more && p.cursor.is_some());
                        cursor.set(p.cursor);
                    }
                    Err(e) => {
                        error.set(Some(e.message()));
                        more.set(false);
                    }
                }
                busy.set(false);
            });
        }
    };
    let load = std::sync::Arc::new(load);
    {
        let l = load.clone();
        Effect::new(move |_| l());
    }
    view! {
        {(total == 0).then(|| view! { <p class="faint">"Nothing on this list."</p> })}
        {move || error.get().map(|e| view! { <p class="danger-text">{e}</p> })}
        <div class="lib-peek-grid">
            {move || items.get().into_iter().map(|i| view! {
                <a class="lib-peek-item" href=release_path(&i.url) title=format!("{} - {}", i.artist_name, i.title)>
                    <Art src=i.art_url.clone() class="alb-art" />
                    <span class="truncate lib-peek-title">{i.title.clone()}</span>
                    <span class="truncate faint lib-peek-artist">{i.artist_name.clone()}{i.in_library.then_some(" · owned")}</span>
                </a>
            }).collect_view()}
        </div>
        {move || more.get().then(|| { let l = load.clone(); view! { <Button size=crate::ds::Size::Sm busy=busy on_click=move |_| l()>"Load more"</Button> } })}
    }
}
