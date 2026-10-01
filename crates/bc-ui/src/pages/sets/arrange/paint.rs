//! Canvas2D overlay of the Arrange timeline: ruler, clip chrome (headers, trim grips, played shade),
//! transition bands with a per-type curve glyph and a chip, playhead, minimap and drag tooltips.
//! The waveforms themselves are drawn by the GL lane canvases underneath (see `lanes.rs`).
#![allow(dead_code)]

use bc_types::sets::{DjSetDetail, SetItemOut};
use bc_types::theme::{ColorMap, hex_to_rgb};
use web_sys::CanvasRenderingContext2d as Ctx;

use super::logic::{self, Band, Clip, Edge, Hit, Layout, Metrics, View};

const SANS: &str = "'Inter', system-ui, sans-serif";
const MONO: &str = "'JetBrains Mono', ui-monospace, monospace";

/// Theme colours as CSS strings (hex + parsed rgb for alpha variants).
#[derive(Clone, Default)]
pub struct Colors {
    map: ColorMap,
}

impl Colors {
    pub fn new(map: ColorMap) -> Self {
        Self { map }
    }
    pub fn hex(&self, k: &str) -> String {
        self.map.get(k).cloned().unwrap_or_else(|| "#888888".into())
    }
    /// `rgba()` of a token with alpha.
    pub fn a(&self, k: &str, alpha: f64) -> String {
        let [r, g, b] = hex_to_rgb(&self.hex(k));
        format!("rgba({},{},{},{alpha:.3})", r.round(), g.round(), b.round())
    }
    pub fn map(&self) -> &ColorMap {
        &self.map
    }
}

/// How a clip's waveform is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WaveState {
    Ready,
    Loading,
    /// The server has no waveform (never analysed / decode failed).
    NoData,
    Missing,
}

pub struct Tip {
    pub text: String,
    pub x: f64,
    pub y: f64,
}

pub struct Scene<'a> {
    pub w: f64,
    pub m: Metrics,
    pub view: View,
    pub layout: &'a Layout,
    pub detail: Option<&'a DjSetDetail>,
    pub colors: &'a Colors,
    pub sel: Option<i64>,
    pub sel_band: bool,
    pub hover: Hit,
    /// `(clip index, timeline seconds)` of the playing item.
    pub playing: Option<(usize, f64)>,
    pub is_playing: bool,
    pub wave_state: &'a [WaveState],
    pub tip: Option<&'a Tip>,
    pub hover_ruler_x: Option<f64>,
    pub mini: Option<&'a web_sys::HtmlCanvasElement>,
    pub dpr: f64,
    pub now_ms: f64,
    pub chip_labels: &'a [String],
    pub chips_compact: bool,
    pub focus: bool,
}

// ---- primitives ---------------------------------------------------------------------------

fn rrect(c: &Ctx, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w * 0.5).min(h * 0.5).max(0.0);
    c.begin_path();
    c.move_to(x + r, y);
    c.line_to(x + w - r, y);
    let _ = c.arc_to(x + w, y, x + w, y + r, r);
    c.line_to(x + w, y + h - r);
    let _ = c.arc_to(x + w, y + h, x + w - r, y + h, r);
    c.line_to(x + r, y + h);
    let _ = c.arc_to(x, y + h, x, y + h - r, r);
    c.line_to(x, y + r);
    let _ = c.arc_to(x, y, x + r, y, r);
    c.close_path();
}

fn rrect_top(c: &Ctx, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w * 0.5).min(h * 0.5).max(0.0);
    c.begin_path();
    c.move_to(x, y + h);
    c.line_to(x, y + r);
    let _ = c.arc_to(x, y, x + r, y, r);
    c.line_to(x + w - r, y);
    let _ = c.arc_to(x + w, y, x + w, y + r, r);
    c.line_to(x + w, y + h);
    c.close_path();
}

fn text(c: &Ctx, s: &str, x: f64, y: f64, font: &str, color: &str, align: &str) {
    c.set_font(font);
    c.set_fill_style_str(color);
    c.set_text_align(align);
    let _ = c.fill_text(s, x, y);
}

/// Truncate to roughly `avail` px at `char_w` px per char.
fn fit(s: &str, avail: f64, char_w: f64) -> String {
    let n = (avail / char_w).floor().max(0.0) as usize;
    if s.chars().count() <= n {
        s.to_string()
    } else if n <= 1 {
        String::new()
    } else {
        let mut t: String = s.chars().take(n - 1).collect();
        t.push('\u{2026}');
        t
    }
}

