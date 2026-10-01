//! The Home rail (xl+) / closing band (below): Top 10 with ranges, library stat
//! tiles and the favourites strip with its "check all for new releases" sweep.
use std::sync::Arc;

use bc_types::bandcamp::SweepStatus;
use bc_types::library::*;
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::logic::{self, Range};
use super::parts::TrackRow;
use super::shelves::Shelves;
use crate::api;
use crate::data::{QuerySpec, use_query, use_topic};
use crate::ds::popover::Rect;
use crate::ds::{Icon, MenuCtx, MenuEntry, MenuItem, confirm, toast_err, toast_ok};
use crate::logic::format::{format_bytes, format_count, format_playtime};
use crate::pages::albums::host::{ids_of, play_items, shuffle_in_place, use_library_host, PickerReq};
use crate::player::use_player;
use crate::util::{ls_get, ls_set};
use crate::widgets::common::Art;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Band,
    Rail,
}

// ---- stats -------------------------------------------------------------------------------------

#[component]
pub fn StatTiles(shelves: Shelves, layout: Layout) -> impl IntoView {
    view! {
        {move || shelves.get().map(|s| {
            let st = &s.stats;
            let tiles = [
                ("Tracks", format_count(st.tracks)),
                ("Albums", format_count(st.releases)),
                ("Artists", format_count(st.artists)),
                ("Playtime", format_playtime(st.total_duration_ms as f64)),
                ("On disk", format_bytes(st.total_bytes as f64)),
                ("Listened", format_playtime(st.listened_ms as f64)),
            ];
            let cells = tiles.into_iter().map(|(k, v)| view! { <div class="hm-stat"><div class="hm-stat-v mono">{v}</div><div class="hm-stat-k">{k}</div></div> }).collect_view();
            match layout {
                Layout::Rail => view! {
                    <div class="hm-card">
                        <div class="hm-card-head"><h2 class="hm-card-title">"Library"</h2><a class="lib-link faint" href="/settings">"Details"</a></div>
                        <div class="hm-stats rail">{cells}</div>
                    </div>
                }.into_any(),
                Layout::Band => view! { <section class="hm-sec"><div class="hm-stats band">{cells}</div></section> }.into_any(),
            }
        })}
    }
}

// ---- top ten -----------------------------------------------------------------------------------

