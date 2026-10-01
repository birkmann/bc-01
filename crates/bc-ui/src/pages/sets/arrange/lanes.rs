//! GL waveform lanes. One canvas (= one WebGL context) per lane; every visible clip is drawn with
//! its own `WaveformView` (a small pool recycled by visibility) into a scissor rectangle of that
//! canvas, so a two-hour set needs 2 contexts, not one per clip, and a clip is never limited by a
//! texture or backing-store size: the shader renders each device column from the right mip level.
//!
//! LIMITATION (WS3): this relies on `WaveformView::draw` not touching the scissor state and on the
//! WebGL2 backend. With the Canvas2D fallback `draw` repaints the whole canvas, so only the last
//! clip of a lane would show; see Requests (`WaveformView::draw_in(rect)`).
#![allow(dead_code)]

use std::rc::Rc;

use bc_waveform::Waveform;
use bc_waveform::render::{Backend, WaveformView};
use bc_waveform::view::{Markers, ViewMode, ViewState, WaveStyle, WaveTheme};
use wasm_bindgen::JsCast;
use web_sys::{HtmlCanvasElement, WebGl2RenderingContext as Gl};

use super::logic::ClipWindow;

/// Views kept per lane (a lane shows at most this many clips with a waveform at once).
pub const POOL: usize = 10;

struct PoolView {
    view: WaveformView,
    item: i64,
    /// `(track id, detail?)` of the uploaded data.
    key: (i64, bool),
    has_markers: bool,
    theme_rev: u64,
    used: u64,
}

pub struct Lane {
    canvas: HtmlCanvasElement,
    gl: Option<Gl>,
    views: Vec<PoolView>,
    size: (f64, f64, f64),
    frame: u64,
    pub backend: Option<Backend>,
}

/// One clip to paint this frame.
pub struct DrawClip {
    pub item: i64,
    pub track: i64,
    pub wave: Rc<Waveform>,
    pub detail: bool,
    pub win: ClipWindow,
    pub markers: Option<Rc<Markers>>,
}

impl Lane {
    pub fn new(canvas: HtmlCanvasElement) -> Self {
        // create the context first so WaveformView::new gets the very same one
        let gl = canvas.get_context("webgl2").ok().flatten().and_then(|o| o.dyn_into::<Gl>().ok());
        Self { canvas, gl, views: Vec::new(), size: (0.0, 0.0, 0.0), frame: 0, backend: None }
    }

    pub fn canvas(&self) -> &HtmlCanvasElement {
        &self.canvas
    }

    /// Resize the drawing buffer (css size + dpr). Returns true when it changed (the caller redraws).
    pub fn resize(&mut self, w: f64, h: f64, dpr: f64) -> bool {
        if self.size == (w, h, dpr) {
            return false;
        }
        self.size = (w, h, dpr);
        if self.views.is_empty() {
            self.canvas.set_width((w * dpr).round().max(1.0) as u32);
            self.canvas.set_height((h * dpr).round().max(1.0) as u32);
        }
        for v in &mut self.views {
            v.view.resize(w, h, dpr);
        }
        true
    }

    fn view_index(&mut self, item: i64, in_use: &[i64]) -> Option<usize> {
        if let Some(i) = self.views.iter().position(|v| v.item == item) {
            return Some(i);
        }
        if self.views.len() < POOL {
            let mut view = WaveformView::new(self.canvas.clone()).ok()?;
            self.backend = Some(view.backend());
            let (w, h, dpr) = self.size;
            view.resize(w.max(1.0), h.max(1.0), dpr.max(1.0));
            self.views.push(PoolView { view, item, key: (-1, false), has_markers: false, theme_rev: u64::MAX, used: 0 });
            return Some(self.views.len() - 1);
        }
        // recycle the least recently used view that is not drawing this frame
        let i = self.views.iter().enumerate().filter(|(_, v)| !in_use.contains(&v.item)).min_by_key(|(_, v)| v.used).map(|(i, _)| i)?;
        let v = &mut self.views[i];
        v.item = item;
        v.key = (-1, false);
        v.has_markers = false;
        Some(i)
    }

    /// Paint all clips of this lane into the canvas. Returns how many were drawn.
    pub fn draw(&mut self, clips: &[DrawClip], theme: &WaveTheme, theme_rev: u64, bg: [f32; 4], style: WaveStyle, normalise: bool) -> usize {
        self.frame += 1;
        let (w, _h, dpr) = self.size;
        let in_use: Vec<i64> = clips.iter().map(|c| c.item).collect();
        // phase 1: assign views (creating one resizes the canvas, which clears it)
        let idx: Vec<Option<usize>> = clips.iter().map(|dc| self.view_index(dc.item, &in_use)).collect();
        let dev_h = self.canvas.height() as i32;
        let dev_w = self.canvas.width() as i32;
        if let Some(gl) = &self.gl {
            gl.disable(Gl::SCISSOR_TEST);
            gl.clear_color(bg[0], bg[1], bg[2], bg[3]);
            gl.clear(Gl::COLOR_BUFFER_BIT);
        }
        let mut drawn = 0;
        for (dc, i) in clips.iter().zip(idx) {
            let Some(i) = i else { continue };
            let frame = self.frame;
            let pv = &mut self.views[i];
            pv.used = frame;
            if pv.key != (dc.track, dc.detail) {
                pv.view.set_data(Some(dc.wave.clone()));
                pv.key = (dc.track, dc.detail);
            }
            if pv.theme_rev != theme_rev {
                pv.view.set_theme(theme.clone());
                pv.theme_rev = theme_rev;
            }
            match (&dc.markers, pv.has_markers) {
                (Some(m), false) => {
                    pv.view.set_markers((**m).clone());
                    pv.has_markers = true;
                }
                (None, true) => {
                    pv.view.set_markers(Markers::default());
                    pv.has_markers = false;
                }
                _ => {}
            }
            pv.view.set_view(ViewState {
                style,
                playhead_s: -1.0e9,
                px_per_s: dc.win.px_per_track_s,
                offset_s: dc.win.offset_s,
                normalise,
                mode: ViewMode::Free,
            });
            if let Some(gl) = &self.gl {
                let x0 = ((dc.win.x0.max(0.0)) * dpr).round() as i32;
                let x1 = ((dc.win.x1.min(w)) * dpr).round() as i32;
                if x1 <= x0 {
                    continue;
                }
                gl.enable(Gl::SCISSOR_TEST);
                gl.scissor(x0.min(dev_w), 0, (x1 - x0).max(0), dev_h);
            }
            pv.view.draw();
            drawn += 1;
        }
        if let Some(gl) = &self.gl {
            gl.disable(Gl::SCISSOR_TEST);
        }
        drawn
    }

    /// Pixel size of the drawing buffer.
    pub fn buffer_size(&self) -> (u32, u32) {
        (self.canvas.width(), self.canvas.height())
    }
}
