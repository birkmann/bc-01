//! Starting playback from a Bandcamp card, a search hit or a whole grid.
//! A card is only a URL: playing it means fetching the release page first (the server
//! caches the scrape), and a grid sweep is a server-side `QueueSource::Explore` that keeps
//! about twelve tracks ahead of the needle instead of scraping a hundred pages up front.
use bc_types::bandcamp::ExploreReleaseOut;
use bc_types::player::{PlayerCommand, PlayerStatus, QueueSource};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::logic::{sweep_cards, stream_items};
use crate::api;
use crate::player::PlayerCtx;
use crate::util::enc;

pub async fn fetch_release(url: &str) -> Result<ExploreReleaseOut, api::ApiErr> {
    api::get(&format!("/explore/release?url={}", enc(url))).await
}

/// Is this release (by page URL, or by its library copy) the thing the player has loaded?
pub fn is_current(player: PlayerCtx, url: String, library_id: Option<i64>) -> Signal<bool> {
    Signal::derive(move || {
        player.state.with(|s| {
            s.current.as_ref().is_some_and(|c| c.page_url.as_deref() == Some(url.as_str()) || (library_id.is_some() && c.release_id == library_id))
        })
    })
}

pub fn is_playing(player: PlayerCtx, current: Signal<bool>) -> Signal<bool> {
    Signal::derive(move || current.get() && player.state.with(|s| s.status == PlayerStatus::Playing))
}

/// Play one release from its card or hit: the library's own files when it is on the shelf,
/// the Bandcamp stream otherwise. `busy` / `err` drive the button that started it.
pub fn play_release(player: PlayerCtx, url: String, library_id: Option<i64>, busy: RwSignal<bool>, err: RwSignal<Option<String>>) {
    if busy.get_untracked() {
        return;
    }
    err.set(None);
    if let Some(release_id) = library_id {
        player.cmd(PlayerCommand::StartSource { source: QueueSource::Release { release_id, listing: serde_json::Value::Null }, shuffle: false });
        return;
    }
    busy.set(true);
    spawn_local(async move {
        match fetch_release(&url).await {
            Ok(r) => {
                let items = stream_items(&r);
                if items.is_empty() {
                    err.set(Some("Bandcamp streams nothing from this release".into()));
                } else {
                    player.cmd(PlayerCommand::PlayQueue { items, start_index: 0, source: None });
                }
            }
            Err(e) => err.set(Some(e.message())),
        }
        let _ = busy.try_set(false);
    });
}

/// Play a grid, in order or shuffled. The server fetches releases on demand.
pub fn start_sweep(player: PlayerCtx, items: Vec<(String, Option<i64>)>, shuffle: bool) {
    let cards = sweep_cards(items.iter().map(|(u, id)| (u.as_str(), *id)));
    if cards.is_empty() {
        return;
    }
    player.cmd(PlayerCommand::StartSource { source: QueueSource::Explore { cards, shuffle, next: 0 }, shuffle });
}
