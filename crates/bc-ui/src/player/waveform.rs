//! Waveform components wrapping WS3's `bc_waveform::render::WaveformView` (WebGL2 with a
//! Canvas2D fallback): player overview, Deck view detail lanes, Arrange clips. The
//! playhead is extrapolated from the `player.clock` topic in rAF; redraws happen only when
//! something changed (zero per-frame CPU work otherwise).
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use bc_types::analysis::TrackMusicInfo;
use bc_types::player::PlayerCommand;
use bc_types::theme::{ColorMap, hex_to_rgb, is_light, resolve_colors};
use bc_waveform::Waveform;
use bc_waveform::render::WaveformView;
use bc_waveform::view::{Markers, Rgba, ViewMode, ViewState, WaveStyle, WaveTheme};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;

use super::store::{position_now, use_player};
use crate::api;
use crate::theme::use_theme;

/// Commands for the browser host (bc-worklet): see `player::local`.
pub fn local_command(c: &PlayerCommand) {
    if let Some(ctx) = leptos::prelude::use_context::<super::store::PlayerCtx>() {
        super::local::command(&ctx, c);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaveLevel {
    Overview,
    Detail,
}

impl WaveLevel {
    fn q(self) -> &'static str {
        match self {
            WaveLevel::Overview => "overview",
            WaveLevel::Detail => "detail",
        }
    }
}

thread_local! {
    static WAVES: RefCell<HashMap<(i64, u8), Rc<Waveform>>> = RefCell::new(HashMap::new());
    static MUSIC: RefCell<HashMap<i64, Rc<TrackMusicInfo>>> = RefCell::new(HashMap::new());
}

fn cached_wave(id: i64, level: WaveLevel) -> Option<Rc<Waveform>> {
    WAVES.with(|w| w.borrow().get(&(id, level as u8)).cloned())
}

/// Fetch (or reuse) the decoded waveform of a track. Detail levels are kept to a small LRU-ish bound.
pub async fn load_wave(id: i64, level: WaveLevel) -> Option<Rc<Waveform>> {
    if let Some(w) = cached_wave(id, level) {
        return Some(w);
    }
    let bytes = api::get_bytes(&format!("/tracks/{id}/waveform?level={}&f={}", level.q(), bc_waveform::format::VERSION)).await.ok()?;
    let w = Rc::new(Waveform::from_bytes(&bytes).ok()?);
    WAVES.with(|m| {
        let mut m = m.borrow_mut();
        let limit = if level == WaveLevel::Detail { 6 } else { 64 };
        let same: Vec<(i64, u8)> = m.keys().filter(|k| k.1 == level as u8).cloned().collect();
        if same.len() >= limit {
            let n = same.len() + 1 - limit;
            for k in same.into_iter().take(n) {
                m.remove(&k);
            }
        }
        m.insert((id, level as u8), w.clone());
    });
    Some(w)
}

/// Beat grid, cues and mix points (one fetch per deck load).
pub async fn load_music(id: i64) -> Option<Rc<TrackMusicInfo>> {
    if let Some(m) = MUSIC.with(|m| m.borrow().get(&id).cloned()) {
        return Some(m);
    }
    let info: TrackMusicInfo = match api::get(&format!("/tracks/{id}/music")).await {
        Ok(v) => v,
        Err(_) => api::get(&format!("/tracks/{id}/beatgrid")).await.ok()?,
    };
    let rc = Rc::new(info);
    MUSIC.with(|m| {
        let mut m = m.borrow_mut();
        if m.len() > 48 {
            m.clear();
        }
        m.insert(id, rc.clone());
    });
    Some(rc)
}

fn rgba(hex: &str, a: f32) -> Rgba {
    let [r, g, b] = hex_to_rgb(hex);
    [(r / 255.0) as f32, (g / 255.0) as f32, (b / 255.0) as f32, a]
}

/// One Rust source feeds both the CSS variables and the WebGL colours.
pub fn wave_theme(c: &ColorMap) -> WaveTheme {
    let g = |k: &str| c.get(k).cloned().unwrap_or_else(|| "#888888".into());
    let light = is_light(&g("surface-1"));
    let mut t = WaveTheme::default();
    // transparent: the waveform sits directly on whatever surface hosts it
    t.background = [0.0, 0.0, 0.0, 0.0];
    t.wave = rgba(&g("ink-muted"), 1.0);
    t.core = rgba(&g("ink"), 0.9);
    // Band anchors follow the theme's chart colours (series-1/2/3). Spectral: low = series-2
    // (warm), mid = series-3 (green), high = series-1 (blue); three-band: low = series-1,
    // mid = series-2, high = ink. Missing tokens fall back to the DJ-convention colours.
    let tok = |k: &str, fallback: &str| rgba(c.get(k).map(String::as_str).unwrap_or(fallback), 1.0);
    let (f_low, f_mid, f_high) = if light { ("#e5281c", "#0f9d58", "#1a66d9") } else { ("#ff3b30", "#34e07a", "#3d8bff") };
    t.low = tok("series-2", f_low);
    t.mid = tok("series-3", f_mid);
    t.high = tok("series-1", f_high);
    let (l_low, l_mid) = if light { ("#2563eb", "#f08c00") } else { ("#215cf2", "#fa9e29") };
    t.layer_low = tok("series-1", l_low);
    t.layer_mid = tok("series-2", l_mid);
    t.layer_high = if light { rgba(&g("ink"), 1.0) } else { rgba("#f5f2e6", 1.0) };
    // Bars style: white played / cyan unplayed (theme tokens wave-played / wave-unplayed)
    t.played = tok("wave-played", if light { "#1a1a1a" } else { "#f0f0f0" });
    t.unplayed = tok("wave-unplayed", if light { "#0891b2" } else { "#2ec7e6" });
    t.played_shade = [0.0, 0.0, 0.0, 0.0];
    t.played_dim = 0.8;
    t.played_to = rgba(&g("surface-2"), 1.0);
    t.playhead = rgba(&g("ink"), 1.0);
    t.beat = rgba(&g("ink"), 0.16);
    t.bar = rgba(&g("ink"), 0.38);
    t.phrase = rgba(&g("warn"), 0.8);
    t.cue = rgba(&g("warn"), 1.0);
    t.mix_in = rgba(&g("ok"), 1.0);
    t.mix_out = rgba(&g("danger"), 1.0);
    t.loop_fill = rgba(&g("info"), 0.25);
    t.loop_edge = rgba(&g("info"), 0.9);
    t.buffered = rgba(&g("ink"), 0.25);
    t.hover = rgba(&g("ink"), 0.6);
    t.chapter = rgba(&g("ink"), 0.7);
    t
}

pub fn style_of(s: &str) -> WaveStyle {
    match s {
        "bands" | "three_band" => WaveStyle::ThreeBand,
        "mono" => WaveStyle::Mono,
        "bars" => WaveStyle::Bars,
        _ => WaveStyle::RgbSpectral,
    }
}

/// Where the playhead comes from.
#[derive(Clone, Copy)]
pub enum Playhead {
    /// The player's current track (extrapolated from the clock every frame).
    Player,
    /// A fixed position signal (no animation; e.g. Arrange clip window).
    Fixed(Signal<f64>),
    /// The incoming deck during a blend: starts at `base` and runs in sync with the transition.
    Incoming { base: Signal<f64> },
}

#[component]
pub fn WaveCanvas(
    #[prop(into)] track_id: Signal<Option<i64>>,
    level: WaveLevel,
    mode: ViewMode,
    playhead: Playhead,
    #[prop(optional, into)] px_per_s: MaybeProp<f64>,
    #[prop(optional, into)] offset_s: MaybeProp<f64>,
    #[prop(optional, into)] style: MaybeProp<String>,
    #[prop(optional, into)] normalise: MaybeProp<bool>,
    #[prop(optional, into)] markers: Option<Signal<Markers>>,
    /// Seek / scrub callback (seconds). Overview: absolute time under the pointer.
    #[prop(optional, into)] on_seek: Option<Callback<f64>>,
    /// Scrolling mode: relative drag (seconds delta) and zoom wheel factor.
    #[prop(optional, into)] on_pan: Option<Callback<f64>>,
    #[prop(optional, into)] on_zoom: Option<Callback<f64>>,
    #[prop(optional, into)] class: String,
    #[prop(optional, into)] height: Option<f64>,
    /// Reports hover time (seconds) for tooltips.
    #[prop(optional, into)] on_hover: Option<Callback<Option<f64>>>,
) -> impl IntoView {
    let canvas = NodeRef::<leptos::html::Canvas>::new();
    let player = use_player();
    let theme = use_theme();
    let view: StoredValue<Option<Rc<RefCell<WaveformView>>>, LocalStorage> = StoredValue::new_local(None);
    let dirty = StoredValue::new(true);
    let alive = StoredValue::new(true);
    let duration = StoredValue::new(0.0f64);
    on_cleanup(move || alive.set_value(false));

    // Create the view once the canvas exists. Only the canvas is tracked (the theme effect below
    // handles colours). The resize observer is attached on every run because a re-run disconnects
    // the previous one: a view left without it keeps a stale width and maps clicks to wrong times.
    Effect::new(move |_| {
        let Some(c) = canvas.get() else { return };
        let el: web_sys::HtmlCanvasElement = c.unchecked_into();
        if view.with_value(|v| v.is_none()) {
            let Ok(v) = WaveformView::new(el.clone()) else { return };
            let v = Rc::new(RefCell::new(v));
            v.borrow_mut().set_theme(wave_theme(&resolve_colors(&theme.active_untracked())));
            view.set_value(Some(v));
        }
        let Some(v) = view.with_value(|v| v.clone()) else { return };
        let size = move |w: f64, h: f64| {
            let dpr = crate::util::window().device_pixel_ratio();
            v.borrow_mut().resize(w, h, dpr);
            dirty.set_value(true);
        };
        let disconnect = crate::util::observe_resize(el.unchecked_ref(), size);
        let guard = send_wrapper::SendWrapper::new(disconnect);
        on_cleanup(move || (guard.take())());
    });

    // theme -> colours
    Effect::new(move |_| {
        let t = wave_theme(&resolve_colors(&theme.active()));
        view.with_value(|v| {
            if let Some(v) = v {
                v.borrow_mut().set_theme(t);
            }
        });
        dirty.set_value(true);
    });

    // data
    Effect::new(move |_| {
        let id = track_id.get();
        match id {
            None => {
                view.with_value(|v| {
                    if let Some(v) = v {
                        v.borrow_mut().set_data(None);
                    }
                });
                dirty.set_value(true);
            }
            Some(id) => {
                // paint the cached overview immediately; fetch otherwise
                spawn_local(async move {
                    let w = load_wave(id, level).await;
                    if !alive.try_get_value().unwrap_or(false) || track_id.try_get_untracked().flatten() != Some(id) {
                        return;
                    }
                    duration.set_value(w.as_ref().map(|w| w.duration_s()).unwrap_or(0.0));
                    view.with_value(|v| {
                        if let Some(v) = v {
                            v.borrow_mut().set_data(w);
                        }
                    });
                    dirty.set_value(true);
                });
            }
        }
    });

    if let Some(m) = markers {
        Effect::new(move |_| {
            let m = m.get();
            view.with_value(|v| {
                if let Some(v) = v {
                    v.borrow_mut().set_markers(m);
                }
            });
            dirty.set_value(true);
        });
    }

    // rAF loop: extrapolate the playhead; redraw only on change.
    let last = StoredValue::new((f64::NAN, f64::NAN, f64::NAN));
    Effect::new(move |started: Option<bool>| {
        if started.is_some() {
            return true;
        }
        crate::util::raf_loop(move |_| {
            if !alive.try_get_value().unwrap_or(false) {
                return false;
            }
            let pos = match playhead {
                Playhead::Player => player.clock.with_untracked(|c| position_now(c, player.clock_at.get_untracked())),
                Playhead::Fixed(s) => s.get_untracked(),
                Playhead::Incoming { base } => {
                    let base = base.get_untracked();
                    match player.transition.get_untracked() {
                        Some(t) => {
                            let el = ((crate::util::unix_ms() - t.started_at_ms as f64) / 1000.0).max(0.0);
                            base + el * player.clock.with_untracked(|c| c.rate.max(0.0))
                        }
                        None => base,
                    }
                }
            };
            let zoom = px_per_s.get_untracked().unwrap_or(100.0);
            let off = offset_s.get_untracked().unwrap_or(0.0);
            if !dirty.get_value() && last.get_value() == (pos, zoom, off) {
                return true;
            }
            last.set_value((pos, zoom, off));
            dirty.set_value(false);
            let st = ViewState {
                style: style_of(&style.get_untracked().unwrap_or_default()),
                playhead_s: pos,
                px_per_s: zoom,
                offset_s: off,
                normalise: normalise.get_untracked().unwrap_or(false),
                mode,
            };
            view.with_value(|v| {
                if let Some(v) = v {
                    let mut v = v.borrow_mut();
                    v.set_view(st);
                    v.draw();
                }
            });
            true
        });
        true
    });
    // style/zoom/normalise changes redraw even when paused
    Effect::new(move |_| {
        px_per_s.get();
        offset_s.get();
        style.get();
        normalise.get();
        dirty.set_value(true);
    });

    // ---- pointer interaction ----------------------------------------------------------
    let dragging = StoredValue::new(false);
    let last_x = StoredValue::new(0.0f64);
    let pointers = StoredValue::new(HashMap::<i32, f64>::new());
    let pinch_base = StoredValue::new(None::<f64>);
    let local_x = move |ev: &web_sys::PointerEvent| -> f64 {
        canvas.get_untracked().map(|c| ev.client_x() as f64 - c.get_bounding_client_rect().left()).unwrap_or(0.0)
    };
    let time_at = move |x: f64| -> f64 { view.with_value(|v| v.as_ref().map(|v| v.borrow().time_at_x(x)).unwrap_or(0.0)) };

    view! {
        <canvas node_ref=canvas class=format!("wave {class}") style=format!("width:100%;height:{};display:block;touch-action:{}", height.map(|h| format!("{h}px")).unwrap_or_else(|| "100%".into()), if mode == ViewMode::Scrolling { "pan-y" } else { "none" })
            on:pointerdown=move |ev: web_sys::PointerEvent| {
                if let Some(c) = canvas.get_untracked() { let _ = c.set_pointer_capture(ev.pointer_id()); }
                let x = local_x(&ev);
                pointers.update_value(|p| { p.insert(ev.pointer_id(), x); });
                dragging.set_value(true);
                last_x.set_value(x);
                if mode == ViewMode::Overview { if let Some(cb) = on_seek { cb.run(time_at(x).max(0.0)); } }
            }
            on:pointermove=move |ev: web_sys::PointerEvent| {
                let x = local_x(&ev);
                if let Some(cb) = on_hover { cb.run(Some(time_at(x).max(0.0))); }
                if !dragging.get_value() { return; }
                pointers.update_value(|p| { p.insert(ev.pointer_id(), x); });
                let n = pointers.with_value(|p| p.len());
                if n >= 2 {
                    // pinch zoom
                    let (a, b) = pointers.with_value(|p| { let mut v = p.values().copied(); (v.next().unwrap_or(0.0), v.next().unwrap_or(0.0)) });
                    let d = (a - b).abs().max(1.0);
                    if let Some(base) = pinch_base.get_value() { if let Some(cb) = on_zoom { cb.run(d / base); } }
                    pinch_base.set_value(Some(d));
                    return;
                }
                pinch_base.set_value(None);
                match mode {
                    ViewMode::Overview => { if let Some(cb) = on_seek { cb.run(time_at(x).clamp(0.0, duration.get_value().max(0.0))); } }
                    _ => {
                        let dx = x - last_x.get_value();
                        last_x.set_value(x);
                        if let Some(cb) = on_pan { cb.run(-dx / px_per_s.get_untracked().unwrap_or(100.0)); }
                    }
                }
            }
            on:pointerup=move |ev: web_sys::PointerEvent| {
                pointers.update_value(|p| { p.remove(&ev.pointer_id()); });
                if pointers.with_value(|p| p.is_empty()) { dragging.set_value(false); pinch_base.set_value(None); }
            }
            on:pointercancel=move |ev: web_sys::PointerEvent| {
                pointers.update_value(|p| { p.remove(&ev.pointer_id()); });
                dragging.set_value(false);
            }
            on:pointerleave=move |_| { if let Some(cb) = on_hover { cb.run(None); } }
            on:wheel=move |ev: web_sys::WheelEvent| {
                if let Some(cb) = on_zoom {
                    ev.prevent_default();
                    cb.run((-ev.delta_y() * 0.0015).exp());
                }
            }
        ></canvas>
    }
}
