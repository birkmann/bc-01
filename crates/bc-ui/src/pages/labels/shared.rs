//! Pieces both people pages use: artist/label kind, favourites, the art collage, the status
//! notice bar and a few small helpers. (Written locally on purpose: no dependency on other pages.)
use std::collections::HashSet;

use bc_types::library::FavoritesOut;
use leptos::prelude::*;

use super::logic::Tone;
use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Icon, toast_err};

/// What a page is about: an artist or a label.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Artist,
    Label,
}

impl Kind {
    pub fn noun(self) -> &'static str {
        match self {
            Kind::Artist => "artist",
            Kind::Label => "label",
        }
    }
    /// API collection (`/artists/{id}`).
    pub fn path(self) -> &'static str {
        match self {
            Kind::Artist => "artists",
            Kind::Label => "labels",
        }
    }
    pub fn icon(self) -> &'static str {
        match self {
            Kind::Artist => "user",
            Kind::Label => "folder",
        }
    }
}

// ---- favourites -----------------------------------------------------------------------------

/// The favourite artists/labels as id sets, with optimistic toggling.
#[derive(Clone, Copy)]
pub struct Favs {
    pub artists: RwSignal<HashSet<i64>>,
    pub labels: RwSignal<HashSet<i64>>,
}

pub fn use_favs() -> Favs {
    let q = use_query::<FavoritesOut>(|| Some(QuerySpec::keyed("favorites", "/favorites", &["artist", "label", "favorite"])));
    let favs = Favs { artists: RwSignal::new(HashSet::new()), labels: RwSignal::new(HashSet::new()) };
    Effect::new(move |_| {
        if let Some(d) = q.data.get() {
            favs.artists.set(d.artists.iter().map(|a| a.id).collect());
            favs.labels.set(d.labels.iter().map(|l| l.id).collect());
        }
    });
    favs
}

impl Favs {
    fn set(&self, kind: Kind) -> RwSignal<HashSet<i64>> {
        match kind {
            Kind::Artist => self.artists,
            Kind::Label => self.labels,
        }
    }

    pub fn is(&self, kind: Kind, id: i64) -> bool {
        self.set(kind).with(|s| s.contains(&id))
    }

    /// Set the favourite flag (optimistic; reverts with a toast when the server refuses).
    pub fn put(&self, kind: Kind, id: i64, on: bool) {
        let set = self.set(kind);
        set.update(|s| {
            if on {
                s.insert(id);
            } else {
                s.remove(&id);
            }
        });
        leptos::task::spawn_local(async move {
            let r = api::call(if on { "PUT" } else { "DELETE" }, &format!("/favorites/{}/{id}", kind.noun())).await;
            if let Err(e) = r {
                toast_err(&e.message());
                let _ = set.try_update(|s| {
                    if on {
                        s.remove(&id);
                    } else {
                        s.insert(id);
                    }
                });
            }
        });
    }

    pub fn toggle(&self, kind: Kind, id: i64) {
        let on = !self.is(kind, id);
        self.put(kind, id, on);
    }
}

/// Heart toggle. `overlay` draws it as a round chip on top of artwork.
#[component]
pub fn FavButton(kind: Kind, id: i64, favs: Favs, #[prop(optional)] overlay: bool, #[prop(optional, into)] name: String) -> impl IntoView {
    let on = move || favs.is(kind, id);
    let cls = move || {
        let base = if overlay { "pp-chipbtn pp-fav" } else { "btn btn-outline btn-icon" };
        if on() { format!("{base} is-on") } else { base.to_string() }
    };
    view! {
        <button type="button" class=cls aria-pressed=move || on().to_string()
            title=move || if on() { "Remove from favourites" } else { "Add to favourites" }
            aria-label=move || format!("{} {}", if on() { "Unfavourite" } else { "Favourite" }, name)
            on:click=move |ev| { ev.stop_propagation(); ev.prevent_default(); favs.toggle(kind, id); }>
            <Icon name=move || if on() { "heart-fill".to_string() } else { "heart".to_string() } />
        </button>
    }
}

// ---- art collage ------------------------------------------------------------------------------

/// The folder face of a label: one cover fills the square, two split it, three give the first
/// the whole left half, four tile. Never padded with placeholder discs.
#[component]
pub fn Collage(urls: Vec<String>, #[prop(optional, into)] class: String) -> impl IntoView {
    let urls: Vec<String> = urls.into_iter().take(4).collect();
    let n = urls.len();
    view! {
        <div class=format!("pp-collage {class}") data-n=n.to_string()>
            {if n == 0 {
                view! { <div class="pp-collage-ph"><Icon name="folder" /></div> }.into_any()
            } else {
                urls.into_iter().map(|u| view! { <img src=u alt="" loading="lazy" decoding="async" on:error=hide_broken /> }).collect_view().into_any()
            }}
        </div>
    }
}

