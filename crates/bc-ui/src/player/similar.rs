//! "More like this" (key `s`), beside the page: the seed track (follows the player, or pinned),
//! the signals it is matched on, and the tracks themselves, each saying why it is here.
//! Where the planner asks "what mixes out of this", this asks "what else is this kind of music".
mod actions;
mod seed_header;
mod signals;

use bc_types::player::{PlayerCommand, QueueItem};
use bc_types::suggest::{SimilarRequest, SimilarResponse, Signals};
use leptos::prelude::*;

use crate::api;
use crate::ds::{Button, Skeleton};
use crate::player::plan::suggest_body::{exclude_ids, is_library};
use crate::player::plan::track_row::{RowTrack, TrackRowView};
use crate::player::use_player;

pub const PAGE: i64 = 30;
const LS_KEY: &str = "bc:similar:v1";

pub(crate) const SIGNAL_KEYS: [&str; 6] = ["tags", "artist", "label", "tempo", "key", "loved"];

pub(crate) fn signal_get(s: &Signals, k: &str) -> bool {
    match k {
        "tags" => s.tags,
        "artist" => s.artist,
        "label" => s.label,
        "tempo" => s.tempo,
        "key" => s.key,
        _ => s.loved,
    }
}
pub(crate) fn signal_toggle(s: &mut Signals, k: &str) {
    match k {
        "tags" => s.tags = !s.tags,
        "artist" => s.artist = !s.artist,
        "label" => s.label = !s.label,
        "tempo" => s.tempo = !s.tempo,
        "key" => s.key = !s.key,
        _ => s.loved = !s.loved,
    }
}

