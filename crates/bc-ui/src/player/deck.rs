//! Deck view (`v`): two stacked scrolling detail waveforms (outgoing / incoming) with a
//! fixed centre playhead, beat grid with downbeats and phrase markers, hot cues, phase meter,
//! smooth zoom (wheel, pinch, buttons, keys; 1x-64x), per-deck tempo/key/EQ readouts and the
//! transition controls. Full-screen on phones.
use std::rc::Rc;

use bc_types::analysis::TrackMusicInfo;
use bc_types::player::*;
use bc_waveform::view::{Markers, ViewMode};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::store::{position_now, use_player};
use super::waveform::{Playhead, WaveCanvas, WaveLevel, load_music};
use crate::app::use_app;
use crate::ds::{Button, Icon, Size, Variant};
use crate::logic::format::format_bpm;

/// Zoom range: px per second. 1x ~ 4 px/s (whole-track feel), 64x ~ 256 px/s.
pub const MIN_PX_S: f64 = 4.0;
pub const MAX_PX_S: f64 = 256.0;

pub fn clamp_zoom(z: f64) -> f64 {
    z.clamp(MIN_PX_S, MAX_PX_S)
}

/// Multiplier displayed to the user ("16x").
pub fn zoom_label(px_per_s: f64) -> String {
    let x = px_per_s / MIN_PX_S;
    if x >= 10.0 { format!("{x:.0}x") } else { format!("{x:.1}x") }
}

#[component]
fn Readout(#[prop(into)] k: String, #[prop(into)] v: Signal<String>) -> impl IntoView {
    view! { <div class="dk-ro"><span class="k">{k}</span><span class="v mono">{move || v.get()}</span></div> }
}

#[component]
pub fn DeckView() -> impl IntoView {
    let app = use_app();
    let player = use_player();
    view! { <Show when=move || app.deck_open.get()><DeckLayer player=player /></Show> }
}

