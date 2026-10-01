//! Pure logic of the Arrange timeline: continuous zoom and pan math, the seconds-based layout
//! (built on `logic::timeline`), hit-testing, trim/blend-length gestures with beat snapping, ruler
//! ticks, playhead and scrub mapping, window planning for the waveform renderer and keyboard
//! navigation. Natively testable; the canvas painting lives in `paint.rs`.
//!
//! Units: **timeline seconds** `t` (set clock), **track seconds** (position in the audio file) and
//! CSS pixels. A clip plays the track window `[cue_in, cue_out]` at tempo multiplier `mult`
//! (`track_s = cue_in + (t - start) * mult`).
#![allow(dead_code)]

use bc_music::beatgrid::{GridQuery, Snap};
use bc_types::analysis::BeatGrid;

use crate::logic::timeline::{self as tl, DragField, DragOverride, GeomItem};

// ---- constants ---------------------------------------------------------------------------

pub const MAX_PPS: f64 = 320.0;
/// Hard floor of the zoom (the real floor is "whole set fits", see [`min_pps`]).
pub const ABS_MIN_PPS: f64 = 0.02;
/// Below this many ms between the cues a clip stops being a clip.
pub const MIN_SPAN_MS: f64 = 5_000.0;
/// Pointer travel before a press becomes a drag.
pub const DRAG_THRESHOLD_PX: f64 = 4.0;
/// Content padding at both ends of the timeline (css px).
pub const EDGE_PAD_PX: f64 = 40.0;
/// Track px/s above which the detail level is worth fetching (the overview has ~5.7 pts/s).
pub const DETAIL_PX_PER_TRACK_S: f64 = 3.0;
/// The transition kinds of `bc_types::player::TransitionKind` as stored (`snake_case`) with labels.
pub const TRANSITION_KINDS: [(&str, &str); 5] =
    [("blend", "Blend"), ("bass_swap", "Bass swap"), ("filter", "Filter"), ("echo_out", "Echo out"), ("cut", "Cut")];

pub fn kind_label(kind: &str) -> &'static str {
    TRANSITION_KINDS.iter().find(|(k, _)| *k == kind).map(|(_, l)| *l).unwrap_or("Blend")
}

// ---- metrics -----------------------------------------------------------------------------

/// Vertical layout of the canvas, in css px.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Metrics {
    pub ruler_h: f64,
    pub pad: f64,
    pub head_h: f64,
    pub wave_h: f64,
    pub gap: f64,
    pub mini_gap: f64,
    pub mini_h: f64,
    /// Half width of a trim handle's grab zone.
    pub grab: f64,
}

impl Metrics {
    pub fn new(compact: bool, coarse: bool) -> Self {
        let grab = if coarse { 14.0 } else { 7.0 };
        if compact {
            Metrics { ruler_h: 30.0, pad: 6.0, head_h: 18.0, wave_h: 58.0, gap: 26.0, mini_gap: 8.0, mini_h: 34.0, grab }
        } else {
            Metrics { ruler_h: 32.0, pad: 8.0, head_h: 20.0, wave_h: 76.0, gap: 26.0, mini_gap: 10.0, mini_h: 42.0, grab }
        }
    }
    pub fn lane_h(&self) -> f64 {
        self.head_h + self.wave_h
    }
    pub fn lane_top(&self, lane: u8) -> f64 {
        self.ruler_h + self.pad + lane as f64 * (self.lane_h() + self.gap)
    }
    /// Top of the waveform part of a lane (below the clip header).
    pub fn wave_top(&self, lane: u8) -> f64 {
        self.lane_top(lane) + self.head_h
    }
    pub fn lanes_bottom(&self) -> f64 {
        self.lane_top(1) + self.lane_h()
    }
    /// Vertical centre of the gap between the lanes (where transition chips sit).
    pub fn gap_mid(&self) -> f64 {
        self.lane_top(0) + self.lane_h() + self.gap * 0.5
    }
    pub fn mini_top(&self) -> f64 {
        self.lanes_bottom() + self.pad + self.mini_gap
    }
    pub fn total_h(&self) -> f64 {
        self.mini_top() + self.mini_h + 4.0
    }
}

// ---- view (pan / zoom) -------------------------------------------------------------------

/// What part of the timeline the viewport shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub left_s: f64,
    pub pps: f64,
    pub w: f64,
}

impl View {
    pub fn x_of(&self, t: f64) -> f64 {
        (t - self.left_s) * self.pps
    }
    pub fn t_at(&self, x: f64) -> f64 {
        self.left_s + x / self.pps
    }
    pub fn right_s(&self) -> f64 {
        self.t_at(self.w)
    }
    pub fn span_s(&self) -> f64 {
        self.w / self.pps
    }
}

/// Zoom floor: the whole set fits the width (never below [`ABS_MIN_PPS`]).
pub fn min_pps(total_s: f64, w: f64) -> f64 {
    if total_s <= 0.0 || w <= 0.0 {
        return 1.0;
    }
    (((w - 2.0 * EDGE_PAD_PX).max(40.0)) / total_s).clamp(ABS_MIN_PPS, MAX_PPS * 0.5)
}

pub fn fit_pps(total_s: f64, w: f64) -> f64 {
    min_pps(total_s, w).min(8.0)
}

pub fn clamp_pps(pps: f64, total_s: f64, w: f64) -> f64 {
    pps.clamp(min_pps(total_s, w) * 0.999, MAX_PPS)
}

/// `left_s` after zooming to `new_pps` with the time under `anchor_x` fixed.
pub fn zoom_about(left_s: f64, pps: f64, anchor_x: f64, new_pps: f64) -> f64 {
    let t = left_s + anchor_x / pps;
    t - anchor_x / new_pps
}

