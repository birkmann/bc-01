//! Loved extras: loved Bandcamp streams, suggestions from the loved profile, DJ mix.
use bc_types::library::{LovedDownloadOut, LovedStreamOut};
use bc_types::player::{ItemOrigin, PlayerCommand, Pool, QueueItem};
use bc_types::suggest::LovedSuggestResponse;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Button, Icon, Size, Variant, toast_err, toast_ok};
use crate::logic::format::{format_count, format_duration_ms};
use crate::player::{PlayerCtx, use_player};
use crate::util::enc;
use crate::widgets::common::{Art, artist_link, title_link};

pub fn stream_item(s: &LovedStreamOut) -> QueueItem {
    QueueItem {
        track_id: -s.id.max(1),
        title: s.stream.title.clone(),
        artist: Some(s.stream.artist_name.clone()).filter(|a| !a.is_empty()),
        album: Some(s.stream.release_title.clone()).filter(|a| !a.is_empty()),
        art_url: s.stream.art_url.clone(),
        duration_ms: s.stream.duration_ms,
        origin: ItemOrigin::Bandcamp,
        stream_url: Some(s.stream_url.clone()),
        page_url: Some(s.stream.page_url.clone()),
        loved: true,
        ..Default::default()
    }
}

pub fn brief_item(t: &bc_types::library::TrackOut) -> QueueItem {
    crate::widgets::common::queue_item(t)
}

pub fn dj_mix_loved(player: PlayerCtx) {
    player.cmd(PlayerCommand::StartMixFrom { pool: Pool::Loved });
}

/// Loved streams: Bandcamp tracks loved straight off the page (not in the library).
#[component]
pub fn LovedStreams() -> impl IntoView {
    let player = use_player();
    let q = use_query::<Vec<LovedStreamOut>>(|| Some(QuerySpec::new("/loved-streams", &["loved"])));
    let data = q.data;
    let err = q.error;
    let q = StoredValue::new(q);
    let dl_busy = RwSignal::new(false);
    let download = move |_| {
        dl_busy.set(true);
        spawn_local(async move {
            match api::post::<_, LovedDownloadOut>("/loved-streams/download", &serde_json::json!({})).await {
                Ok(r) => toast_ok(&r.detail),
                Err(e) => toast_err(&e.message()),
            }
            dl_busy.set(false);
        });
    };
    view! {
        {move || {
            let list = data.get().map(|d| (*d).clone()).unwrap_or_default();
            if list.is_empty() {
                // a missing route (older server) or an empty shelf both mean: nothing to show
                let _ = err.get();
                return ().into_any();
            }
            let all = std::sync::Arc::new(list.clone());
            let a2 = all.clone();
            view! {
                <section class="lv-sec">
                    <div class="hm-sec-head">
                        <h2 class="hm-sec-title"><span class="hm-panel-icon"><Icon name="heart-fill" /></span>"Loved on Bandcamp"<span class="mono faint lv-n">{format_count(list.len() as i64)}</span></h2>
                        <div class="hm-sec-actions">
                            <button type="button" class="hm-act" on:click=move |_| player.cmd(PlayerCommand::PlayQueue { items: a2.iter().map(stream_item).collect(), start_index: 0, source: None })><Icon name="play" size=13 />"Play"</button>
                            <button type="button" class="hm-act" disabled=move || dl_busy.get() title="Queue the albums behind these streams for download" on:click=download><Icon name="download" size=13 />"Download"</button>
                        </div>
                    </div>
                    <div class="hm-list lv-list">
                        {list.into_iter().enumerate().map(|(i, s)| {
                            let all = all.clone();
                            let (page, key) = (s.stream.page_url.clone(), s.stream.track_key.clone());
                            let art = s.stream.art_url.clone();
                            view! {
                                <div class="hm-trk">
                                    <button type="button" class="hm-trk-art" aria-label=format!("Play {}", s.stream.title)
                                        on:click=move |_| player.cmd(PlayerCommand::PlayQueue { items: all.iter().map(stream_item).collect(), start_index: i, source: None })>
                                        <Art src=art class="alb-art" /><span class="hm-trk-play"><Icon name="play" size=14 /></span>
                                    </button>
                                    <span class="hm-trk-text">
                                        <span class="hm-trk-title truncate">{s.stream.title.clone()}</span>
                                        <span class="hm-trk-sub truncate">{s.stream.artist_name.clone()}{(!s.stream.release_title.is_empty()).then(|| format!(" · {}", s.stream.release_title))}</span>
                                    </span>
                                    <span class="hm-trk-trail mono">{format_duration_ms(s.stream.duration_ms.map(|d| d as f64))}</span>
                                    <button type="button" class="btn btn-ghost btn-sm btn-icon is-on" aria-label="Unlove this stream" title="Unlove" on:click=move |_| {
                                        let path = format!("/loved-streams?page_url={}&track_key={}", enc(&page), enc(&key));
                                        spawn_local(async move {
                                            match api::call("DELETE", &path).await {
                                                Ok(()) => q.get_value().refetch(),
                                                Err(e) => toast_err(&e.message()),
                                            }
                                        });
                                    }><Icon name="heart-fill" /></button>
                                </div>
                            }
                        }).collect_view()}
                    </div>
                </section>
            }.into_any()
        }}
    }
}