fn dash(c: &Ctx, on: f64, off: f64) {
    let _ = c.set_line_dash(&js_sys::Array::of2(&on.into(), &off.into()));
}
fn solid(c: &Ctx) {
    let _ = c.set_line_dash(&js_sys::Array::new());
}

fn vline(c: &Ctx, x: f64, y0: f64, y1: f64) {
    c.begin_path();
    c.move_to(x, y0);
    c.line_to(x, y1);
    c.stroke();
}

fn px(x: f64) -> f64 {
    x.floor() + 0.5
}

// ---- entry ----------------------------------------------------------------------------------

pub fn draw(c: &Ctx, s: &Scene) {
    let _ = c.set_transform(s.dpr, 0.0, 0.0, s.dpr, 0.0, 0.0);
    c.clear_rect(0.0, 0.0, s.w, s.m.total_h() + 2.0);
    ruler(c, s);
    lane_heads(c, s);
    for cl in &s.layout.clips {
        clip(c, s, cl);
    }
    for b in &s.layout.bands {
        band(c, s, b);
    }
    playhead(c, s);
    minimap(c, s);
    if let Some(t) = s.tip {
        tip(c, s, t);
    }
    if s.focus {
        c.set_stroke_style_str(&s.colors.a("accent", 0.75));
        c.set_line_width(2.0);
        c.stroke_rect(1.0, 1.0, s.w - 2.0, s.m.total_h() - 2.0);
    }
}

// ---- ruler ----------------------------------------------------------------------------------

fn ruler(c: &Ctx, s: &Scene) {
    let (m, col, v) = (&s.m, s.colors, &s.view);
    c.set_fill_style_str(&col.hex("surface-1"));
    c.fill_rect(0.0, 0.0, s.w, m.ruler_h);
    c.set_line_width(1.0);
    c.set_stroke_style_str(&col.hex("line"));
    c.begin_path();
    c.move_to(0.0, m.ruler_h - 0.5);
    c.line_to(s.w, m.ruler_h - 0.5);
    c.stroke();

    // set extent shading: the ruler of the part past the end is dimmer
    let end_x = v.x_of(s.layout.total_s);
    if end_x < s.w {
        c.set_fill_style_str(&col.a("surface-0", 0.5));
        c.fill_rect(end_x.max(0.0), 0.0, s.w - end_x.max(0.0), m.ruler_h - 1.0);
    }

    for t in logic::ruler_ticks(v.left_s - 5.0, v.right_s() + 5.0, v.pps) {
        let x = px(v.x_of(t.t));
        if x < -40.0 || x > s.w + 40.0 {
            continue;
        }
        c.set_stroke_style_str(&if t.major { col.hex("ink-faint") } else { col.hex("line-strong") });
        let h = if t.major { 9.0 } else { 4.0 };
        vline(c, x, m.ruler_h - 1.0 - h, m.ruler_h - 1.0);
        if let Some(l) = &t.label {
            text(c, l, x + 4.0, m.ruler_h - 6.0, &format!("10px {MONO}"), &col.hex("ink-faint"), "left");
        }
    }

    // track numbers at clip starts
    c.set_text_baseline("middle");
    for cl in &s.layout.clips {
        let x = v.x_of(cl.start_s);
        if x < -20.0 || x > s.w + 20.0 {
            continue;
        }
        let sel = s.sel == Some(cl.id);
        let label = format!("{}", cl.index + 1);
        let w = 8.0 + label.len() as f64 * 6.6;
        rrect(c, x, 3.0, w, 12.0, 3.0);
        c.set_fill_style_str(&if sel { col.hex("accent") } else { col.a("ink", 0.16) });
        c.fill();
        text(c, &label, x + w * 0.5, 9.5, &format!("600 9.5px {MONO}"), &if sel { col.hex("accent-ink") } else { col.hex("ink-muted") }, "center");
    }
    c.set_text_baseline("alphabetic");

    if let Some(hx) = s.hover_ruler_x {
        let t = v.t_at(hx);
        if t >= 0.0 && t <= s.layout.total_s {
            c.set_stroke_style_str(&col.a("ink", 0.35));
            vline(c, px(hx), m.ruler_h - 1.0, m.lanes_bottom());
            let label = logic::fmt_clock(t);
            let w = label.len() as f64 * 6.6 + 12.0;
            let x = (hx - w * 0.5).clamp(2.0, s.w - w - 2.0);
            rrect(c, x, m.ruler_h - 19.0, w, 15.0, 3.0);
            c.set_fill_style_str(&col.hex("surface-4"));
            c.fill();
            c.set_text_baseline("middle");
            text(c, &label, x + w * 0.5, m.ruler_h - 11.0, &format!("10px {MONO}"), &col.hex("ink"), "center");
            c.set_text_baseline("alphabetic");
        }
    }
}