/// Keep some content in view: the set may be scrolled until only the last/first `EDGE_PAD_PX` remain.
pub fn clamp_left(left_s: f64, pps: f64, w: f64, total_s: f64) -> f64 {
    let pad = EDGE_PAD_PX / pps;
    let view_s = w / pps;
    let max_left = (total_s + pad - view_s).max(-pad);
    left_s.clamp(-pad, max_left.max(-pad))
}

/// Multiplicative zoom for a wheel event (`delta_mode` 0 px, 1 lines, 2 pages).
pub fn wheel_zoom_factor(delta_y: f64, delta_mode: u32) -> f64 {
    let px = match delta_mode {
        1 => delta_y * 16.0,
        2 => delta_y * 120.0,
        _ => delta_y,
    };
    (-px.clamp(-240.0, 240.0) * 0.0022).exp()
}

/// One step of exponential smoothing toward `target` (`dt` seconds, time constant `tau`).
pub fn smooth_toward(cur: f64, target: f64, dt: f64, tau: f64) -> f64 {
    let k = 1.0 - (-dt / tau).exp();
    let v = cur + (target - cur) * k;
    if ((target - v) / target.abs().max(1e-9)).abs() < 5e-4 { target } else { v }
}

/// Velocity (px/s) of a pan from `(time_ms, x)` samples of the last ~100 ms.
pub fn release_velocity(samples: &[(f64, f64)]) -> f64 {
    let Some(&(t1, x1)) = samples.last() else { return 0.0 };
    let first = samples.iter().find(|(t, _)| t1 - t <= 100.0).copied().unwrap_or((t1, x1));
    let dt = (t1 - first.0) / 1000.0;
    if dt < 0.008 { 0.0 } else { (x1 - first.1) / dt }
}

/// Inertia: advance a pan velocity (px/s). Returns `(dx_px, new_velocity)`; the velocity is 0 once it is
/// below 8 px/s.
pub fn inertia_step(v: f64, dt: f64) -> (f64, f64) {
    let nv = v * (-dt / 0.35).exp();
    if nv.abs() < 8.0 { (v * dt, 0.0) } else { (v * dt, nv) }
}

// ---- layout ------------------------------------------------------------------------------

/// One clip on the timeline (seconds).
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub index: usize,
    pub id: i64,
    pub lane: u8,
    pub start_s: f64,
    pub len_s: f64,
    pub cue_in_s: f64,
    pub cue_out_s: f64,
    pub mult: f64,
}

impl Clip {
    pub fn end_s(&self) -> f64 {
        self.start_s + self.len_s
    }
    pub fn track_s_at(&self, t: f64) -> f64 {
        self.cue_in_s + (t - self.start_s) * self.mult
    }
    pub fn t_at_track(&self, track_s: f64) -> f64 {
        self.start_s + (track_s - self.cue_in_s) / self.mult
    }
}

/// The transition joining `items[index]` into `items[index + 1]` (a zero-length band is a cut).
#[derive(Debug, Clone, PartialEq)]
pub struct Band {
    pub index: usize,
    pub start_s: f64,
    pub len_s: f64,
    pub beats: u32,
}

impl Band {
    pub fn end_s(&self) -> f64 {
        self.start_s + self.len_s
    }
    pub fn mid_s(&self) -> f64 {
        self.start_s + self.len_s * 0.5
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layout {
    pub clips: Vec<Clip>,
    pub bands: Vec<Band>,
    pub total_s: f64,
}

/// `overlaps[i]` = server overlap (ms) of the join `i -> i + 1`.
pub fn build_layout(items: &[GeomItem], overlaps: &[f64], ov: Option<DragOverride>) -> Layout {
    // pps = 1 makes px == seconds
    let g = tl::compute_geometry(items, overlaps, 1.0, ov);
    let clips: Vec<Clip> = g
        .clips
        .iter()
        .zip(items)
        .map(|(c, it)| {
            let dur = it.duration_ms.unwrap_or(0.0);
            let mut cue_in = it.cue_in_ms.unwrap_or(0.0);
            let mut cue_out = it.cue_out_ms.unwrap_or(dur);
            if let Some(o) = ov.filter(|o| o.item_id == it.id) {
                match o.field {
                    DragField::CueIn => cue_in = o.value,
                    DragField::CueOut => cue_out = o.value,
                    DragField::TransitionBeats => {}
                }
            }
            Clip {
                index: c.index,
                id: it.id,
                lane: c.lane,
                start_s: c.left_px,
                len_s: c.width_px,
                cue_in_s: cue_in / 1000.0,
                cue_out_s: cue_out / 1000.0,
                mult: tl::tempo_multiplier(it),
            }
        })
        .collect();
    let mut bands = Vec::new();
    for i in 0..items.len().saturating_sub(1) {
        let inc = &items[i + 1];
        let (beats, ov_ms) = match ov {
            Some(o) if o.item_id == inc.id && o.field == DragField::TransitionBeats => {
                let b = o.value.round().max(0.0) as u32;
                (b, tl::overlap_ms(Some(b), inc.effective_bpm))
            }
            _ => (inc.transition_beats.unwrap_or(0), overlaps.get(i).copied().unwrap_or(0.0)),
        };
        let cap = clips[i].len_s.min(clips[i + 1].len_s);
        bands.push(Band { index: i, start_s: clips[i + 1].start_s, len_s: (ov_ms / 1000.0).min(cap).max(0.0), beats });
    }
    let total_s = clips.iter().map(|c| c.end_s()).fold(0.0, f64::max);
    Layout { clips, bands, total_s }
}

/// Window plan for the waveform renderer: where a clip's canvas rectangle is and which track time
/// is at canvas x = 0 with what zoom (`ViewMode::Free`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipWindow {
    pub x0: f64,
    pub x1: f64,
    /// Track seconds at canvas x = 0 (the lane canvas spans the whole viewport width).
    pub offset_s: f64,
    /// Renderer zoom: css px per *track* second.
    pub px_per_track_s: f64,
}

/// `None` when the clip is outside `[0, view.w]`.
pub fn clip_window(c: &Clip, v: &View) -> Option<ClipWindow> {
    let x0 = v.x_of(c.start_s);
    let x1 = v.x_of(c.end_s());
    if x1 < 0.0 || x0 > v.w || x1 - x0 < 0.5 {
        return None;
    }
    Some(ClipWindow { x0, x1, offset_s: c.track_s_at(v.left_s), px_per_track_s: v.pps / c.mult })
}

/// Clips intersecting `[from_s, to_s]`.
pub fn visible_clips(l: &Layout, from_s: f64, to_s: f64) -> Vec<usize> {
    l.clips.iter().filter(|c| c.end_s() >= from_s && c.start_s <= to_s).map(|c| c.index).collect()
}

/// Whether the detail waveform level is needed at this zoom.
pub fn wants_detail(px_per_track_s: f64) -> bool {
    px_per_track_s > DETAIL_PX_PER_TRACK_S
}

// ---- ruler ------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct RulerTick {
    pub t: f64,
    pub major: bool,
    pub label: Option<String>,
}

