//! The Arrange engine: viewport (continuous zoom, pan with inertia, pinch), gestures (trim, blend
//! length, scrub, minimap), data (waveforms, beat grids) and the per-frame draw of the GL lanes and
//! the 2D overlay. It owns no Leptos signals: it takes events in and queues [`Out`] events that the
//! component executes after releasing the engine borrow.
#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use bc_types::analysis::{BeatGrid, TrackMusicInfo};
use bc_types::sets::DjSetDetail;
use bc_waveform::Waveform;
use bc_waveform::view::{Markers, WaveStyle, WaveTheme};
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d as Ctx, HtmlCanvasElement};

use super::lanes::{DrawClip, Lane};
use super::logic::{self, Dir, Edge, Hit, HitCtx, Layout, Metrics, View};
use super::paint::{self, Colors, Scene, Tip, WaveState};
use crate::logic::timeline::{DragField, DragOverride, GeomItem};
use crate::player::waveform::{WaveLevel, load_music, load_wave, wave_theme};

#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    Select(Option<i64>, bool),
    /// A drag was released with this override (the component PATCHes or clears it).
    Commit(DragOverride),
    Scrub { index: usize, track_s: f64, live: bool },
    Remove(i64),
    Zoom(String),
    Announce(String),
    Load(Load),
    /// Play from the start of this clip's cue window.
    PlayFrom(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    Wave(i64, bool),
    Music(i64),
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// What the player is doing, sampled by the frame loop.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct PlayerSnap {
    pub item: Option<usize>,
    pub track_pos_s: f64,
    pub playing: bool,
}

#[derive(Default)]
struct WaveSlot {
    overview: Option<Rc<Waveform>>,
    detail: Option<Rc<Waveform>>,
    /// 0 idle, 1 in flight, 2 failed
    ov_req: u8,
    det_req: u8,
}

enum Gesture {
    Idle,
    Press { x: f64, y: f64, hit: Hit },
    Pan { last_x: f64, samples: Vec<(f64, f64)> },
    Trim { item_id: i64, index: usize, edge: Edge, x0: f64, orig_ms: f64 },
    Blend { band: usize, item_id: i64, x0: f64, orig_overlap_ms: f64 },
    Scrub { pending: Option<(usize, f64)> },
    Mini,
    Pinch { d0: f64, pps0: f64, t_anchor: f64 },
}