// ---- lanes -----------------------------------------------------------------------------------

fn lane_heads(c: &Ctx, s: &Scene) {
    let m = &s.m;
    for lane in 0..2u8 {
        c.set_fill_style_str(&s.colors.hex("surface-1"));
        c.fill_rect(0.0, m.lane_top(lane), s.w, m.head_h);
        // lane label
        text(c, if lane == 0 { "A" } else { "B" }, s.w - 8.0, m.lane_top(lane) + m.head_h - 6.0, &format!("700 9px {MONO}"), &s.colors.a("ink", 0.16), "right");
    }
}

fn clip(c: &Ctx, s: &Scene, cl: &Clip) {
    let (m, col, v) = (&s.m, s.colors, &s.view);
    let (x0, x1) = (v.x_of(cl.start_s), v.x_of(cl.end_s()));
    if x1 < -4.0 || x0 > s.w + 4.0 {
        return;
    }
    let Some(item) = s.detail.and_then(|d| d.items.get(cl.index)) else { return };
    let top = m.lane_top(cl.lane);
    let w = (x1 - x0).max(1.0);
    let selected = s.sel == Some(cl.id) && !s.sel_band;
    let playing = s.playing.map(|p| p.0) == Some(cl.index);
    let state = s.wave_state.get(cl.index).copied().unwrap_or(WaveState::Loading);
    let hovered = matches!(s.hover, Hit::Clip(i) | Hit::ClipEdge(i, _) if i == cl.index);

    // body background under the waveform when it is not there (loading / missing)
    let wave_y = m.wave_top(cl.lane);
    if state != WaveState::Ready {
        c.set_fill_style_str(&col.hex("surface-2"));
        c.fill_rect(x0.max(-2.0), wave_y, (x1.min(s.w + 2.0)) - x0.max(-2.0), m.wave_h);
        let cx0 = x0.max(0.0);
        let cx1 = x1.min(s.w);
        if cx1 - cx0 > 6.0 {
            match state {
                WaveState::Loading => {
                    // shimmering skeleton bars
                    let ph = (s.now_ms / 900.0) % 1.0;
                    let mut x = (cx0 / 5.0).floor() * 5.0;
                    while x < cx1 {
                        let k = ((x / 140.0 + ph * 6.283).sin() * 0.5 + 0.5) * 0.6 + 0.2;
                        let bh = m.wave_h * (0.12 + 0.34 * ((x * 0.21).sin().abs() * k));
                        c.set_fill_style_str(&col.a("ink", 0.07 + 0.07 * k));
                        c.fill_rect(x.max(cx0), wave_y + m.wave_h * 0.5 - bh, 3.0, bh * 2.0);
                        x += 5.0;
                    }
                }
                WaveState::NoData | WaveState::Missing => {
                    let msg = if state == WaveState::Missing { "File missing" } else { "No waveform: analyse this track" };
                    c.set_text_baseline("middle");
                    let tx = (cx0 + 10.0).max(10.0);
                    text(c, &fit(msg, cx1 - tx - 6.0, 6.2), tx, wave_y + m.wave_h * 0.5, &format!("11px {SANS}"), &col.hex("ink-faint"), "left");
                    c.set_text_baseline("alphabetic");
                }
                WaveState::Ready => {}
            }
        }
    }

    // played shade (the waveform is below; the shade is a translucent overlay)
    if let Some((pi, pt)) = s.playing
        && pi == cl.index
    {
        let px_ = v.x_of(pt).clamp(x0, x1);
        let sx0 = x0.max(0.0);
        let sx1 = px_.min(s.w);
        if sx1 > sx0 {
            c.set_fill_style_str(&col.a("surface-0", 0.55));
            c.fill_rect(sx0, wave_y, sx1 - sx0, m.wave_h);
        }
    }
    if selected {
        c.set_fill_style_str(&col.a("accent", 0.06));
        c.fill_rect(x0.max(0.0), wave_y, x1.min(s.w) - x0.max(0.0), m.wave_h);
    }

    // header
    rrect_top(c, x0, top, w, m.head_h, 4.0);
    c.set_fill_style_str(&col.hex(if selected { "surface-3" } else { "surface-4" }));
    c.fill();
    if selected {
        rrect_top(c, x0, top, w, m.head_h, 4.0);
        c.set_fill_style_str(&col.a("accent", 0.24));
        c.fill();
    }

    // header text (sticky to the viewport's left edge while the clip is partly scrolled out)
    c.save();
    c.begin_path();
    c.rect(x0, top, w, m.head_h);
    c.clip();
    let tx0 = (x0 + 8.0).max(8.0).min(x1 - 60.0).max(x0 + 8.0);
    let mut right = x1 - 6.0;
    c.set_text_baseline("middle");
    let ty = top + m.head_h * 0.5 + 0.5;
    // badges (right aligned): key, bpm
    let mut badges: Vec<(String, String)> = vec![];
    if let Some(k) = &item.effective_camelot {
        badges.push((k.clone(), "ink-muted".into()));
    }
    if let Some(b) = item.effective_bpm {
        let pct = if item.tempo_adjust_pct.abs() > 0.04 { format!(" {:+.1}%", item.tempo_adjust_pct) } else { String::new() };
        badges.push((format!("{:.0}{pct}", b), "ink-muted".into()));
    }
    let mut shown = 0;
    for (label, color) in badges.iter().rev() {
        let bw = label.chars().count() as f64 * 6.2 + 8.0;
        if right - bw < tx0 + 70.0 && shown > 0 || right - bw < tx0 + 36.0 {
            break;
        }
        rrect(c, right - bw, top + 3.0, bw, m.head_h - 6.0, 3.0);
        c.set_fill_style_str(&col.a("ink", 0.1));
        c.fill();
        text(c, label, right - bw * 0.5, ty, &format!("10px {MONO}"), &col.hex(color), "center");
        right -= bw + 4.0;
        shown += 1;
    }
    let title = if item.title.is_empty() { "(missing track)".to_string() } else { item.title.clone() };
    let num = format!("{}  ", cl.index + 1);
    let avail = right - tx0 - 2.0;
    let full = format!("{num}{title}");
    text(c, &fit(&full, avail, 6.3), tx0, ty, &format!("600 11px {SANS}"), &col.hex("ink"), "left");
    let used = full.chars().count() as f64 * 6.3;
    if avail - used > 50.0 && !item.artist.is_empty() {
        text(c, &fit(&format!(" \u{2014} {}", item.artist), avail - used, 6.0), tx0 + used, ty, &format!("11px {SANS}"), &col.hex("ink-faint"), "left");
    }
    c.restore();
    c.set_text_baseline("alphabetic");

    // outline
    let h = m.lane_h();
    rrect(c, x0 + 0.5, top + 0.5, w - 1.0, h - 1.0, 4.0);
    if item.missing {
        dash(c, 4.0, 3.0);
    }
    c.set_line_width(if selected { 1.75 } else { 1.0 });
    c.set_stroke_style_str(&if selected {
        col.hex("accent")
    } else if playing {
        col.a("accent", 0.7)
    } else if hovered {
        col.hex("line-strong")
    } else {
        col.hex("line")
    });
    if selected || playing {
        c.set_shadow_color(&col.a("accent", if selected { 0.5 } else { 0.35 }));
        c.set_shadow_blur(12.0);
    }
    c.stroke();
    c.set_shadow_blur(0.0);
    c.set_shadow_color("rgba(0,0,0,0)");
    solid(c);

    // trim grips
    if !item.missing && w >= 3.0 * m.grab {
        for (edge, ex) in [(Edge::Left, x0), (Edge::Right, x1)] {
            let active = matches!(s.hover, Hit::ClipEdge(i, e) if i == cl.index && e == edge);
            if !(selected || hovered || active) || ex < -2.0 || ex > s.w + 2.0 {
                continue;
            }
            let gx = if edge == Edge::Left { ex + 1.0 } else { ex - 4.0 };
            rrect(c, gx, top + m.head_h + m.wave_h * 0.5 - 16.0, 3.0, 32.0, 1.5);
            c.set_fill_style_str(&if active { col.hex("accent-hi") } else { col.a("accent", 0.9) });
            c.fill();
        }
    }
}