pub fn fmt_clock(s: f64) -> String {
    let t = s.max(0.0).round() as i64;
    if t >= 3600 { format!("{}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60) } else { format!("{}:{:02}", t / 60, t % 60) }
}

pub fn fmt_clock_tenths(s: f64) -> String {
    let neg = s < 0.0;
    let ms = (s.abs() * 10.0).round() as i64;
    let (t, d) = (ms / 10, ms % 10);
    let body = if t >= 3600 { format!("{}:{:02}:{:02}.{d}", t / 3600, (t / 60) % 60, t % 60) } else { format!("{}:{:02}.{d}", t / 60, t % 60) };
    if neg { format!("-{body}") } else { body }
}

/// Nice major interval (seconds) with at least `min_px` between labels, and the minor subdivision count.
pub fn tick_step(pps: f64, min_px: f64) -> (f64, u32) {
    const STEPS: [(f64, u32); 15] = [
        (1.0, 4),
        (2.0, 4),
        (5.0, 5),
        (10.0, 5),
        (15.0, 3),
        (30.0, 6),
        (60.0, 4),
        (120.0, 4),
        (300.0, 5),
        (600.0, 5),
        (900.0, 3),
        (1800.0, 6),
        (3600.0, 6),
        (7200.0, 4),
        (14400.0, 4),
    ];
    for (s, m) in STEPS {
        if s * pps >= min_px {
            return (s, m);
        }
    }
    (14400.0, 4)
}

/// Ticks inside `[from_s, to_s]`; minor ticks only when they are >= 9 px apart.
pub fn ruler_ticks(from_s: f64, to_s: f64, pps: f64) -> Vec<RulerTick> {
    let (major, subs) = tick_step(pps, 84.0);
    let minor = major / subs as f64;
    let show_minor = minor * pps >= 9.0;
    let step = if show_minor { minor } else { major };
    let first = ((from_s.max(0.0) / step).floor() as i64).max(0);
    let mut out = Vec::new();
    let mut k = first;
    loop {
        let t = k as f64 * step;
        if t > to_s + step || out.len() > 2000 {
            break;
        }
        let is_major = !show_minor || k % subs as i64 == 0;
        out.push(RulerTick { t, major: is_major, label: is_major.then(|| fmt_clock(t)) });
        k += 1;
    }
    out
}

// ---- playhead / scrub --------------------------------------------------------------------

/// Timeline seconds of a player position inside item `index` (clamped to the clip).
pub fn playhead_t(c: &Clip, track_pos_s: f64) -> f64 {
    c.t_at_track(track_pos_s).clamp(c.start_s, c.end_s())
}

/// The clip a timeline position scrubs to (inside an overlap the earlier clip wins, like the legacy
/// ruler) and the track position there.
pub fn scrub_target(l: &Layout, t: f64) -> Option<(usize, f64)> {
    if l.clips.is_empty() {
        return None;
    }
    let t = t.clamp(0.0, (l.total_s - 0.05).max(0.0));
    let c = l.clips.iter().find(|c| t >= c.start_s && t < c.end_s()).or_else(|| l.clips.last())?;
    Some((c.index, c.track_s_at(t.clamp(c.start_s, c.end_s())).max(0.0)))
}

// ---- hit testing -------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    None,
    Ruler,
    Minimap,
    /// Clip body (index into `Layout::clips`).
    Clip(usize),
    /// Trim handle: `Left` = cue in, `Right` = cue out.
    ClipEdge(usize, Edge),
    /// The chip/body of the transition `i -> i + 1` (drag = blend length).
    Band(usize),
    /// `Left` = blend start (length in beats), `Right` = blend end (cue out of the outgoing clip).
    BandEdge(usize, Edge),
}

pub fn chip_w(label_chars: usize) -> f64 {
    label_chars as f64 * 6.6 + 34.0
}

pub fn chip_h() -> f64 {
    20.0
}

/// Text of a band chip, e.g. `BASS SWAP 32` / `CUT`.
pub fn chip_label(kind: &str, beats: u32) -> String {
    if beats == 0 || kind == "cut" { "CUT".into() } else { format!("{} {}", kind_label(kind).to_uppercase(), beats) }
}

/// Where the chip of a band is drawn, `(cx, w)`; the chip stays visible while its band is partly offscreen.
pub fn chip_x(b: &Band, v: &View, label_chars: usize) -> (f64, f64) {
    let w = chip_w(label_chars);
    let (x0, x1) = (v.x_of(b.start_s), v.x_of(b.end_s()));
    let mut cx = (x0 + x1) * 0.5;
    if x1 - x0 > w + 16.0 {
        cx = cx.clamp(x0 + w * 0.5 + 8.0, x1 - w * 0.5 - 8.0).clamp(w * 0.5 + 4.0, (v.w - w * 0.5 - 4.0).max(x0 + w * 0.5));
        cx = cx.clamp(x0 + w * 0.5, x1 - w * 0.5);
    }
    (cx, w)
}

pub struct HitCtx<'a> {
    pub layout: &'a Layout,
    pub view: View,
    pub m: Metrics,
    /// chip label length per band (chars)
    pub chip_chars: &'a [usize],
    pub canvas_h: f64,
}