/// A cover that failed to load is hidden instead of showing the browser's broken-image glyph.
pub fn hide_broken(ev: web_sys::ErrorEvent) {
    use wasm_bindgen::JsCast;
    if let Some(img) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok()) {
        let _ = img.style().set_property("visibility", "hidden");
    }
}

/// Round/square artwork with a fade-in and an icon placeholder (single image).
#[component]
pub fn Cover(#[prop(into)] src: MaybeProp<String>, #[prop(optional, into)] class: String, #[prop(default = "disc")] icon: &'static str) -> impl IntoView {
    let loaded = RwSignal::new(false);
    view! {
        <div class=format!("pp-cover {class}")>
            <div class="pp-cover-ph"><Icon name=icon /></div>
            {move || src.get().filter(|s| !s.is_empty()).map(|s| view! {
                <img src=s alt="" loading="lazy" decoding="async" class=move || if loaded.get() { "loaded" } else { "" } on:load=move |_| loaded.set(true) on:error=hide_broken />
            })}
        </div>
    }
}

// ---- notice bar ---------------------------------------------------------------------------------

/// A report line above a page ("found 12 on Bandcamp ...") with the actions it can offer.
#[derive(Clone, PartialEq, Debug)]
pub struct Notice {
    pub tone: Tone,
    pub text: String,
    /// Inbox items awaiting download: offers a "Download n" action.
    pub download_ids: Vec<i64>,
    pub to_harvest: bool,
    pub to_downloads: bool,
    /// Locate could not prove a page: a link to the tab that lists candidates.
    pub pick_href: Option<String>,
}

impl Notice {
    pub fn ok(text: impl Into<String>) -> Self {
        Self { tone: Tone::Ok, text: text.into(), download_ids: vec![], to_harvest: false, to_downloads: false, pick_href: None }
    }
    pub fn err(text: impl Into<String>) -> Self {
        Self { tone: Tone::Err, ..Self::ok(text) }
    }
}

#[component]
pub fn NoticeBar(notice: RwSignal<Option<Notice>>, #[prop(into)] busy: Signal<bool>, on_download: Callback<Vec<i64>>) -> impl IntoView {
    view! {
        {move || notice.get().map(|n| {
            let cls = if n.tone == Tone::Err { "pp-notice err" } else { "pp-notice" };
            let ids = n.download_ids.clone();
            let n_ids = ids.len();
            view! {
                <div class=cls role=if n.tone == Tone::Err { "alert" } else { "status" }>
                    {move || busy.get().then(|| view! { <span class="pp-spin" aria-hidden="true"></span> })}
                    <span class="pp-notice-text">{n.text.clone()}</span>
                    {(n_ids > 0).then(|| view! {
                        <button type="button" class="btn btn-primary btn-sm" disabled=move || busy.get() on:click=move |_| on_download.run(ids.clone())>
                            <Icon name="download" />{format!("Download {}", crate::logic::format::format_count(n_ids as i64))}
                        </button>
                    })}
                    {n.pick_href.clone().map(|h| view! { <a class="pp-link" href=h>"Choose the page"</a> })}
                    {n.to_harvest.then(|| view! { <a class="pp-link" href="/harvest">"Review in Harvest"</a> })}
                    {n.to_downloads.then(|| view! { <a class="pp-link" href="/downloads">"Open Downloads"</a> })}
                    <button type="button" class="btn btn-ghost btn-sm btn-icon" aria-label="Dismiss" on:click=move |_| notice.set(None)><Icon name="x" /></button>
                </div>
            }
        })}
    }
}