// ---- transition bands ----------------------------------------------------------------------------

/// Gain curves (`out`, `in`) of a transition kind sampled at `x` in 0..1, as fractions of full volume.
pub fn curve(kind: &str, x: f64) -> (f64, f64) {
    use std::f64::consts::FRAC_PI_2;
    match kind {
        "bass_swap" => {
            // highs crossfade, the bass swaps hard at the midpoint
            let hi = ((x - 0.15) / 0.7).clamp(0.0, 1.0);
            let out = (1.0 - hi) * if x < 0.5 { 1.0 } else { 0.55 };
            let inn = hi * if x < 0.5 { 0.55 } else { 1.0 };
            (out, inn)
        }
        "filter" => ((1.0 - x).powf(2.2), 1.0 - (1.0 - x).powf(0.6)),
        "echo_out" => {
            let decay = (1.0 - x).powf(2.6);
            let echo = ((x * 18.0).sin().abs() * 0.22 + 0.0) * (1.0 - x).powf(1.4);
            (decay + echo, (x * 1.9).min(1.0).powf(0.7))
        }
        "cut" => (if x < 0.5 { 1.0 } else { 0.0 }, if x < 0.5 { 0.0 } else { 1.0 }),
        _ => ((x * FRAC_PI_2).cos(), (x * FRAC_PI_2).sin()),
    }
}

