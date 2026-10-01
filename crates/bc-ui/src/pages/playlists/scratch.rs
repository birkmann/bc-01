//! The scratch pool at the foot of the playlists shelf: drop playlists (or tracks) on it, it
//! holds their union, "Save as playlist" keeps it. Stored server-side (`ui_state/scratch-pool`).
use bc_types::library::{PlaylistAddTracks, PlaylistCreate, PlaylistOut, TrackOut};
use leptos::prelude::*;

use super::logic::{PICKED, ScratchPool as Pool, free_name};
use super::fetch_playlist_tracks;
use crate::api;
use crate::ds::{Button, Icon, Size, Variant, toast_err, toast_ok};
use crate::logic::format::format_long_duration;
use crate::widgets::common::{Art, queue_item};
use crate::widgets::dnd::{self, DragPayload};

const KEY: &str = "/ui-state/scratch-pool";

#[derive(Clone, Copy)]
pub struct ScratchCtx {
    pub pool: RwSignal<Pool>,
}

impl ScratchCtx {
    /// Create the store, load it from the server and persist every later change (debounced).
    pub fn provide() -> Self {
        let pool = RwSignal::new(Pool::default());
        let loaded = RwSignal::new(false);
        let last = StoredValue::new(String::new());
        leptos::task::spawn_local(async move {
            if let Ok(p) = api::get::<Pool>(KEY).await {
                last.set_value(serde_json::to_string(&p).unwrap_or_default());
                let _ = pool.try_set(p);
            } else {
                last.set_value(serde_json::to_string(&Pool::default()).unwrap_or_default());
            }
            let _ = loaded.try_set(true);
        });
        let timer = StoredValue::new(None::<i32>);
        Effect::new(move |_| {
            let p = pool.get();
            if !loaded.get() {
                return;
            }
            let json = serde_json::to_string(&p).unwrap_or_default();
            if json == last.get_value() {
                return;
            }
            let w = crate::util::window();
            if let Some(t) = timer.get_value() {
                w.clear_timeout_with_handle(t);
            }
            let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
                last.set_value(json.clone());
                leptos::task::spawn_local(async move {
                    if let Err(e) = api::call_json("PUT", KEY, &p).await {
                        toast_err(&e.message());
                    }
                });
            });
            use wasm_bindgen::JsCast;
            timer.set_value(w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), 400).ok());
        });
        let ctx = Self { pool };
        provide_context(ctx);
        ctx
    }
}

