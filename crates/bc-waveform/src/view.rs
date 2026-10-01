//! Renderer-independent view logic: styles, theme, markers, view resolution, marker pixel
//! geometry, texture layout planning and texture packing. Pure and natively testable; the
//! WebGL2 / Canvas2D code (feature `webgl`) only consumes the results.

use bc_music::beatgrid::GridQuery;
use bc_types::analysis::{BeatGrid, CueKind, CuePoint};

use crate::format::PLANES;
use crate::mip::Pyramid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaveStyle {
    /// Serato-like: hue from the low/mid/high ratio.
    #[default]
    RgbSpectral,
    /// Rekordbox-like: stacked low/mid/high, layered additively.
    ThreeBand,
    /// Peak envelope with an RMS core.
    Mono,
    /// Mirrored, gapped bars (the player bar and row mini-waves): white played part, cyan
    /// unplayed part, per-track p95 normalisation applied automatically.
    Bars,
}

/// Alias used by the published API (`bc_waveform::Style::Bars`).
pub type Style = WaveStyle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    /// Whole track fits the width.
    #[default]
    Overview,
    /// Playhead fixed at the centre, track scrolls under it.
    Scrolling,
    /// `offset_s` is the left edge and `px_per_s` the zoom (timeline clips).
    Free,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewState {
    pub style: WaveStyle,
    pub playhead_s: f64,
    /// Zoom, CSS pixels per second (Scrolling / Free).
    pub px_per_s: f64,
    /// Left edge in seconds (Free).
    pub offset_s: f64,
    /// p95 normalisation (never stored): with it on, the 95th percentile of the track's RMS /
    /// band levels reaches full height (see [`crate::bars::Refs`]); off uses fixed absolute
    /// references. [`WaveStyle::Bars`] always normalises.
    pub normalise: bool,
    pub mode: ViewMode,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            style: WaveStyle::default(),
            playhead_s: 0.0,
            px_per_s: 100.0,
            offset_s: 0.0,
            normalise: false,
            mode: ViewMode::Overview,
        }
    }
}

pub type Rgba = [f32; 4];

/// Sub-rectangle of a canvas in CSS pixels (origin top-left) for `WaveformView::draw_in`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Plain colours (straight alpha, 0..1) so the UI can map its theme tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct WaveTheme {
    pub background: Rgba,
    /// Mono envelope.
    pub wave: Rgba,
    /// Mono RMS core.
    pub core: Rgba,
    /// RGB spectral hue anchors (bass / mids / highs).
    pub low: Rgba,
    pub mid: Rgba,
    pub high: Rgba,
    /// Three-band layer colours, drawn back to front.
    pub layer_low: Rgba,
    pub layer_mid: Rgba,
    pub layer_high: Rgba,
    /// Overlay on the already-played part (alpha 0 = no overlay rectangle).
    pub played_shade: Rgba,
    /// Desaturate the waveform itself left of the playhead and fade it towards `played_to`
    /// (0 = off, 1 = strongest). Unlike `played_shade` it leaves the background untouched, so it
    /// works on transparent backgrounds.
    pub played_dim: f32,
    /// Colour the played part fades towards: the surface behind the canvas.
    pub played_to: Rgba,
    pub playhead: Rgba,
    pub beat: Rgba,
    pub bar: Rgba,
    pub phrase: Rgba,
    pub cue: Rgba,
    pub mix_in: Rgba,
    pub mix_out: Rgba,
    pub loop_fill: Rgba,
    pub loop_edge: Rgba,
    pub buffered: Rgba,
    pub hover: Rgba,
    pub chapter: Rgba,
    /// [`WaveStyle::Bars`]: colour of the already-played bars (default near-white).
    pub played: Rgba,
    /// [`WaveStyle::Bars`]: colour of the not-yet-played bars (default cyan).
    pub unplayed: Rgba,
    /// [`WaveStyle::Bars`]: bar width in CSS px (default 2.0). Converted to device px with the
    /// DPR and snapped to an integer (at least 1).
    pub bar_w_css: f32,
    /// [`WaveStyle::Bars`]: gap between bars in CSS px (default 1.0), snapped like `bar_w_css`.
    pub gap_css: f32,
}

