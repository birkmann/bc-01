//! "Save as playlist": keep what a library view is showing (the `/tracks` filters) as a playlist.
//! A snapshot, not a saved search; once saved the button becomes the way in to what it made.
use bc_types::library::{PlaylistFromTracks, PlaylistOut, TrackQuery};
use leptos::prelude::*;

use crate::api;
use crate::ds::{Button, Icon, Size, Variant, toast_err};

#[component]
pub fn SaveAsPlaylist(
    /// The filters of the view (no paging).
    #[prop(into)] filter: Signal<TrackQuery>,
    /// Overrides the server's own name for the view (Loved, a tag).
    #[prop(optional, into)] name: Option<String>,
    #[prop(optional)] compact: bool,
) -> impl IntoView {
    let busy = RwSignal::new(false);
    let saved = RwSignal::new(None::<PlaylistOut>);
    // a changed view is a different snapshot: take the link back off
    Effect::new(move |prev: Option<()>| {
        filter.track();
        if prev.is_some() {
            saved.set(None);
        }
    });
    let go = move |_| {
        busy.set(true);
        let body = PlaylistFromTracks { name: name.clone(), filter: filter.get_untracked() };
        leptos::task::spawn_local(async move {
            match api::post::<_, PlaylistOut>("/playlists/from-tracks", &body).await {
                Ok(p) => saved.set(Some(p)),
                Err(e) => toast_err(&e.message()),
            }
            let _ = busy.try_set(false);
        });
    };
    let size = if compact { Size::Sm } else { Size::Md };
    view! {
        {move || match saved.get() {
            Some(p) => view! {
                <a class="btn btn-outline btn-sm pls-saved" href=format!("/playlists/{}", p.id) title=format!("Open {}", p.name)>
                    <Icon name="check" /><span class="truncate">{p.name.clone()}</span>
                </a>
            }.into_any(),
            None => view! {
                <Button size=size variant=Variant::Outline icon="plus" busy=busy on_click=go.clone()
                    title="Keep these tracks as a playlist you can reorder and mix">"Save as playlist"</Button>
            }.into_any(),
        }}
    }
}