fn band(c: &Ctx, s: &Scene, b: &Band) {
    let (m, col, v) = (&s.m, s.colors, &s.view);
    let (x0, x1) = (v.x_of(b.start_s), v.x_of(b.end_s()));
    let chars = if s.chips_compact { 0 } else { s.chip_labels.get(b.index).map(|l| l.chars().count()).unwrap_or(8) };
    let (cx, cw) = logic::chip_x(b, v, chars);
    if (x1 < -cw && cx + cw < 0.0) || (x0 > s.w + cw && cx - cw > s.w) {
        return;
    }
    let Some(d) = s.detail else { return };
    let (Some(out_it), Some(inc)) = (d.items.get(b.index), d.items.get(b.index + 1)) else { return };
    let kind = if b.beats == 0 { "cut" } else { inc.transition_type.as_deref().unwrap_or("blend") };
    let trans = d.transitions.get(b.index);
    let selected = s.sel == Some(inc.id) && s.sel_band;
    let hovered = matches!(s.hover, Hit::Band(i) | Hit::BandEdge(i, _) if i == b.index);
    let lanes_y0 = m.lane_top(0);
    let lanes_y1 = m.lanes_bottom();
    let tint = if trans.is_some_and(|t| !t.ok) { "warn" } else { "accent" };

    if x1 - x0 >= 1.5 {
        let (cl, cr) = (x0.max(-2.0), x1.min(s.w + 2.0));
        c.set_fill_style_str(&col.a(tint, if selected { 0.2 } else if hovered { 0.16 } else { 0.11 }));
        c.fill_rect(cl, lanes_y0, cr - cl, lanes_y1 - lanes_y0);
        c.set_line_width(1.0);
        c.set_stroke_style_str(&col.a(tint, if selected { 0.95 } else { 0.55 }));
        if x0 >= -1.0 && x0 <= s.w + 1.0 {
            vline(c, px(x0), lanes_y0, lanes_y1);
        }
        if x1 >= -1.0 && x1 <= s.w + 1.0 {
            vline(c, px(x1), lanes_y0, lanes_y1);
        }
        // the gain curves
        if x1 - x0 >= 28.0 {
            let (yt, yb) = (lanes_y0 + 10.0, lanes_y1 - 10.0);
            let out_lane = (b.index % 2) as u8;
            let ycv = |g: f64, lane: u8| if lane == 0 { yt + (1.0 - g) * (yb - yt) } else { yb - (1.0 - g) * (yb - yt) };
            let steps = (((x1.min(s.w) - x0.max(0.0)) / 3.0) as usize).clamp(8, 160);
            for (which, lane) in [(0, out_lane), (1, 1 - out_lane)] {
                c.begin_path();
                for i in 0..=steps {
                    let u = i as f64 / steps as f64;
                    let (go, gi) = curve(kind, u);
                    let g = if which == 0 { go } else { gi };
                    let (x, y) = (x0 + u * (x1 - x0), ycv(g.clamp(0.0, 1.0), lane));
                    if i == 0 {
                        c.move_to(x, y);
                    } else {
                        c.line_to(x, y);
                    }
                }
                c.set_line_width(1.6);
                c.set_stroke_style_str(&col.a("ink", if which == 0 { 0.7 } else { 0.95 }));
                if which == 0 {
                    dash(c, 4.0, 3.0);
                }
                c.stroke();
                solid(c);
            }
        }
    } else {
        // a cut: a thin marker across both lanes
        let x = px(x0);
        c.set_stroke_style_str(&col.a(tint, if selected { 1.0 } else { 0.7 }));
        c.set_line_width(1.5);
        dash(c, 3.0, 3.0);
        vline(c, x, lanes_y0, lanes_y1);
        solid(c);
    }

    // grips (the blend start/end handles) in the gap row
    if x1 - x0 > 3.0 * m.grab {
        for (edge, ex) in [(Edge::Left, x0), (Edge::Right, x1)] {
            let active = matches!(s.hover, Hit::BandEdge(i, e) if i == b.index && e == edge);
            if ex < -3.0 || ex > s.w + 3.0 || !(hovered || selected) {
                continue;
            }
            rrect(c, ex - 3.0, m.gap_mid() - 9.0, 6.0, 18.0, 3.0);
            c.set_fill_style_str(&if active { col.hex("accent-hi") } else { col.hex("accent") });
            c.fill();
        }
    }

    // chip
    let (chx, chy, chh) = (cx - cw * 0.5, m.gap_mid() - logic::chip_h() * 0.5, logic::chip_h());
    rrect(c, chx, chy, cw, chh, chh * 0.5);
    c.set_fill_style_str(&col.hex("surface-3"));
    c.fill();
    c.set_line_width(if selected { 1.75 } else { 1.0 });
    c.set_stroke_style_str(&if selected { col.hex("accent") } else if hovered { col.hex("accent-lo") } else { col.hex("line-strong") });
    c.stroke();
    let label = if s.chips_compact { String::new() } else { s.chip_labels.get(b.index).cloned().unwrap_or_default() };
    c.set_text_baseline("middle");
    text(c, &label, chx + 11.0, chy + chh * 0.5 + 0.5, &format!("600 10px {MONO}"), &col.hex("ink"), "left");
    c.set_text_baseline("alphabetic");
    // verdict glyph: check (ok) or warning triangle: never colour alone
    let gx = chx + cw - 14.0;
    let gy = chy + chh * 0.5;
    let ok = trans.map(|t| t.ok);
    match ok {
        Some(true) => {
            c.begin_path();
            c.move_to(gx - 4.0, gy);
            c.line_to(gx - 1.0, gy + 3.0);
            c.line_to(gx + 4.5, gy - 3.5);
            c.set_line_width(1.8);
            c.set_stroke_style_str(&col.hex("ok"));
            c.stroke();
        }
        Some(false) => {
            c.begin_path();
            c.move_to(gx, gy - 4.5);
            c.line_to(gx + 5.0, gy + 4.0);
            c.line_to(gx - 5.0, gy + 4.0);
            c.close_path();
            c.set_fill_style_str(&col.hex("warn"));
            c.fill();
            c.set_fill_style_str(&col.hex("surface-0"));
            c.fill_rect(gx - 0.7, gy - 2.0, 1.4, 3.2);
            c.fill_rect(gx - 0.7, gy + 2.0, 1.4, 1.4);
        }
        None => {}
    }
    let _ = out_it;
}