impl Default for WaveTheme {
    fn default() -> Self {
        Self {
            background: [0.06, 0.06, 0.08, 1.0],
            wave: [0.75, 0.78, 0.85, 1.0],
            core: [1.0, 1.0, 1.0, 0.9],
            low: [0.95, 0.15, 0.2, 1.0],
            mid: [0.2, 0.9, 0.35, 1.0],
            high: [0.2, 0.5, 1.0, 1.0],
            layer_low: [0.13, 0.36, 0.95, 1.0],
            layer_mid: [0.98, 0.62, 0.16, 1.0],
            layer_high: [0.96, 0.95, 0.9, 1.0],
            played_shade: [0.0, 0.0, 0.0, 0.45],
            played_dim: 0.0,
            played_to: [0.0, 0.0, 0.0, 1.0],
            playhead: [1.0, 1.0, 1.0, 1.0],
            beat: [1.0, 1.0, 1.0, 0.38],
            bar: [1.0, 1.0, 1.0, 0.7],
            phrase: [1.0, 0.8, 0.2, 0.75],
            cue: [1.0, 0.6, 0.1, 1.0],
            mix_in: [0.2, 0.9, 0.5, 1.0],
            mix_out: [1.0, 0.35, 0.35, 1.0],
            loop_fill: [0.3, 0.6, 1.0, 0.25],
            loop_edge: [0.3, 0.6, 1.0, 0.9],
            buffered: [1.0, 1.0, 1.0, 0.3],
            hover: [1.0, 1.0, 1.0, 0.6],
            chapter: [1.0, 1.0, 1.0, 0.7],
            played: [0.94, 0.94, 0.94, 1.0],
            unplayed: [0.18, 0.78, 0.9, 1.0],
            bar_w_css: 2.0,
            gap_css: 1.0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Markers {
    pub grid: Option<BeatGrid>,
    pub cues: Vec<CuePoint>,
    pub mix_in_s: Option<f64>,
    pub mix_out_s: Option<f64>,
    pub loop_region: Option<(f64, f64)>,
    pub buffered_to_s: Option<f64>,
    pub hover_s: Option<f64>,
    /// Ticks every 16 bars (needs a grid).
    pub chapter_ticks: bool,
}

/// A view resolved against a canvas: left edge and zoom in CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resolved {
    pub t_left_s: f64,
    pub px_per_s: f64,
}

pub fn resolve_view(v: &ViewState, duration_s: f64, css_w: f64) -> Resolved {
    match v.mode {
        ViewMode::Overview => Resolved {
            t_left_s: 0.0,
            px_per_s: if duration_s > 0.0 {
                css_w / duration_s
            } else {
                1.0
            },
        },
        ViewMode::Scrolling => {
            let z = v.px_per_s.max(1e-6);
            Resolved {
                t_left_s: v.playhead_s - css_w * 0.5 / z,
                px_per_s: z,
            }
        }
        ViewMode::Free => Resolved {
            t_left_s: v.offset_s,
            px_per_s: v.px_per_s.max(1e-6),
        },
    }
}

impl Resolved {
    pub fn x_at_time(&self, t_s: f64) -> f64 {
        (t_s - self.t_left_s) * self.px_per_s
    }
    pub fn time_at_x(&self, x: f64) -> f64 {
        self.t_left_s + x / self.px_per_s
    }
    pub fn visible(&self, css_w: f64) -> (f64, f64) {
        (self.t_left_s, self.time_at_x(css_w))
    }
}

/// Mix `gain` in u8-scale offset: with `normalise` the loudest peak maps to 255.
pub fn normalise_offset(max_peak_u8: u8, normalise: bool) -> f32 {
    if normalise && max_peak_u8 > 0 {
        (255 - max_peak_u8) as f32 / 255.0
    } else {
        0.0
    }
}

// ---- display mapping -----------------------------------------------------------------------
//
// Stored bytes are dBFS (-60..0, see `scale`); pooling happens in the linear domain (energy
// mean, see `format::Levels::resample_overview`). Heights are the linear amplitude divided by a
// per-track reference (the 95th percentile, [`crate::bars::Refs`]) and expanded with
// [`DECK_EXPONENT`], so a loud master fills the height without saturating and quiet passages
// stay visibly lower. Every band is normalised by its own p95, so the spectral hue reflects the
// real balance instead of collapsing towards grey. The WebGL shader mirrors these functions
// (constants are spliced into its source).

/// Height expansion exponent for the deck styles.
pub const DECK_EXPONENT: f32 = 1.3;
/// Exponent sharpening the normalised band levels before they pick the spectral hue.
pub const HUE_SHARPNESS: f32 = 3.0;
/// Minimum saturation of the spectral colour (0 = may go grey, 1 = always fully saturated).
pub const SAT_FLOOR: f32 = 0.7;
/// Alpha factor of the already-played part of the deck styles (same hue, 40 % alpha).
pub const PLAYED_ALPHA: f32 = 0.4;
/// Alpha of the mid and high layers of the three-band style.
pub const LAYER_ALPHA: f32 = 0.85;
/// Mono body reference headroom over the RMS p99 so loud sections stay below full height and
/// the body keeps its bar-to-bar variation.
pub const MONO_BODY_HEADROOM: f32 = 1.3;
/// Peak halo alpha of the mono style.
pub const HALO_ALPHA: f32 = 0.35;

/// Stored byte (as 0..1) to linear amplitude.
pub fn byte_to_lin(x: f32) -> f32 {
    if x <= 0.0 {
        0.0
    } else {
        10f32.powf(3.0 * x.min(1.0) - 3.0)
    }
}

/// Height (0..1 of the half-height) of a linear amplitude against its reference.
pub fn norm_height(lin: f32, reference: f32) -> f32 {
    (lin / reference.max(1e-6)).clamp(0.0, 1.0).powf(DECK_EXPONENT)
}

/// Band levels (low, mid, high; linear) normalised by their own references, each 0..1.
pub fn band_levels(low: f32, mid: f32, high: f32, r: &crate::bars::Refs) -> [f32; 3] {
    [
        (low / r.low.max(1e-6)).clamp(0.0, 1.0),
        (mid / r.mid.max(1e-6)).clamp(0.0, 1.0),
        (high / r.high.max(1e-6)).clamp(0.0, 1.0),
    ]
}

/// Spectral colour from normalised band levels: the sharpened levels give a position along
/// low -> mid -> high, the colour is interpolated between the two neighbouring anchors (so it
/// never washes out to white or olive), saturation follows the spread between the bands (never
/// below [`SAT_FLOOR`]), and the result is brightened halfway to full value.
pub fn spectral_color(n: [f32; 3], theme: &WaveTheme) -> [f32; 3] {
    let w = n.map(|v| v.powf(HUE_SHARPNESS));
    let sum = w[0] + w[1] + w[2];
    let rgb = |c: Rgba| [c[0], c[1], c[2]];
    if sum <= 1e-6 {
        return rgb(theme.low);
    }
    let pos = (w[1] * 0.5 + w[2]) / sum; // 0 = low, 0.5 = mid, 1 = high
    let (a, b, f) = if pos < 0.5 {
        (rgb(theme.low), rgb(theme.mid), pos * 2.0)
    } else {
        (rgb(theme.mid), rgb(theme.high), (pos - 0.5) * 2.0)
    };
    let c: [f32; 3] = std::array::from_fn(|k| a[k] + (b[k] - a[k]) * f);
    let (mx, mn) = (n[0].max(n[1]).max(n[2]), n[0].min(n[1]).min(n[2]));
    let sat = SAT_FLOOR + (1.0 - SAT_FLOOR) * ((mx - mn) / mx.max(1e-4));
    let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let c: [f32; 3] = std::array::from_fn(|k| l + (c[k] - l) * sat);
    let m = c[0].max(c[1]).max(c[2]).max(1e-4);
    let boost = 1.0 + (1.0 / m - 1.0) * 0.5;
    c.map(|v| (v * boost).min(1.0))
}

// ---- markers -------------------------------------------------------------------------------

/// A filled rectangle in CSS pixels, `y` from the top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub color: Rgba,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Beat,
    Bar,
    Phrase,
}

/// Minimum spacing between drawn grid lines.
pub const MIN_LINE_PX: f64 = 4.0;

/// Grid lines inside the visible window as `(x_css, kind)`. Beats are dropped when they would be
/// closer than [`MIN_LINE_PX`], then bars, then phrases.
pub fn grid_lines(grid: &BeatGrid, res: &Resolved, css_w: f64) -> Vec<(f64, LineKind)> {
    if !grid.is_usable() || css_w <= 0.0 {
        return vec![];
    }
    let (t0, t1) = res.visible(css_w);
    let (from_ms, to_ms) = (t0 * 1000.0 - 50.0, t1 * 1000.0 + 50.0);
    let bpm = grid.bpm().max(1e-6);
    let beat_px = 60.0 / bpm * res.px_per_s;
    let bar_px = beat_px * grid.beats_per_bar.max(1) as f64;
    let phrase_bars = 4; // phrase marker every 4 bars of lines; chapters are separate
    let phrase_px = bar_px * phrase_bars as f64;
    let (min_kind, ok) = if beat_px >= MIN_LINE_PX {
        (0, true)
    } else if grid.downbeat_phase.is_some() && bar_px >= MIN_LINE_PX {
        (1, true)
    } else if grid.downbeat_phase.is_some() && phrase_px >= MIN_LINE_PX {
        (2, true)
    } else {
        (3, false)
    };
    if !ok {
        return vec![];
    }
    let ticks = grid.ticks(from_ms, to_ms, phrase_bars, 60_000);
    let mut out = Vec::with_capacity(ticks.len());
    for t in ticks {
        let kind = if t.is_phrase {
            LineKind::Phrase
        } else if t.is_downbeat {
            LineKind::Bar
        } else {
            LineKind::Beat
        };
        let rank = match kind {
            LineKind::Beat => 0,
            LineKind::Bar => 1,
            LineKind::Phrase => 2,
        };
        if rank >= min_kind {
            out.push((res.x_at_time(t.t_ms / 1000.0), kind));
        }
    }
    out
}

/// Chapter tick x positions (every 16 bars), at most one per [`MIN_LINE_PX`].
pub fn chapter_ticks(grid: &BeatGrid, res: &Resolved, css_w: f64) -> Vec<f64> {
    if !grid.is_usable() || grid.downbeat_phase.is_none() {
        return vec![];
    }
    let (t0, t1) = res.visible(css_w);
    let ticks = grid.ticks(t0 * 1000.0 - 50.0, t1 * 1000.0 + 50.0, 16, 100_000);
    let mut out: Vec<f64> = vec![];
    for t in ticks.into_iter().filter(|t| t.is_phrase) {
        let x = res.x_at_time(t.t_ms / 1000.0);
        if out.last().is_none_or(|l| x - l >= MIN_LINE_PX) {
            out.push(x);
        }
    }
    out
}

pub fn parse_hex_color(s: &str) -> Option<Rgba> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let c = |i: usize| {
        u8::from_str_radix(&h[i..i + 2], 16)
            .ok()
            .map(|v| v as f32 / 255.0)
    };
    Some([c(0)?, c(2)?, c(4)?, 1.0])
}