/// Plain progress/report banner for sweeps (tone + text + optional stop and dismiss).
#[component]
pub fn StatusBar(
    #[prop(into)] line: Signal<Option<(Tone, String)>>,
    #[prop(into)] running: Signal<bool>,
    #[prop(optional, into)] on_stop: Option<Callback<()>>,
    #[prop(optional, into)] on_dismiss: Option<Callback<()>>,
    #[prop(optional, into)] links: Option<ChildrenFn>,
) -> impl IntoView {
    view! {
        {move || line.get().map(|(tone, text)| {
            let cls = if tone == Tone::Err { "pp-notice err" } else { "pp-notice" };
            view! {
                <div class=cls role=if tone == Tone::Err { "alert" } else { "status" }>
                    {move || running.get().then(|| view! { <span class="pp-spin" aria-hidden="true"></span> })}
                    <span class="pp-notice-text">{text}</span>
                    {move || (running.get()).then(|| on_stop.map(|s| view! { <button type="button" class="btn btn-outline btn-sm" on:click=move |_| s.run(())>"Stop"</button> }))}
                    {links.as_ref().map(|l| l())}
                    {move || (!running.get()).then(|| on_dismiss.map(|d| view! {
                        <button type="button" class="btn btn-ghost btn-sm btn-icon" aria-label="Dismiss" on:click=move |_| d.run(())><Icon name="x" /></button>
                    }))}
                </div>
            }
        })}
    }
}

/// `window.open` in a new tab (exports, Bandcamp).
pub fn open_tab(url: &str) {
    let _ = crate::util::window().open_with_url_and_target_and_features(url, "_blank", "noopener");
}

// ---- release card ---------------------------------------------------------------------------------

use bc_types::library::ReleaseOut;
use bc_types::player::{PlayerCommand, QueueSource};

use crate::player::use_player;
use crate::widgets::common::artist_link;

/// A library release in a grid: cover, play on hover, title, byline; links to the album page.
#[component]
pub fn ReleaseCard(r: ReleaseOut, #[prop(into)] listing: Signal<serde_json::Value>, #[prop(optional)] show_artist: bool) -> impl IntoView {
    let player = use_player();
    let id = r.id;
    let now = move || player.state.with(|s| s.current.as_ref().and_then(|c| c.release_id) == Some(id));
    let art = r.art_url.as_ref().map(|u| super::logic::thumb(u));
    let artist = r.artist.as_ref().filter(|_| show_artist).map(|a| view! { {artist_link(Some(a))}" \u{b7} " });
    let mut sub: Vec<String> = vec![];
    if let Some(y) = r.year {
        sub.push(y.to_string());
    }
    sub.push(super::logic::count_of(r.track_count, "track"));
    let partial = r.expected_track_count.filter(|e| *e > r.track_count);
    view! {
        <div class="pp-card pp-release" class:cur=now>
            <div class="pp-card-art">
                <Cover src=art />
                {partial.map(|e| view! { <div class="pp-card-badges"><span class="badge badge-warn" title="Part of this album is missing">{format!("{}/{}", r.track_count, e)}</span></div> })}
                <button type="button" class="pp-chipbtn pp-chipbtn-primary pp-card-act pp-card-play" aria-label=format!("Play {}", r.title)
                    on:click=move |ev| {
                        ev.stop_propagation(); ev.prevent_default();
                        player.cmd(PlayerCommand::StartSource { source: QueueSource::Release { release_id: id, listing: listing.get_untracked() }, shuffle: false });
                    }>
                    <Icon name="play" />
                </button>
            </div>
            <div class="pp-card-meta">
                <div class="pp-card-title truncate" title=r.title.clone()>{r.title.clone()}</div>
                <div class="pp-card-sub truncate">{artist}{sub.join(" \u{b7} ")}</div>
            </div>
            <a class="pp-card-link" href=format!("/albums/{id}") aria-label=r.title.clone()></a>
        </div>
    }
}

// ---- Copy query handle -----------------------------------------------------------------------------

use std::sync::Arc;

use crate::api::ApiErr;
use crate::data::{Query, QuerySpec as Spec};

/// `data::Query` is only `Copy` when its payload is; this wraps it so any payload can be captured
/// by `move` closures freely.
pub struct Q<T: Send + Sync + 'static> {
    pub data: RwSignal<Option<Arc<T>>>,
    pub error: RwSignal<Option<ApiErr>>,
    pub loading: RwSignal<bool>,
    inner: StoredValue<Query<T>>,
}

impl<T: Send + Sync + 'static> Clone for Q<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: Send + Sync + 'static> Copy for Q<T> {}

impl<T: Send + Sync + 'static> Q<T> {
    pub fn refetch(&self) {
        self.inner.with_value(|q| q.refetch());
    }
}

pub fn use_q<T>(spec: impl Fn() -> Option<Spec> + Send + Sync + 'static) -> Q<T>
where
    T: serde::de::DeserializeOwned + Send + Sync + 'static,
{
    let q = use_query::<T>(spec);
    Q { data: q.data, error: q.error, loading: q.loading, inner: StoredValue::new(q) }
}
