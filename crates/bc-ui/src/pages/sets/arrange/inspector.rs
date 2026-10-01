//! Inspector of the Arrange view: numeric cues, tempo, key lock, the transition into the selected
//! clip (type, length, verdicts, notes) and the track's cue points. Every control maps to one PATCH
//! field, committed on blur / Enter / release (never per keystroke).

use std::rc::Rc;
use std::sync::Arc;

use bc_types::analysis::{CueKind, TrackMusicInfo, Verdict};
use bc_types::sets::{DjSetDetail, SetItemOut, TransitionOut};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Value, json};

use super::logic;
use super::paint;
use crate::ds::{Badge, Button, Icon, Size, StatusBadge, Switch, Tone, Variant};
use crate::logic::timeline::{BEAT_CHOICES, overlap_ms};
use crate::player::waveform::load_music;
use crate::widgets::common::Art;

type Patch = Callback<(i64, Value)>;

fn tone_of(v: Verdict) -> Tone {
    match v {
        Verdict::Perfect | Verdict::Good => Tone::Ok,
        Verdict::Energy => Tone::Info,
        Verdict::Risky => Tone::Warn,
        Verdict::Clash => Tone::Danger,
    }
}

fn verdict_label(what: &str, v: Verdict) -> String {
    format!("{what}: {}", v.as_str())
}

/// A seconds field (`83.5` or `1:23.5`), committed on blur/Enter; empty clears.
#[component]
fn SecField(
    #[prop(into)] label: String,
    #[prop(into)] value: Signal<Option<i64>>,
    tick: RwSignal<u32>,
    on_commit: Callback<Option<i64>>,
    #[prop(optional, into)] placeholder: String,
) -> impl IntoView {
    let text = RwSignal::new(String::new());
    Effect::new(move |_| {
        tick.get();
        text.set(value.get().map(|ms| logic::fmt_clock_tenths(ms as f64 / 1000.0)).unwrap_or_default());
    });
    let commit = move || match logic::parse_seconds(&text.get_untracked()) {
        Some(v) if v != value.get_untracked() => on_commit.run(v),
        Some(_) => text.set(value.get_untracked().map(|ms| logic::fmt_clock_tenths(ms as f64 / 1000.0)).unwrap_or_default()),
        None => {
            crate::ds::toast_warn("Enter seconds like 83.5 or 1:23.5");
            tick.update(|t| *t += 1);
        }
    };
    view! {
        <label class="arr-field">
            <span>{label}</span>
            <input
                class="input num"
                type="text"
                inputmode="decimal"
                placeholder=placeholder
                prop:value=move || text.get()
                on:input=move |ev| text.set(event_target_value(&ev))
                on:blur=move |_| commit()
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    if ev.key() == "Enter" {
                        commit();
                    }
                    ev.stop_propagation();
                }
            />
        </label>
    }
}

fn curve_svg(kind: &str) -> String {
    let (w, h, n) = (44.0, 22.0, 24);
    let mut out = String::new();
    for (which, dash) in [(0, "3 2"), (1, "")] {
        let mut d = String::new();
        for i in 0..=n {
            let u = i as f64 / n as f64;
            let (go, gi) = paint::curve(kind, u);
            let g = if which == 0 { go } else { gi }.clamp(0.0, 1.0);
            d.push_str(&format!("{}{:.1},{:.1} ", if i == 0 { "M" } else { "L" }, 2.0 + u * (w - 4.0), 2.0 + (1.0 - g) * (h - 4.0)));
        }
        out.push_str(&format!("<path d=\"{d}\" fill=\"none\" stroke=\"currentColor\" stroke-width=\"1.6\" stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-dasharray=\"{dash}\"/>"));
    }
    out
}