/// The request behind "more like this track" (port of `similarBody.ts`). A Bandcamp stream has
/// no row on the server so its id is never sent, but its tags alone are enough to rank against.
pub fn similar_request(seed: &QueueItem, signals: Signals, exclude: Vec<i64>, limit: i64, offset: i64, shuffle_seed: i64) -> SimilarRequest {
    SimilarRequest {
        seed_track_id: is_library(seed).then_some(seed.track_id),
        seed: Some(bc_types::suggest::SeedOverride { bpm: seed.bpm, camelot: seed.camelot.clone(), energy: seed.energy, tags: seed.tags.clone() }),
        signals,
        exclude_track_ids: exclude,
        limit,
        offset,
        shuffle_seed,
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SimilarCtx {
    pub signals: RwSignal<Signals>,
    pub pinned: RwSignal<Option<QueueItem>>,
    pub shuffle_seed: RwSignal<i64>,
    pub limit: RwSignal<i64>,
    pub data: RwSignal<Option<SimilarResponse>>,
    pub loading: RwSignal<bool>,
    pub error: RwSignal<bool>,
}

fn load_signals() -> Signals {
    crate::util::ls_get(LS_KEY).and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()).and_then(|v| serde_json::from_value(v.get("signals")?.clone()).ok()).unwrap_or_default()
}

#[component]
pub fn SimilarPanel() -> impl IntoView {
    let player = use_player();
    let ctx = SimilarCtx {
        signals: RwSignal::new(load_signals()),
        pinned: RwSignal::new(None),
        shuffle_seed: RwSignal::new(0),
        limit: RwSignal::new(PAGE),
        data: RwSignal::new(None),
        loading: RwSignal::new(false),
        error: RwSignal::new(false),
    };
    Effect::new(move |_| {
        let s = ctx.signals.get();
        crate::util::ls_set(LS_KEY, &serde_json::json!({ "signals": s }).to_string());
    });
    // follow the player by default; the pin is the escape hatch
    let seed = Memo::new(move |_| ctx.pinned.get().or_else(|| player.state.with(|s| s.current.clone())));
    let exclude = Memo::new(move |_| {
        player.state.with(|s| {
            let from = ((s.queue_index + 1).max(0) as usize).min(s.queue.len());
            let up: Vec<&QueueItem> = s.queue[from..].iter().collect();
            exclude_ids(s.current.as_ref(), &up, &s.history, &s.queue)
        })
    });
    let body = Memo::new(move |_| seed.get().map(|s| similar_request(&s, ctx.signals.get(), exclude.get(), ctx.limit.get(), 0, ctx.shuffle_seed.get())));
    let stamp = StoredValue::new(0u64);
    let last = StoredValue::new(String::new());
    Effect::new(move |_| {
        let Some(b) = body.get() else {
            ctx.data.set(None);
            return;
        };
        let key = serde_json::to_string(&b).unwrap_or_default();
        if key == last.get_value() {
            return;
        }
        last.set_value(key);
        stamp.update_value(|s| *s += 1);
        let g = stamp.get_value();
        ctx.loading.set(true);
        leptos::task::spawn_local(async move {
            match api::post::<_, SimilarResponse>("/suggest/similar", &b).await {
                Ok(r) => {
                    if stamp.try_get_value() == Some(g) {
                        ctx.error.set(false);
                        ctx.data.set(Some(r));
                    }
                }
                Err(_) => {
                    let _ = ctx.error.try_set(true);
                }
            }
            let _ = ctx.loading.try_set(false);
        });
    });

    // Escape closes the panel (a menu or dialog above owns it first; a filled search box clears itself)
    let app = crate::app::use_app();
    let handle = window_event_listener(leptos::ev::keydown, move |e| {
        if e.key() != "Escape" || crate::util::document().query_selector("[role='menu'],[role='dialog']").ok().flatten().is_some() {
            return;
        }
        app.panel.set(None);
    });
    on_cleanup(move || handle.remove());

    let items = Signal::derive(move || ctx.data.with(|d| d.as_ref().map(|d| d.items.clone()).unwrap_or_default()));
    let tracks = Memo::new(move |_| items.with(|i| i.iter().map(|x| RowTrack::from(&x.track).queue_item()).collect::<Vec<_>>()));
    let more = Signal::derive(move || items.with(|i| i.len() as i64) >= ctx.limit.get());
    let pool_size = Signal::derive(move || ctx.data.with(|d| d.as_ref().map(|d| d.pool_size)));

    view! {
        <div class="rp-body plan-panel sim">
            {move || seed.get().map(|s| view! { <seed_header::SeedHeader ctx=ctx seed=s /> })}
            {move || seed.get().map(|s| view! { <actions::SimilarActions ctx=ctx tracks=tracks seed=s /> })}
            <signals::SignalChips ctx=ctx />
            <div class=move || if ctx.loading.get() && ctx.data.with(|d| d.is_some()) { "pp-sug busy" } else { "pp-sug" }>
                {move || {
                    if seed.with(|s| s.is_none()) {
                        return view! { <div class="pp-empty">"Play something to see what else sounds like it."</div> }.into_any();
                    }
                    let list = items.get();
                    if list.is_empty() {
                        if ctx.data.with(|d| d.is_none()) && !ctx.error.get() {
                            return view! { <div class="pp-skel">{(0..5).map(|_| view! { <Skeleton height="48px" /> }).collect_view()}</div> }.into_any();
                        }
                        if ctx.error.get() {
                            return view! { <div class="pp-empty bad">"Similar tracks are unavailable right now."</div> }.into_any();
                        }
                        let no_tags = seed.with(|s| s.as_ref().map(|s| s.tags.is_empty()).unwrap_or(false));
                        return view! {
                            <div class="pp-empty">
                                {if no_tags { view! { "This track carries no tags, so there is nothing to match on. "<a href="/analysis">"Run analysis"</a>" or check its metadata." }.into_any() }
                                 else { view! { "Nothing matches under these signals. Switch a few back on." }.into_any() }}
                            </div>
                        }.into_any();
                    }
                    list.into_iter().map(|it| {
                        let rt: RowTrack = (&it.track).into();
                        let q = rt.queue_item();
                        let (q1, q2, q3) = (q.clone(), q.clone(), q);
                        let why: Vec<String> = it.why.iter().take(3).cloned().collect();
                        let title = rt.title.clone();
                        view! {
                            <div class="pp-row">
                                <TrackRowView track=rt
                                    below=crate::ds::children(move || {
                                        let why = why.clone();
                                        (!why.is_empty()).then(|| view! { <span class="trow-why">{why.into_iter().map(|w| view! { <span class="faint truncate">{w}</span> }).collect_view()}</span> })
                                    })
                                    trail=crate::ds::children(move || {
                                        let (q1, q2, q3, t) = (q1.clone(), q2.clone(), q3.clone(), title.clone());
                                        view! {
                                            <span class="trow-trail">
                                                <button type="button" class="btn btn-ghost btn-sm btn-icon" title="Play now" aria-label=format!("Play {t}")
                                                    on:click=move |_| player.cmd(PlayerCommand::PlayTrack { item: q1.clone(), queue: None })><crate::ds::Icon name="play" size=13 /></button>
                                                <button type="button" class="btn btn-ghost btn-sm btn-icon" title="Play next" aria-label="Play next"
                                                    on:click=move |_| player.cmd(PlayerCommand::PlayNext { items: vec![q2.clone()] })><crate::ds::Icon name="skip-next" size=13 /></button>
                                                <button type="button" class="btn btn-ghost btn-sm btn-icon" title="Add to end" aria-label="Add to the end"
                                                    on:click=move |_| player.cmd(PlayerCommand::AddToQueue { items: vec![q3.clone()] })><crate::ds::Icon name="queue" size=13 /></button>
                                            </span>
                                        }
                                    }) />
                            </div>
                        }
                    }).collect_view().into_any()
                }}
                <Show when=move || more.get()>
                    <div class="pp-morewrap"><Button on_click=move |_| ctx.limit.update(|l| *l += PAGE) busy=ctx.loading>{format!("Show {PAGE} more")}</Button></div>
                </Show>
                {move || pool_size.get().map(|n| view! { <div class="pp-foot faint mono">{format!("ranked from {} candidates", crate::logic::format::format_count(n))}</div> })}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_types::player::ItemOrigin;

    fn track(id: i64) -> QueueItem {
        QueueItem { track_id: id, title: "Nocturne".into(), bpm: Some(128.0), camelot: Some("8A".into()), energy: Some(0.6), tags: vec!["techno".into(), "dub techno".into()], ..Default::default() }
    }

    #[test]
    fn library_track_sends_the_id() {
        assert_eq!(similar_request(&track(42), Signals::default(), vec![], 30, 0, 0).seed_track_id, Some(42));
    }

    #[test]
    fn stream_withholds_id_but_sends_what_it_knows() {
        let mut t = track(-7);
        t.origin = ItemOrigin::Bandcamp;
        let b = similar_request(&t, Signals::default(), vec![], 30, 0, 0);
        assert_eq!(b.seed_track_id, None);
        let s = b.seed.unwrap();
        assert_eq!((s.bpm, s.camelot.as_deref(), s.energy), (Some(128.0), Some("8A"), Some(0.6)));
        assert_eq!(s.tags, vec!["techno".to_string(), "dub techno".to_string()]);
    }

    #[test]
    fn override_always_sent() {
        assert_eq!(similar_request(&track(42), Signals::default(), vec![], 30, 0, 0).seed.unwrap().tags, vec!["techno".to_string(), "dub techno".to_string()]);
    }

    #[test]
    fn signals_pass_through_verbatim() {
        let off = Signals { label: false, key: false, ..Signals::default() };
        assert_eq!(similar_request(&track(42), off, vec![], 30, 0, 0).signals, off);
    }

    #[test]
    fn paging_reroll_and_excludes_are_carried() {
        let b = similar_request(&track(42), Signals::default(), vec![1, 2], 60, 30, 3);
        assert_eq!((b.exclude_track_ids, b.limit, b.offset, b.shuffle_seed), (vec![1, 2], 60, 30, 3));
    }

    #[test]
    fn defaults_to_first_page_without_reroll() {
        let b = similar_request(&track(42), Signals::default(), vec![], PAGE, 0, 0);
        assert_eq!((b.offset, b.shuffle_seed, b.limit), (0, 0, 30));
    }

    #[test]
    fn signal_toggles() {
        let mut s = Signals::default();
        signal_toggle(&mut s, "key");
        assert!(!signal_get(&s, "key") && signal_get(&s, "tags"));
    }
}
