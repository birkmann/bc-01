//! Optional per-row mini waveform (Canvas2D, overview level only: ~12 KB per track, fetched
//! lazily and cached). Draws the same gapped, p95-normalised bars as the player bar
//! (`bc_waveform::bars::compute_bars`): unplayed colour at 70 % alpha, and the now-playing
//! row shows progress in the played colour. Colours come from the theme tokens.
use std::rc::Rc;

use bc_types::theme::{hex_to_rgb, resolve_colors};
use bc_waveform::Waveform;
use bc_waveform::bars::{BAR_MIN_PX, BarValue, compute_bars, reference_levels};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;

use crate::player::store::{position_now, use_player};
use crate::player::waveform::{WaveLevel, load_wave};
use crate::theme::use_theme;

pub const BAR_W: f64 = 1.5;
pub const BAR_GAP: f64 = 1.0;

/// Number of bars that fit `width` CSS px.
pub fn bar_count(width: f64) -> usize {
    (((width + BAR_GAP) / (BAR_W + BAR_GAP)).floor() as usize).max(1)
}

/// (x, y, w, h) rectangles (CSS px) for bars centred in `height`, each at least the minimum height.
pub fn bar_rects(bars: &[BarValue], height: f64) -> Vec<(f64, f64, f64, f64)> {
    let min_h = BAR_MIN_PX as f64;
    bars.iter()
        .enumerate()
        .map(|(i, b)| {
            let h = (b.h as f64 * height).clamp(min_h, height);
            (i as f64 * (BAR_W + BAR_GAP), (height - h) / 2.0, BAR_W, h)
        })
        .collect()
}

fn css(hex: &str, a: f64) -> String {
    let [r, g, b] = hex_to_rgb(hex);
    format!("rgba({r},{g},{b},{a})")
}

#[component]
pub fn MiniWave(track_id: i64, #[prop(default = 96)] width: u32, #[prop(default = 22)] height: u32) -> impl IntoView {
    let canvas = NodeRef::<leptos::html::Canvas>::new();
    let theme = use_theme();
    let player = use_player();
    let data = RwSignal::new_local(None::<Vec<BarValue>>);
    let n_bars = bar_count(width as f64);
    spawn_local(async move {
        if let Some(w) = load_wave(track_id, WaveLevel::Overview).await {
            let w: Rc<Waveform> = w;
            let refs = reference_levels(&w.overview);
            let _ = data.try_set(Some(compute_bars(&w.overview, &refs, 0.0, 1.0, n_bars)));
        }
    });
    // Now-playing row: number of already-played bars, polled only while this row is current.
    let is_cur = Memo::new(move |_| player.state.with(|s| s.current.as_ref().map(|c| c.track_id) == Some(track_id)));
    let played = RwSignal::new(0usize);
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    Effect::new(move |_| {
        if !is_cur.get() {
            played.set(0);
            return;
        }
        crate::util::raf_loop(move |_| {
            if !alive.try_get_value().unwrap_or(false) || !is_cur.try_get_untracked().unwrap_or(false) {
                return false;
            }
            let (pos, dur) = player.clock.with_untracked(|c| (position_now(c, player.clock_at.get_untracked()), c.duration_s));
            let n = if dur > 0.0 { ((pos / dur).clamp(0.0, 1.0) * n_bars as f64).floor() as usize } else { 0 };
            if played.try_get_untracked() != Some(n) {
                let _ = played.try_set(n);
            }
            true
        });
    });
    Effect::new(move |_| {
        let (Some(c), Some(bars)) = (canvas.get(), data.get()) else { return };
        let played_n = played.get();
        let colors = resolve_colors(&theme.active());
        let tok = |k: &str, d: &str| colors.get(k).cloned().unwrap_or_else(|| d.into());
        let unplayed = css(&tok("wave-unplayed", "#2ec7e6"), 0.7);
        let played_c = css(&tok("wave-played", "#f0f0f0"), 1.0);
        let el: web_sys::HtmlCanvasElement = c.unchecked_into();
        let dpr = crate::util::window().device_pixel_ratio().max(1.0);
        el.set_width((width as f64 * dpr) as u32);
        el.set_height((height as f64 * dpr) as u32);
        let Some(ctx) = el.get_context("2d").ok().flatten().and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok()) else { return };
        let _ = ctx.scale(dpr, dpr);
        for (i, (x, y, w, h)) in bar_rects(&bars, height as f64).into_iter().enumerate() {
            ctx.set_fill_style_str(if i < played_n { &played_c } else { &unplayed });
            ctx.fill_rect(x, y, w, h);
        }
    });
    view! { <canvas node_ref=canvas class="mini-wave" style=format!("width:{width}px;height:{height}px;display:block") aria-hidden="true"></canvas> }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bars_fit_the_width_with_gaps() {
        assert_eq!(bar_count(96.0), 38);
        assert_eq!(bar_count(1.0), 1);
    }
    #[test]
    fn rects_are_centred_and_never_vanish() {
        let r = bar_rects(&[BarValue { h: 0.0 }, BarValue { h: 1.0 }], 22.0);
        assert!((r[0].3 - BAR_MIN_PX as f64).abs() < 1e-9);
        assert!((r[0].1 - (22.0 - r[0].3) / 2.0).abs() < 1e-9);
        assert_eq!((r[1].1, r[1].3), (0.0, 22.0));
        assert!((r[1].0 - r[0].0 - (BAR_W + BAR_GAP)).abs() < 1e-9);
    }
}
