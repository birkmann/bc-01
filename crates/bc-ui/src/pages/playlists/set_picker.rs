//! "Add to DJ set": pick a set or name a new one, with a toggle for where the tracks land:
//! the set's working Pool (planning-first, default) or straight onto the Tracklist.
//! Render inside a `Dialog`/`Sheet`; `ids` is read at pick time.
use bc_types::sets::{AddTracks, DjSetCreate, DjSetDetail, DjSetListOut, PoolSource};
use leptos::prelude::*;

use crate::api;
use crate::data::QuerySpec;
use crate::player::plan::qh::use_qh;
use crate::ds::{Button, EmptyState, ErrorPanel, Icon, Skeleton, Variant, toast_err, toast_ok};
use crate::pages::sets::set_logic::with_picked_tracks;

const MODE_KEY: &str = "bc:set-picker-mode";

#[component]
pub fn SetPicker(
    /// Track ids to add (read when a set is picked).
    #[prop(into)] ids: Callback<(), Vec<i64>>,
    #[prop(into)] on_done: Callback<()>,
) -> impl IntoView {
    let sets = use_qh::<Vec<DjSetListOut>>(|| Some(QuerySpec::new("/sets", &["set"])));
    let mode = RwSignal::new(crate::util::ls_get(MODE_KEY).unwrap_or_else(|| "pool".into()));
    Effect::new(move |_| crate::util::ls_set(MODE_KEY, &mode.get()));
    let name = RwSignal::new(String::new());
    let busy = RwSignal::new(false);

    let add_to = move |set_id: i64| {
        let ids = ids.run(());
        if ids.is_empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        let tracklist = mode.get_untracked() == "tracklist";
        leptos::task::spawn_local(async move {
            let res: Result<(), api::ApiErr> = async {
                if tracklist {
                    let _: DjSetDetail = api::post(&format!("/sets/{set_id}/items"), &AddTracks { track_ids: ids }).await?;
                } else {
                    let d: DjSetDetail = api::get(&format!("/sets/{set_id}")).await?;
                    // only the changed key: every key sent (even null) is applied
                    let patch = serde_json::json!({ "pool_sources": with_picked_tracks(&d.pool_sources, &ids) });
                    let _: DjSetDetail = api::patch(&format!("/sets/{set_id}"), &patch).await?;
                }
                Ok(())
            }
            .await;
            match res {
                Ok(()) => {
                    toast_ok(if tracklist { "Added to the tracklist" } else { "Added to the set's pool" });
                    on_done.run(());
                }
                Err(e) => toast_err(&e.message()),
            }
            let _ = busy.try_set(false);
        });
    };
    let create = move || {
        let n = name.get_untracked().trim().to_string();
        if n.is_empty() || busy.get_untracked() {
            return;
        }
        let ids = ids.run(());
        busy.set(true);
        let tracklist = mode.get_untracked() == "tracklist";
        leptos::task::spawn_local(async move {
            let res: Result<(), api::ApiErr> = async {
                let pool_sources: Vec<PoolSource> = if tracklist || ids.is_empty() { vec![] } else { with_picked_tracks(&[], &ids) };
                let created: DjSetDetail = api::post(
                    "/sets",
                    &DjSetCreate { name: n, venue: None, event_date: None, target_minutes: None, from_playlist_id: None, pool_sources },
                )
                .await?;
                if tracklist && !ids.is_empty() {
                    let _: DjSetDetail = api::post(&format!("/sets/{}/items", created.set.id), &AddTracks { track_ids: ids }).await?;
                }
                Ok(())
            }
            .await;
            match res {
                Ok(()) => {
                    toast_ok("Set created");
                    on_done.run(());
                }
                Err(e) => toast_err(&e.message()),
            }
            let _ = busy.try_set(false);
        });
    };

    view! {
        <div class="setpick">
            <div class="segmented" role="group" aria-label="Where the tracks land">
                <button type="button" aria-pressed=move || (mode.get() == "pool").to_string()
                    title="Feed the set's working pool and plan from it" on:click=move |_| mode.set("pool".into())>"Pool"</button>
                <button type="button" aria-pressed=move || (mode.get() == "tracklist").to_string()
                    title="Append straight onto the set's tracklist" on:click=move |_| mode.set("tracklist".into())>"Tracklist"</button>
            </div>
            <form class="setpick-new" on:submit=move |ev| { ev.prevent_default(); create(); }>
                <input class="input" placeholder="New set name" aria-label="New set name"
                    prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                <Button kind="submit" variant=Variant::Primary icon="plus" disabled=Signal::derive(move || name.get().trim().is_empty() || busy.get())>"Create and add"</Button>
            </form>
            <div class="section-title">"DJ sets"</div>
            <div class="setpick-list">
                {move || match (sets.data.get(), sets.error.get()) {
                    (Some(l), _) if l.is_empty() => view! { <EmptyState title="No sets yet" hint="Name one above." icon="sliders" /> }.into_any(),
                    (Some(l), _) => l.iter().map(|s| {
                        let id = s.id;
                        view! {
                            <button type="button" class="setpick-item" disabled=move || busy.get() on:click=move |_| add_to(id)>
                                <Icon name=move || if mode.get() == "pool" { "sliders" } else { "list-music" } />
                                <span class="truncate">{s.name.clone()}</span>
                                <span class="mono faint">{s.track_count}</span>
                            </button>
                        }
                    }).collect_view().into_any(),
                    (None, Some(e)) => view! { <ErrorPanel message=e.message() on_retry=Callback::new(move |_| sets.refetch()) /> }.into_any(),
                    (None, None) => view! { <Skeleton height="36px" /> }.into_any(),
                }}
            </div>
        </div>
    }
}