// ---- playhead -----------------------------------------------------------------------------------

fn playhead(c: &Ctx, s: &Scene) {
    let Some((_, t)) = s.playing else { return };
    let (m, col) = (&s.m, s.colors);
    let x = s.view.x_of(t);
    if x < -10.0 || x > s.w + 10.0 {
        return;
    }
    c.set_line_width(1.5);
    c.set_stroke_style_str(&col.a("ink", if s.is_playing { 1.0 } else { 0.55 }));
    c.set_shadow_color("rgba(0,0,0,0.6)");
    c.set_shadow_blur(3.0);
    vline(c, x.floor() + 0.5, m.ruler_h - 2.0, m.lanes_bottom() + 2.0);
    c.set_shadow_blur(0.0);
    // the head
    c.begin_path();
    c.move_to(x - 5.0, m.ruler_h - 9.0);
    c.line_to(x + 5.0, m.ruler_h - 9.0);
    c.line_to(x, m.ruler_h - 1.0);
    c.close_path();
    c.set_fill_style_str(&col.a("ink", if s.is_playing { 1.0 } else { 0.55 }));
    c.fill();
    // set clock chip
    let label = logic::fmt_clock(t);
    let w = label.len() as f64 * 6.6 + 10.0;
    let lx = if x + 8.0 + w < s.w { x + 8.0 } else { x - 8.0 - w };
    rrect(c, lx, 2.0, w, 13.0, 3.0);
    c.set_fill_style_str(&col.hex("ink"));
    c.fill();
    c.set_text_baseline("middle");
    text(c, &label, lx + w * 0.5, 9.0, &format!("600 10px {MONO}"), &col.hex("ink-invert"), "center");
    c.set_text_baseline("alphabetic");
}