#[component]
pub fn TopTen(shelves: Shelves) -> impl IntoView {
    let player = use_player();
    let menu = expect_context::<MenuCtx>();
    let host = use_library_host();
    let range = RwSignal::new(logic::parse_range(ls_get(logic::RANGE_KEY).as_deref()));
    Effect::new(move |_| ls_set(logic::RANGE_KEY, &logic::range_to_string(range.get())));

    let recent = use_query::<HistoryTop>(move || match range.get() {
        Range::All => None,
        Range::Days(d) => Some(QuerySpec::keyed(format!("home:top-ten:{d}"), format!("/history/top?days={d}&limit=10"), &[])),
    });
    let recent_data = recent.data;
    let recent_loading = recent.loading;
    let entries = Memo::new(move |_| -> Vec<(TrackOut, String)> {
        match range.get() {
            Range::All => shelves
                .get()
                .map(|s| s.top_ten.iter().filter(|t| t.play_count > 0).map(|t| (t.clone(), format!("{}×", t.play_count))).collect())
                .unwrap_or_default(),
            Range::Days(_) => recent_data.get().map(|h| h.items.iter().map(|i| (i.track.clone(), format!("{}×", i.plays))).collect()).unwrap_or_default(),
        }
    });
    let tracks = Memo::new(move |_| entries.get().into_iter().map(|(t, _)| t).collect::<Vec<_>>());

    let open_more = move |ev: leptos::ev::MouseEvent| {
        use wasm_bindgen::JsCast;
        let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
        let r = range.get_untracked();
        let w = logic::window_of(r);
        let ids: Vec<i64> = tracks.get_untracked().iter().map(|t| t.id).collect();
        let reset_label = match r {
            Range::All => "Reset play counts…".to_string(),
            Range::Days(_) => format!("Reset {}…", w.label.to_lowercase()),
        };
        let entries: Vec<MenuEntry> = vec![
            MenuItem::new("Save as playlist…").icon("list").on(move || host.picker.set(Some(PickerReq { ids: ids_of(ids.clone()) }))).into(),
            MenuEntry::Sep,
            MenuItem::new(reset_label).icon("refresh").danger().on(move || reset_history(r)).into(),
        ];
        menu.open(Rect::of(&el), entries);
    };

    view! {
        <div class="hm-card">
            <div class="hm-card-head">
                <h2 class="hm-card-title"><span class="hm-panel-icon"><Icon name="activity" /></span>"Top 10"</h2>
                {move || (!entries.get().is_empty()).then(|| view! {
                    <div class="hm-panel-btns">
                        <button type="button" class="hm-pill primary" on:click=move |_| play_items(player, &tracks.get_untracked(), 0, None, false)><Icon name="play" size=11 />"Play all"</button>
                        <button type="button" class="lib-roundbtn small" aria-label="Shuffle the Top 10" title="Shuffle the Top 10" on:click=move |_| {
                            let mut d = tracks.get_untracked(); shuffle_in_place(&mut d); play_items(player, &d, 0, None, true)
                        }><Icon name="shuffle" size=12 /></button>
                        <button type="button" class="lib-roundbtn small" aria-label="More actions for the Top 10" aria-haspopup="menu" title="Save or reset" on:click=open_more><Icon name="more" size=12 /></button>
                    </div>
                })}
            </div>
            <div class="hm-ranges" role="group" aria-label="Chart window">
                {logic::WINDOWS.iter().map(|w| {
                    let r = w.range;
                    view! { <button type="button" class="hm-range" aria-pressed=move || (range.get() == r).to_string() on:click=move |_| range.set(r)>{w.label}</button> }
                }).collect_view()}
            </div>
            {move || {
                let list = entries.get();
                if list.is_empty() {
                    let r = range.get();
                    let msg = match r {
                        Range::All => "Play counts build this chart. Nothing has been played yet.".to_string(),
                        Range::Days(_) if recent_loading.get() && recent_data.get().is_none() => "Loading…".to_string(),
                        Range::Days(_) => format!("Nothing on repeat in {} yet.", logic::window_of(r).noun),
                    };
                    return view! { <p class="faint hm-empty">{msg}</p> }.into_any();
                }
                let all = Arc::new(tracks.get());
                view! {
                    <div class="hm-list">
                        {list.into_iter().enumerate().map(|(i, (t, trail))| {
                            let all = all.clone();
                            view! { <TrackRow track=t index=i rank=i + 1 trailing=trail on_play=Callback::new(move |ix| play_items(player, &all, ix, None, false)) /> }
                        }).collect_view()}
                    </div>
                }.into_any()
            }}
        </div>
    }
}

fn reset_history(r: Range) {
    spawn_local(async move {
        let w = logic::window_of(r);
        let (title, body) = match r {
            Range::All => (
                "Reset all play counts?".to_string(),
                "Every play count and the whole play history will be cleared. The Top 10, \"Dust off\" and any listening stats start from zero. This cannot be undone.".to_string(),
            ),
            Range::Days(_) => (
                format!("Forget {}?", w.noun),
                format!("Plays from {} will be removed from the history and subtracted from each track's play count; older plays are kept. This cannot be undone.", w.noun),
            ),
        };
        if !confirm(&title, &body, "Reset", true).await {
            return;
        }
        let path = match r {
            Range::All => "/history".to_string(),
            Range::Days(d) => format!("/history?days={d}"),
        };
        match api::call("DELETE", &path).await {
            Ok(()) => {
                toast_ok("History reset");
                crate::data::invalidate_all();
                crate::data::invalidate_prefix("home");
            }
            Err(e) => toast_err(&e.message()),
        }
    });
}

// ---- favourites ---------------------------------------------------------------------------------

