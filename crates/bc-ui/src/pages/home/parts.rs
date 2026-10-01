//! Building blocks of the Home shelves: the section heading, the play/shuffle pair
//! every shelf carries, and the compact track row.
use std::sync::Arc;

use bc_types::library::TrackOut;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::ds::{Icon, toast_err};
use crate::pages::albums::host::{play_items, shuffle_in_place};
use crate::player::use_player;
use crate::widgets::common::{Art, album_link, artist_link, title_link};

pub type TracksFut = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<TrackOut>, crate::api::ApiErr>>>>;
pub type TracksFn = Arc<dyn Fn() -> TracksFut + Send + Sync>;

#[component]
pub fn SectionHeader(
    #[prop(into)] title: String,
    /// "View all" target.
    #[prop(optional, into)] to: Option<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    view! {
        <div class="hm-sec-head">
            <h2 class="hm-sec-title">{title}</h2>
            <div class="hm-sec-actions">
                {children.map(|c| c())}
                {to.map(|t| view! { <a class="hm-viewall" href=t>"View all"<Icon name="chevron-right" size=13 /></a> })}
            </div>
        </div>
    }
}

/// Play the list top to bottom, or deal it into a random order first.
#[component]
pub fn ShelfControls(get_tracks: TracksFn) -> impl IntoView {
    let player = use_player();
    let busy = RwSignal::new(None::<bool>);
    let start = Arc::new(move |shuffle: bool| {
        busy.set(Some(shuffle));
        let f = get_tracks.clone();
        spawn_local(async move {
            match f().await {
                Ok(mut t) => {
                    if shuffle {
                        shuffle_in_place(&mut t);
                    }
                    play_items(player, &t, 0, None, shuffle);
                }
                Err(e) => toast_err(&e.message()),
            }
            busy.set(None);
        });
    });
    let (a, b) = (start.clone(), start);
    view! {
        <button type="button" class="hm-act" disabled=move || busy.get().is_some() on:click=move |_| a(false)>
            <Icon name="play" size=13 />"Play"
        </button>
        <button type="button" class="hm-act" disabled=move || busy.get().is_some() on:click=move |_| b(true)>
            <Icon name="shuffle" size=13 />"Shuffle"
        </button>
    }
}

/// A compact track row: clicking plays the whole list from here.
#[component]
pub fn TrackRow(
    track: TrackOut,
    index: usize,
    #[prop(optional)] rank: Option<usize>,
    #[prop(optional, into)] trailing: Option<String>,
    on_play: Callback<usize>,
) -> impl IntoView {
    let player = use_player();
    let id = track.id;
    let current = Memo::new(move |_| player.current_track_id() == Some(id));
    let title = track.title.clone();
    let aria = format!("Play {title}");
    let art = track.art_url.clone().or_else(|| track.release.as_ref().and_then(|r| r.art_url.clone()));
    let artist = track.artist.clone();
    let release = track.release.clone();
    view! {
        <div class="hm-trk" class:cur=move || current.get()>
            {rank.map(|r| view! { <span class="hm-rank mono">{r}</span> })}
            <button type="button" class="hm-trk-art" aria-label=aria on:click=move |_| on_play.run(index)>
                <Art src=art class="alb-art" />
                <span class="hm-trk-play"><Icon name="play" size=14 /></span>
            </button>
            <span class="hm-trk-text">
                <span class="hm-trk-title truncate">{title_link(&title, release.as_ref())}</span>
                <span class="hm-trk-sub truncate">
                    {artist_link(artist.as_ref())}
                    {release.as_ref().map(|r| view! { " · " {album_link(Some(r))} })}
                </span>
            </span>
            {trailing.map(|t| view! { <span class="hm-trk-trail mono">{t}</span> })}
        </div>
    }
}
