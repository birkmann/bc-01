use std::rc::Rc;

use bc_music::beatgrid::GridExt;
use bc_types::analysis::BeatGrid;
use bc_waveform::builder::build_from_mono;
use bc_waveform::render::{Backend, WaveformView};
use bc_waveform::Waveform;
use bc_waveform::view::{Markers, Region, ViewMode, ViewState, WaveStyle, WaveTheme};
use wasm_bindgen::prelude::*;

/// 2 minutes of 128 BPM kicks + hats at 44.1 kHz.
fn synth() -> Vec<f32> {
    let sr = 44_100.0f32;
    let n = (sr * 120.0) as usize;
    let period = 60.0 / 128.0;
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            let m = t % period;
            let loud = if t > 30.0 && t < 100.0 { 1.0 } else { 0.35 };
            loud * ((-12.0 * m).exp() * (2.0 * std::f32::consts::PI * 55.0 * t).sin() * 0.8
                + (-60.0 * m).exp() * (2.0 * std::f32::consts::PI * 7000.0 * t).sin() * 0.3)
        })
        .collect()
}

fn set(key: &str, v: &str) {
    let w = web_sys::window().unwrap();
    let _ = js_sys::Reflect::set(&w, &key.into(), &v.into());
}

fn param(search: &str, key: &str) -> Option<String> {
    search.trim_start_matches('?').split('&').find_map(|kv| kv.strip_prefix(&format!("{key}=")).map(str::to_string))
}

/// Waveform bytes injected by the smoke script (`window.__wf_bytes`, a `.bcw2` wire file).
fn injected() -> Option<Waveform> {
    let w = web_sys::window()?;
    let v = js_sys::Reflect::get(&w, &"__wf_bytes".into()).ok()?;
    if v.is_undefined() || v.is_null() {
        return None;
    }
    Waveform::from_bytes(&js_sys::Uint8Array::new(&v).to_vec()).ok()
}

fn main() {
    let doc = web_sys::window().unwrap().document().unwrap();
    let canvas: web_sys::HtmlCanvasElement = doc.get_element_by_id("c").unwrap().dyn_into().unwrap();
    let search = web_sys::window().unwrap().location().search().unwrap_or_default();
    let css_w: f64 = param(&search, "w").and_then(|v| v.parse().ok()).unwrap_or(1000.0);
    let dpr: f64 = param(&search, "dpr").and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let css_h = 620.0;
    canvas.set_width((css_w * dpr) as u32);
    canvas.set_height((css_h * dpr) as u32);
    let st = canvas.style();
    let _ = st.set_property("width", &format!("{css_w}px"));
    let _ = st.set_property("height", &format!("{css_h}px"));
    if search.contains("fallback=1") {
        // taking a 2d context first makes webgl2 unavailable on this canvas
        let _ = canvas.get_context("2d");
    }
    let mut view = match WaveformView::new(canvas.clone()) {
        Ok(v) => v,
        Err(e) => {
            set("__wf_error", &format!("{e:?}"));
            return;
        }
    };
    set("__wf_backend", if view.backend() == Backend::WebGl2 { "webgl2" } else { "canvas2d" });
    let wf = Rc::new(injected().unwrap_or_else(|| build_from_mono(44_100, &synth(), [7; 16])));
    let dur = wf.duration_s();
    let play = dur * 0.38;
    view.resize(css_w, css_h, dpr);
    view.set_data(Some(wf));
    let mut theme = WaveTheme::default();
    theme.background = [0.059, 0.067, 0.09, 1.0];
    theme.played_shade = [0.0, 0.0, 0.0, 0.0];
    view.set_theme(theme);
    let mut grid = BeatGrid::constant(128.0, 100.0, 1.0, "demo");
    grid.downbeat_phase = Some(0);
    let base = Markers { mix_in_s: Some(dur * 0.25), mix_out_s: Some(dur * 0.85), ..Default::default() };
    let mk = |style, mode, px, off| ViewState { style, mode, px_per_s: px, offset_s: off, playhead_s: play, normalise: true };
    let ov = |style| mk(style, ViewMode::Overview, 100.0, 0.0);
    let rg = |x: f64, y: f64, w: f64, h: f64| Region { x, y, w, h };
    // strips: id, region, view, markers
    let hover = Markers { hover_s: Some(dur * 0.55), buffered_to_s: Some(dur * 0.8), ..base.clone() };
    let zoom_m = Markers { grid: Some(grid), ..base.clone() };
    let zoom = |style| mk(style, ViewMode::Free, 40.0, play - 3.0);
    let w3 = (css_w / 3.0).floor();
    let strips: Vec<(&str, Region, ViewState, &Markers)> = vec![
        ("bars", rg(0.0, 0.0, css_w, 56.0), ov(WaveStyle::Bars), &base),
        ("bars_hover", rg(0.0, 64.0, css_w, 56.0), ov(WaveStyle::Bars), &hover),
        ("bars_narrow", rg(0.0, 128.0, 375.0, 36.0), ov(WaveStyle::Bars), &base),
        ("spectral", rg(0.0, 172.0, css_w, 96.0), ov(WaveStyle::RgbSpectral), &base),
        ("threeband", rg(0.0, 276.0, css_w, 96.0), ov(WaveStyle::ThreeBand), &base),
        ("mono", rg(0.0, 380.0, css_w, 96.0), ov(WaveStyle::Mono), &base),
        ("zoom_spectral", rg(0.0, 484.0, w3, 128.0), zoom(WaveStyle::RgbSpectral), &zoom_m),
        ("zoom_threeband", rg(w3, 484.0, w3, 128.0), zoom(WaveStyle::ThreeBand), &zoom_m),
        ("zoom_mono", rg(2.0 * w3, 484.0, w3, 128.0), zoom(WaveStyle::Mono), &zoom_m),
    ];
    let mut info = String::from("[");
    for (i, (name, r, v, m)) in strips.iter().enumerate() {
        view.set_markers((*m).clone());
        view.set_view(*v);
        view.draw_in(*r);
        if i > 0 {
            info.push(',');
        }
        info.push_str(&format!("{{\"name\":\"{name}\",\"x\":{},\"y\":{},\"w\":{},\"h\":{}}}", r.x, r.y, r.w, r.h));
    }
    info.push(']');
    set("__wf_strips", &info);
    set("__wf_dur", &format!("{dur}"));
    // resizing to the same size must not clear what we drew
    view.resize(css_w, css_h, dpr);
    set("__wf_done", "1");
}