fn vline(x: f64, w: f64, y0: f32, y1: f32, color: Rgba, css_w: f64) -> Option<Rect> {
    let (a, b) = (x - w * 0.5, x + w * 0.5);
    if b < 0.0 || a > css_w || color[3] <= 0.0 {
        return None;
    }
    Some(Rect {
        x0: a as f32,
        y0,
        x1: b as f32,
        y1,
        color,
    })
}

/// All marker geometry, back to front. Pure: shared by the WebGL and Canvas2D renderers.
pub fn build_marker_rects(
    m: &Markers,
    v: &ViewState,
    theme: &WaveTheme,
    duration_s: f64,
    css_w: f64,
    css_h: f64,
) -> Vec<Rect> {
    let res = resolve_view(v, duration_s, css_w);
    let (w, h) = (css_w as f32, css_h as f32);
    let mut out: Vec<Rect> = Vec::new();
    let clip = |x: f64| x.clamp(0.0, css_w) as f32;
    let bars = v.style == WaveStyle::Bars;
    let px = res.x_at_time(v.playhead_s);

    if bars {
        // The colour boundary is the playhead: no shade, no playhead line, no grid, no hover
        // line (hover is a tint in the shader). Cues and mix points are 1 px lines + a flag.
        let flag = (h * 0.22).clamp(3.0, 6.0);
        let mut mark = |t: f64, col: Rgba| {
            let x = res.x_at_time(t);
            out.extend(vline(x, 1.0, 0.0, h, col, css_w));
            out.extend(vline(x + 2.0, 3.0, 0.0, flag, col, css_w));
        };
        for c in &m.cues {
            let col = c.color.as_deref().and_then(parse_hex_color).unwrap_or(match c.kind {
                CueKind::MixIn => theme.mix_in,
                CueKind::MixOut => theme.mix_out,
                _ => theme.cue,
            });
            mark(c.pos_ms / 1000.0, col);
        }
        for (t, col) in [(m.mix_in_s, theme.mix_in), (m.mix_out_s, theme.mix_out)] {
            if let Some(t) = t {
                mark(t, col);
            }
        }
        let _ = w;
        return out;
    }

    // played shading
    if px > 0.0 && theme.played_shade[3] > 0.0 {
        out.push(Rect {
            x0: 0.0,
            y0: 0.0,
            x1: clip(px),
            y1: h,
            color: theme.played_shade,
        });
    }
    // loop region
    if let Some((a, b)) = m.loop_region {
        let (xa, xb) = (res.x_at_time(a.min(b)), res.x_at_time(a.max(b)));
        if xb >= 0.0 && xa <= css_w {
            out.push(Rect {
                x0: clip(xa),
                y0: 0.0,
                x1: clip(xb),
                y1: h,
                color: theme.loop_fill,
            });
            out.extend(vline(xa, 1.5, 0.0, h, theme.loop_edge, css_w));
            out.extend(vline(xb, 1.5, 0.0, h, theme.loop_edge, css_w));
        }
    }
    // grid
    if let Some(g) = &m.grid {
        for (x, kind) in grid_lines(g, &res, css_w) {
            let (c, y0, wd) = match kind {
                LineKind::Beat => (theme.beat, h * 0.35, 1.0),
                LineKind::Bar => (theme.bar, 0.0, 1.0),
                LineKind::Phrase => (theme.phrase, 0.0, 1.5),
            };
            out.extend(vline(x, wd, y0, h, c, css_w));
        }
    }
    // chapter ticks
    if m.chapter_ticks
        && let Some(g) = &m.grid
    {
        for x in chapter_ticks(g, &res, css_w) {
            out.extend(vline(
                x,
                1.0,
                0.0,
                (h * 0.18).max(4.0).min(h),
                theme.chapter,
                css_w,
            ));
        }
    }
    // buffered range: thin bar at the bottom
    if let Some(b) = m.buffered_to_s {
        let x1 = clip(res.x_at_time(b));
        if x1 > 0.0 {
            out.push(Rect {
                x0: 0.0,
                y0: (h - 2.0).max(0.0),
                x1,
                y1: h,
                color: theme.buffered,
            });
        }
    }
    // cues
    for c in &m.cues {
        let col = c
            .color
            .as_deref()
            .and_then(parse_hex_color)
            .unwrap_or(match c.kind {
                CueKind::MixIn => theme.mix_in,
                CueKind::MixOut => theme.mix_out,
                _ => theme.cue,
            });
        let x = res.x_at_time(c.pos_ms / 1000.0);
        out.extend(vline(x, 2.0, 0.0, h, col, css_w));
        out.extend(vline(x + 3.0, 6.0, 0.0, 6.0_f32.min(h), col, css_w));
        if let (CueKind::Loop, Some(e)) = (c.kind, c.end_ms) {
            let xe = res.x_at_time(e / 1000.0);
            out.extend(vline(xe, 1.0, 0.0, h, col, css_w));
        }
    }
    // mix in/out
    for (t, col) in [(m.mix_in_s, theme.mix_in), (m.mix_out_s, theme.mix_out)] {
        if let Some(t) = t {
            out.extend(vline(res.x_at_time(t), 2.0, 0.0, h, col, css_w));
        }
    }
    if let Some(t) = m.hover_s {
        out.extend(vline(res.x_at_time(t), 1.0, 0.0, h, theme.hover, css_w));
    }
    out.extend(vline(px, 2.0, 0.0, h, theme.playhead, css_w));
    let _ = w;
    out
}