async fn play_filter(player: crate::player::PlayerCtx, q: TrackQuery, shuffle: bool) {
    let mut q = q;
    q.limit = Some(500);
    if shuffle {
        q.sort = Some(TrackSort::Random);
    } else {
        q.sort = Some(TrackSort::Album);
        q.order = Some(SortDir::Asc);
    }
    match api::get::<TrackPage>(&format!("/tracks{}", crate::util::qs_pairs(&q.to_pairs()))).await {
        Ok(p) => play_items(player, &p.page.items, 0, None, shuffle),
        Err(e) => toast_err(&e.message()),
    }
}

#[component]
fn PlayOverlay(#[prop(into)] label: String, on_play: Callback<()>) -> impl IntoView {
    let busy = RwSignal::new(false);
    view! {
        <button type="button" class="hm-fav-play" aria-label=label title="Play" disabled=move || busy.get()
            on:click=move |ev| { ev.stop_propagation(); busy.set(true); on_play.run(()); crate::util::after(600, move || { let _ = busy.try_set(false); }); }>
            <Icon name="play" size=13 />
        </button>
    }
}

#[component]
fn FavoritesControls() -> impl IntoView {
    let player = use_player();
    let busy = RwSignal::new(None::<bool>);
    let run = Arc::new(move |shuffle: bool| {
        busy.set(Some(shuffle));
        spawn_local(async move {
            let q = TrackQuery { favorites: Some(true), ..Default::default() };
            let q = if shuffle { q } else { TrackQuery { sort: Some(TrackSort::Added), order: Some(SortDir::Desc), ..q } };
            // plain play: newest first; shuffle: an unbiased draw of the whole pool
            let mut q = q;
            q.limit = Some(500);
            if shuffle {
                q.sort = Some(TrackSort::Random);
            }
            match api::get::<TrackPage>(&format!("/tracks{}", crate::util::qs_pairs(&q.to_pairs()))).await {
                Ok(p) => play_items(player, &p.page.items, 0, None, shuffle),
                Err(e) => toast_err(&e.message()),
            }
            busy.set(None);
        });
    });
    let (a, b) = (run.clone(), run);
    let saving = RwSignal::new(false);
    let save = move |_| {
        saving.set(true);
        spawn_local(async move {
            let body = PlaylistFromTracks { name: Some("Favourites".into()), filter: TrackQuery { favorites: Some(true), ..Default::default() } };
            match api::post::<_, PlaylistOut>("/playlists/from-tracks", &body).await {
                Ok(p) => {
                    toast_ok(&format!("Saved \"{}\" ({} tracks)", p.name, format_count(p.track_count)));
                    crate::data::invalidate_entity("playlist", &[]);
                }
                Err(e) => toast_err(&e.message()),
            }
            saving.set(false);
        });
    };
    view! {
        <div class="hm-panel-btns">
            <button type="button" class="hm-pill primary" disabled=move || busy.get().is_some() on:click=move |_| a(false)><Icon name="play" size=11 />"Play all"</button>
            <button type="button" class="lib-roundbtn small" aria-label="Shuffle the favourites" title="Shuffle the favourites" disabled=move || busy.get().is_some() on:click=move |_| b(true)><Icon name="shuffle" size=12 /></button>
            <button type="button" class="hm-pill" disabled=move || saving.get() title="Freeze the pinned artists, labels and tags into a playlist" on:click=save><Icon name="list" size=11 />"Save"</button>
        </div>
    }
}

const SWEEP_TITLE: &str = "Checks every pinned artist and label on Bandcamp for records you do not have and queues them for download. Pinned tags are not checked: a tag has no catalogue page. Runs in the background; you can stop it at any time.";