pub fn hit_test(c: &HitCtx, x: f64, y: f64) -> Hit {
    let m = &c.m;
    if y < m.ruler_h {
        return Hit::Ruler;
    }
    if y >= m.mini_top() - 2.0 && y <= m.mini_top() + m.mini_h {
        return Hit::Minimap;
    }
    // chips (gap row), above clips
    for b in &c.layout.bands {
        let chars = c.chip_chars.get(b.index).copied().unwrap_or(8);
        let (cx, w) = chip_x(b, &c.view, chars);
        let (x0, x1) = (c.view.x_of(b.start_s), c.view.x_of(b.end_s()));
        let gy = m.gap_mid();
        if (x - cx).abs() <= w * 0.5 && (y - gy).abs() <= chip_h() * 0.5 + 2.0 {
            return Hit::Band(b.index);
        }
        // band edge grips live in the gap row
        if (y - gy).abs() <= m.gap * 0.5 && x1 - x0 > 3.0 * m.grab {
            let dl = (x - x0).abs();
            let dr = (x - x1).abs();
            if dl <= m.grab && dl <= dr {
                return Hit::BandEdge(b.index, Edge::Left);
            }
            if dr <= m.grab {
                return Hit::BandEdge(b.index, Edge::Right);
            }
        }
    }
    // clips
    let mut best: Option<(f64, Hit)> = None;
    for cl in &c.layout.clips {
        let top = m.lane_top(cl.lane);
        if y < top || y > top + m.lane_h() {
            continue;
        }
        let (x0, x1) = (c.view.x_of(cl.start_s), c.view.x_of(cl.end_s()));
        if x < x0 - m.grab || x > x1 + m.grab {
            continue;
        }
        let wide = x1 - x0 >= 3.0 * m.grab;
        if wide {
            for (edge, ex) in [(Edge::Left, x0), (Edge::Right, x1)] {
                let d = (x - ex).abs();
                if d <= m.grab && best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                    best = Some((d, Hit::ClipEdge(cl.index, edge)));
                }
            }
        }
        if x >= x0 && x <= x1 && best.is_none() {
            best = Some((1e9, Hit::Clip(cl.index)));
        }
    }
    best.map(|(_, h)| h).unwrap_or(Hit::None)
}

// ---- gestures: trim, blend length, beat snapping ----------------------------------------

/// What snapping to use for a track zoom: bars when beats would be < 6 px apart.
pub fn snap_mode(grid: &BeatGrid, px_per_track_s: f64) -> Snap2 {
    let beat_px = 60.0 / grid.bpm().max(1.0) * px_per_track_s;
    if beat_px >= 6.0 { Snap2::Beat } else { Snap2::Bar }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Snap2 {
    Beat,
    Bar,
}

/// Snap a track position (ms) to the beat or bar lattice; unusable grids and `bypass` (Alt) keep the value.
pub fn snap_cue_ms(grid: Option<&BeatGrid>, ms: f64, px_per_track_s: f64, bypass: bool) -> f64 {
    let Some(g) = grid.filter(|g| !bypass && g.is_usable()) else { return ms };
    match snap_mode(g, px_per_track_s) {
        Snap2::Beat => g.snap_beat_ms(ms, Snap::Nearest),
        Snap2::Bar => g.snap_bar_ms(ms, Snap::Nearest),
    }
    .round()
}

/// New cue (ms of track time) while dragging a trim handle `dx_px` from where it was grabbed.
/// `snapped` is applied by the caller via [`snap_cue_ms`]; this clamps to the track and [`MIN_SPAN_MS`].
pub fn trim_value(edge: Edge, orig_ms: f64, dx_px: f64, pps: f64, mult: f64, cue_in_ms: f64, cue_out_ms: f64, duration_ms: f64) -> f64 {
    let raw = orig_ms + dx_px / pps * 1000.0 * mult;
    match edge {
        Edge::Left => raw.clamp(0.0, (cue_out_ms - MIN_SPAN_MS).max(0.0)),
        Edge::Right => raw.clamp(cue_in_ms + MIN_SPAN_MS, duration_ms.max(cue_in_ms + MIN_SPAN_MS)),
    }
}

/// Clamp after snapping so the span stays >= [`MIN_SPAN_MS`] and inside the track.
pub fn clamp_cue(edge: Edge, v: f64, cue_in_ms: f64, cue_out_ms: f64, duration_ms: f64) -> f64 {
    match edge {
        Edge::Left => v.clamp(0.0, (cue_out_ms - MIN_SPAN_MS).max(0.0)),
        Edge::Right => v.clamp(cue_in_ms + MIN_SPAN_MS, duration_ms.max(cue_in_ms + MIN_SPAN_MS)),
    }
}

/// Blend length (beats) while dragging the start of a band `dx_px` (left = longer), snapped to [`BEAT_CHOICES`].
pub fn blend_beats(orig_overlap_ms: f64, dx_px: f64, pps: f64, bpm: f64, cap_ms: f64) -> u32 {
    let raw = (orig_overlap_ms - dx_px / pps * 1000.0).clamp(0.0, cap_ms.max(0.0));
    tl::quantize_beats(raw, bpm)
}

// ---- patches -----------------------------------------------------------------------------

/// The JSON body that commits a drag (explicit `null` clears, like the server expects).
pub fn override_patch(ov: &DragOverride, item: &GeomItem, transition_type: Option<&str>) -> Option<serde_json::Value> {
    use serde_json::json;
    match ov.field {
        DragField::TransitionBeats => {
            let beats = ov.value.round().max(0.0) as i64;
            if beats as u32 == item.transition_beats.unwrap_or(0) {
                return None;
            }
            let ty = if beats == 0 {
                "cut"
            } else {
                match transition_type {
                    Some(t) if t != "cut" => t,
                    _ => "blend",
                }
            };
            Some(json!({ "transition_beats": if beats == 0 { None } else { Some(beats) }, "transition_type": ty }))
        }
        DragField::CueIn => {
            let cur = item.cue_in_ms.unwrap_or(0.0);
            (ov.value.round() != cur.round()).then(|| json!({ "cue_in_ms": ov.value.round() as i64 }))
        }
        DragField::CueOut => {
            let cur = item.cue_out_ms.or(item.duration_ms).unwrap_or(0.0);
            (ov.value.round() != cur.round()).then(|| json!({ "cue_out_ms": ov.value.round() as i64 }))
        }
    }
}

/// Beats to use when a kind is picked on a cut (0 beats): a musical default.
pub const DEFAULT_BEATS: u32 = 16;

pub fn kind_patch(kind: &str, current_beats: Option<u32>, has_bpm: bool) -> serde_json::Value {
    use serde_json::json;
    if kind == "cut" {
        json!({ "transition_type": "cut", "transition_beats": null })
    } else if current_beats.unwrap_or(0) == 0 && has_bpm {
        json!({ "transition_type": kind, "transition_beats": DEFAULT_BEATS })
    } else {
        json!({ "transition_type": kind })
    }
}

/// Parse a seconds field (`83`, `1:23.5`, `83.5`) into ms; `None` = invalid, `Some(None)` = empty (clear).
pub fn parse_seconds(text: &str) -> Option<Option<i64>> {
    let t = text.trim();
    if t.is_empty() {
        return Some(None);
    }
    let secs = if let Some((m, s)) = t.split_once(':') {
        let (m, s): (f64, f64) = (m.trim().parse().ok()?, s.trim().parse().ok()?);
        m * 60.0 + s
    } else {
        t.parse::<f64>().ok()?
    };
    (secs.is_finite() && secs >= 0.0).then(|| Some((secs * 1000.0).round() as i64))
}

// ---- keyboard ----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Prev,
    Next,
}