#[component]
fn KindPicker(kind: Signal<String>, has_bpm: Signal<bool>, beats: Signal<u32>, on_pick: Callback<serde_json::Value>) -> impl IntoView {
    view! {
        <div class="arr-kinds" role="radiogroup" aria-label="Transition type">
            {logic::TRANSITION_KINDS
                .iter()
                .map(|(k, label)| {
                    let k = *k;
                    view! {
                        <button
                            type="button"
                            role="radio"
                            class="arr-kind"
                            aria-checked=move || (kind.get() == k).to_string()
                            on:click=move |_| on_pick.run(logic::kind_patch(k, Some(beats.get_untracked()), has_bpm.get_untracked()))
                        >
                            <svg viewBox="0 0 44 22" width="44" height="22" aria-hidden="true" inner_html=curve_svg(k)></svg>
                            <span>{*label}</span>
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
}

#[component]
fn Body(
    id: i64,
    cur: RwSignal<Option<Arc<DjSetDetail>>>,
    sel_band: RwSignal<bool>,
    patch: Patch,
    tick: RwSignal<u32>,
    on_play: Callback<usize>,
    on_zoom: Callback<()>,
    on_remove: Callback<i64>,
) -> impl IntoView {
    let item: Memo<Option<SetItemOut>> = Memo::new(move |_| cur.with(|d| d.as_ref().and_then(|d| d.items.iter().find(|i| i.id == id).cloned())));
    let trans: Memo<Option<TransitionOut>> = Memo::new(move |_| {
        let i = item.get()?;
        if i.index == 0 {
            return None;
        }
        cur.with(|d| d.as_ref().and_then(|d| d.transitions.get(i.index - 1).cloned()))
    });
    let outgoing: Memo<Option<SetItemOut>> = Memo::new(move |_| {
        let i = item.get()?;
        if i.index == 0 {
            return None;
        }
        cur.with(|d| d.as_ref().and_then(|d| d.items.get(i.index - 1).cloned()))
    });
    let field = |f: fn(&SetItemOut) -> Option<i64>| Signal::derive(move || item.get().and_then(|i| f(&i)));
    let cue_in = field(|i| i.cue_in_ms);
    let cue_out = field(|i| i.cue_out_ms);
    let dur = Signal::derive(move || item.get().and_then(|i| i.duration_ms));
    let eff_bpm = Signal::derive(move || item.get().and_then(|i| i.effective_bpm));
    let beats = Signal::derive(move || item.get().and_then(|i| i.transition_beats).unwrap_or(0).max(0) as u32);
    let kind = Signal::derive(move || {
        let i = item.get();
        let b = i.as_ref().and_then(|i| i.transition_beats).unwrap_or(0);
        if b == 0 { "cut".to_string() } else { i.and_then(|i| i.transition_type).unwrap_or_else(|| "blend".into()) }
    });
    let has_bpm = Signal::derive(move || eff_bpm.get().is_some());

    // tempo
    let tempo_text = RwSignal::new(String::new());
    Effect::new(move |_| {
        tick.get();
        tempo_text.set(item.get().map(|i| format!("{}", i.tempo_adjust_pct)).unwrap_or_default());
    });
    let commit_tempo = move || match tempo_text.get_untracked().trim().parse::<f64>() {
        Ok(v) if (-10.0..=10.0).contains(&v) => {
            if item.get_untracked().is_some_and(|i| (i.tempo_adjust_pct - v).abs() > 1e-9) {
                patch.run((id, json!({ "tempo_adjust_pct": v })));
            }
        }
        _ => {
            crate::ds::toast_warn("Tempo adjust is between -10 and +10 %");
            tick.update(|t| *t += 1);
        }
    };

    // music info (cue points)
    let music: RwSignal<Option<Rc<TrackMusicInfo>>, LocalStorage> = RwSignal::new_local(None);
    Effect::new(move |_| {
        let tid = item.with(|i| i.as_ref().and_then(|i| i.track_id));
        music.set(None);
        if let Some(t) = tid {
            spawn_local(async move {
                let m = load_music(t).await;
                if item.get_untracked().and_then(|i| i.track_id) == Some(t) {
                    music.set(m);
                }
            });
        }
    });

    let notes = RwSignal::new(String::new());
    Effect::new(move |_| {
        tick.get();
        notes.set(item.get().and_then(|i| i.transition_notes).unwrap_or_default());
    });
    let commit_notes = move || {
        let v = notes.get_untracked().trim().to_string();
        let old = item.get_untracked().and_then(|i| i.transition_notes).unwrap_or_default();
        if v != old {
            patch.run((id, json!({ "transition_notes": if v.is_empty() { Value::Null } else { Value::String(v) } })));
        }
    };

    let key_lock = RwSignal::new(false);
    Effect::new(move |_| key_lock.set(item.get().is_some_and(|i| i.key_lock)));

    let cue_rows = move || {
        let info = music.get();
        let it = item.get();
        let (Some(info), Some(it)) = (info, it) else { return view! { <p class="arr-muted">"No cue points loaded."</p> }.into_any() };
        let mut rows: Vec<(String, f64, &'static str)> = vec![];
        if let Some(mp) = info.mix_points {
            rows.push(("Mix in".into(), mp.cue_in_ms as f64, "ok"));
            if let Some(o) = mp.cue_out_ms {
                rows.push(("Mix out".into(), o as f64, "danger"));
            }
        }
        for c in &info.cues {
            if matches!(c.kind, CueKind::MixIn | CueKind::MixOut) {
                continue;
            }
            let name = c.label.clone().filter(|l| !l.is_empty()).unwrap_or_else(|| format!("{:?}", c.kind));
            rows.push((name, c.pos_ms, "warn"));
        }
        if rows.is_empty() {
            return view! { <p class="arr-muted">"This track has no cue points."</p> }.into_any();
        }
        let _ = it;
        view! {
            <ul class="arr-cues">
                {rows
                    .into_iter()
                    .map(|(name, ms, tone)| {
                        let ms_i = ms.round() as i64;
                        view! {
                            <li>
                                <span class=format!("arr-dot {tone}") aria-hidden="true"></span>
                                <span class="arr-cue-name">{name}</span>
                                <span class="num arr-cue-time">{logic::fmt_clock_tenths(ms / 1000.0)}</span>
                                <Button size=Size::Sm variant=Variant::Ghost title="Use as cue in" on_click=Callback::new(move |_| patch.run((id, json!({ "cue_in_ms": ms_i }))))>"In"</Button>
                                <Button size=Size::Sm variant=Variant::Ghost title="Use as cue out" on_click=Callback::new(move |_| patch.run((id, json!({ "cue_out_ms": ms_i }))))>"Out"</Button>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        }
        .into_any()
    };

    view! {
        {move || item.get().map(|it| {
            let idx = it.index;
            let art = it.art_url.clone();
            let title = if it.title.is_empty() { "(missing track)".to_string() } else { it.title.clone() };
            let artist = it.artist.clone();
            let bpm = it.effective_bpm.map(|b| format!("{b:.1} BPM"));
            let camelot = it.effective_camelot.clone();
            let missing = it.missing;
            view! {
                <header class="arr-insp-head">
                    <Art src=art size=44.0 />
                    <div class="arr-insp-title">
                        <h3><span class="num arr-idx">{idx + 1}</span>{title}</h3>
                        <p>{artist}</p>
                    </div>
                    <div class="arr-insp-badges">
                        {bpm.map(|b| view! { <Badge>{b}</Badge> })}
                        {camelot.map(|k| view! { <Badge>{k}</Badge> })}
                        {missing.then(|| view! { <Badge tone=Tone::Danger icon="alert">"File missing"</Badge> })}
                    </div>
                    <div class="arr-insp-actions">
                        <Button size=Size::Sm icon="play" title="Play the set from this track (Enter)" on_click=Callback::new(move |_| on_play.run(idx))>"Play"</Button>
                        <Button size=Size::Sm icon="zoom-in" title="Zoom the timeline to this clip (F)" on_click=Callback::new(move |_| on_zoom.run(()))>"Zoom"</Button>
                        <Button size=Size::Sm variant=Variant::Danger icon="trash" title="Remove from the set (Delete)" on_click=Callback::new(move |_| on_remove.run(id))>"Remove"</Button>
                    </div>
                </header>
            }
        })}
        <div class="arr-insp-grid">
            <div class="arr-card">
                <h4>"Clip"</h4>
                <div class="arr-row">
                    <SecField label="Cue in" value=cue_in tick=tick on_commit=Callback::new(move |v: Option<i64>| patch.run((id, json!({ "cue_in_ms": v })))) placeholder="start" />
                    <SecField label="Cue out" value=cue_out tick=tick on_commit=Callback::new(move |v: Option<i64>| patch.run((id, json!({ "cue_out_ms": v })))) placeholder="end" />
                </div>
                <p class="arr-muted num">
                    {move || dur.get().map(|d| format!("Track length {}", logic::fmt_clock_tenths(d as f64 / 1000.0)))}
                    {move || item.get().map(|i| format!(" \u{b7} plays {}", logic::fmt_clock_tenths(i.played_ms as f64 / 1000.0)))}
                </p>
                <div class="arr-row arr-tempo">
                    <label class="arr-field">
                        <span>"Tempo %"</span>
                        <input
                            class="input num"
                            type="text"
                            inputmode="decimal"
                            prop:value=move || tempo_text.get()
                            on:input=move |ev| tempo_text.set(event_target_value(&ev))
                            on:blur=move |_| commit_tempo()
                            on:keydown=move |ev: web_sys::KeyboardEvent| {
                                if ev.key() == "Enter" {
                                    commit_tempo();
                                }
                                ev.stop_propagation();
                            }
                        />
                    </label>
                    <input
                        class="arr-slider"
                        type="range"
                        min="-10"
                        max="10"
                        step="0.5"
                        aria-label="Tempo adjust, percent"
                        prop:value=move || tempo_text.get()
                        on:input=move |ev| tempo_text.set(event_target_value(&ev))
                        on:change=move |_| commit_tempo()
                    />
                    <Button size=Size::Sm variant=Variant::Ghost icon="refresh" title="Reset tempo to 0 %" on_click=Callback::new(move |_| patch.run((id, json!({ "tempo_adjust_pct": 0.0 }))))>"Reset"</Button>
                </div>
                <div class="arr-row arr-lock">
                    <span id=format!("arr-kl-{id}")>"Key lock"</span>
                    <Switch value=key_lock label="Key lock" on_change=Callback::new(move |v: bool| patch.run((id, json!({ "key_lock": v })))) />
                    <span class="arr-muted">{move || if key_lock.get() { "Pitch stays when the tempo changes" } else { "Pitch follows the tempo" }}</span>
                </div>
            </div>

            {move || trans.get().map(|t| {
                let out_title = outgoing.get().map(|o| o.title).unwrap_or_default();
                let (kv, bv) = (t.key_verdict, t.bpm_verdict);
                let (kr, br) = (t.key_reason.clone(), t.bpm_reason.clone());
                let delta = t.tempo_delta_pct;
                view! {
                    <div class="arr-card" class:is-focus=move || sel_band.get()>
                        <h4>"Transition in" <span class="arr-muted">" from " {out_title}</span></h4>
                        <KindPicker kind=kind has_bpm=has_bpm beats=beats on_pick=Callback::new(move |b: Value| patch.run((id, b))) />
                        <div class="arr-beats" role="radiogroup" aria-label="Transition length in beats">
                            {BEAT_CHOICES
                                .iter()
                                .map(|b| {
                                    let b = *b;
                                    let is_off = move || b > 0 && !has_bpm.get();
                                    let ttl = move || if has_bpm.get() { String::new() } else { "Needs BPM analysis".to_string() };
                                    let checked = move || (beats.get() == b).to_string();
                                    view! {
                                        <button
                                            type="button"
                                            role="radio"
                                            class="arr-beat num"
                                            aria-checked=checked
                                            disabled=is_off
                                            title=ttl
                                            on:click=move |_| {
                                                let ty = kind.get_untracked();
                                                let body = if b == 0 {
                                                    json!({ "transition_beats": null, "transition_type": "cut" })
                                                } else if ty == "cut" {
                                                    json!({ "transition_beats": b, "transition_type": "blend" })
                                                } else {
                                                    json!({ "transition_beats": b })
                                                };
                                                patch.run((id, body));
                                            }
                                        >
                                            {if b == 0 { "Cut".to_string() } else { b.to_string() }}
                                        </button>
                                    }
                                })
                                .collect_view()}
                        </div>
                        <p class="arr-muted num">
                            {move || {
                                let b = beats.get();
                                if b == 0 {
                                    "Hard cut on the downbeat".to_string()
                                } else {
                                    let s = overlap_ms(Some(b), eff_bpm.get()) / 1000.0;
                                    format!("{b} beats \u{b7} {} bars \u{b7} {s:.1}s overlap", b / 4)
                                }
                            }}
                        </p>
                        <div class="arr-verdicts">
                            <div><StatusBadge tone=tone_of(kv) label=verdict_label("Key", kv) /><span class="arr-muted">{kr}</span></div>
                            <div><StatusBadge tone=tone_of(bv) label=verdict_label("Tempo", bv) /><span class="arr-muted">{br}</span></div>
                            <span class="arr-muted num">{format!("Tempo delta {delta:+.1}%")}</span>
                        </div>
                        <label class="arr-field arr-notes">
                            <span>"Notes"</span>
                            <input
                                class="input"
                                type="text"
                                placeholder="e.g. bass swap on the drop"
                                prop:value=move || notes.get()
                                on:input=move |ev| notes.set(event_target_value(&ev))
                                on:blur=move |_| commit_notes()
                                on:keydown=move |ev: web_sys::KeyboardEvent| {
                                    if ev.key() == "Enter" {
                                        commit_notes();
                                    }
                                    ev.stop_propagation();
                                }
                            />
                        </label>
                    </div>
                }
            })}

            <div class="arr-card">
                <h4>"Cue points"</h4>
                {cue_rows}
            </div>
        </div>
    }
}

#[component]
pub fn Inspector(
    cur: RwSignal<Option<Arc<DjSetDetail>>>,
    sel: RwSignal<Option<i64>>,
    sel_band: RwSignal<bool>,
    patch: Patch,
    tick: RwSignal<u32>,
    on_play: Callback<usize>,
    on_zoom: Callback<()>,
    on_remove: Callback<i64>,
) -> impl IntoView {
    view! {
        <section class="arr-insp" aria-label="Inspector">
            {move || match sel.get() {
                Some(id) => view! { <Body id=id cur=cur sel_band=sel_band patch=patch tick=tick on_play=on_play on_zoom=on_zoom on_remove=on_remove /> }.into_any(),
                None => view! {
                    <div class="arr-insp-empty">
                        <Icon name="sliders" />
                        <div>
                            <strong>"Select a clip or a transition"</strong>
                            <p>"Click a track to trim and tune it, or a transition chip to change how it blends. Arrow keys move the selection."</p>
                        </div>
                    </div>
                }
                .into_any(),
            }}
        </section>
    }
}
