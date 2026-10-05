//! "Supported by": who else bought this record on Bandcamp, straight off the
//! record's own page. A fan can be peeked at (collection / wishlist), played from right
//! there like the library's own grid, and followed. The peek panel is Explore's.
use bc_types::bandcamp::{CollectorOut, CollectorsOut};
use leptos::prelude::*;

use crate::data::{QuerySpec, use_query};
use crate::ds::Icon;
use crate::logic::format::format_count;
use crate::pages::explore::supporters::FanPeekPanel;
use crate::util::{enc, ls_get, ls_set};

const FIRST: i64 = 80;
const STEP: i64 = 160;
const OPEN_KEY: &str = "bc:supporters:open:v1";

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
                    {move || peeking.get().map(|f| view! { <FanPeekPanel fan=f on_close=Callback::new(move |_| peeking.set(None)) /> })}
                </div>
            })}
        </section>
    }
}