/// Next selected clip index (`None` selects the first/last).
pub fn step_selection(cur: Option<usize>, n: usize, dir: Dir) -> Option<usize> {
    if n == 0 {
        return None;
    }
    Some(match (cur, dir) {
        (None, Dir::Next) => 0,
        (None, Dir::Prev) => n - 1,
        (Some(i), Dir::Next) => (i + 1).min(n - 1),
        (Some(i), Dir::Prev) => i.saturating_sub(1),
    })
}

/// Scroll target (left_s) that brings `[a, b]` into view with a margin, `None` if already visible.
pub fn reveal(v: &View, a: f64, b: f64) -> Option<f64> {
    let margin = 48.0 / v.pps;
    let span = v.span_s();
    if a - margin >= v.left_s && b + margin <= v.right_s() {
        return None;
    }
    if b - a + 2.0 * margin >= span || a - margin < v.left_s {
        Some(a - margin)
    } else {
        Some(b + margin - span)
    }
}

// ---- minimap ------------------------------------------------------------------------------

pub fn mini_x(t: f64, total_s: f64, w: f64) -> f64 {
    EDGE_PAD_PX.min(w * 0.05) + t / total_s.max(1e-9) * (w - 2.0 * EDGE_PAD_PX.min(w * 0.05))
}

pub fn mini_t(x: f64, total_s: f64, w: f64) -> f64 {
    let p = EDGE_PAD_PX.min(w * 0.05);
    ((x - p) / (w - 2.0 * p).max(1.0) * total_s).clamp(0.0, total_s)
}

// ---- player mapping -----------------------------------------------------------------------

/// Which set item is playing: the n-th playable item when the queue is this set's, else the first item
/// with the current track id.
pub fn pick_playing_item(current: Option<i64>, queue_index: i64, from_set: bool, track_ids: &[Option<i64>]) -> Option<usize> {
    let cur = current?;
    if from_set && queue_index >= 0 {
        let nth = track_ids.iter().enumerate().filter(|(_, t)| t.is_some()).nth(queue_index as usize);
        if let Some((i, Some(t))) = nth
            && *t == cur
        {
            return Some(i);
        }
    }
    track_ids.iter().position(|t| *t == Some(cur))
}