/// Tracks you do not have yet but that sit close to the loved profile.
#[component]
pub fn LovedSuggestions() -> impl IntoView {
    let player = use_player();
    let seed = RwSignal::new(0i64);
    let open = RwSignal::new(crate::util::ls_get("bc:loved:suggest").as_deref() == Some("1"));
    Effect::new(move |_| crate::util::ls_set("bc:loved:suggest", if open.get() { "1" } else { "0" }));
    let q = use_query::<LovedSuggestResponse>(move || open.get().then(|| QuerySpec::keyed(format!("loved-suggest:{}", seed.get()), format!("/suggest/loved?limit=12&seed={}", seed.get()), &[])));
    let data = q.data;
    let loading = q.loading;
    view! {
        {move || {
            if !open.get() {
                return view! {
                    <section class="lv-sec">
                        <button type="button" class="hm-act" aria-expanded="false" on:click=move |_| open.set(true)><Icon name="sparkles" size=13 />"Suggested from your loved"<Icon name="chevron-down" size=13 /></button>
                    </section>
                }.into_any();
            }
            let Some(d) = data.get() else {
                return view! { <section class="lv-sec"><p class="faint lv-prof">"Looking for suggestions…"</p></section> }.into_any();
            };
            if d.items.is_empty() {
                return ().into_any();
            }
            let tags: Vec<String> = d.profile.tags.iter().take(4).map(|t| t.name.clone()).collect();
            let items = std::sync::Arc::new(d.items.iter().map(|i| i.track.clone()).collect::<Vec<_>>());
            let list = d.items.clone();
            view! {
                <section class="lv-sec">
                    <div class="hm-sec-head">
                        <h2 class="hm-sec-title"><span class="hm-panel-icon"><Icon name="sparkles" /></span>"Suggested from your loved"</h2>
                        <div class="hm-sec-actions">
                            {(!tags.is_empty()).then(|| view! { <span class="faint lv-prof">{format!("leaning {}", tags.join(", "))}</span> })}
                            <button type="button" class="hm-act" disabled=move || loading.get() on:click=move |_| seed.update(|s| *s += 1)><Icon name="refresh" size=13 />"Reshuffle"</button>
                            <button type="button" class="hm-act" aria-expanded="true" on:click=move |_| open.set(false)><Icon name="chevron-up" size=13 />"Hide"</button>
                        </div>
                    </div>
                    <div class="hm-list lv-list">
                        {list.into_iter().enumerate().map(|(i, s)| {
                            let items = items.clone();
                            let t = s.track.clone();
                            view! {
                                <div class="hm-trk">
                                    <button type="button" class="hm-trk-art" aria-label=format!("Play {}", t.title)
                                        on:click=move |_| player.cmd(PlayerCommand::PlayQueue { items: items.iter().map(brief_item).collect(), start_index: i, source: None })>
                                        <Art src=t.art_url.clone() class="alb-art" /><span class="hm-trk-play"><Icon name="play" size=14 /></span>
                                    </button>
                                    <span class="hm-trk-text">
                                        <span class="hm-trk-title truncate">{title_link(&t.title, t.release.as_ref())}</span>
                                        <span class="hm-trk-sub truncate">{artist_link(t.artist.as_ref())}{s.why.first().map(|w| format!(" · {w}"))}</span>
                                    </span>
                                    <span class="hm-trk-trail mono">{format!("{:.0}%", (s.score * 100.0).clamp(0.0, 100.0))}</span>
                                </div>
                            }
                        }).collect_view()}
                    </div>
                </section>
            }.into_any()
        }}
    }
}

#[allow(dead_code)]
fn _keep() {
    let _ = (Button, Size::Sm, Variant::Ghost);
}