// ---- minimap -------------------------------------------------------------------------------------

/// Render the static part of the minimap (clips + waveform silhouettes + transitions) into `ctx`
/// of an offscreen canvas of `w x h` css px. `peaks[i]` are 0..255 samples across clip `i`.
pub fn draw_minimap_cache(ctx: &Ctx, dpr: f64, w: f64, h: f64, layout: &Layout, peaks: &[Option<Vec<u8>>], bands_ok: &[bool], col: &Colors) {
    let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
    ctx.clear_rect(0.0, 0.0, w, h);
    rrect(ctx, 0.0, 0.0, w, h, 6.0);
    ctx.set_fill_style_str(&col.hex("surface-1"));
    ctx.fill();
    let row_h = (h - 6.0) * 0.5;
    for cl in &layout.clips {
        let x0 = logic::mini_x(cl.start_s, layout.total_s, w);
        let x1 = logic::mini_x(cl.end_s(), layout.total_s, w);
        let y0 = 2.0 + cl.lane as f64 * (row_h + 2.0);
        let tint = if cl.lane == 0 { "series-1" } else { "series-2" };
        ctx.set_fill_style_str(&col.a(tint, 0.22));
        ctx.fill_rect(x0, y0, (x1 - x0).max(1.0), row_h);
        if let Some(Some(p)) = peaks.get(cl.index) {
            let n = p.len();
            if n > 0 {
                let mid = y0 + row_h * 0.5;
                ctx.set_stroke_style_str(&col.a(tint, 0.9));
                ctx.set_line_width(1.0);
                ctx.begin_path();
                let cols = ((x1 - x0).floor() as usize).clamp(1, n);
                for i in 0..cols {
                    let a = i * n / cols;
                    let b = (((i + 1) * n / cols).max(a + 1)).min(n);
                    let v = p[a..b].iter().copied().max().unwrap_or(0) as f64 / 255.0;
                    let hh = (v * v * row_h * 0.5).max(0.5);
                    let x = (x0 + i as f64 * ((x1 - x0) / cols as f64)).floor() + 0.5;
                    ctx.move_to(x, mid - hh);
                    ctx.line_to(x, mid + hh);
                }
                ctx.stroke();
            }
        }
    }
    for (i, b) in layout.bands.iter().enumerate() {
        let x = logic::mini_x(b.start_s, layout.total_s, w);
        let x1 = logic::mini_x(b.end_s(), layout.total_s, w);
        ctx.set_fill_style_str(&if bands_ok.get(i).copied().unwrap_or(true) { col.a("accent", 0.5) } else { col.a("warn", 0.8) });
        ctx.fill_rect(x, 2.0 + row_h - 1.0, (x1 - x).max(2.0), 4.0);
    }
}