// ---- tests -------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::timeline::BEAT_CHOICES;
    use bc_music::beatgrid::GridExt;

    fn item(id: i64, start: f64) -> GeomItem {
        GeomItem { id, start_ms: start, played_ms: 300_000.0, duration_ms: Some(300_000.0), effective_bpm: Some(128.0), ..Default::default() }
    }

    fn three() -> (Vec<GeomItem>, Vec<f64>) {
        let a = item(1, 0.0);
        let mut b = item(2, 285_000.0);
        b.transition_beats = Some(32);
        let c = item(3, 570_000.0);
        (vec![a, b, c], vec![15_000.0, 0.0])
    }

    #[test]
    fn layout_in_seconds_with_bands_for_every_join() {
        let (items, ov) = three();
        let l = build_layout(&items, &ov, None);
        assert_eq!(l.clips.len(), 3);
        assert_eq!(l.clips[1].start_s, 285.0);
        assert_eq!(l.clips[1].lane, 1);
        assert_eq!(l.bands.len(), 2);
        assert_eq!(l.bands[0].start_s, 285.0);
        assert_eq!(l.bands[0].len_s, 15.0);
        assert_eq!(l.bands[0].beats, 32);
        // a cut still has a (zero-length) band: the chip needs a place to sit
        assert_eq!(l.bands[1].len_s, 0.0);
        assert_eq!(l.total_s, 870.0);
    }

    #[test]
    fn layout_applies_live_overrides() {
        let (items, ov) = three();
        let o = DragOverride { item_id: 1, field: DragField::CueOut, value: 290_000.0 };
        let l = build_layout(&items, &ov, Some(o));
        assert_eq!(l.clips[0].len_s, 290.0);
        assert_eq!(l.clips[0].cue_out_s, 290.0);
        assert_eq!(l.clips[1].start_s, 275.0);
        assert_eq!(l.bands[0].start_s, 275.0);

        let o = DragOverride { item_id: 2, field: DragField::TransitionBeats, value: 16.0 };
        let l = build_layout(&items, &ov, Some(o));
        assert_eq!(l.bands[0].beats, 16);
        assert!((l.bands[0].len_s - 7.5).abs() < 1e-9);
        let o = DragOverride { item_id: 2, field: DragField::CueIn, value: 20_000.0 };
        let l = build_layout(&items, &ov, Some(o));
        assert_eq!(l.clips[1].cue_in_s, 20.0);
        assert_eq!(l.clips[1].start_s, 285.0 + 20.0);
    }

    #[test]
    fn clip_time_maps_honour_tempo_and_cue() {
        let mut a = item(1, 0.0);
        a.cue_in_ms = Some(30_000.0);
        a.tempo_adjust_pct = 100.0;
        a.played_ms = 135_000.0;
        let l = build_layout(&[a], &[], None);
        let c = &l.clips[0];
        assert_eq!(c.track_s_at(10.0), 50.0);
        assert_eq!(c.t_at_track(50.0), 10.0);
        assert_eq!(playhead_t(c, 5.0), 0.0, "before the cue clamps to the clip start");
    }

    #[test]
    fn zoom_keeps_the_anchor_fixed() {
        let left = zoom_about(100.0, 2.0, 300.0, 8.0);
        let t_before = 100.0 + 300.0 / 2.0;
        let t_after = left + 300.0 / 8.0;
        assert!((t_before - t_after).abs() < 1e-9);
        assert!(wheel_zoom_factor(-100.0, 0) > 1.0);
        assert!(wheel_zoom_factor(100.0, 0) < 1.0);
        assert!((wheel_zoom_factor(1.0, 1) - wheel_zoom_factor(16.0, 0)).abs() < 1e-12);
    }

    #[test]
    fn zoom_floor_fits_the_set() {
        let m = min_pps(1800.0, 1440.0);
        assert!((m * 1800.0 + 2.0 * EDGE_PAD_PX - 1440.0).abs() < 1.0);
        assert_eq!(clamp_pps(1000.0, 1800.0, 1440.0), MAX_PPS);
        assert!(clamp_pps(0.0001, 1800.0, 1440.0) >= m * 0.99);
        assert!(fit_pps(60.0, 1440.0) <= 8.0);
        assert!(min_pps(0.0, 100.0) > 0.0);
    }

    #[test]
    fn clamp_left_keeps_content_visible() {
        let pps = 2.0;
        assert_eq!(clamp_left(-500.0, pps, 800.0, 600.0), -EDGE_PAD_PX / pps);
        let l = clamp_left(10_000.0, pps, 800.0, 600.0);
        assert!(l <= 600.0 + EDGE_PAD_PX / pps - 800.0 / pps + 1e-9 || l == -EDGE_PAD_PX / pps);
        // a set shorter than the view stays left-aligned
        assert_eq!(clamp_left(50.0, 10.0, 800.0, 30.0), -EDGE_PAD_PX / 10.0);
    }

    #[test]
    fn smoothing_converges_and_snaps() {
        let mut v = 1.0;
        for _ in 0..200 {
            v = smooth_toward(v, 4.0, 1.0 / 60.0, 0.07);
        }
        assert_eq!(v, 4.0);
        assert!(smooth_toward(1.0, 4.0, 1.0 / 60.0, 0.07) > 1.0);
    }

    #[test]
    fn inertia_decays_to_rest() {
        let (mut v, mut dist) = (2000.0, 0.0);
        for _ in 0..600 {
            let (dx, nv) = inertia_step(v, 1.0 / 60.0);
            dist += dx;
            v = nv;
        }
        assert_eq!(v, 0.0);
        assert!(dist > 300.0 && dist < 1500.0, "{dist}");
        assert_eq!(release_velocity(&[(0.0, 0.0), (50.0, 50.0), (100.0, 100.0)]), 1000.0);
        assert_eq!(release_velocity(&[(0.0, 0.0), (500.0, 50.0), (510.0, 50.0)]), 0.0);
        assert_eq!(release_velocity(&[]), 0.0);
    }

    #[test]
    fn ruler_ticks_have_labels_on_majors() {
        let t = ruler_ticks(0.0, 600.0, 1.0);
        assert!(t.iter().any(|x| x.major && x.label.is_some()));
        assert!(t.iter().all(|x| x.major == x.label.is_some()));
        let (step, _) = tick_step(1.0, 84.0);
        assert!(step >= 84.0);
        // zoomed out: no minors closer than 9px
        let ticks = ruler_ticks(0.0, 3600.0, 0.1);
        let xs: Vec<f64> = ticks.iter().map(|t| t.t * 0.1).collect();
        assert!(xs.windows(2).all(|w| w[1] - w[0] >= 9.0 - 1e-9));
        assert_eq!(fmt_clock(75.0), "1:15");
        assert_eq!(fmt_clock(3725.0), "1:02:05");
        assert_eq!(fmt_clock_tenths(83.54), "1:23.5");
    }

    #[test]
    fn scrub_picks_earlier_clip_in_overlaps() {
        let (items, ov) = three();
        let l = build_layout(&items, &ov, None);
        let (i, ts) = scrub_target(&l, 290.0).unwrap();
        assert_eq!(i, 0);
        assert_eq!(ts, 290.0);
        let (i, _) = scrub_target(&l, 301.0).unwrap();
        assert_eq!(i, 1);
        let (i, _) = scrub_target(&l, 99_999.0).unwrap();
        assert_eq!(i, 2);
        assert!(scrub_target(&Layout::default(), 1.0).is_none());
    }

    fn ctx_for<'a>(l: &'a Layout, v: View, chars: &'a [usize]) -> HitCtx<'a> {
        let m = Metrics::new(false, false);
        HitCtx { layout: l, view: v, m, chip_chars: chars, canvas_h: m.total_h() }
    }

    #[test]
    fn hit_test_regions() {
        let (items, ov) = three();
        let l = build_layout(&items, &ov, None);
        let v = View { left_s: 0.0, pps: 2.0, w: 1200.0 };
        let chars = [12usize, 3];
        let c = ctx_for(&l, v, &chars);
        let m = c.m;
        assert_eq!(hit_test(&c, 100.0, 5.0), Hit::Ruler);
        assert_eq!(hit_test(&c, 100.0, m.mini_top() + 5.0), Hit::Minimap);
        // inside clip 0 (lane 0)
        assert_eq!(hit_test(&c, 200.0, m.lane_top(0) + 40.0), Hit::Clip(0));
        // right edge of clip 0 at x = 600
        assert_eq!(hit_test(&c, 598.0, m.lane_top(0) + 40.0), Hit::ClipEdge(0, Edge::Right));
        // left edge of clip 1 at x = 570 on lane 1
        assert_eq!(hit_test(&c, 572.0, m.lane_top(1) + 40.0), Hit::ClipEdge(1, Edge::Left));
        // the chip of band 0 is in the gap row, centred on the band (x 570..600)
        assert_eq!(hit_test(&c, 585.0, m.gap_mid()), Hit::Band(0));
        // empty space right of the set
        assert_eq!(hit_test(&c, 1190.0, m.lane_top(1) + 40.0), Hit::None);
    }

    #[test]
    fn narrow_clips_have_no_edge_handles() {
        let mut a = item(1, 0.0);
        a.played_ms = 1000.0;
        let l = build_layout(&[a], &[], None);
        let v = View { left_s: 0.0, pps: 10.0, w: 800.0 };
        let c = ctx_for(&l, v, &[]);
        assert_eq!(hit_test(&c, 5.0, c.m.lane_top(0) + 30.0), Hit::Clip(0));
    }

    #[test]
    fn chips_follow_wide_bands_but_stay_inside() {
        let b = Band { index: 0, start_s: 0.0, len_s: 100.0, beats: 32 };
        let v = View { left_s: 50.0, pps: 4.0, w: 400.0 };
        let (cx, w) = chip_x(&b, &v, 12);
        assert!(cx - w / 2.0 >= v.x_of(b.start_s) - 1e-9 && cx + w / 2.0 <= v.x_of(b.end_s()) + 1e-9);
    }

    #[test]
    fn trim_bounds_and_min_span() {
        // dragging the cue out left past the cue in stops at 5 s after it
        let v = trim_value(Edge::Right, 120_000.0, -10_000.0, 2.0, 1.0, 20_000.0, 120_000.0, 300_000.0);
        assert_eq!(v, 25_000.0);
        // dragging past the end of the track
        assert_eq!(trim_value(Edge::Right, 290_000.0, 1_000.0, 2.0, 1.0, 0.0, 290_000.0, 300_000.0), 300_000.0);
        // cue in cannot go below 0 nor past cue_out - span
        assert_eq!(trim_value(Edge::Left, 1_000.0, -500.0, 2.0, 1.0, 1_000.0, 200_000.0, 300_000.0), 0.0);
        assert_eq!(trim_value(Edge::Left, 100_000.0, 2000.0, 2.0, 1.0, 100_000.0, 200_000.0, 300_000.0), 195_000.0);
        // pitched +100 %: the same pixels cover twice the track time
        assert_eq!(trim_value(Edge::Left, 0.0, 10.0, 1.0, 2.0, 0.0, 200_000.0, 300_000.0), 20_000.0);
        assert_eq!(clamp_cue(Edge::Left, 199_000.0, 0.0, 200_000.0, 300_000.0), 195_000.0);
    }

    #[test]
    fn snap_to_beat_or_bar_by_zoom() {
        let g = bc_types::analysis::BeatGrid::constant(120.0, 0.0, 1.0, "t");
        // 120 bpm: beats every 500 ms. 3 px/track-s => 1.5 px per beat => bars (2000 ms)
        assert_eq!(snap_cue_ms(Some(&g), 1_900.0, 3.0, false), 2_000.0);
        // 100 px/s => 50 px per beat
        assert_eq!(snap_cue_ms(Some(&g), 1_900.0, 100.0, false), 2_000.0);
        assert_eq!(snap_cue_ms(Some(&g), 1_700.0, 100.0, false), 1_500.0);
        // bypass / no grid / unusable
        assert_eq!(snap_cue_ms(Some(&g), 1_700.0, 100.0, true), 1_700.0);
        assert_eq!(snap_cue_ms(None, 1_700.0, 100.0, false), 1_700.0);
    }

    #[test]
    fn blend_length_snaps_to_choices() {
        // 128 bpm: 32 beats = 15 s. Dragging 20 px left at 2 px/s (+10 s) from 8 beats (3.75 s) = 13.75 s ~ 32 beats
        let b = blend_beats(3_750.0, -20.0, 2.0, 128.0, 60_000.0);
        assert_eq!(b, 32);
        assert_eq!(blend_beats(3_750.0, 5_000.0, 2.0, 128.0, 60_000.0), 0);
        // capped by the shorter clip
        assert_eq!(blend_beats(0.0, -10_000.0, 1.0, 128.0, 30_000.0), 64);
        assert!(BEAT_CHOICES.contains(&blend_beats(7_000.0, -3.0, 1.0, 128.0, 60_000.0)));
    }

    #[test]
    fn override_patches() {
        let mut it = item(2, 0.0);
        it.transition_beats = Some(16);
        let o = DragOverride { item_id: 2, field: DragField::TransitionBeats, value: 16.0 };
        assert!(override_patch(&o, &it, Some("blend")).is_none(), "unchanged");
        let o = DragOverride { item_id: 2, field: DragField::TransitionBeats, value: 0.0 };
        let p = override_patch(&o, &it, Some("blend")).unwrap();
        assert_eq!(p["transition_type"], "cut");
        assert!(p["transition_beats"].is_null());
        let o = DragOverride { item_id: 2, field: DragField::TransitionBeats, value: 32.0 };
        let p = override_patch(&o, &it, Some("cut")).unwrap();
        assert_eq!((p["transition_beats"].as_i64(), p["transition_type"].as_str()), (Some(32), Some("blend")));
        let p = override_patch(&o, &it, Some("filter")).unwrap();
        assert_eq!(p["transition_type"], "filter");
        let o = DragOverride { item_id: 2, field: DragField::CueOut, value: 200_000.4 };
        assert_eq!(override_patch(&o, &it, None).unwrap()["cue_out_ms"], 200_000);
        let o = DragOverride { item_id: 2, field: DragField::CueOut, value: 300_000.0 };
        assert!(override_patch(&o, &it, None).is_none(), "cue out at the end of the track is the stored default");
        let o = DragOverride { item_id: 2, field: DragField::CueIn, value: 0.2 };
        assert!(override_patch(&o, &it, None).is_none());
    }

    #[test]
    fn kind_patches() {
        assert!(kind_patch("cut", Some(16), true)["transition_beats"].is_null());
        assert_eq!(kind_patch("filter", None, true)["transition_beats"], DEFAULT_BEATS);
        assert!(kind_patch("filter", Some(32), true).get("transition_beats").is_none());
        assert!(kind_patch("filter", None, false).get("transition_beats").is_none());
        assert_eq!(kind_label("bass_swap"), "Bass swap");
        assert_eq!(chip_label("bass_swap", 32), "BASS SWAP 32");
        assert_eq!(chip_label("blend", 0), "CUT");
    }

    #[test]
    fn seconds_parsing() {
        assert_eq!(parse_seconds(""), Some(None));
        assert_eq!(parse_seconds("83.5"), Some(Some(83_500)));
        assert_eq!(parse_seconds("1:23.5"), Some(Some(83_500)));
        assert_eq!(parse_seconds(" 0 "), Some(Some(0)));
        assert_eq!(parse_seconds("-3"), None);
        assert_eq!(parse_seconds("abc"), None);
    }

    #[test]
    fn selection_stepping() {
        assert_eq!(step_selection(None, 3, Dir::Next), Some(0));
        assert_eq!(step_selection(None, 3, Dir::Prev), Some(2));
        assert_eq!(step_selection(Some(2), 3, Dir::Next), Some(2));
        assert_eq!(step_selection(Some(0), 3, Dir::Prev), Some(0));
        assert_eq!(step_selection(None, 0, Dir::Next), None);
    }

    #[test]
    fn reveal_scrolls_minimally() {
        let v = View { left_s: 100.0, pps: 2.0, w: 400.0 };
        assert_eq!(reveal(&v, 150.0, 200.0), None);
        let r = reveal(&v, 400.0, 420.0).unwrap();
        assert!(r > 100.0 && v.span_s() + r >= 420.0);
        let r = reveal(&v, 20.0, 40.0).unwrap();
        assert!(r < 100.0 && r <= 20.0);
    }

    #[test]
    fn clip_windows_and_levels() {
        let (items, ov) = three();
        let l = build_layout(&items, &ov, None);
        let v = View { left_s: 100.0, pps: 4.0, w: 800.0 };
        let w = clip_window(&l.clips[0], &v).unwrap();
        assert_eq!(w.offset_s, 100.0);
        assert_eq!(w.px_per_track_s, 4.0);
        assert_eq!(w.x0, -400.0);
        assert_eq!(w.x1, 800.0);
        assert!(clip_window(&l.clips[2], &v).is_none());
        assert_eq!(visible_clips(&l, 100.0, 300.0), vec![0, 1]);
        assert!(wants_detail(4.0) && !wants_detail(1.0));
    }

    #[test]
    fn playing_item_mapping() {
        let ids = [Some(10), None, Some(20), Some(10)];
        // queue of this set skips the missing slot
        assert_eq!(pick_playing_item(Some(20), 1, true, &ids), Some(2));
        assert_eq!(pick_playing_item(Some(10), 2, true, &ids), Some(3), "a track used twice resolves by queue index");
        // not from this set: first match
        assert_eq!(pick_playing_item(Some(10), 2, false, &ids), Some(0));
        assert_eq!(pick_playing_item(Some(99), 0, true, &ids), None);
        assert_eq!(pick_playing_item(None, 0, true, &ids), None);
    }

    #[test]
    fn minimap_roundtrip() {
        let x = mini_x(300.0, 900.0, 1000.0);
        assert!((mini_t(x, 900.0, 1000.0) - 300.0).abs() < 1e-6);
        assert_eq!(mini_t(-50.0, 900.0, 1000.0), 0.0);
    }

    #[test]
    fn metrics_are_ordered() {
        for (c, t) in [(false, false), (true, true)] {
            let m = Metrics::new(c, t);
            assert!(m.ruler_h < m.lane_top(0) && m.lane_top(0) < m.lane_top(1) && m.lanes_bottom() < m.mini_top());
            assert!(m.gap_mid() > m.lane_top(0) + m.lane_h() && m.gap_mid() < m.lane_top(1));
            assert!(m.total_h() > m.mini_top());
        }
    }
}