#[component]
fn DeckLayer(player: super::store::PlayerCtx) -> impl IntoView {
    let app = use_app();
    let st = player.state;
    let zoom = RwSignal::new(crate::util::ls_get("bc:deck:zoom").and_then(|v| v.parse().ok()).map(clamp_zoom).unwrap_or(64.0));
    Effect::new(move |_| crate::util::ls_set("bc:deck:zoom", &zoom.get().to_string()));
    let style = crate::prefs::wave_style_pref();
    let normalise = RwSignal::new(true);

    let cur_id = Signal::derive(move || st.with(|s| s.current.as_ref().map(|c| c.track_id).filter(|i| *i > 0)));
    let next_item = Signal::derive(move || {
        st.with(|s| {
            let i = s.queue_index + 1;
            if i >= 0 { s.queue.get(i as usize).cloned() } else { None }
        })
    });
    let next_id = Signal::derive(move || next_item.get().map(|n| n.track_id).filter(|i| *i > 0));

    let music_cur: RwSignal<Option<Rc<TrackMusicInfo>>, LocalStorage> = RwSignal::new_local(None);
    let music_next: RwSignal<Option<Rc<TrackMusicInfo>>, LocalStorage> = RwSignal::new_local(None);
    for (id, slot) in [(cur_id, music_cur), (next_id, music_next)] {
        Effect::new(move |_| {
            let tid = id.get();
            slot.set(None);
            if let Some(tid) = tid {
                spawn_local(async move {
                    let m = load_music(tid).await;
                    if id.try_get_untracked().flatten() == Some(tid) {
                        let _ = slot.try_set(m);
                    }
                });
            }
        });
    }
    let markers_for = move |m: RwSignal<Option<Rc<TrackMusicInfo>>, LocalStorage>, out: bool| {
        Signal::derive(move || {
            let mut k = Markers::default();
            if let Some(info) = m.get() {
                k.grid = info.grid.clone();
                k.cues = info.cues.clone();
                k.chapter_ticks = true;
                if let Some(mp) = &info.mix_points {
                    k.mix_in_s = Some(mp.cue_in_ms as f64 / 1000.0);
                    if out {
                        k.mix_out_s = mp.cue_out_ms.map(|v| v as f64 / 1000.0);
                    }
                }
            }
            if out {
                k.mix_out_s = st.with(|s| s.mix_out_override_s.or(s.mix_out_s)).or(k.mix_out_s);
            }
            k
        })
    };
    let mk_cur = markers_for(music_cur, true);
    let mk_next = markers_for(music_next, false);
    let next_start = Signal::derive(move || music_next.get().and_then(|m| m.mix_points).map(|p| p.cue_in_ms as f64 / 1000.0).unwrap_or(0.0));

    // Pan = scrub on the playing deck.
    let on_pan = Callback::new(move |dt: f64| {
        let pos = player.clock.with_untracked(|c| position_now(c, player.clock_at.get_untracked()));
        player.seek((pos + dt).max(0.0));
    });
    let on_zoom = Callback::new(move |f: f64| zoom.update(|z| *z = clamp_zoom(*z * f)));

    let close = move || app.deck_open.set(false);
    let tempo_cur = Signal::derive(move || format_bpm(music_cur.get().and_then(|m| m.bpm)));
    let key_cur = Signal::derive(move || music_cur.get().and_then(|m| m.camelot.clone()).unwrap_or_default());
    let tempo_next = Signal::derive(move || format_bpm(music_next.get().and_then(|m| m.bpm)));
    let key_next = Signal::derive(move || music_next.get().and_then(|m| m.camelot.clone()).unwrap_or_default());
    let rate = Signal::derive(move || player.clock.with(|c| format!("{:+.1}%", (c.rate - 1.0) * 100.0)));
    let phase = Signal::derive(move || {
        player.transition.with(|t| t.as_ref().and_then(|t| t.phase_error_ms).map(|e| format!("{e:+.1} ms")).unwrap_or_else(|| "-".into()))
    });
    let strip = Signal::derive(move || st.with(|s| s.strip.clone()));
    let set_strip = move |patch: StripPatch| player.cmd(PlayerCommand::SetStrip { patch });
    let title_cur = move || st.with(|s| s.current.as_ref().map(|c| c.title.clone()).unwrap_or_else(|| "Nothing playing".into()));
    let title_next = move || next_item.get().map(|n| n.title).unwrap_or_else(|| "Queue ends here".into());

    view! {
        <div class="deck" role="dialog" aria-modal="true" aria-label="Deck view"
            on:keydown=move |ev| match ev.key().as_str() {
                "Escape" | "v" => close(),
                "+" | "=" => zoom.update(|z| *z = clamp_zoom(*z * 1.25)),
                "-" => zoom.update(|z| *z = clamp_zoom(*z / 1.25)),
                _ => {}
            }>
            <div class="deck-head">
                <span class="display">"Deck"</span>
                <span class="spacer"></span>
                <div class="row">
                    <Button variant=Variant::Ghost icon="minus" title="Zoom out (-)" on_click=move |_| zoom.update(|z| *z = clamp_zoom(*z / 1.25)) />
                    <input type="range" min="0" max="1" step="0.001" aria-label="Zoom"
                        prop:value=move || ((zoom.get() / MIN_PX_S).ln() / (MAX_PX_S / MIN_PX_S).ln()).to_string()
                        on:input=move |ev| {
                            let t: f64 = event_target_value(&ev).parse().unwrap_or(0.5);
                            zoom.set(clamp_zoom(MIN_PX_S * (MAX_PX_S / MIN_PX_S).powf(t)));
                        } />
                    <Button variant=Variant::Ghost icon="plus" title="Zoom in (+)" on_click=move |_| zoom.update(|z| *z = clamp_zoom(*z * 1.25)) />
                    <span class="mono muted" style="width:44px;text-align:right">{move || zoom_label(zoom.get())}</span>
                </div>
                <div class="segmented" role="group" aria-label="Waveform style">
                    <button type="button" aria-pressed=move || (style.get() == "rgb").to_string() on:click=move |_| style.set("rgb".into())>"RGB"</button>
                    <button type="button" aria-pressed=move || (style.get() == "bands").to_string() on:click=move |_| style.set("bands".into())>"3-band"</button>
                    <button type="button" aria-pressed=move || (style.get() == "mono").to_string() on:click=move |_| style.set("mono".into())>"Mono"</button>
                </div>
                <Button variant=Variant::Ghost size=Size::Sm pressed=normalise on_click=move |_| normalise.update(|n| *n = !*n)>"Normalise"</Button>
                <Button variant=Variant::Ghost icon="x" title="Close (Esc)" on_click=move |_| close() />
            </div>

            <div class="deck-lane">
                <div class="deck-label"><span class="badge badge-accent">"A"</span><span class="truncate">{title_cur}</span>
                    <span class="spacer"></span>
                    <Readout k="BPM" v=tempo_cur /><Readout k="KEY" v=key_cur /><Readout k="RATE" v=rate /></div>
                <WaveCanvas track_id=cur_id level=WaveLevel::Detail mode=ViewMode::Scrolling playhead=Playhead::Player px_per_s=zoom
                    style=style normalise=normalise markers=mk_cur on_pan=on_pan on_zoom=on_zoom height=150.0 class="deck-wave" />
            </div>

            <div class="deck-phase" aria-label="Phase meter">
                <Readout k="PHASE" v=phase />
                <div class="phase-bar"><i style=move || {
                    let e = player.transition.with(|t| t.as_ref().and_then(|t| t.phase_error_ms)).unwrap_or(0.0).clamp(-50.0, 50.0);
                    format!("left:{:.1}%", 50.0 + e)
                }></i></div>
            </div>

            <div class="deck-lane">
                <div class="deck-label"><span class="badge">"B"</span><span class="truncate">{title_next}</span>
                    <span class="spacer"></span>
                    <Readout k="BPM" v=tempo_next /><Readout k="KEY" v=key_next /></div>
                <WaveCanvas track_id=next_id level=WaveLevel::Detail mode=ViewMode::Scrolling playhead=Playhead::Incoming { base: next_start }
                    px_per_s=zoom style=style normalise=normalise markers=mk_next on_zoom=on_zoom height=150.0 class="deck-wave" />
            </div>

            <div class="deck-foot">
                <div class="dk-strip">
                    <span class="k">"EQ"</span>
                    {[("Low", 0usize), ("Mid", 1), ("High", 2)].into_iter().map(|(label, which)| {
                        let get = move || { let s = strip.get(); match which { 0 => s.low_db, 1 => s.mid_db, _ => s.high_db } };
                        let kill = move || { let s = strip.get(); match which { 0 => s.kill_low, 1 => s.kill_mid, _ => s.kill_high } };
                        view! {
                            <div class="dk-knob">
                                <input type="range" min="-24" max="12" step="0.5" aria-label=label prop:value=move || get().to_string()
                                    on:input=move |ev| {
                                        let v: f64 = event_target_value(&ev).parse().unwrap_or(0.0);
                                        set_strip(match which { 0 => StripPatch { low_db: Some(v), ..Default::default() }, 1 => StripPatch { mid_db: Some(v), ..Default::default() }, _ => StripPatch { high_db: Some(v), ..Default::default() } });
                                    } />
                                <span class="mono muted">{label}" "{move || format!("{:+.0}", get())}</span>
                                <Button size=Size::Sm variant=Variant::Ghost pressed=Signal::derive(kill) title="Kill"
                                    on_click=move |_| { let on = !kill(); set_strip(match which { 0 => StripPatch { kill_low: Some(on), ..Default::default() }, 1 => StripPatch { kill_mid: Some(on), ..Default::default() }, _ => StripPatch { kill_high: Some(on), ..Default::default() } }); }>"K"</Button>
                            </div>
                        }
                    }).collect_view()}
                    <div class="dk-knob">
                        <input type="range" min="-1" max="1" step="0.02" aria-label="Filter" prop:value=move || strip.get().filter.to_string()
                            on:input=move |ev| set_strip(StripPatch { filter: Some(event_target_value(&ev).parse().unwrap_or(0.0)), ..Default::default() }) />
                        <span class="mono muted">"Filter"</span>
                    </div>
                </div>
                <div class="row" style="flex-wrap:wrap;justify-content:flex-end">
                    <Button variant=Variant::Outline size=Size::Sm on_click=move |_| player.cmd(PlayerCommand::MixNow) icon="mix">"Mix now"</Button>
                    <Button variant=Variant::Outline size=Size::Sm on_click=move |_| player.cmd(PlayerCommand::Retime { factor: 2.0 })>"Slower"</Button>
                    <Button variant=Variant::Outline size=Size::Sm on_click=move |_| player.cmd(PlayerCommand::Retime { factor: 0.5 })>"Faster"</Button>
                    <Button variant=Variant::Outline size=Size::Sm on_click=move |_| player.cmd(PlayerCommand::Nudge { delta_s: -0.01 })>"Nudge -"</Button>
                    <Button variant=Variant::Outline size=Size::Sm on_click=move |_| player.cmd(PlayerCommand::Nudge { delta_s: 0.01 })>"Nudge +"</Button>
                    <Button variant=Variant::Danger size=Size::Sm on_click=move |_| player.cmd(PlayerCommand::CutNow) icon="cut">"Cut now"</Button>
                </div>
            </div>
        </div>
    }
}

#[allow(dead_code)]
fn _i() -> impl IntoView {
    view! { <Icon name="x" /> }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zoom_clamps_to_1x_64x() {
        assert_eq!(clamp_zoom(0.1), MIN_PX_S);
        assert_eq!(clamp_zoom(1e6), MAX_PX_S);
        assert_eq!(zoom_label(MAX_PX_S), "64x");
        assert_eq!(zoom_label(MIN_PX_S), "1.0x");
    }
}
