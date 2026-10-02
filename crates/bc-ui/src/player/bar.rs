//! Player bar: overview waveform, transport, love, volume, mix-out marker, transition
//! strip, output picker (this device / desktop) and the entry to the Deck view.
use std::rc::Rc;

use bc_types::player::*;
use bc_waveform::view::{Markers, ViewMode};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::store::{PlaybackTarget, position_now, use_player};
use super::waveform::{Playhead, WaveCanvas, WaveLevel, load_music};
use crate::app::use_app;
use crate::ds::{Button, Icon, MenuButton, MenuEntry, MenuItem, Size, Variant, dyn_icon};
use crate::logic::format::format_duration_s;
use crate::pages::explore::logic::{band_path, origin_of, release_path};
use crate::widgets::common::{EntityLink, LoveButton, album_href, artist_href};

/// Elapsed / remaining text, updated only when the displayed second changes.
#[component]
pub fn PlayTime(#[prop(optional)] remaining: bool) -> impl IntoView {
    let player = use_player();
    let text = RwSignal::new(String::from("0:00"));
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    Effect::new(move |started: Option<bool>| {
        if started.is_some() {
            return true;
        }
        let last = std::cell::Cell::new(-1i64);
        crate::util::raf_loop(move |_| {
            if !alive.try_get_value().unwrap_or(false) {
                return false;
            }
            let (pos, dur) = player.clock.with_untracked(|c| (position_now(c, player.clock_at.get_untracked()), c.duration_s));
            let shown = if remaining { (dur - pos).max(0.0) } else { pos };
            let sec = shown.floor() as i64;
            if sec != last.get() {
                last.set(sec);
                let _ = text.try_set(if remaining { format!("-{}", format_duration_s(shown)) } else { format_duration_s(shown) });
            }
            true
        });
        true
    });
    view! { <span class="mono pt">{move || text.get()}</span> }
}

/// Plain seek bar for tracks without a waveform (streams, files not analysed yet). The rAF loop
/// writes position and buffer straight into CSS variables, so playback costs no reactive updates.
#[component]
fn SeekBar(hover: RwSignal<Option<f64>>) -> impl IntoView {
    let player = use_player();
    let st = player.state;
    let el = NodeRef::<leptos::html::Div>::new();
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    Effect::new(move |started: Option<bool>| {
        if started.is_some() {
            return true;
        }
        let last = std::cell::Cell::new((-1i64, -1i64));
        crate::util::raf_loop(move |_| {
            if !alive.try_get_value().unwrap_or(false) {
                return false;
            }
            let Some(e) = el.get_untracked() else { return true };
            let (pos, dur, buf) = player.clock.with_untracked(|c| (position_now(c, player.clock_at.get_untracked()), c.duration_s, c.buffered_s));
            // hundredths of a percent: enough for a smooth bar, few style writes
            let pct = |s: f64| if dur > 0.0 { (s / dur * 10_000.0).clamp(0.0, 10_000.0).round() as i64 } else { 0 };
            let now = (pct(pos), pct(buf.max(pos)));
            if now != last.get() {
                last.set(now);
                let style = web_sys::HtmlElement::style(&e);
                let _ = style.set_property("--p", &format!("{:.2}%", now.0 as f64 / 100.0));
                let _ = style.set_property("--b", &format!("{:.2}%", now.1 as f64 / 100.0));
            }
            true
        });
        true
    });
    let time_at = move |ev: &web_sys::PointerEvent| -> Option<f64> {
        let e = el.get_untracked()?;
        let r = e.get_bounding_client_rect();
        let dur = player.clock.with_untracked(|c| c.duration_s);
        (r.width() > 0.0 && dur > 0.0).then(|| ((ev.client_x() as f64 - r.left()) / r.width()).clamp(0.0, 1.0) * dur)
    };
    let dragging = StoredValue::new(false);
    let mix_out = move || {
        let dur = player.clock.with_untracked(|c| c.duration_s);
        st.with(|s| s.mix_out_override_s.or(s.mix_out_s).filter(|_| s.mix))
            .filter(|t| dur > 0.0 && *t > 0.0)
            .map(|t| view! { <i class="sb-mark" style=format!("left:{:.3}%", (t / dur * 100.0).clamp(0.0, 100.0))></i> })
    };
    view! {
        <div class="pl-scrub" node_ref=el
            on:pointerdown=move |ev: web_sys::PointerEvent| {
                if let Some(e) = el.get_untracked() { let _ = e.set_pointer_capture(ev.pointer_id()); }
                dragging.set_value(true);
                if let Some(t) = time_at(&ev) { player.seek(t); }
            }
            on:pointermove=move |ev: web_sys::PointerEvent| {
                let t = time_at(&ev);
                hover.set(t);
                if dragging.get_value() { if let Some(t) = t { player.seek(t); } }
            }
            on:pointerup=move |_| dragging.set_value(false)
            on:pointercancel=move |_| dragging.set_value(false)
            on:pointerleave=move |_| hover.set(None)>
            <div class="sb-track"><i class="sb-buf"></i><i class="sb-played"></i></div>
            {mix_out}
            <i class="sb-knob"></i>
        </div>
    }
}

