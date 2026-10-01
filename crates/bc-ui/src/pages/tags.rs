//! Tags: the library's genre signal as a cloud or a list, with favourite pins and search.
use bc_types::library::{FavoritesOut, LibraryStats, TagOut};
use leptos::prelude::*;

use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Button, EmptyState, ErrorPanel, Icon, PageHeader, SearchInput, Select, SelectOption, Skeleton, toast_err, use_debounced};
use crate::logic::format::format_count;
use crate::pages::albums::logic::{bar_width, cloud_step, tag_hue};
use crate::util::{enc, ls_get, ls_set};

const VIEW_KEY: &str = "bc:tags:view:v1";
const LIMIT: usize = 500;

fn norm(t: &str) -> String {
    t.trim().to_lowercase()
}

/// Heart toggle of one tag: optimistic, with the server value behind it.
#[component]
fn TagHeart(name: String, favs: RwSignal<Vec<String>>) -> impl IntoView {
    let key = norm(&name);
    let k2 = key.clone();
    let on = Memo::new(move |_| favs.with(|f| f.contains(&key)));
    let name2 = name.clone();
    view! {
        <button type="button" class="tg-heart" class:on=move || on.get() aria-pressed=move || on.get().to_string()
            aria-label=move || format!("{} {}", if on.get() { "Unpin" } else { "Pin" }, name2) title=move || if on.get() { "Unpin from favourites" } else { "Pin to favourites" }
            on:click={
                let name = name.clone();
                move |ev: leptos::ev::MouseEvent| {
                    ev.prevent_default();
                    ev.stop_propagation();
                    let want = !favs.with_untracked(|f| f.contains(&k2));
                    favs.update(|f| { f.retain(|x| *x != k2); if want { f.push(k2.clone()); } });
                    let (name, k) = (name.clone(), k2.clone());
                    leptos::task::spawn_local(async move {
                        let path = format!("/favorites/tag?name={}", enc(&name));
                        if let Err(e) = api::call(if want { "PUT" } else { "DELETE" }, &path).await {
                            favs.update(|f| { f.retain(|x| *x != k); if !want { f.push(k.clone()); } });
                            toast_err(&e.message());
                        }
                    });
                }
            }>
            <Icon name=move || if on.get() { "heart-fill" } else { "heart" } size=14 />
        </button>
    }
}

