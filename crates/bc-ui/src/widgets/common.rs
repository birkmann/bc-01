//! Shared bits for pages: artwork with fade-in, love button, artist/album/label links,
//! TrackOut -> QueueItem, play helpers.
use bc_types::library::{ArtistRef, ReleaseRef, TrackOut};
use bc_types::player::{ItemOrigin, PlayerCommand, QueueItem, QueueSource};
use leptos::prelude::*;

use crate::ds::Icon;
use crate::player::use_player;

/// Library track -> queue item (the session fills in path/analysis by id).
pub fn queue_item(t: &TrackOut) -> QueueItem {
    QueueItem {
        track_id: t.id,
        title: t.title.clone(),
        artist: t.artist.as_ref().map(|a| a.name.clone()),
        artist_id: t.artist.as_ref().map(|a| a.id),
        album: t.release.as_ref().map(|r| r.title.clone()),
        release_id: t.release.as_ref().map(|r| r.id),
        track_no: t.track_no.map(|n| n as i32),
        duration_ms: t.duration_ms,
        bpm: t.bpm,
        camelot: t.camelot.clone(),
        energy: t.energy,
        tags: t.tags.clone(),
        loved: t.loved,
        art_url: t.art_url.clone(),
        origin: ItemOrigin::Library,
        item_id: t.item_id,
        is_snippet: t.is_snippet,
        ..Default::default()
    }
}

/// Start playback of `tracks` at `start`.
pub fn play_tracks(tracks: &[TrackOut], start: usize, source: Option<QueueSource>) {
    let player = use_player();
    let items: Vec<QueueItem> = tracks.iter().map(queue_item).collect();
    player.cmd(PlayerCommand::PlayQueue { items, start_index: start, source });
}

pub fn enqueue_tracks(tracks: &[TrackOut]) {
    let player = use_player();
    player.cmd(PlayerCommand::AddToQueue { items: tracks.iter().map(queue_item).collect() });
}

pub fn play_next_tracks(tracks: &[TrackOut]) {
    let player = use_player();
    player.cmd(PlayerCommand::PlayNext { items: tracks.iter().map(queue_item).collect() });
}

/// Square artwork: placeholder icon, then a fade-in once the image has loaded (no layout shift).
#[component]
pub fn Art(
    #[prop(into)] src: MaybeProp<String>,
    #[prop(optional, into)] size: Option<f64>,
    #[prop(optional, into)] class: String,
    #[prop(optional, into)] alt: String,
) -> impl IntoView {
    let loaded = RwSignal::new(false);
    let style = size.map(|s| format!("width:{s}px;height:{s}px;flex:none"));
    view! {
        <div class=format!("art {class}") style=style>
            <div class="ph"><Icon name="disc" /></div>
            {move || src.get().filter(|s| !s.is_empty()).map(|s| view! {
                <img src=s alt=alt.clone() loading="lazy" decoding="async"
                    class=move || if loaded.get() { "loaded" } else { "" }
                    on:load=move |_| loaded.set(true) />
            })}
        </div>
    }
}

/// A name that leads to its library page (artist, album, label). Rows and cards around it
/// act on click themselves (play, select, open the card), so the click stops here: it only
/// navigates. It also sits above a card's stretched link and never starts a native drag.
#[component]
pub fn EntityLink(#[prop(into)] href: String, #[prop(optional, into)] class: String, #[prop(optional, into)] title: Option<String>, children: Children) -> impl IntoView {
    view! {
        <a class=format!("ent-link {class}") href=href title=title draggable="false"
            on:click=|ev: leptos::ev::MouseEvent| ev.stop_propagation()>{children()}</a>
    }
}

pub fn artist_href(id: i64) -> String {
    format!("/artists/{id}")
}

pub fn album_href(id: i64) -> String {
    format!("/albums/{id}")
}

pub fn label_href(id: i64) -> String {
    format!("/labels/{id}")
}

/// The artist's name linking to their page; empty when unknown.
pub fn artist_link(a: Option<&ArtistRef>) -> impl IntoView + use<> {
    a.map(|a| {
        let name = a.name.clone();
        view! { <EntityLink href=artist_href(a.id)>{name}</EntityLink> }
    })
}

/// The release title linking to the album page; empty when the track has none.
pub fn album_link(r: Option<&ReleaseRef>) -> impl IntoView + use<> {
    r.map(|r| {
        let title = r.title.clone();
        view! { <EntityLink href=album_href(r.id)>{title}</EntityLink> }
    })
}

/// A track title linking to its album; plain text when the track has none.
pub fn title_link(title: &str, r: Option<&ReleaseRef>) -> impl IntoView + use<> {
    let title = title.to_string();
    match r {
        Some(r) => view! { <EntityLink href=album_href(r.id) title=format!("Open {}", r.title)>{title}</EntityLink> }.into_any(),
        None => view! { {title} }.into_any(),
    }
}

/// A label name, linked when the label is known to the library.
pub fn label_link(name: Option<&str>, id: Option<i64>) -> impl IntoView + use<> {
    name.filter(|n| !n.is_empty()).map(|n| {
        let n = n.to_string();
        match id {
            Some(id) => view! { <EntityLink href=label_href(id) title=format!("All releases on {n}")>{n}</EntityLink> }.into_any(),
            None => view! { <span>{n}</span> }.into_any(),
        }
    })
}

/// Heart toggle with optimistic update; `loved` is the server value.
#[component]
pub fn LoveButton(track_id: i64, #[prop(into)] loved: Signal<bool>, #[prop(optional)] on_change: Option<Callback<bool>>) -> impl IntoView {
    let optimistic = RwSignal::new(None::<bool>);
    let shown = move || optimistic.get().unwrap_or_else(|| loved.get());
    view! {
        <button type="button" class=move || if shown() { "btn btn-ghost btn-sm btn-icon is-on" } else { "btn btn-ghost btn-sm btn-icon" }
            aria-pressed=move || shown().to_string() title=move || if shown() { "Unlove" } else { "Love" }
            on:click=move |ev| {
                ev.stop_propagation();
                let now = !shown();
                optimistic.set(Some(now));
                leptos::task::spawn_local(async move {
                    match crate::api::post::<_, serde_json::Value>(&format!("/tracks/{track_id}/love"), &serde_json::json!({})).await {
                        Ok(_) => { if let Some(cb) = on_change { cb.run(now); } }
                        Err(e) => { crate::ds::toast_err(&e.message()); optimistic.set(None); }
                    }
                });
            }>
            <Icon name=move || if shown() { "heart-fill" } else { "heart" } />
        </button>
    }
}