fn output_menu(player: super::store::PlayerCtx) -> Vec<MenuEntry> {
    let st = player.state.get_untracked();
    let target = player.target.get_untracked();
    let mut v: Vec<MenuEntry> = vec![
        MenuEntry::Label("Play on".into()),
        MenuItem::new("This device").icon("phone").checked(target == PlaybackTarget::ThisDevice)
            .on(move || player.target.set(PlaybackTarget::ThisDevice)).into(),
        MenuItem::new("Desktop speakers").icon("speaker").checked(target == PlaybackTarget::Server)
            .on(move || player.target.set(PlaybackTarget::Server)).into(),
    ];
    if !st.devices.outputs.is_empty() {
        v.push(MenuEntry::Sep);
        v.push(MenuEntry::Label("Desktop output device".into()));
        for d in &st.devices.outputs {
            let name = d.name.clone();
            let cur = st.devices.target.device.as_deref() == Some(d.name.as_str()) || (st.devices.target.device.is_none() && d.is_default);
            let tgt = st.devices.target.clone();
            v.push(
                MenuItem::new(if d.is_default { format!("{} (default)", d.name) } else { d.name.clone() })
                    .icon("hdd")
                    .checked(cur)
                    .on(move || {
                        let mut t2 = tgt.clone();
                        t2.device = Some(name.clone());
                        player.cmd(PlayerCommand::SetOutput { target: t2 });
                    })
                    .into(),
            );
        }
    }
    v
}

#[component]
pub fn TransitionStrip() -> impl IntoView {
    let player = use_player();
    let t = player.transition;
    let progress = RwSignal::new(0.0f64);
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    Effect::new(move |started: Option<bool>| {
        if started.is_some() {
            return true;
        }
        crate::util::raf_loop(move |_| {
            if !alive.try_get_value().unwrap_or(false) {
                return false;
            }
            if let Some(tr) = t.get_untracked() {
                let span = (tr.ends_at_ms as f64 - tr.started_at_ms as f64).max(1.0);
                let p = ((crate::util::unix_ms() - tr.started_at_ms as f64) / span).clamp(0.0, 1.0);
                if (p - progress.get_untracked()).abs() > 0.004 {
                    let _ = progress.try_set(p);
                }
            }
            true
        });
        true
    });
    view! {
        <Show when=move || t.with(|t| t.is_some())>
            <div class="trans-strip" role="status" aria-label="Transition">
                <span class="status info"><Icon name="mix" />{move || t.with(|t| t.as_ref().map(|t| t.kind.label()).unwrap_or_default())}</span>
                <div class="meter grow"><i style=move || format!("width:{:.1}%", progress.get() * 100.0)></i></div>
                {move || t.with(|t| t.as_ref().and_then(|t| t.phase.clone())).map(|p| view! { <span class="badge">{format!("{p:?}").to_lowercase()}</span> })}
                <Button size=Size::Sm variant=Variant::Ghost title="Slower" on_click=move |_| player.cmd(PlayerCommand::Retime { factor: 2.0 })>"Slower"</Button>
                <Button size=Size::Sm variant=Variant::Ghost title="Faster" on_click=move |_| player.cmd(PlayerCommand::Retime { factor: 0.5 })>"Faster"</Button>
                <Button size=Size::Sm variant=Variant::Ghost title="Echo" pressed=Signal::derive(move || t.with(|t| t.as_ref().map(|t| t.echo).unwrap_or(false)))
                    on_click=move |_| { let on = !t.with_untracked(|t| t.as_ref().map(|t| t.echo).unwrap_or(false)); player.cmd(PlayerCommand::SetTransitionEcho { on }) }>"Echo"</Button>
                <Button size=Size::Sm variant=Variant::Ghost title="Nudge -10ms" on_click=move |_| player.cmd(PlayerCommand::Nudge { delta_s: -0.01 })>"-"</Button>
                <Button size=Size::Sm variant=Variant::Ghost title="Nudge +10ms" on_click=move |_| player.cmd(PlayerCommand::Nudge { delta_s: 0.01 })>"+"</Button>
                <Button size=Size::Sm variant=Variant::Danger title="Cut now (x)" on_click=move |_| player.cmd(PlayerCommand::CutNow)>"Cut"</Button>
            </div>
        </Show>
    }
}

