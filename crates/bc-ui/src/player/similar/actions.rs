//! What to do with the whole list: play it now, queue it, keep it as a playlist, or draw a
//! different slice of the same taste.
use bc_types::library::{PlaylistAddTracks, PlaylistCreate, PlaylistOut};
use bc_types::player::{PlayerCommand, QueueItem};
use leptos::prelude::*;

use super::SimilarCtx;
use crate::api;
use crate::ds::{Button, Size, toast_err, toast_ok};
use crate::player::use_player;

#[component]
pub(crate) fn SimilarActions(ctx: SimilarCtx, #[prop(into)] tracks: Signal<Vec<QueueItem>>, seed: QueueItem) -> impl IntoView {
    let player = use_player();
    let saving = RwSignal::new(false);
    let empty = Signal::derive(move || tracks.with(|t| t.is_empty()));
    let savable = move || tracks.with(|t| t.iter().filter(|t| t.is_library()).map(|t| t.track_id).collect::<Vec<_>>());
    let name = format!("Like {}", seed.title);
    let save = move |_| {
        let ids = savable();
        if ids.is_empty() {
            return;
        }
        saving.set(true);
        let name = name.clone();
        leptos::task::spawn_local(async move {
            let res: Result<(), api::ApiErr> = async {
                let p: PlaylistOut = api::post("/playlists", &PlaylistCreate { name: name.clone(), ..Default::default() }).await?;
                let _: serde_json::Value = api::post(&format!("/playlists/{}/tracks", p.id), &PlaylistAddTracks { track_ids: ids, at_index: None }).await?;
                toast_ok(&format!("Saved as {name}"));
                Ok(())
            }
            .await;
            if let Err(e) = res {
                toast_err(&e.message());
            }
            let _ = saving.try_set(false);
        });
    };
    view! {
        <div class="pp-sec pp-acts">
            <Button size=Size::Sm icon="play" disabled=empty title="Replace the queue with these and start playing"
                on_click=move |_| player.cmd(PlayerCommand::PlayQueue { items: tracks.get_untracked(), start_index: 0, source: None })>"Play all"</Button>
            <Button size=Size::Sm icon="queue" disabled=empty title="Add every suggestion to the end of the queue"
                on_click=move |_| player.cmd(PlayerCommand::AddToQueue { items: tracks.get_untracked() })>"Queue all"</Button>
            <Button size=Size::Sm icon="plus" busy=saving disabled=empty title="Save the library tracks as a playlist" on_click=save>"Save"</Button>
            <span class="spacer"></span>
            <Button size=Size::Sm icon="refresh" disabled=Signal::derive(move || empty.get()) busy=ctx.loading title="A different slice of the same taste"
                on_click=move |_| { ctx.shuffle_seed.update(|s| *s += 1); ctx.limit.set(super::PAGE); }>"Reroll"</Button>
        </div>
    }
}