#[component]
pub fn TagsPage() -> impl IntoView {
    let search = RwSignal::new(String::new());
    let q = use_debounced(search, 120);
    let view_mode = RwSignal::new(ls_get(VIEW_KEY).filter(|v| v == "list").unwrap_or_else(|| "cloud".into()));
    Effect::new(move |_| ls_set(VIEW_KEY, &view_mode.get()));
    let sort = RwSignal::new("count".to_string());
    let only_favs = RwSignal::new(false);

    let tags = use_query::<Vec<TagOut>>(move || {
        let needle = q.get();
        let mut url = format!("/tags?limit={LIMIT}");
        if !needle.trim().is_empty() {
            url.push_str(&format!("&q={}", enc(needle.trim())));
        }
        Some(QuerySpec::new(url, &["tag", "track"]))
    });
    let stats = use_query::<LibraryStats>(|| Some(QuerySpec::new("/library/stats", &["track", "stats"])));
    let favs_q = use_query::<FavoritesOut>(|| Some(QuerySpec::new("/favorites", &["favorite"])));
    let favs = RwSignal::new(Vec::<String>::new());
    let fav_names = RwSignal::new(Vec::<TagOut>::new());
    let fav_data = favs_q.data;
    Effect::new(move |_| {
        if let Some(f) = fav_data.get() {
            favs.set(f.tags.iter().map(|t| norm(&t.name)).collect());
            fav_names.set(f.tags.clone());
        }
    });

    let tag_data = tags.data;
    let tag_err = tags.error;
    let tag_loading = tags.loading;
    let tags_q = StoredValue::new(tags);
    let total_tags = Memo::new(move |_| stats.data.get().map(|s| s.tags));

    let shown = Memo::new(move |_| -> Vec<TagOut> {
        let needle = norm(&q.get());
        let mut v: Vec<TagOut> = tag_data
            .get()
            .map(|d| d.iter().filter(|t| needle.is_empty() || norm(&t.name).contains(&needle)).cloned().collect())
            .unwrap_or_default();
        if only_favs.get() {
            v.retain(|t| favs.with(|f| f.contains(&norm(&t.name))));
        }
        if sort.get() == "name" {
            v.sort_by_key(|t| t.name.to_lowercase());
        }
        v
    });
    let range = Memo::new(move |_| {
        let d = tag_data.get();
        let counts: Vec<i64> = d.map(|d| d.iter().map(|t| t.track_count).collect()).unwrap_or_default();
        (counts.iter().copied().min().unwrap_or(1), counts.iter().copied().max().unwrap_or(1))
    });

    let subtitle = Signal::derive(move || {
        let n = shown.with(|s| s.len()) as i64;
        match total_tags.get() {
            Some(t) if !search.get().trim().is_empty() => Some(format!("{} of {} tags", format_count(n), format_count(t))),
            Some(t) => Some(format!("{} tags. Bandcamp's genre signal, the basis for recommendations", format_count(t))),
            None => None,
        }
    });
    let sort_opts = Signal::derive(|| vec![SelectOption::new("count", "Most tracks"), SelectOption::new("name", "A–Z")]);
    let favs_only_n = Memo::new(move |_| favs.with(|f| f.len()));

    view! {
        <div class="page">
            <PageHeader title="Tags" subtitle=subtitle
                actions=crate::ds::children(move || view! {
                    <a class="btn btn-outline" href="/explore"><Icon name="compass" /><span class="hide-sm">"Browse tags on Bandcamp"</span></a>
                }) />
            <div class="lib-filters tg-filters">
                <SearchInput value=search placeholder="Filter tags…" class="tg-search" />
                <button type="button" class="lib-pill" class:on=move || only_favs.get() aria-pressed=move || only_favs.get().to_string() on:click=move |_| only_favs.update(|v| *v = !*v)>
                    <Icon name="heart" size=12 />"Pinned"
                    <span class="mono faint">{move || favs_only_n.get()}</span>
                </button>
                <span class="spacer"></span>
                <div class="segmented" role="group" aria-label="View">
                    <button type="button" aria-pressed=move || (view_mode.get() == "cloud").to_string() on:click=move |_| view_mode.set("cloud".into()) title="Cloud"><Icon name="tag" size=13 />"Cloud"</button>
                    <button type="button" aria-pressed=move || (view_mode.get() == "list").to_string() on:click=move |_| view_mode.set("list".into()) title="List"><Icon name="rows" size=13 />"List"</button>
                </div>
                <Select options=sort_opts value=sort aria_label="Sort tags" />
            </div>
            <div class="page-scroll tg-scroll">
                {move || {
                    if tag_err.get().is_some() && tag_data.get().is_none() {
                        let t = tags_q;
                        return view! { <ErrorPanel message=Signal::derive(move || tag_err.get().map(|e| e.message()).unwrap_or_default()) on_retry=Callback::new(move |_| t.get_value().refetch()) /> }.into_any();
                    }
                    if tag_data.get().is_none() {
                        return view! { <div class="tg-cloud">{(0..40).map(|i| view! { <Skeleton width=format!("{}px", 70 + (i * 37) % 90) height="30px" class="tg-skel" /> }).collect_view()}</div> }.into_any();
                    }
                    let list = shown.get();
                    if list.is_empty() {
                        let msg = if only_favs.get() { "No pinned tags yet".to_string() } else if search.get().trim().is_empty() { "No tags yet".to_string() } else { format!("No tag matches \"{}\"", search.get().trim()) };
                        let hint = if only_favs.get() { "Heart a tag to pin it here and on Home." } else { "Tags come from the files and from Bandcamp." };
                        return view! { <EmptyState title=msg hint=hint icon="tag" /> }.into_any();
                    }
                    let (min, max) = range.get();
                    // pinned tags first in the cloud
                    let (pinned, rest): (Vec<TagOut>, Vec<TagOut>) = list.iter().cloned().partition(|t| favs.with(|f| f.contains(&norm(&t.name))));
                    let loading = tag_loading.get();
                    if view_mode.get() == "list" {
                        view! {
                            <div class="tg-list" class:dim=loading>
                                {list.into_iter().map(|t| {
                                    let w = bar_width(t.track_count, min, max);
                                    let name = t.name.clone();
                                    view! {
                                        <div class="tg-row">
                                            <a class="tg-row-link" href=format!("/tracks?tag={}", enc(&t.name)) aria-label=format!("Tracks tagged {}", t.name)></a>
                                            <div class="tg-row-top">
                                                <span class="tg-row-name truncate">{t.name.clone()}</span>
                                                <TagHeart name=name favs=favs />
                                                <span class="mono faint tg-row-count">{format_count(t.track_count)}</span>
                                            </div>
                                            <div class="tg-bar"><i style=format!("width:{w:.1}%")></i></div>
                                        </div>
                                    }
                                }).collect_view()}
                            </div>
                        }.into_any()
                    } else {
                        let chip = move |t: TagOut| {
                            let step = cloud_step(t.track_count, min, max);
                            let hue = tag_hue(&t.name);
                            let name = t.name.clone();
                            view! {
                                <div class=format!("tg-chip s{step}") style=format!("--hue:{hue}")>
                                    <a class="tg-chip-link" href=format!("/tracks?tag={}", enc(&t.name)) title=format!("{} tracks", format_count(t.track_count))>
                                        <span class="tg-chip-name">{t.name.clone()}</span>
                                        <span class="mono tg-chip-n">{format_count(t.track_count)}</span>
                                    </a>
                                    <TagHeart name=name favs=favs />
                                </div>
                            }
                        };
                        view! {
                            {(!pinned.is_empty() && !only_favs.get()).then(|| {
                                let chip = chip.clone();
                                view! {
                                    <h2 class="section-title tg-sec">"Pinned"</h2>
                                    <div class="tg-cloud pinned">{pinned.iter().cloned().map(chip).collect_view()}</div>
                                    <h2 class="section-title tg-sec">"All tags"</h2>
                                }
                            })}
                            <div class="tg-cloud" class:dim=loading>{if only_favs.get() { pinned.into_iter().map(chip.clone()).collect_view() } else { rest.into_iter().map(chip.clone()).collect_view() }}</div>
                        }.into_any()
                    }
                }}
                <p class="faint tg-foot">{move || {
                    let n = tag_data.get().map(|d| d.len()).unwrap_or(0);
                    (n >= LIMIT).then(|| format!("Showing the {LIMIT} most used. Search to find a rarer tag."))
                }}</p>
            </div>
        </div>
    }
}

#[allow(dead_code)]
fn _keep() {
    let _ = Button;
}