// ---- texture layout ------------------------------------------------------------------------

/// Maximum number of pyramid levels the shader handles.
pub const MAX_LEVELS: usize = 24;
/// Preferred texture width in texels.
pub const TEX_WIDTH: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelSlot {
    pub n: usize,
    pub row_off: usize,
}

/// Where each pyramid level lives in the two RGBA8 textures: every level occupies
/// `ceil(n / width)` consecutive rows (long levels wrap), levels stacked top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub width: usize,
    pub rows: usize,
    /// Index (into the pyramid) of the first uploaded level; finer levels were dropped because
    /// they did not fit `MAX_TEXTURE_SIZE`.
    pub first_level: usize,
    pub slots: Vec<LevelSlot>,
}

/// Plan the texture layout for levels with the given point counts.
pub fn plan_layout(counts: &[usize], max_tex: usize) -> Layout {
    let width = TEX_WIDTH.min(max_tex.max(1));
    let mut first = 0;
    loop {
        let rows: usize = counts[first..]
            .iter()
            .map(|n| n.div_ceil(width).max(1))
            .sum();
        let too_many = counts.len() - first > MAX_LEVELS;
        if (rows <= max_tex && !too_many) || first + 1 >= counts.len() {
            let mut slots = Vec::new();
            let mut off = 0;
            for &n in &counts[first..] {
                slots.push(LevelSlot { n, row_off: off });
                off += n.div_ceil(width).max(1);
            }
            return Layout {
                width,
                rows: off.max(1),
                first_level: first,
                slots,
            };
        }
        first += 1;
    }
}