fn minimap(c: &Ctx, s: &Scene) {
    let (m, col, v) = (&s.m, s.colors, &s.view);
    let (top, h) = (m.mini_top(), m.mini_h);
    if let Some(cache) = s.mini {
        let _ = c.draw_image_with_html_canvas_element_and_dw_and_dh(cache, 0.0, top, s.w, h);
    } else {
        rrect(c, 0.0, top, s.w, h, 6.0);
        c.set_fill_style_str(&col.hex("surface-1"));
        c.fill();
    }
    let total = s.layout.total_s.max(1.0);
    // dim outside the viewport, outline the viewport
    let a = logic::mini_x(v.left_s.max(0.0), total, s.w);
    let b = logic::mini_x(v.right_s().min(total), total, s.w);
    c.set_fill_style_str(&col.a("surface-0", 0.5));
    c.fill_rect(0.0, top, (a).max(0.0), h);
    c.fill_rect(b, top, (s.w - b).max(0.0), h);
    rrect(c, a + 0.5, top + 0.5, (b - a).max(6.0) - 1.0, h - 1.0, 4.0);
    c.set_line_width(1.5);
    c.set_stroke_style_str(&col.a("accent", 0.9));
    c.stroke();
    c.set_fill_style_str(&col.a("accent", 0.08));
    c.fill();
    if let Some((_, t)) = s.playing {
        let x = px(logic::mini_x(t, total, s.w));
        c.set_stroke_style_str(&col.hex("ink"));
        c.set_line_width(1.5);
        vline(c, x, top, top + h);
    }
}

// ---- tooltip ----------------------------------------------------------------------------------------

fn tip(c: &Ctx, s: &Scene, t: &Tip) {
    let w = t.text.chars().count() as f64 * 6.6 + 16.0;
    let h = 20.0;
    let x = (t.x - w * 0.5).clamp(4.0, (s.w - w - 4.0).max(4.0));
    let y = (t.y - h - 12.0).max(s.m.ruler_h + 2.0);
    rrect(c, x, y, w, h, 5.0);
    c.set_fill_style_str(&s.colors.hex("surface-4"));
    c.set_shadow_color("rgba(0,0,0,0.45)");
    c.set_shadow_blur(10.0);
    c.fill();
    c.set_shadow_blur(0.0);
    c.set_line_width(1.0);
    c.set_stroke_style_str(&s.colors.hex("line-strong"));
    c.stroke();
    c.set_text_baseline("middle");
    text(c, &t.text, x + w * 0.5, y + h * 0.5 + 0.5, &format!("11px {MONO}"), &s.colors.hex("ink"), "center");
    c.set_text_baseline("alphabetic");
}

/// Plain-text description of a clip for the live region / accessible label.
pub fn describe(item: &SetItemOut) -> String {
    let mut s = format!("Track {}: {}", item.index + 1, item.title);
    if !item.artist.is_empty() {
        s.push_str(&format!(" by {}", item.artist));
    }
    if let Some(b) = item.effective_bpm {
        s.push_str(&format!(", {b:.0} BPM"));
    }
    if let Some(k) = &item.effective_camelot {
        s.push_str(&format!(", key {k}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_cross_and_end_correctly() {
        for kind in ["blend", "bass_swap", "filter", "echo_out"] {
            let (o0, i0) = curve(kind, 0.0);
            let (o1, i1) = curve(kind, 1.0);
            assert!(o0 > 0.9 && i0 < 0.1, "{kind} start {o0} {i0}");
            assert!(o1 < 0.1 && i1 > 0.9, "{kind} end {o1} {i1}");
            for k in 0..=20 {
                let (o, i) = curve(kind, k as f64 / 20.0);
                assert!((0.0..=1.2).contains(&o) && (0.0..=1.0).contains(&i), "{kind}");
            }
        }
        // equal power blend: out^2 + in^2 = 1
        let (o, i) = curve("blend", 0.37);
        assert!((o * o + i * i - 1.0).abs() < 1e-9);
        assert_eq!(curve("cut", 0.2), (1.0, 0.0));
        assert_eq!(curve("cut", 0.8), (0.0, 1.0));
    }

    #[test]
    fn fit_truncates_with_ellipsis() {
        assert_eq!(fit("hello", 100.0, 6.0), "hello");
        assert_eq!(fit("hello world", 36.0, 6.0), "hello\u{2026}");
        assert_eq!(fit("hello", 5.0, 6.0), "");
    }

    #[test]
    fn colors_alpha() {
        let mut m = ColorMap::new();
        m.insert("ink".into(), "#ff8000".into());
        let c = Colors::new(m);
        assert_eq!(c.a("ink", 0.5), "rgba(255,128,0,0.500)");
        assert_eq!(c.hex("nope"), "#888888");
    }
}