#[component]
pub fn PlayerBar() -> impl IntoView {
    let player = use_player();
    let app = use_app();
    let st = player.state;
    let track_id = Signal::derive(move || st.with(|s| s.current.as_ref().map(|c| c.track_id).filter(|id| *id > 0)));
    let music: RwSignal<Option<Rc<bc_types::analysis::TrackMusicInfo>>, LocalStorage> = RwSignal::new_local(None);
    Effect::new(move |_| {
        let id = track_id.get();
        music.set(None);
        if let Some(id) = id {
            spawn_local(async move {
                let m = load_music(id).await;
                if track_id.try_get_untracked().flatten() == Some(id) {
                    let _ = music.try_set(m);
                }
            });
        }
    });
    let hover = RwSignal::new(None::<f64>);
    let has_wave = RwSignal::new(false);
    let prefs = crate::prefs::use_prefs();
    // cues, mix-in and mix-out only mean something while DJ mix is on, and Settings can hide them
    let mixing = Signal::derive(move || st.with(|s| s.mix) && prefs.prefs.with(|p| p.player_wave_markers));
    let markers = Signal::derive(move || {
        let mut m = Markers::default();
        if let Some(info) = music.get() {
            m.grid = info.grid.clone();
            m.chapter_ticks = info.grid.is_some();
            if mixing.get() {
                m.cues = info.cues.clone();
            }
            if let Some(mp) = info.mix_points.as_ref().filter(|_| mixing.get()) {
                m.mix_in_s = Some(mp.cue_in_ms as f64 / 1000.0);
            }
        }
        m.hover_s = hover.get();
        m.mix_out_s = st.with(|s| s.mix_out_override_s.or(s.mix_out_s)).filter(|_| mixing.get());
        // Bars: a dimmed "unbuffered" tail makes the whole bar look faded while the decoder is still
        // catching up (seek / fresh track), so only the spectral style shows it.
        if prefs.prefs.with(|p| p.player_wave_style == "rgb") {
            m.buffered_to_s = Some(player.clock.with(|c| c.buffered_s)).filter(|b| *b > 0.0);
        }
        m
    });
    // reactive: follows Settings changes immediately; "bars" is the default
    let waveform_style = Signal::derive(move || prefs.prefs.with(|p| if p.player_wave_style == "rgb" { "rgb".to_string() } else { "bars".to_string() }));
    let playing = Signal::derive(move || st.with(|s| s.status == PlayerStatus::Playing));
    // Library items link up to their album and artist; Bandcamp streams to the release and band
    // pages in Explore (the band is the release page's site); "nothing playing" stays text.
    // Memoised so the links rebuild only when the track changes, not on every state tick.
    let now = Memo::new(move |_| {
        st.with(|s| {
            s.current.as_ref().map(|c| {
                let page = c.page_url.clone().filter(|u| !u.is_empty());
                let album_link = c.release_id.filter(|id| *id > 0).map(album_href).or_else(|| page.as_deref().map(release_path));
                let artist_link = c.artist_id.filter(|id| *id > 0).map(artist_href).or_else(|| page.as_deref().and_then(origin_of).map(|o| band_path(&o)));
                (c.title.clone(), album_link, c.album.clone(), c.artist.clone(), artist_link)
            })
        })
    });
    let title = move || match now.get() {
        Some((t, Some(href), album, ..)) => {
            let tip = album.map(|a| format!("Open {a}")).unwrap_or_else(|| "Open album".into());
            view! { <EntityLink href=href title=tip>{t}</EntityLink> }.into_any()
        }
        Some((t, None, ..)) => view! { {t} }.into_any(),
        None => view! { "Nothing playing" }.into_any(),
    };
    let artist = move || match now.get() {
        Some((_, _, _, Some(name), Some(href))) => view! { <EntityLink href=href>{name}</EntityLink> }.into_any(),
        Some((_, _, _, Some(name), None)) => view! { {name} }.into_any(),
        _ => ().into_any(),
    };
    let art = Signal::derive(move || st.with(|s| s.current.as_ref().and_then(|c| c.art_url.clone())));
    let volume = RwSignal::new(0.8f64);
    Effect::new(move |_| volume.set(st.with(|s| s.volume)));
    let loved = Signal::derive(move || st.with(|s| s.current.as_ref().map(|c| c.loved).unwrap_or(false)));
    let overflow = Callback::new(move |_: ()| output_menu(player));
    let seek_overview = Callback::new(move |s: f64| player.seek(s));
    let on_shift_mixout = move |ev: web_sys::MouseEvent| {
        if ev.shift_key() {
            if let Some(t) = hover.get_untracked() {
                player.cmd(PlayerCommand::SetMixOutOverride { seconds: Some(t) });
            }
        }
    };

    view! {
        <footer class="player" aria-label="Player">
            <TransitionStrip />
            <Show when=move || st.with(|s| s.error.is_some())>
                <div class="banner danger"><Icon name="alert-circle" /><span class="grow">{move || st.with(|s| s.error.clone().unwrap_or_default())}</span>
                    <Button size=Size::Sm variant=Variant::Ghost on_click=move |_| player.cmd(PlayerCommand::ClearError)>"Dismiss"</Button></div>
            </Show>
            <div class="player-main">
                <div class="pl-now">
                    <button class="pl-art" type="button" title="Open deck view (v)" on:click=move |_| app.deck_open.set(true)>
                        <crate::widgets::common::Art src=art />
                    </button>
                    <div class="pl-meta">
                        <div class="pl-title truncate">{title}</div>
                        <div class="pl-artist truncate muted">{artist}</div>
                    </div>
                    {move || track_id.get().map(|id| view! { <LoveButton track_id=id loved=loved /> })}
                </div>
                <div class="pl-center">
                    <div class="pl-controls">
                        <Button variant=Variant::Ghost class="pl-opt" icon="shuffle" title="Shuffle" pressed=Signal::derive(move || st.with(|s| s.shuffle)) on_click=move |_| player.cmd(PlayerCommand::ToggleShuffle) />
                        <Button variant=Variant::Ghost icon="skip-prev" title="Previous (Shift+Left)" on_click=move |_| player.previous() />
                        <button class="pl-play" type="button" title=move || if playing.get() { "Pause (Space)" } else { "Play (Space)" }
                            aria-label=move || if playing.get() { "Pause" } else { "Play" } on:click=move |_| player.toggle()>
                            <Icon name=dyn_icon(move || if playing.get() { "pause" } else { "play" }) />
                        </button>
                        <Button variant=Variant::Ghost icon="skip-next" title="Next (Shift+Right)" on_click=move |_| player.next() />
                        <Button variant=Variant::Ghost class="pl-opt" icon=dyn_icon(move || if st.with(|s| s.repeat == RepeatMode::One) { "repeat-1" } else { "repeat" }) title="Repeat"
                            pressed=Signal::derive(move || st.with(|s| s.repeat != RepeatMode::Off)) on_click=move |_| player.cmd(PlayerCommand::CycleRepeat) />
                    </div>
                    <div class="pl-seek" on:click=on_shift_mixout>
                        <PlayTime />
                        <div class="pl-wave" class:no-wave=move || !has_wave.get() title="Click to seek. Shift+click sets the mix-out point.">
                            <WaveCanvas track_id=track_id level=WaveLevel::Overview mode=ViewMode::Overview playhead=Playhead::Player
                                style=waveform_style normalise=true markers=markers on_seek=seek_overview on_hover=Callback::new(move |h| hover.set(h)) has_data=has_wave />
                            <Show when=move || !has_wave.get()><SeekBar hover=hover /></Show>
                            {move || hover.get().map(|t| {
                                let dur = player.clock.with_untracked(|c| c.duration_s).max(1.0);
                                view! { <span class="pl-hover mono" style=format!("left:{:.3}%", (t / dur * 100.0).clamp(0.0, 100.0))>{format_duration_s(t)}</span> }
                            })}
                        </div>
                        <PlayTime remaining=true />
                    </div>
                </div>
                <div class="pl-right">
                    <Button variant=Variant::Ghost class="pl-opt" icon="mix" title="DJ mix" pressed=Signal::derive(move || st.with(|s| s.mix)) on_click=move |_| player.cmd(PlayerCommand::ToggleMix)>
                        <span class="hide-sm">"Mix"</span></Button>
                    <Button variant=Variant::Ghost class="pl-opt" icon=dyn_icon(move || if st.with(|s| s.muted) || volume.get() == 0.0 { "volume-off" } else { "volume" }) title="Mute" on_click=move |_| player.cmd(PlayerCommand::ToggleMute) />
                    <input class="pl-vol" type="range" min="0" max="1" step="0.01" aria-label="Volume"
                        prop:value=move || volume.get().to_string()
                        on:input=move |ev| {
                            let v: f64 = event_target_value(&ev).parse().unwrap_or(0.8);
                            volume.set(v);
                            player.cmd(PlayerCommand::SetVolume { volume: v });
                        } />
                    <super::queue::QueueButton />
                    <Button variant=Variant::Ghost class="pl-opt" icon="waveform" title="Deck view (v)" on_click=move |_| app.deck_open.update(|d| *d = !*d) />
                    <MenuButton entries=overflow icon="speaker" title="Output" />
                </div>
            </div>
        </footer>
    }
}