pub struct Engine {
    overlay: Option<HtmlCanvasElement>,
    ctx: Option<Ctx>,
    lanes: [Option<Lane>; 2],
    mini: Option<(HtmlCanvasElement, Ctx)>,
    w: f64,
    dpr: f64,
    pub compact: bool,
    coarse: bool,
    m: Metrics,
    // data
    pub detail: Option<Arc<DjSetDetail>>,
    geom: Vec<GeomItem>,
    overlaps: Vec<f64>,
    layout: Layout,
    ov: Option<DragOverride>,
    chip_labels: Vec<String>,
    chip_chars: Vec<usize>,
    chips_compact: bool,
    // viewport
    view: View,
    target_pps: f64,
    anchor: Option<(f64, f64)>,
    vel: f64,
    scroll_to: Option<f64>,
    fitted: bool,
    pub follow: bool,
    pub snap: bool,
    // interaction
    sel: Option<i64>,
    sel_band: bool,
    hover: Hit,
    hover_ruler_x: Option<f64>,
    gesture: Gesture,
    pointers: Vec<(i32, f64, f64)>,
    tip: Option<Tip>,
    pub focus: bool,
    // caches
    waves: HashMap<i64, WaveSlot>,
    music: HashMap<i64, Option<Rc<TrackMusicInfo>>>,
    music_req: HashMap<i64, u8>,
    markers: HashMap<i64, Rc<Markers>>,
    // theme
    colors: Colors,
    wtheme: WaveTheme,
    theme_rev: u64,
    pub style: WaveStyle,
    pub normalise: bool,
    bg: [f32; 4],
    // frame state
    dirty_gl: bool,
    dirty_ov: bool,
    mini_dirty: bool,
    last_ms: f64,
    last_drawn_view: (f64, f64, f64),
    player: PlayerSnap,
    wave_state: Vec<WaveState>,
    in_flight: usize,
    last_zoom_label: String,
    last_cursor: &'static str,
    pub outbox: Vec<Out>,
    // stats
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    frames: u32,
    since_ms: f64,
    work_ms: f64,
    work_max: f64,
    drawn: usize,
    gl_draws: u32,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        let m = Metrics::new(false, false);
        Engine {
            overlay: None,
            ctx: None,
            lanes: [None, None],
            mini: None,
            w: 0.0,
            dpr: 1.0,
            compact: false,
            coarse: false,
            m,
            detail: None,
            geom: vec![],
            overlaps: vec![],
            layout: Layout::default(),
            ov: None,
            chip_labels: vec![],
            chip_chars: vec![],
            chips_compact: false,
            view: View { left_s: 0.0, pps: 1.0, w: 800.0 },
            target_pps: 1.0,
            anchor: None,
            vel: 0.0,
            scroll_to: None,
            fitted: false,
            follow: true,
            snap: true,
            sel: None,
            sel_band: false,
            hover: Hit::None,
            hover_ruler_x: None,
            gesture: Gesture::Idle,
            pointers: vec![],
            tip: None,
            focus: false,
            waves: HashMap::new(),
            music: HashMap::new(),
            music_req: HashMap::new(),
            markers: HashMap::new(),
            colors: Colors::default(),
            wtheme: WaveTheme::default(),
            theme_rev: 0,
            style: WaveStyle::RgbSpectral,
            normalise: false,
            bg: [0.08, 0.08, 0.08, 1.0],
            dirty_gl: true,
            dirty_ov: true,
            mini_dirty: true,
            last_ms: 0.0,
            last_drawn_view: (f64::NAN, f64::NAN, f64::NAN),
            player: PlayerSnap::default(),
            wave_state: vec![],
            in_flight: 0,
            last_zoom_label: String::new(),
            last_cursor: "default",
            outbox: vec![],
            stats: Stats::default(),
        }
    }

    // ---- setup ---------------------------------------------------------------------------------

    pub fn attach(&mut self, lane0: HtmlCanvasElement, lane1: HtmlCanvasElement, overlay: HtmlCanvasElement) {
        self.ctx = overlay.get_context("2d").ok().flatten().and_then(|o| o.dyn_into::<Ctx>().ok());
        self.overlay = Some(overlay);
        self.lanes = [Some(Lane::new(lane0)), Some(Lane::new(lane1))];
        self.apply_size();
    }

    /// Host size in css px. Phones (<= 640 px wide) get the compact lane metrics.
    pub fn resize(&mut self, w: f64, dpr: f64, coarse: bool) {
        let compact = w <= 640.0;
        if (w - self.w).abs() < 0.5 && dpr == self.dpr && compact == self.compact && coarse == self.coarse {
            return;
        }
        let keep_center = self.view.left_s + self.view.span_s() * 0.5;
        self.w = w.max(100.0);
        self.dpr = dpr.max(1.0);
        self.compact = compact;
        self.coarse = coarse;
        self.m = Metrics::new(compact, coarse);
        self.view.w = self.w;
        if self.fitted {
            // keep the centre and the zoom, but never zoom out beyond what fits
            self.view.pps = logic::clamp_pps(self.view.pps, self.layout.total_s, self.w);
            self.target_pps = self.view.pps;
            self.view.left_s = keep_center - self.view.span_s() * 0.5;
        }
        self.apply_size();
        self.try_fit();
        self.dirty_all();
    }

    pub fn height(&self) -> f64 {
        self.m.total_h()
    }

    fn apply_size(&mut self) {
        let (w, m, dpr) = (self.w, self.m, self.dpr);
        if w <= 0.0 {
            return;
        }
        if let Some(o) = &self.overlay {
            o.set_width((w * dpr).round() as u32);
            o.set_height((m.total_h() * dpr).round() as u32);
            let st = o.style();
            let _ = st.set_property("width", &format!("{w}px"));
            let _ = st.set_property("height", &format!("{}px", m.total_h()));
        }
        for (i, l) in self.lanes.iter_mut().enumerate() {
            if let Some(l) = l {
                let st = l.canvas().style();
                let _ = st.set_property("width", &format!("{w}px"));
                let _ = st.set_property("height", &format!("{}px", m.wave_h));
                let _ = st.set_property("top", &format!("{}px", m.wave_top(i as u8)));
                l.resize(w, m.wave_h, dpr);
            }
        }
        self.mini = None;
        self.mini_dirty = true;
    }

    pub fn set_colors(&mut self, map: bc_types::theme::ColorMap) {
        self.wtheme = wave_theme(&map);
        self.wtheme.playhead[3] = 0.0;
        self.wtheme.beat[3] = 0.3;
        self.wtheme.bar[3] = 0.6;
        self.colors = Colors::new(map);
        let [r, g, b] = bc_types::theme::hex_to_rgb(&self.colors.hex("surface-2"));
        self.bg = [(r / 255.0) as f32, (g / 255.0) as f32, (b / 255.0) as f32, 1.0];
        // clips keep an opaque body (the player's waveforms are transparent)
        self.wtheme.background = self.bg;
        self.wtheme.background = self.bg;
        self.theme_rev += 1;
        self.mini_dirty = true;
        self.dirty_all();
    }

    pub fn set_style(&mut self, s: WaveStyle) {
        self.style = s;
        self.dirty_all();
    }

    pub fn set_normalise(&mut self, n: bool) {
        self.normalise = n;
        self.dirty_all();
    }

    pub fn set_focus(&mut self, f: bool) {
        self.focus = f;
        self.dirty_ov = true;
    }

    fn dirty_all(&mut self) {
        self.dirty_gl = true;
        self.dirty_ov = true;
    }

    // ---- data ----------------------------------------------------------------------------------

    pub fn set_detail(&mut self, d: Option<Arc<DjSetDetail>>) {
        self.ov = None;
        self.detail = d;
        self.geom = self
            .detail
            .as_ref()
            .map(|d| {
                d.items
                    .iter()
                    .map(|it| GeomItem {
                        id: it.id,
                        start_ms: it.start_ms as f64,
                        played_ms: it.played_ms as f64,
                        duration_ms: it.duration_ms.map(|v| v as f64),
                        cue_in_ms: it.cue_in_ms.map(|v| v as f64),
                        cue_out_ms: it.cue_out_ms.map(|v| v as f64),
                        tempo_adjust_pct: it.tempo_adjust_pct,
                        transition_beats: it.transition_beats.map(|b| b.max(0) as u32),
                        effective_bpm: it.effective_bpm,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let n = self.geom.len();
        self.overlaps = (0..n.saturating_sub(1))
            .map(|i| self.detail.as_ref().and_then(|d| d.transitions.get(i)).map(|t| t.overlap_ms as f64).unwrap_or(0.0))
            .collect();
        if self.sel.is_some_and(|id| !self.geom.iter().any(|g| g.id == id)) {
            self.sel = None;
            self.sel_band = false;
            self.outbox.push(Out::Select(None, false));
        }
        self.relayout();
        self.try_fit();
    }

    pub fn geom_item(&self, id: i64) -> Option<&GeomItem> {
        self.geom.iter().find(|g| g.id == id)
    }

    pub fn transition_type_of(&self, id: i64) -> Option<String> {
        self.detail.as_ref()?.items.iter().find(|i| i.id == id)?.transition_type.clone()
    }

    pub fn clear_override(&mut self) {
        if self.ov.take().is_some() {
            self.relayout();
        }
    }

    fn relayout(&mut self) {
        self.layout = logic::build_layout(&self.geom, &self.overlaps, self.ov);
        self.chip_labels = self
            .layout
            .bands
            .iter()
            .map(|b| {
                let kind = self
                    .detail
                    .as_ref()
                    .and_then(|d| d.items.get(b.index + 1))
                    .and_then(|i| i.transition_type.clone())
                    .unwrap_or_else(|| "blend".into());
                logic::chip_label(&kind, b.beats)
            })
            .collect();
        self.refresh_chip_chars();
        self.mini_dirty = true;
        self.dirty_all();
    }

    fn refresh_chip_chars(&mut self) {
        self.chip_chars = self.chip_labels.iter().map(|l| if self.chips_compact { 0 } else { l.chars().count() }).collect();
    }

    fn try_fit(&mut self) {
        if self.fitted || self.w <= 0.0 || self.layout.total_s <= 0.0 {
            return;
        }
        self.fitted = true;
        self.view.w = self.w;
        self.view.pps = logic::fit_pps(self.layout.total_s, self.w);
        self.target_pps = self.view.pps;
        self.view.left_s = -logic::EDGE_PAD_PX / self.view.pps;
        self.dirty_all();
    }

    pub fn on_wave(&mut self, id: i64, detail: bool, w: Option<Rc<Waveform>>) {
        let slot = self.waves.entry(id).or_default();
        let (req, store) = if detail { (&mut slot.det_req, &mut slot.detail) } else { (&mut slot.ov_req, &mut slot.overview) };
        match w {
            Some(w) => {
                *store = Some(w);
                *req = 0;
            }
            None => *req = 2,
        }
        self.in_flight = self.in_flight.saturating_sub(1);
        self.mini_dirty = true;
        self.dirty_all();
    }

    pub fn on_music(&mut self, id: i64, info: Option<Rc<TrackMusicInfo>>) {
        if let Some(i) = &info {
            let mut m = Markers::default();
            if let Some(g) = &i.grid {
                let mut g = g.clone();
                // the grid anchors beats; without a detected downbeat assume the first beat is "the one"
                // so bars and phrases are drawn (a heuristic, only used for display)
                if g.downbeat_phase.is_none() {
                    g.downbeat_phase = Some(0);
                }
                m.grid = Some(g);
            }
            m.cues = i.cues.clone();
            m.mix_in_s = i.mix_points.map(|p| p.cue_in_ms as f64 / 1000.0);
            m.mix_out_s = i.mix_points.and_then(|p| p.cue_out_ms).map(|v| v as f64 / 1000.0);
            m.chapter_ticks = false;
            self.markers.insert(id, Rc::new(m));
        }
        self.music.insert(id, info);
        self.dirty_all();
    }

    fn grid_of(&self, track: Option<i64>) -> Option<BeatGrid> {
        let t = track?;
        self.music.get(&t)?.as_ref()?.grid.clone()
    }

    // ---- selection -------------------------------------------------------------------------------

    pub fn selected(&self) -> Option<i64> {
        self.sel
    }

    pub fn select(&mut self, id: Option<i64>, band: bool, reveal: bool) {
        if self.sel == id && self.sel_band == band {
            return;
        }
        self.sel = id;
        self.sel_band = band && id.is_some();
        self.outbox.push(Out::Select(self.sel, self.sel_band));
        if let (Some(id), Some(d)) = (id, &self.detail)
            && let Some(it) = d.items.iter().find(|i| i.id == id)
        {
            self.outbox.push(Out::Announce(format!("Selected. {}", paint::describe(it))));
            if reveal && let Some(c) = self.layout.clips.iter().find(|c| c.id == id) {
                let (a, b) = if self.sel_band && c.index > 0 {
                    let bd = &self.layout.bands[c.index - 1];
                    (bd.start_s - 2.0, bd.end_s() + 2.0)
                } else {
                    (c.start_s, c.end_s())
                };
                self.reveal_range(a, b);
            }
        }
        self.dirty_ov = true;
    }

    fn reveal_range(&mut self, a: f64, b: f64) {
        if let Some(l) = logic::reveal(&self.view, a, b) {
            self.scroll_to = Some(logic::clamp_left(l, self.view.pps, self.w, self.layout.total_s));
        }
    }

    // ---- viewport ----------------------------------------------------------------------------------

    pub fn zoom_by(&mut self, factor: f64, anchor_x: f64) {
        let pps = logic::clamp_pps(self.target_pps * factor, self.layout.total_s, self.w);
        self.target_pps = pps;
        self.anchor = Some((self.view.t_at(anchor_x), anchor_x));
        self.vel = 0.0;
        self.scroll_to = None;
    }

    pub fn zoom_center(&mut self, factor: f64) {
        self.zoom_by(factor, self.w * 0.5);
    }

    pub fn fit(&mut self) {
        let pps = logic::min_pps(self.layout.total_s, self.w);
        self.target_pps = pps;
        self.anchor = Some((self.layout.total_s * 0.5, self.w * 0.5));
        self.vel = 0.0;
        self.scroll_to = None;
    }

    pub fn zoom_selection(&mut self) {
        if let Some(c) = self.sel.and_then(|id| self.layout.clips.iter().find(|c| c.id == id)) {
            let pps = logic::clamp_pps((self.w - 2.0 * logic::EDGE_PAD_PX) / c.len_s.max(1.0), self.layout.total_s, self.w);
            self.target_pps = pps;
            self.anchor = Some(((c.start_s + c.end_s()) * 0.5, self.w * 0.5));
            self.scroll_to = None;
        }
    }

    pub fn scroll_by_px(&mut self, dx: f64) {
        self.view.left_s = logic::clamp_left(self.view.left_s + dx / self.view.pps, self.view.pps, self.w, self.layout.total_s);
        self.scroll_to = None;
        self.vel = 0.0;
    }

    pub fn scroll_to_start(&mut self) {
        self.scroll_to = Some(-logic::EDGE_PAD_PX / self.view.pps);
    }

    pub fn scroll_to_end(&mut self) {
        self.scroll_to = Some(logic::clamp_left(f64::MAX / 4.0, self.view.pps, self.w, self.layout.total_s));
    }

    pub fn pps(&self) -> f64 {
        self.view.pps
    }

    pub fn zoom_label(&self) -> String {
        let p = self.view.pps;
        if p >= 10.0 { format!("{p:.0} px/s") } else if p >= 1.0 { format!("{p:.1} px/s") } else { format!("{p:.2} px/s") }
    }

    // ---- input -------------------------------------------------------------------------------------

    fn hit_at(&self, x: f64, y: f64) -> Hit {
        logic::hit_test(&HitCtx { layout: &self.layout, view: self.view, m: self.m, chip_chars: &self.chip_chars, canvas_h: self.m.total_h() }, x, y)
    }

    fn set_cursor(&mut self, c: &'static str) {
        if c != self.last_cursor {
            self.last_cursor = c;
            if let Some(o) = &self.overlay {
                let _ = o.style().set_property("cursor", c);
            }
        }
    }

    fn cursor_for(hit: Hit, has_bpm: bool) -> &'static str {
        match hit {
            Hit::Ruler | Hit::Minimap => "pointer",
            Hit::ClipEdge(..) | Hit::BandEdge(..) => "ew-resize",
            Hit::Band(_) => {
                if has_bpm {
                    "ew-resize"
                } else {
                    "pointer"
                }
            }
            Hit::Clip(_) => "grab",
            Hit::None => "default",
        }
    }

    pub fn pointer_down(&mut self, id: i32, x: f64, y: f64, _mods: Mods) {
        self.vel = 0.0;
        self.scroll_to = None;
        self.pointers.retain(|p| p.0 != id);
        self.pointers.push((id, x, y));
        if self.pointers.len() >= 2 {
            self.cancel_edit();
            let (a, b) = (self.pointers[0], self.pointers[1]);
            let d0 = ((a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt().max(8.0);
            let mid = (a.1 + b.1) * 0.5;
            self.target_pps = self.view.pps;
            self.anchor = None;
            self.gesture = Gesture::Pinch { d0, pps0: self.view.pps, t_anchor: self.view.t_at(mid) };
            self.set_cursor("grabbing");
            return;
        }
        let hit = self.hit_at(x, y);
        match hit {
            Hit::Ruler => {
                let t = self.view.t_at(x);
                if let Some((index, track_s)) = logic::scrub_target(&self.layout, t) {
                    self.outbox.push(Out::Scrub { index, track_s, live: false });
                }
                self.gesture = Gesture::Scrub { pending: None };
            }
            Hit::Minimap => {
                self.jump_minimap(x);
                self.gesture = Gesture::Mini;
            }
            _ => self.gesture = Gesture::Press { x, y, hit },
        }
    }

    fn jump_minimap(&mut self, x: f64) {
        let t = logic::mini_t(x, self.layout.total_s, self.w);
        let left = t - self.view.span_s() * 0.5;
        self.view.left_s = logic::clamp_left(left, self.view.pps, self.w, self.layout.total_s);
        self.dirty_all();
    }

    fn cancel_edit(&mut self) {
        if matches!(self.gesture, Gesture::Trim { .. } | Gesture::Blend { .. }) {
            self.clear_override();
            self.tip = None;
        }
        self.gesture = Gesture::Idle;
    }

    pub fn pointer_move(&mut self, id: i32, x: f64, y: f64, mods: Mods, now: f64) {
        if let Some(p) = self.pointers.iter_mut().find(|p| p.0 == id) {
            p.1 = x;
            p.2 = y;
        }
        // pinch
        if let Gesture::Pinch { d0, pps0, t_anchor } = self.gesture
            && self.pointers.len() >= 2
        {
            let (a, b) = (self.pointers[0], self.pointers[1]);
            let d = ((a.1 - b.1).powi(2) + (a.2 - b.2).powi(2)).sqrt().max(8.0);
            let mid = (a.1 + b.1) * 0.5;
            let pps = logic::clamp_pps(pps0 * d / d0, self.layout.total_s, self.w);
            self.view.pps = pps;
            self.target_pps = pps;
            self.view.left_s = logic::clamp_left(t_anchor - mid / pps, pps, self.w, self.layout.total_s);
            self.dirty_all();
            return;
        }
        match &mut self.gesture {
            Gesture::Idle => {
                let hit = self.hit_at(x, y);
                let has_bpm = matches!(hit, Hit::Band(i) if self.geom.get(i + 1).is_some_and(|g| g.effective_bpm.is_some()));
                self.set_cursor(Self::cursor_for(hit, has_bpm));
                let hx = matches!(hit, Hit::Ruler).then_some(x);
                if hit != self.hover || hx != self.hover_ruler_x {
                    self.hover = hit;
                    self.hover_ruler_x = hx;
                    self.dirty_ov = true;
                }
            }
            Gesture::Press { x: x0, y: y0, hit } => {
                let (dx, dy) = (x - *x0, y - *y0);
                if dx.hypot(dy) < logic::DRAG_THRESHOLD_PX {
                    return;
                }
                let (sx, hit) = (*x0, *hit);
                self.begin_drag(hit, sx, x, y, now);
            }
            Gesture::Pan { last_x, samples } => {
                let dx = x - *last_x;
                *last_x = x;
                samples.push((now, x));
                if samples.len() > 12 {
                    samples.remove(0);
                }
                let pps = self.view.pps;
                self.view.left_s = logic::clamp_left(self.view.left_s - dx / pps, pps, self.w, self.layout.total_s);
                self.dirty_all();
            }
            Gesture::Trim { item_id, index, edge, x0, orig_ms } => {
                let (item_id, index, edge, x0, orig_ms) = (*item_id, *index, *edge, *x0, *orig_ms);
                self.update_trim(item_id, index, edge, x0, orig_ms, x, y, mods);
            }
            Gesture::Blend { band, item_id, x0, orig_overlap_ms } => {
                let (band, item_id, x0, orig) = (*band, *item_id, *x0, *orig_overlap_ms);
                self.update_blend(band, item_id, x0, orig, x, y);
            }
            Gesture::Scrub { pending } => {
                let t = self.view.t_at(x);
                if let Some((index, track_s)) = logic::scrub_target(&self.layout, t) {
                    if self.player.item == Some(index) {
                        *pending = None;
                        self.outbox.push(Out::Scrub { index, track_s, live: true });
                    } else {
                        *pending = Some((index, track_s));
                    }
                }
                self.hover_ruler_x = Some(x);
                self.dirty_ov = true;
            }
            Gesture::Mini => self.jump_minimap(x),
            Gesture::Pinch { .. } => {}
        }
    }

    fn begin_drag(&mut self, hit: Hit, x_start: f64, x: f64, y: f64, now: f64) {
        match hit {
            Hit::ClipEdge(i, edge) => {
                let (Some(c), Some(g)) = (self.layout.clips.get(i), self.geom.get(i)) else { return };
                let id = g.id;
                let orig = match edge {
                    Edge::Left => c.cue_in_s * 1000.0,
                    Edge::Right => c.cue_out_s * 1000.0,
                };
                self.select(Some(id), false, false);
                self.gesture = Gesture::Trim { item_id: id, index: i, edge, x0: x_start, orig_ms: orig };
                self.update_trim(id, i, edge, x_start, orig, x, y, Mods::default());
            }
            Hit::BandEdge(i, Edge::Right) => {
                let (Some(c), Some(g)) = (self.layout.clips.get(i), self.geom.get(i)) else { return };
                let (id, orig) = (g.id, c.cue_out_s * 1000.0);
                let inc_id = self.geom.get(i + 1).map(|g| g.id).unwrap_or(id);
                self.select(Some(inc_id), true, false);
                self.gesture = Gesture::Trim { item_id: id, index: i, edge: Edge::Right, x0: x_start, orig_ms: orig };
                self.update_trim(id, i, Edge::Right, x_start, orig, x, y, Mods::default());
            }
            Hit::Band(i) | Hit::BandEdge(i, Edge::Left) => {
                let (Some(b), Some(inc)) = (self.layout.bands.get(i), self.geom.get(i + 1)) else { return };
                let (id, orig, has_bpm) = (inc.id, b.len_s * 1000.0, inc.effective_bpm.is_some());
                self.select(Some(id), true, false);
                if !has_bpm {
                    // cannot size in beats without a tempo: behave like a pan
                    self.gesture = Gesture::Pan { last_x: x, samples: vec![(now, x)] };
                    return;
                }
                self.gesture = Gesture::Blend { band: i, item_id: id, x0: x_start, orig_overlap_ms: orig };
                self.update_blend(i, id, x_start, orig, x, y);
            }
            _ => {
                self.gesture = Gesture::Pan { last_x: x, samples: vec![(now, x)] };
                self.set_cursor("grabbing");
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn update_trim(&mut self, item_id: i64, index: usize, edge: Edge, x0: f64, orig_ms: f64, x: f64, y: f64, mods: Mods) {
        let Some(g) = self.geom.get(index).cloned() else { return };
        let mult = crate::logic::timeline::tempo_multiplier(&g);
        let dur = g.duration_ms.unwrap_or(0.0);
        let (cue_in, cue_out) = (g.cue_in_ms.unwrap_or(0.0), g.cue_out_ms.or(g.duration_ms).unwrap_or(0.0));
        let pps = self.view.pps;
        let raw = logic::trim_value(edge, orig_ms, x - x0, pps, mult, cue_in, cue_out, dur);
        let grid = self.grid_of(self.detail.as_ref().and_then(|d| d.items.get(index)).and_then(|i| i.track_id));
        let snapped = logic::snap_cue_ms(grid.as_ref(), raw, pps / mult, mods.alt || !self.snap);
        let value = logic::clamp_cue(edge, snapped, cue_in, cue_out, dur);
        let field = if edge == Edge::Left { DragField::CueIn } else { DragField::CueOut };
        let ov = DragOverride { item_id, field, value };
        if self.ov != Some(ov) {
            self.ov = Some(ov);
            self.relayout();
        }
        let label = if edge == Edge::Left { "cue in" } else { "cue out" };
        let delta = (value - orig_ms) / 1000.0;
        self.tip = Some(Tip { text: format!("{label} {}  ({:+.1}s)", logic::fmt_clock_tenths(value / 1000.0), delta), x, y });
        self.dirty_ov = true;
    }

    fn update_blend(&mut self, band: usize, item_id: i64, x0: f64, orig_overlap_ms: f64, x: f64, y: f64) {
        let Some(inc) = self.geom.get(band + 1).cloned() else { return };
        let Some(bpm) = inc.effective_bpm else { return };
        let cap = self.layout.clips.get(band).map(|c| c.len_s).unwrap_or(0.0).min(self.layout.clips.get(band + 1).map(|c| c.len_s).unwrap_or(0.0)) * 1000.0;
        let beats = logic::blend_beats(orig_overlap_ms, x - x0, self.view.pps, bpm, cap);
        let ov = DragOverride { item_id, field: DragField::TransitionBeats, value: beats as f64 };
        if self.ov != Some(ov) {
            self.ov = Some(ov);
            self.relayout();
        }
        let secs = crate::logic::timeline::overlap_ms(Some(beats), Some(bpm)) / 1000.0;
        let text = if beats == 0 { "cut".to_string() } else { format!("{beats} beats \u{b7} {} bars \u{b7} {secs:.1}s", beats / 4) };
        self.tip = Some(Tip { text, x, y });
        self.dirty_ov = true;
    }

    pub fn pointer_up(&mut self, id: i32, x: f64, y: f64, now: f64) {
        let had = self.pointers.len();
        self.pointers.retain(|p| p.0 != id);
        if matches!(self.gesture, Gesture::Pinch { .. }) {
            if self.pointers.len() < 2 {
                self.gesture = Gesture::Idle;
                self.set_cursor("default");
            }
            return;
        }
        let g = std::mem::replace(&mut self.gesture, Gesture::Idle);
        match g {
            Gesture::Press { hit, .. } => match hit {
                Hit::Clip(i) | Hit::ClipEdge(i, _) => {
                    if let Some(c) = self.layout.clips.get(i) {
                        let id = c.id;
                        self.select(Some(id), false, false);
                    }
                }
                Hit::Band(i) | Hit::BandEdge(i, _) => {
                    if let Some(g) = self.geom.get(i + 1) {
                        let id = g.id;
                        self.select(Some(id), true, false);
                    }
                }
                _ => self.select(None, false, false),
            },
            Gesture::Pan { samples, .. } => {
                self.vel = logic::release_velocity(&samples);
                if now - samples.last().map(|s| s.0).unwrap_or(now) > 90.0 {
                    self.vel = 0.0;
                }
            }
            Gesture::Trim { .. } | Gesture::Blend { .. } => {
                self.tip = None;
                if let Some(ov) = self.ov {
                    self.outbox.push(Out::Commit(ov));
                }
                self.dirty_ov = true;
            }
            Gesture::Scrub { pending } => {
                if let Some((index, track_s)) = pending {
                    self.outbox.push(Out::Scrub { index, track_s, live: false });
                }
            }
            Gesture::Mini | Gesture::Idle | Gesture::Pinch { .. } => {}
        }
        let _ = (had, y);
        let hit = self.hit_at(x, y);
        self.hover = hit;
        self.set_cursor(Self::cursor_for(hit, false));
        self.dirty_ov = true;
    }

    pub fn pointer_cancel(&mut self, id: i32) {
        self.pointers.retain(|p| p.0 != id);
        self.cancel_edit();
        self.set_cursor("default");
        self.dirty_ov = true;
    }

    pub fn pointer_leave(&mut self) {
        if matches!(self.gesture, Gesture::Idle) && (self.hover != Hit::None || self.hover_ruler_x.is_some()) {
            self.hover = Hit::None;
            self.hover_ruler_x = None;
            self.dirty_ov = true;
        }
    }

    pub fn wheel(&mut self, x: f64, dx: f64, dy: f64, mode: u32, mods: Mods) {
        if mods.shift || dx.abs() > dy.abs() * 1.2 {
            let d = if dx.abs() > dy.abs() { dx } else { dy };
            let mult = if mode == 1 { 16.0 } else { 1.0 };
            self.scroll_by_px(d * mult);
            self.dirty_all();
            return;
        }
        let mut f = logic::wheel_zoom_factor(dy, mode);
        if mods.ctrl {
            // trackpad pinch arrives as ctrl+wheel with small deltas
            f = f.powf(2.2);
        }
        self.zoom_by(f, x);
    }

    /// Returns true when the key was handled (the caller prevents default).
    pub fn key(&mut self, key: &str, mods: Mods) -> bool {
        let n = self.layout.clips.len();
        let cur = self.sel.and_then(|id| self.layout.clips.iter().position(|c| c.id == id));
        let step = |e: &mut Self, dir: Dir| {
            if let Some(i) = logic::step_selection(cur, n, dir)
                && let Some(id) = e.layout.clips.get(i).map(|c| c.id)
            {
                e.select(Some(id), false, true);
            }
        };
        match key {
            "ArrowRight" | "ArrowDown" if !mods.shift => step(self, Dir::Next),
            "ArrowLeft" | "ArrowUp" if !mods.shift => step(self, Dir::Prev),
            "ArrowRight" | "ArrowDown" => self.scroll_by_px(self.w * 0.25),
            "ArrowLeft" | "ArrowUp" => self.scroll_by_px(-self.w * 0.25),
            "+" | "=" => self.zoom_center(1.5),
            "-" | "_" => self.zoom_center(1.0 / 1.5),
            "0" => self.fit(),
            "f" | "F" => self.zoom_selection(),
            "Home" => self.scroll_to_start(),
            "End" => self.scroll_to_end(),
            "Escape" => {
                if self.sel.is_some() {
                    self.select(None, false, false);
                } else {
                    return false;
                }
            }
            "Delete" | "Backspace" => {
                if let Some(id) = self.sel {
                    self.outbox.push(Out::Remove(id));
                } else {
                    return false;
                }
            }
            "Enter" => {
                if let Some(i) = cur {
                    self.outbox.push(Out::PlayFrom(i));
                } else {
                    return false;
                }
            }
            "b" | "B" => {
                // toggle between the clip and its incoming transition
                if let Some(id) = self.sel {
                    let band = !self.sel_band;
                    let idx = cur.unwrap_or(0);
                    if band && idx == 0 {
                        return true;
                    }
                    self.sel = None;
                    self.select(Some(id), band, true);
                }
            }
            _ => return false,
        }
        self.dirty_all();
        true
    }

    // ---- frame ---------------------------------------------------------------------------------------

    pub fn set_player(&mut self, p: PlayerSnap) {
        if p != self.player {
            if p.item != self.player.item || p.playing != self.player.playing || p.track_pos_s != self.player.track_pos_s {
                self.dirty_ov = true;
            }
            self.player = p;
        }
    }

    /// Whether the overlay needs to repaint continuously (playing or skeleton shimmer).
    fn animating(&self) -> bool {
        self.player.playing || self.wave_state.contains(&WaveState::Loading)
    }

    fn playing_t(&self) -> Option<(usize, f64)> {
        let i = self.player.item?;
        let c = self.layout.clips.get(i)?;
        Some((i, logic::playhead_t(c, self.player.track_pos_s)))
    }

    pub fn frame(&mut self, now: f64) {
        let t0 = crate::util::perf_now();
        let dt = if self.last_ms == 0.0 { 1.0 / 60.0 } else { ((now - self.last_ms) / 1000.0).clamp(0.001, 0.25) };
        self.last_ms = now;
        if self.w <= 0.0 || self.ctx.is_none() {
            return;
        }
        self.try_fit();

        // inertia
        if self.vel != 0.0 && !matches!(self.gesture, Gesture::Pan { .. } | Gesture::Pinch { .. }) {
            let (dx, nv) = logic::inertia_step(self.vel, dt);
            let before = self.view.left_s;
            self.view.left_s = logic::clamp_left(before - dx / self.view.pps, self.view.pps, self.w, self.layout.total_s);
            self.vel = if (self.view.left_s - (before - dx / self.view.pps)).abs() > 1e-9 { 0.0 } else { nv };
        }
        // smooth zoom around the anchor
        if (self.target_pps - self.view.pps).abs() > 1e-12 && !matches!(self.gesture, Gesture::Pinch { .. }) {
            let new = logic::smooth_toward(self.view.pps, self.target_pps, dt, 0.075);
            if let Some((t, x)) = self.anchor {
                self.view.left_s = t - x / new;
            }
            self.view.pps = new;
        }
        // follow the playhead
        if self.follow && self.player.playing && self.scroll_to.is_none() && self.vel == 0.0 && matches!(self.gesture, Gesture::Idle) {
            if let Some((_, t)) = self.playing_t() {
                let x = self.view.x_of(t);
                if x > self.w * 0.85 || x < 0.04 * self.w {
                    self.scroll_to = Some(t - self.view.span_s() * 0.3);
                }
            }
        }
        if let Some(target) = self.scroll_to {
            let target = logic::clamp_left(target, self.view.pps, self.w, self.layout.total_s);
            let v = logic::smooth_toward(self.view.left_s, target, dt, 0.11);
            if (v - target).abs() * self.view.pps < 0.3 {
                self.view.left_s = target;
                self.scroll_to = None;
            } else {
                self.view.left_s = v;
            }
        }
        self.view.w = self.w;
        self.view.left_s = logic::clamp_left(self.view.left_s, self.view.pps, self.w, self.layout.total_s);

        let crowded = self.view.pps < if self.compact { 0.6 } else { 0.35 };
        if crowded != self.chips_compact {
            self.chips_compact = crowded;
            self.refresh_chip_chars();
            self.dirty_ov = true;
        }
        let vkey = (self.view.left_s, self.view.pps, self.w);
        if vkey != self.last_drawn_view {
            self.last_drawn_view = vkey;
            self.dirty_gl = true;
            self.dirty_ov = true;
        }

        self.plan_loads();
        if self.dirty_gl {
            self.draw_lanes();
            self.dirty_gl = false;
            self.stats.gl_draws += 1;
        }
        if self.dirty_ov || self.animating() {
            self.draw_overlay(now);
            self.dirty_ov = false;
        }

        let label = self.zoom_label();
        if label != self.last_zoom_label {
            self.last_zoom_label = label.clone();
            self.outbox.push(Out::Zoom(label));
        }
        self.record_stats(now, crate::util::perf_now() - t0);
    }

    fn record_stats(&mut self, now: f64, work: f64) {
        let s = &mut self.stats;
        if s.since_ms == 0.0 {
            s.since_ms = now;
        }
        s.frames += 1;
        s.work_ms += work;
        s.work_max = s.work_max.max(work);
        if now - s.since_ms >= 1000.0 {
            let secs = (now - s.since_ms) / 1000.0;
            let o = js_sys::Object::new();
            let set = |k: &str, v: f64| {
                let _ = js_sys::Reflect::set(&o, &k.into(), &v.into());
            };
            set("fps", s.frames as f64 / secs);
            set("work_ms_avg", s.work_ms / s.frames.max(1) as f64);
            set("work_ms_max", s.work_max);
            set("clips_drawn", s.drawn as f64);
            set("gl_draws_per_s", s.gl_draws as f64 / secs);
            set("pps", self.view.pps);
            let _ = js_sys::Reflect::set(&crate::util::window(), &"__arrange".into(), &o);
            *s = Stats { since_ms: now, drawn: s.drawn, ..Default::default() };
        }
    }

    fn visible_range(&self, margin_views: f64) -> (f64, f64) {
        let span = self.view.span_s();
        (self.view.left_s - span * margin_views, self.view.right_s() + span * margin_views)
    }

    fn plan_loads(&mut self) {
        let Some(d) = self.detail.clone() else { return };
        let (a, b) = self.visible_range(0.6);
        let (va, vb) = self.visible_range(0.0);
        let mut loads = vec![];
        for i in logic::visible_clips(&self.layout, a, b) {
            let Some(item) = d.items.get(i) else { continue };
            let (Some(track), false) = (item.track_id, item.missing) else { continue };
            let c = &self.layout.clips[i];
            let slot = self.waves.entry(track).or_default();
            if slot.overview.is_none() && slot.ov_req == 0 {
                slot.ov_req = 1;
                loads.push(Load::Wave(track, false));
            }
            let truly_visible = c.end_s() >= va && c.start_s <= vb;
            if truly_visible && logic::wants_detail(self.view.pps / c.mult) && slot.detail.is_none() && slot.det_req == 0 && self.in_flight < 3 {
                slot.det_req = 1;
                loads.push(Load::Wave(track, true));
            }
            if !self.music.contains_key(&track) && !self.music_req.contains_key(&track) {
                self.music_req.insert(track, 1);
                loads.push(Load::Music(track));
            }
        }
        // drop detail waveforms that are far away (memory)
        let (fa, fb) = self.visible_range(2.5);
        let far: Vec<i64> = d
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let c = self.layout.clips.get(i)?;
                (c.end_s() < fa || c.start_s > fb).then_some(it.track_id).flatten()
            })
            .collect();
        for t in far {
            if let Some(s) = self.waves.get_mut(&t)
                && s.detail.is_some()
                && s.det_req == 0
            {
                s.detail = None;
            }
        }
        for l in loads {
            if matches!(l, Load::Wave(_, _)) {
                self.in_flight += 1;
            }
            self.outbox.push(Out::Load(l));
        }
    }

    fn wave_for(&self, track: i64, px_per_track_s: f64) -> Option<(Rc<Waveform>, bool)> {
        let s = self.waves.get(&track)?;
        if logic::wants_detail(px_per_track_s) {
            if let Some(d) = &s.detail {
                return Some((d.clone(), true));
            }
        }
        if let Some(o) = &s.overview {
            return Some((o.clone(), false));
        }
        s.detail.as_ref().map(|d| (d.clone(), true))
    }

    fn draw_lanes(&mut self) {
        let Some(d) = self.detail.clone() else {
            for l in self.lanes.iter_mut().flatten() {
                l.draw(&[], &self.wtheme, self.theme_rev, self.bg, self.style, self.normalise);
            }
            return;
        };
        let (a, b) = self.visible_range(0.02);
        let mut per_lane: [Vec<DrawClip>; 2] = [vec![], vec![]];
        self.wave_state.clear();
        self.wave_state.resize(self.layout.clips.len(), WaveState::Loading);
        for (i, c) in self.layout.clips.iter().enumerate() {
            let Some(item) = d.items.get(i) else { continue };
            let Some(track) = item.track_id.filter(|_| !item.missing) else {
                self.wave_state[i] = WaveState::Missing;
                continue;
            };
            let slot = self.waves.get(&track);
            if let Some(s) = slot {
                if s.overview.is_some() || s.detail.is_some() {
                    self.wave_state[i] = WaveState::Ready;
                } else if s.ov_req == 2 {
                    self.wave_state[i] = WaveState::NoData;
                }
            }
            if c.end_s() < a || c.start_s > b {
                continue;
            }
            let Some(win) = logic::clip_window(c, &self.view) else { continue };
            let Some((wave, detail)) = self.wave_for(track, win.px_per_track_s) else { continue };
            per_lane[c.lane as usize].push(DrawClip { item: c.id, track, wave, detail, win, markers: self.markers.get(&track).cloned() });
        }
        let mut drawn = 0;
        for (i, l) in self.lanes.iter_mut().enumerate() {
            if let Some(l) = l {
                drawn += l.draw(&per_lane[i], &self.wtheme, self.theme_rev, self.bg, self.style, self.normalise);
            }
        }
        self.stats.drawn = drawn;
    }

    fn peaks_for(&self, clip_idx: usize, cols: usize) -> Option<Vec<u8>> {
        let d = self.detail.as_ref()?;
        let track = d.items.get(clip_idx)?.track_id?;
        let s = self.waves.get(&track)?;
        let w = s.overview.as_ref().or(s.detail.as_ref())?;
        let (pos, neg) = (w.overview.peak_pos(), w.overview.peak_neg());
        let n = pos.len().min(neg.len());
        let dur = w.duration_s();
        if n == 0 || dur <= 0.0 {
            return None;
        }
        let c = &self.layout.clips[clip_idx];
        let (t0, t1) = (c.cue_in_s, c.cue_out_s.max(c.cue_in_s + 0.1));
        let cols = cols.clamp(1, 600);
        Some(
            (0..cols)
                .map(|k| {
                    let ta = t0 + (t1 - t0) * k as f64 / cols as f64;
                    let tb = t0 + (t1 - t0) * (k + 1) as f64 / cols as f64;
                    let ia = ((ta / dur * n as f64).floor().max(0.0) as usize).min(n - 1);
                    let ib = ((tb / dur * n as f64).ceil() as usize).clamp(ia + 1, n);
                    (ia..ib).map(|i| pos[i].max(neg[i])).max().unwrap_or(0)
                })
                .collect(),
        )
    }

    fn rebuild_minimap(&mut self) {
        let (w, h, dpr) = (self.w, self.m.mini_h, self.dpr);
        if self.mini.is_none() {
            let Ok(el) = crate::util::document().create_element("canvas") else { return };
            let Ok(cv) = el.dyn_into::<HtmlCanvasElement>() else { return };
            cv.set_width((w * dpr).round() as u32);
            cv.set_height((h * dpr).round() as u32);
            let Some(ctx) = cv.get_context("2d").ok().flatten().and_then(|o| o.dyn_into::<Ctx>().ok()) else { return };
            self.mini = Some((cv, ctx));
        }
        let peaks: Vec<Option<Vec<u8>>> = (0..self.layout.clips.len())
            .map(|i| {
                let c = &self.layout.clips[i];
                let cols = ((c.len_s / self.layout.total_s.max(1.0)) * (w - 2.0 * logic::EDGE_PAD_PX.min(w * 0.05))).floor() as usize;
                self.peaks_for(i, cols)
            })
            .collect();
        let ok: Vec<bool> = (0..self.layout.bands.len()).map(|i| self.detail.as_ref().and_then(|d| d.transitions.get(i)).map(|t| t.ok).unwrap_or(true)).collect();
        if let Some((_, ctx)) = &self.mini {
            paint::draw_minimap_cache(ctx, dpr, w, h, &self.layout, &peaks, &ok, &self.colors);
        }
        self.mini_dirty = false;
    }

    fn draw_overlay(&mut self, now: f64) {
        if self.mini_dirty {
            self.rebuild_minimap();
        }
        let Some(ctx) = self.ctx.clone() else { return };
        let playing = self.playing_t();
        let scene = Scene {
            w: self.w,
            m: self.m,
            view: self.view,
            layout: &self.layout,
            detail: self.detail.as_deref(),
            colors: &self.colors,
            sel: self.sel,
            sel_band: self.sel_band,
            hover: self.hover,
            playing,
            is_playing: self.player.playing,
            wave_state: &self.wave_state,
            tip: self.tip.as_ref(),
            hover_ruler_x: self.hover_ruler_x,
            mini: self.mini.as_ref().map(|m| &m.0),
            dpr: self.dpr,
            now_ms: now,
            chip_labels: &self.chip_labels,
            chips_compact: self.chips_compact,
            focus: self.focus,
        };
        paint::draw(&ctx, &scene);
    }
}

/// Run the loads queued by the engine (outside the engine borrow).
pub fn spawn_loads(eng: &Rc<RefCell<Engine>>, loads: Vec<Load>) {
    for l in loads {
        let e = eng.clone();
        leptos::task::spawn_local(async move {
            match l {
                Load::Wave(id, detail) => {
                    let w = load_wave(id, if detail { WaveLevel::Detail } else { WaveLevel::Overview }).await;
                    e.borrow_mut().on_wave(id, detail, w);
                }
                Load::Music(id) => {
                    let m = load_music(id).await;
                    e.borrow_mut().on_music(id, m);
                }
            }
        });
    }
}