/// Interleave planar levels into the two RGBA8 textures: A = (peak_pos, peak_neg, rms, 255),
/// B = (low, mid, high, 255).
pub fn pack_textures(p: &Pyramid, layout: &Layout) -> (Vec<u8>, Vec<u8>) {
    let mut a = vec![0u8; layout.width * layout.rows * 4];
    let mut b = vec![0u8; layout.width * layout.rows * 4];
    for (si, slot) in layout.slots.iter().enumerate() {
        let lvl = &p.levels[layout.first_level + si];
        for i in 0..slot.n {
            let o = (slot.row_off * layout.width + i) * 4;
            let pt = lvl.point(i);
            a[o..o + 4].copy_from_slice(&[pt[0], pt[1], pt[2], 255]);
            b[o..o + 4].copy_from_slice(&[pt[3], pt[4], pt[5], 255]);
        }
    }
    (a, b)
}

/// Texel coordinates of point `i` of slot `slot`.
pub fn texel_of(layout: &Layout, slot: usize, i: usize) -> (usize, usize) {
    let s = layout.slots[slot];
    (i % layout.width, s.row_off + i / layout.width)
}

/// The level the shader picks for a device column covering `sec_per_px` seconds:
/// mirrors [`Pyramid::level_for`], clamped to the uploaded range. Returns a pyramid index.
pub fn level_for_column(sec_per_px: f64, dt0_s: f64, layout: &Layout) -> usize {
    let last = layout.first_level + layout.slots.len() - 1;
    crate::mip::level_for(sec_per_px / dt0_s, last + 1).max(layout.first_level)
}

/// Scalar reference of the shader's column lookup (used by the Canvas2D fallback and tests):
/// max over the points the column covers at the best level.
pub fn column_value(p: &Pyramid, t0: f64, t1: f64) -> [u8; PLANES] {
    p.sample_column(t0, t1)
}