#[component]
fn SweepAction() -> impl IntoView {
    let status = RwSignal::new(None::<SweepStatus>);
    let report = RwSignal::new(false);
    let starting = RwSignal::new(false);
    let start_err = RwSignal::new(None::<String>);
    spawn_local(async move {
        if let Ok(s) = api::get::<SweepStatus>("/harvest/favorites/sweep").await {
            status.set(Some(s));
        }
    });
    use_topic::<SweepStatus>("favorites.sweep", move |s| {
        let was = status.with_untracked(|p| p.as_ref().map(|p| p.running).unwrap_or(false));
        if was && !s.running {
            report.set(true);
            crate::data::invalidate_entity("job", &[]);
        }
        status.set(Some(s));
    });
    let running = Memo::new(move |_| status.with(|s| s.as_ref().map(|s| s.running).unwrap_or(false)));
    let start = move |_| {
        starting.set(true);
        start_err.set(None);
        spawn_local(async move {
            match api::post::<_, SweepStatus>("/harvest/favorites/sweep", &serde_json::json!({})).await {
                Ok(s) => status.set(Some(s)),
                Err(e) => start_err.set(Some(e.message())),
            }
            starting.set(false);
        });
    };
    let stop = move |_| {
        spawn_local(async move {
            let _ = api::call("DELETE", "/harvest/favorites/sweep").await;
            if let Ok(s) = api::get::<SweepStatus>("/harvest/favorites/sweep").await {
                status.set(Some(s));
            }
        });
    };
    let note = Memo::new(move |_| -> Option<(bool, String)> {
        if let Some(e) = start_err.get() {
            return Some((true, e));
        }
        let s = status.get()?;
        match s.phase.as_str() {
            "harvesting" => {
                let found = if s.new > 0 { format!(" · {} new", format_count(s.new)) } else { String::new() };
                Some((false, format!("{} of {}{}{}", format_count(s.done), format_count(s.total.unwrap_or(0)), s.current.map(|c| format!(" · {c}")).unwrap_or_default(), found)))
            }
            "queueing" => Some((false, "Queueing what it found…".into())),
            "failed" if report.get() => Some((true, s.error.unwrap_or_else(|| "The check failed.".into()))),
            "done" if report.get() => {
                let stopped = s.error.as_deref() == Some("Stopped");
                let lead = if stopped { format!("Stopped at {} of {}", format_count(s.done), format_count(s.total.unwrap_or(0))) } else { format!("Checked {}", format_count(s.total.unwrap_or(0))) };
                let skipped = if s.no_url > 0 { format!(" · {} with no Bandcamp page skipped", format_count(s.no_url)) } else { String::new() };
                Some((false, format!("{lead} · {} new · {} queued{skipped}", format_count(s.new), format_count(s.queued))))
            }
            _ => None,
        }
    });
    view! {
        <div class="hm-sweep">
            <div class="row">
                <button type="button" class="hm-sweepbtn" title=SWEEP_TITLE disabled=move || running.get() || starting.get() on:click=start>
                    <Icon name=move || if running.get() { "refresh" } else { "download" } size=12 />
                    {move || if running.get() { "Checking favourites…" } else { "Check all for new releases" }}
                </button>
                {move || running.get().then(|| view! {
                    <button type="button" class="lib-roundbtn small" aria-label="Stop checking" title="Stop the walk: whatever it has already found is still queued" on:click=stop><Icon name="x" size=12 /></button>
                })}
            </div>
            {move || note.get().map(|(err, t)| view! {
                <p class=if err { "hm-sweepnote danger-text" } else { "hm-sweepnote faint" }>{t}
                    {move || (!running.get() && status.with(|s| s.as_ref().map(|s| s.phase == "done" && s.queued > 0).unwrap_or(false))).then(|| view! { " " <a class="lib-link" href="/downloads">"Downloads"</a> })}
                </p>
            })}
        </div>
    }
}

#[component]
fn ArtistCard(a: ArtistOut) -> impl IntoView {
    let player = use_player();
    let id = a.id;
    view! {
        <div class="hm-fav">
            <a class="hm-fav-link" href=format!("/artists/{id}") aria-label=a.name.clone()></a>
            <span class="hm-fav-art round"><Art src=a.art_url.clone() class="alb-art" /></span>
            <span class="hm-fav-name truncate">{a.name.clone()}</span>
            <span class="mono faint hm-fav-sub truncate">{format!("{} release{}", format_count(a.release_count), if a.release_count == 1 { "" } else { "s" })}</span>
            <PlayOverlay label=format!("Play {}", a.name) on_play=Callback::new(move |_| { spawn_local(play_filter(player, TrackQuery { artist_id: Some(id), ..Default::default() }, false)); }) />
        </div>
    }
}