#[component]
pub fn ScratchPool() -> impl IntoView {
    let ctx = expect_context::<ScratchCtx>();
    let player = crate::player::use_player();
    let pool = ctx.pool;
    let open = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let taking = RwSignal::new(None::<String>);
    let saving = RwSignal::new(false);
    let saved = RwSignal::new(None::<PlaylistOut>);
    let shape = Memo::new(move |_| pool.with(|p| p.shape()));
    Effect::new(move |_| {
        shape.track();
        saved.set(None);
    });

    let tracks = Memo::new(move |_| pool.with(|p| p.tracks_in_order()));
    let empty = Signal::derive(move || pool.with(|p| p.is_empty()));
    let duration = Signal::derive(move || tracks.with(|t| t.iter().map(|t| t.duration_ms.unwrap_or(0)).sum::<i64>()));

    dnd::register_target("scratch-pool", &["playlist", "track"], move |payload: DragPayload, _| {
        let label = payload.label.clone();
        taking.set(Some(label));
        leptos::task::spawn_local(async move {
            if payload.kind == "playlist" {
                for id in payload.ids {
                    match fetch_playlist_tracks(id).await {
                        Ok(items) => pool.update(|p| p.add_source(&format!("playlist:{id}"), &payload.label, &items)),
                        Err(e) => toast_err(&e.message()),
                    }
                }
            } else {
                let mut got: Vec<TrackOut> = vec![];
                for id in payload.ids {
                    if let Ok(t) = api::get::<TrackOut>(&format!("/tracks/{id}")).await {
                        got.push(t);
                    }
                }
                pool.update(|p| p.add_tracks(&got));
            }
            let _ = taking.try_set(None);
        });
    });
    let over = dnd::over();
    let is_over = move || over.get().map(|(id, _)| id == "scratch-pool").unwrap_or(false);

    let save = move || {
        if saving.get_untracked() {
            return;
        }
        saving.set(true);
        let wanted = name.get_untracked().trim().to_string();
        let ids: Vec<i64> = tracks.get_untracked().iter().map(|t| t.id).collect();
        leptos::task::spawn_local(async move {
            let taken: Vec<String> = api::get::<Vec<PlaylistOut>>("/playlists").await.map(|l| l.into_iter().map(|p| p.name).collect()).unwrap_or_default();
            let n = if wanted.is_empty() { free_name(&taken, "Pool") } else { wanted };
            let res: Result<PlaylistOut, api::ApiErr> = async {
                let created: PlaylistOut = api::post("/playlists", &PlaylistCreate { name: n, ..Default::default() }).await?;
                let _: serde_json::Value = api::post(&format!("/playlists/{}/tracks", created.id), &PlaylistAddTracks { track_ids: ids, at_index: None }).await?;
                Ok(created)
            }
            .await;
            match res {
                Ok(p) => {
                    name.set(String::new());
                    toast_ok(&format!("Saved as {}", p.name));
                    saved.set(Some(p));
                }
                Err(e) => toast_err(&e.message()),
            }
            let _ = saving.try_set(false);
        });
    };

    view! {
        <div class=move || format!("pls-pool{}{}", if is_over() { " over" } else { "" }, if empty.get() { " is-empty" } else { "" })
            data-dnd-target="scratch-pool" aria-label="Scratch pool">
            <div class="pls-pool-head">
                <span class="pls-pool-ico"><Icon name="layers" /></span>
                {move || if empty.get() {
                    view! { <span class="muted">"Drag playlists here, or press Pool on a row, to build a pool you can save as one playlist"</span> }.into_any()
                } else {
                    view! {
                        <span class="pls-pool-title">"Scratch pool"</span>
                        <span class="mono muted">{move || format!("{} sources · {} tracks · {}", pool.with(|p| p.sources.len()), tracks.with(|t| t.len()), format_long_duration(duration.get() as f64))}</span>
                    }.into_any()
                }}
                {move || taking.get().map(|n| view! { <span class="faint pls-taking"><Icon name="refresh" size=12 />{n}</span> })}
                <span class="spacer"></span>
                {move || (!empty.get()).then(|| view! {
                    <div class="pls-pool-actions">
                        <input class="input pls-pool-name" placeholder="Pool" aria-label="Name for the saved playlist"
                            prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev))
                            on:keydown=move |ev| if ev.key() == "Enter" { save() } />
                        <Button size=Size::Sm variant=Variant::Primary icon="plus" busy=saving on_click=move |_| save()>"Save as playlist"</Button>
                        {move || saved.get().map(|p| view! {
                            <a class="pls-saved" href=format!("/playlists/{}", p.id)><Icon name="check" size=12 /><span class="truncate">{p.name.clone()}</span></a>
                        })}
                        <Button size=Size::Sm variant=Variant::Ghost icon="play" title="Play the pool"
                            on_click=move |_| player.cmd(bc_types::player::PlayerCommand::PlayQueue { items: tracks.get_untracked().iter().map(queue_item).collect(), start_index: 0, source: None })><span class="pls-lbl">"Play"</span></Button>
                        <Button size=Size::Sm variant=Variant::Ghost icon=crate::ds::dyn_icon(move || if open.get() { "chevron-down" } else { "chevron-up" })
                            pressed=open on_click=move |_| open.update(|o| *o = !*o)>
                            <span class="pls-lbl">{move || if open.get() { "Hide" } else { "Tracks" }}</span>
                        </Button>
                        <Button size=Size::Sm variant=Variant::Ghost icon="trash" title="Empty the pool"
                            on_click=move |_| pool.update(|p| p.clear()) />
                    </div>
                })}
            </div>
            {move || (!empty.get()).then(|| view! {
                <div class="pls-chips">
                    {move || pool.with(|p| p.sources.clone()).into_iter().map(|s| {
                        let key = s.key.clone();
                        let label = s.label.clone();
                        let aria = format!("Take {} out of the pool", s.label);
                        view! {
                            <span class="pls-chip">
                                <Icon name=if s.key == PICKED { "plus" } else { "layers" } size=11 />
                                <span class="truncate">{label}</span>
                                <span class="mono faint">{s.track_ids.len()}</span>
                                <button type="button" aria-label=aria on:click=move |_| pool.update(|p| p.remove_source(&key))><Icon name="x" size=11 /></button>
                            </span>
                        }
                    }).collect_view()}
                </div>
            })}
            {move || (open.get() && !empty.get()).then(|| view! {
                <div class="pls-pool-tracks">
                    {tracks.get().into_iter().map(|t| {
                        let art = t.art_url.clone();
                        view! {
                            <div class="pls-mini">
                                <Art src=art size=24.0 />
                                <span class="truncate pls-mini-t">{t.title.clone()}</span>
                                <span class="truncate muted pls-mini-a">{crate::widgets::common::artist_link(t.artist.as_ref())}</span>
                                <span class="mono muted">{crate::logic::format::format_duration_ms(t.duration_ms.map(|d| d as f64))}</span>
                            </div>
                        }
                    }).collect_view()}
                </div>
            })}
        </div>
    }
}