#[component]
fn LabelCard(l: LabelOut) -> impl IntoView {
    let player = use_player();
    let id = l.id;
    let arts: Vec<String> = l.art_urls.iter().take(4).cloned().collect();
    let collage = arts.len() >= 2;
    view! {
        <div class="hm-fav">
            <a class="hm-fav-link" href=format!("/labels/{id}") aria-label=l.name.clone()></a>
            <span class="hm-fav-art" class:collage=collage>
                {if arts.is_empty() { view! { <span class="hm-fav-ph"><Icon name="folder" /></span> }.into_any() }
                 else if arts.len() == 1 { view! { <img src=crate::pages::albums::logic::art_size(&arts[0], "thumb") alt="" loading="lazy" /> }.into_any() }
                 else { (0..4).map(|i| view! { <img src=crate::pages::albums::logic::art_size(&arts[i % arts.len()], "thumb") alt="" loading="lazy" /> }).collect_view().into_any() }}
            </span>
            <span class="hm-fav-name truncate">{l.name.clone()}</span>
            <span class="mono faint hm-fav-sub truncate">{format!("{} release{}", format_count(l.release_count), if l.release_count == 1 { "" } else { "s" })}</span>
            <PlayOverlay label=format!("Play {}", l.name) on_play=Callback::new(move |_| { spawn_local(play_filter(player, TrackQuery { label_id: Some(id), ..Default::default() }, false)); }) />
        </div>
    }
}

#[component]
pub fn Favorites(shelves: Shelves, layout: Layout) -> impl IntoView {
    view! {
        {move || shelves.get().and_then(|s| {
            let f = &s.favorites;
            if f.artists.len() + f.labels.len() + f.tags.len() == 0 {
                return None;
            }
            let has_cards = !f.artists.is_empty() || !f.labels.is_empty();
            let cards = f.artists.iter().cloned().map(|a| view! { <ArtistCard a=a /> }).collect_view();
            let cards2 = f.labels.iter().cloned().map(|l| view! { <LabelCard l=l /> }).collect_view();
            let chips = f.tags.iter().map(|t| view! {
                <a class="lib-pill" href=format!("/tracks?tag={}", crate::util::enc(&t.name))>{t.name.clone()}<span class="mono faint">{format_count(t.track_count)}</span></a>
            }).collect_view();
            let sweep = has_cards.then(|| view! { <SweepAction /> });
            Some(match layout {
                Layout::Rail => view! {
                    <section class="hm-card" aria-label="Favourites">
                        <div class="hm-card-head wrap"><h2 class="hm-card-title"><span class="hm-panel-icon"><Icon name="heart-fill" /></span>"Favourites"</h2><FavoritesControls /></div>
                        {has_cards.then(|| view! { <div class="hm-fav-scroll"><div class="hm-fav-grid rail">{cards}{cards2}</div></div> })}
                        {(!f.tags.is_empty()).then(|| view! { <div class="hm-chips tight">{chips}</div> })}
                        {sweep}
                        <p class="faint hm-hint">"Heart an artist, label or tag anywhere to pin it here"</p>
                    </section>
                }.into_any(),
                Layout::Band => view! {
                    <section class="hm-sec" aria-label="Favourites">
                        <div class="hm-sec-head"><h2 class="hm-sec-title"><span class="hm-panel-icon"><Icon name="heart-fill" /></span>"Favourites"</h2><div class="hm-sec-actions"><FavoritesControls /></div></div>
                        {has_cards.then(|| view! { <div class="lib-row"><div class="hm-fav-grid band">{cards}{cards2}</div></div> })}
                        {(!f.tags.is_empty()).then(|| view! { <div class="hm-chips tight">{chips}</div> })}
                        {sweep}
                        <p class="faint hm-hint">"Heart an artist, label or tag anywhere to pin it here"</p>
                    </section>
                }.into_any(),
            })
        })}
    }
}
