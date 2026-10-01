//! WebGL2 renderer with a Canvas2D fallback (feature `webgl`).
//!
//! Textures are uploaded once per track (`set_data`): the mip pyramid is packed into two RGBA8
//! textures (see [`crate::view::plan_layout`]). A fragment shader renders every device-pixel
//! column from the right mip level, so per-frame CPU work is only uniforms plus the marker
//! rectangles (a few hundred floats).

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{
    CanvasRenderingContext2d, Event, HtmlCanvasElement, WebGl2RenderingContext as Gl, WebGlBuffer,
    WebGlProgram, WebGlShader, WebGlTexture, WebGlUniformLocation, WebGlVertexArrayObject,
};

use crate::bars::{Refs, bar_in_pyramid, reference_levels};
use crate::format::Waveform;
use crate::mip::Pyramid;
use crate::view::{
    HALO_ALPHA, LAYER_ALPHA, Layout, MONO_BODY_HEADROOM, MAX_LEVELS, Markers, PLAYED_ALPHA, Rect, Region, Rgba,
    ViewState, WaveStyle, WaveTheme, band_levels, build_marker_rects, byte_to_lin, norm_height,
    pack_textures, plan_layout, resolve_view, spectral_color,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    WebGl2,
    Canvas2d,
}

const VERT: &str = r#"#version 300 es
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

/// Fragment shader; the display-mapping constants come from [`crate::view`] / [`crate::bars`] so
/// the shader and the Canvas2D fallback cannot drift apart.
fn frag() -> String {
    use crate::bars::{BAR_EXPONENT, BAR_PEAK_WEIGHT, BAR_RMS_WEIGHT};
    use crate::view::{
        DECK_EXPONENT, HALO_ALPHA, MONO_BODY_HEADROOM, HUE_SHARPNESS, LAYER_ALPHA, PLAYED_ALPHA, SAT_FLOOR,
    };
    format!(
        r#"#version 300 es
precision highp float;
precision highp int;
precision highp sampler2D;
const float BAR_EXP = {BAR_EXPONENT:.4};
const float BAR_RMS_W = {BAR_RMS_WEIGHT:.4};
const float BAR_PEAK_W = {BAR_PEAK_WEIGHT:.4};
const float DECK_EXP = {DECK_EXPONENT:.4};
const float SHARP = {HUE_SHARPNESS:.4};
const float SAT_FLOOR = {SAT_FLOOR:.4};
const float PLAYED_A = {PLAYED_ALPHA:.4};
const float LAYER_A = {LAYER_ALPHA:.4};
const float MONO_HEAD = {MONO_BODY_HEADROOM:.4};
const float HALO_A = {HALO_ALPHA:.4};
{FRAG_BODY}"#
    )
}

const FRAG_BODY: &str = r#"
uniform sampler2D u_a;
uniform sampler2D u_b;
uniform vec2 u_res;
uniform vec2 u_org;
uniform float u_t_left;
uniform float u_spp;
uniform float u_dt0;
uniform float u_dur;
uniform int u_first;
uniform int u_nlev;
uniform int u_width;
uniform int u_style;
uniform ivec2 u_lv[24];
uniform float u_play;
uniform vec4 u_ref;    // rms, peak, 0, 0 (linear)
uniform vec3 u_bref;   // low, mid, high (linear)
uniform vec4 u_bg;
uniform vec4 u_wave;
uniform vec4 u_core;
uniform vec4 u_low;
uniform vec4 u_mid;
uniform vec4 u_high;
uniform vec4 u_llow;
uniform vec4 u_lmid;
uniform vec4 u_lhigh;
uniform vec4 u_pcol;   // played (bars)
uniform vec4 u_ucol;   // unplayed (bars)
uniform vec2 u_bar;    // bar width, gap in device px
uniform float u_hover; // seconds, < 0 = off
uniform float u_buf;   // buffered-to seconds, < 0 = off
out vec4 o;

void fetchPt(int slot, int i, out vec4 a, out vec4 b) {
    ivec2 lv = u_lv[slot];
    i = clamp(i, 0, lv.x - 1);
    ivec2 uv = ivec2(i % u_width, lv.y + i / u_width);
    a = texelFetch(u_a, uv, 0);
    b = texelFetch(u_b, uv, 0);
}
// stored dBFS byte (0..1) -> linear amplitude (view::byte_to_lin)
float lin(float x) { return x > 0.0 ? pow(10.0, 3.0 * min(x, 1.0) - 3.0) : 0.0; }
float nh(float l, float r) { return pow(clamp(l / max(r, 1e-6), 0.0, 1.0), DECK_EXP); }
float fillTo(float h, float ad, float aa) { return 1.0 - smoothstep(h - aa, h + aa, ad); }
// premultiplied "over"
vec4 over(vec4 dst, vec3 c, float a) { return vec4(c * a, a) + dst * (1.0 - a); }
// view::spectral_color
vec3 spectral(vec3 n) {
    vec3 w = pow(n, vec3(SHARP));
    float sum = w.x + w.y + w.z;
    if (sum <= 1e-6) { return u_low.rgb; }
    float pos = (w.y * 0.5 + w.z) / sum;
    vec3 c = pos < 0.5 ? mix(u_low.rgb, u_mid.rgb, pos * 2.0) : mix(u_mid.rgb, u_high.rgb, (pos - 0.5) * 2.0);
    float mx = max(n.x, max(n.y, n.z));
    float mn = min(n.x, min(n.y, n.z));
    float sat = SAT_FLOOR + (1.0 - SAT_FLOOR) * ((mx - mn) / max(mx, 1e-4));
    float l = dot(c, vec3(0.2126, 0.7152, 0.0722));
    c = vec3(l) + (c - vec3(l)) * sat;
    float m = max(max(c.r, max(c.g, c.b)), 1e-4);
    return min(c * (1.0 + (1.0 / m - 1.0) * 0.5), vec3(1.0));
}

// Energy-mean / max accumulation over [t0, t1) at slot. sr/sl/sm/sh are summed squares,
// pk = (peak_pos, peak_neg) maxima, returns the point count.
float gather(int slot, float t0, float t1, out vec4 e, out vec2 pk) {
    float dt = u_dt0 * exp2(float(slot + u_first));
    int n = u_lv[slot].x;
    int i0 = int(floor(max(t0, 0.0) / dt));
    int i1 = max(int(ceil(t1 / dt)), i0 + 1);
    i0 = min(i0, n - 1);
    i1 = min(i1, n);
    i1 = min(i1, i0 + 64);
    e = vec4(0.0); pk = vec2(0.0);
    float wsum = 0.0;
    for (int i = i0; i < i1; i++) {
        vec4 a; vec4 b;
        fetchPt(slot, i, a, b);
        // overlap of the point's span with [t0, t1), so columns do not flicker between 1 and 2 points
        float w = max(min(t1, float(i + 1) * dt) - max(t0, float(i) * dt), 1e-4 * dt);
        float r = lin(a.z); float l = lin(b.r); float m = lin(b.g); float h = lin(b.b);
        e += w * vec4(r * r, l * l, m * m, h * h);
        pk = max(pk, a.xy);
        wsum += w;
    }
    e = sqrt(e / max(wsum, 1e-9));   // rms, low, mid, high (linear)
    return wsum;
}

void bars(float lx) {
    float pitch = u_bar.x + u_bar.y;
    float idx = floor(lx / pitch);
    if (lx - idx * pitch >= u_bar.x) { o = vec4(u_bg.rgb * u_bg.a, u_bg.a); return; }
    float tb0 = u_t_left + idx * pitch * u_spp;
    float tb1 = tb0 + pitch * u_spp;
    if (tb1 <= 0.0 || tb0 >= u_dur) { o = vec4(u_bg.rgb * u_bg.a, u_bg.a); return; }
    float ppp = (tb1 - tb0) / u_dt0;
    int k = ppp < 8.0 ? 0 : int(floor(log2(ppp / 4.0)));
    int slot = clamp(k - u_first, 0, u_nlev - 1);
    vec4 e; vec2 pk;
    gather(slot, tb0, tb1, e, pk);
    float peak = lin(max(pk.x, pk.y));
    float r = pow(clamp(e.x / max(u_ref.x, 1e-6), 0.0, 1.0), BAR_EXP);
    float h = min(BAR_RMS_W * r + BAR_PEAK_W * clamp(peak / max(u_ref.y, 1e-6), 0.0, 1.0), 1.0);
    float hh = 0.5 * u_res.y;
    float half_h = max(h * hh, 0.75);
    float ad = abs(gl_FragCoord.y - u_org.y - hh);
    float cov = clamp(half_h - ad + 0.5, 0.0, 1.0);
    float tp = u_t_left + (lx + 0.5) * u_spp;
    vec4 col = tp < u_play ? u_pcol : u_ucol;
    if (u_hover >= 0.0 && tp >= u_play && tp < u_hover) { col = mix(u_ucol, u_pcol, 0.35); }
    float a = cov * col.a * ((u_buf >= 0.0 && tp > u_buf) ? 0.45 : 1.0);
    o = over(vec4(u_bg.rgb * u_bg.a, u_bg.a), col.rgb, a);
}

void main() {
    float lx = floor(gl_FragCoord.x - u_org.x);
    float t0 = u_t_left + lx * u_spp;
    float t1 = t0 + u_spp;
    vec4 bgp = vec4(u_bg.rgb * u_bg.a, u_bg.a);
    if (u_nlev == 0 || t1 <= 0.0 || t0 >= u_dur) { o = bgp; return; }
    if (u_style == 3) { bars(lx); return; }
    float ppp = u_spp / u_dt0;
    int k = ppp < 8.0 ? 0 : int(floor(log2(ppp / 4.0)));
    int slot = clamp(k - u_first, 0, u_nlev - 1);
    float dt = u_dt0 * exp2(float(slot + u_first));
    vec4 e; vec2 pk;
    if (u_spp >= dt * 0.5) {
        gather(slot, t0, t1, e, pk);
    } else {
        // zoomed in past one point per pixel: nearest point, no interpolation
        gather(slot, 0.5 * (t0 + t1), 0.5 * (t0 + t1), e, pk);
    }
    float hh = 0.5 * u_res.y;
    float d = (gl_FragCoord.y - u_org.y - hh) / hh;
    float aa = 1.0 / hh;
    float ad = abs(d);
    float pa = mix(1.0, PLAYED_A, clamp((u_play - t0) / max(t1 - t0, 1e-9), 0.0, 1.0));
    vec4 acc = bgp;
    if (u_style == 2) {
        // peak halo at 35 % alpha, RMS body solid
        float pkv = d >= 0.0 ? pk.x : pk.y;
        float ph = max(nh(lin(pkv), u_ref.y), aa);
        float rm = max(nh(e.x, u_ref.x * MONO_HEAD), aa);
        acc = over(acc, u_wave.rgb, fillTo(ph, ad, aa) * u_wave.a * HALO_A * pa);
        acc = over(acc, u_core.rgb, fillTo(rm, ad, aa) * u_core.a * pa);
    } else if (u_style == 0) {
        // one silhouette (RMS), hue from the per-band-normalised balance
        vec3 n = clamp(e.yzw / u_bref, 0.0, 1.0);
        float h = max(nh(e.x, u_ref.x), aa);
        acc = over(acc, spectral(n), fillTo(h, ad, aa) * pa);
    } else {
        // three layers, tallest first so none hides another
        float hs[3];
        vec4 cs[3];
        hs[0] = nh(e.y, u_bref.x); cs[0] = u_llow;
        hs[1] = nh(e.z, u_bref.y); cs[1] = vec4(u_lmid.rgb, u_lmid.a * LAYER_A);
        hs[2] = nh(e.w, u_bref.z); cs[2] = vec4(u_lhigh.rgb, u_lhigh.a * LAYER_A);
        for (int p = 0; p < 2; p++) {
            for (int q = 0; q < 2; q++) {
                if (hs[q] < hs[q + 1]) {
                    float th = hs[q]; hs[q] = hs[q + 1]; hs[q + 1] = th;
                    vec4 tc = cs[q]; cs[q] = cs[q + 1]; cs[q + 1] = tc;
                }
            }
        }
        for (int j = 0; j < 3; j++) {
            acc = over(acc, cs[j].rgb, fillTo(max(hs[j], j == 0 ? aa : 0.0), ad, aa) * cs[j].a * pa);
        }
    }
    o = acc;
}
"#;

const RECT_VERT: &str = r#"#version 300 es
in vec2 a_pos;
in vec4 a_col;
uniform vec2 u_res;
out vec4 v_col;
void main() {
    v_col = a_col;
    gl_Position = vec4(a_pos.x / u_res.x * 2.0 - 1.0, 1.0 - a_pos.y / u_res.y * 2.0, 0.0, 1.0);
}
"#;

const RECT_FRAG: &str = r#"#version 300 es
precision mediump float;
in vec4 v_col;
out vec4 o;
void main() { o = vec4(v_col.rgb * v_col.a, v_col.a); }
"#;

struct WaveUniforms {
    a: Option<WebGlUniformLocation>,
    b: Option<WebGlUniformLocation>,
    res: Option<WebGlUniformLocation>,
    org: Option<WebGlUniformLocation>,
    t_left: Option<WebGlUniformLocation>,
    spp: Option<WebGlUniformLocation>,
    dt0: Option<WebGlUniformLocation>,
    dur: Option<WebGlUniformLocation>,
    first: Option<WebGlUniformLocation>,
    nlev: Option<WebGlUniformLocation>,
    width: Option<WebGlUniformLocation>,
    style: Option<WebGlUniformLocation>,
    lv: Option<WebGlUniformLocation>,
    play: Option<WebGlUniformLocation>,
    rf: Option<WebGlUniformLocation>,
    bref: Option<WebGlUniformLocation>,
    pcol: Option<WebGlUniformLocation>,
    ucol: Option<WebGlUniformLocation>,
    bar: Option<WebGlUniformLocation>,
    hover: Option<WebGlUniformLocation>,
    buf: Option<WebGlUniformLocation>,
    bg: Option<WebGlUniformLocation>,
    wave: Option<WebGlUniformLocation>,
    core: Option<WebGlUniformLocation>,
    low: Option<WebGlUniformLocation>,
    mid: Option<WebGlUniformLocation>,
    high: Option<WebGlUniformLocation>,
    llow: Option<WebGlUniformLocation>,
    lmid: Option<WebGlUniformLocation>,
    lhigh: Option<WebGlUniformLocation>,
}

struct GlState {
    gl: Gl,
    wave: WebGlProgram,
    wu: WaveUniforms,
    rect: WebGlProgram,
    rect_res: Option<WebGlUniformLocation>,
    rect_vao: WebGlVertexArrayObject,
    rect_buf: WebGlBuffer,
    empty_vao: WebGlVertexArrayObject,
    tex_a: WebGlTexture,
    tex_b: WebGlTexture,
    max_tex: usize,
}

fn compile(gl: &Gl, kind: u32, src: &str) -> Result<WebGlShader, JsValue> {
    let s = gl
        .create_shader(kind)
        .ok_or_else(|| JsValue::from_str("create_shader"))?;
    gl.shader_source(&s, src);
    gl.compile_shader(&s);
    if gl
        .get_shader_parameter(&s, Gl::COMPILE_STATUS)
        .as_bool()
        .unwrap_or(false)
    {
        Ok(s)
    } else {
        Err(JsValue::from_str(
            &gl.get_shader_info_log(&s).unwrap_or_default(),
        ))
    }
}

fn link(gl: &Gl, vs: &str, fs: &str) -> Result<WebGlProgram, JsValue> {
    let v = compile(gl, Gl::VERTEX_SHADER, vs)?;
    let f = compile(gl, Gl::FRAGMENT_SHADER, fs)?;
    let p = gl
        .create_program()
        .ok_or_else(|| JsValue::from_str("create_program"))?;
    gl.attach_shader(&p, &v);
    gl.attach_shader(&p, &f);
    gl.link_program(&p);
    if gl
        .get_program_parameter(&p, Gl::LINK_STATUS)
        .as_bool()
        .unwrap_or(false)
    {
        Ok(p)
    } else {
        Err(JsValue::from_str(
            &gl.get_program_info_log(&p).unwrap_or_default(),
        ))
    }
}

fn make_texture(gl: &Gl) -> Result<WebGlTexture, JsValue> {
    let t = gl
        .create_texture()
        .ok_or_else(|| JsValue::from_str("create_texture"))?;
    gl.bind_texture(Gl::TEXTURE_2D, Some(&t));
    for (p, v) in [
        (Gl::TEXTURE_MIN_FILTER, Gl::NEAREST),
        (Gl::TEXTURE_MAG_FILTER, Gl::NEAREST),
        (Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE),
        (Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE),
    ] {
        gl.tex_parameteri(Gl::TEXTURE_2D, p, v as i32);
    }
    Ok(t)
}

impl GlState {
    fn new(gl: Gl) -> Result<GlState, JsValue> {
        let wave = link(&gl, VERT, &frag())?;
        let u = |n: &str| gl.get_uniform_location(&wave, n);
        let wu = WaveUniforms {
            a: u("u_a"),
            b: u("u_b"),
            res: u("u_res"),
            org: u("u_org"),
            t_left: u("u_t_left"),
            spp: u("u_spp"),
            dt0: u("u_dt0"),
            dur: u("u_dur"),
            first: u("u_first"),
            nlev: u("u_nlev"),
            width: u("u_width"),
            style: u("u_style"),
            lv: u("u_lv"),
            play: u("u_play"),
            rf: u("u_ref"),
            bref: u("u_bref"),
            pcol: u("u_pcol"),
            ucol: u("u_ucol"),
            bar: u("u_bar"),
            hover: u("u_hover"),
            buf: u("u_buf"),
            bg: u("u_bg"),
            wave: u("u_wave"),
            core: u("u_core"),
            low: u("u_low"),
            mid: u("u_mid"),
            high: u("u_high"),
            llow: u("u_llow"),
            lmid: u("u_lmid"),
            lhigh: u("u_lhigh"),
        };
        let rect = link(&gl, RECT_VERT, RECT_FRAG)?;
        let rect_res = gl.get_uniform_location(&rect, "u_res");
        let rect_vao = gl
            .create_vertex_array()
            .ok_or_else(|| JsValue::from_str("vao"))?;
        let rect_buf = gl
            .create_buffer()
            .ok_or_else(|| JsValue::from_str("buffer"))?;
        gl.bind_vertex_array(Some(&rect_vao));
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&rect_buf));
        let pos = gl.get_attrib_location(&rect, "a_pos") as u32;
        let col = gl.get_attrib_location(&rect, "a_col") as u32;
        gl.enable_vertex_attrib_array(pos);
        gl.vertex_attrib_pointer_with_i32(pos, 2, Gl::FLOAT, false, 24, 0);
        gl.enable_vertex_attrib_array(col);
        gl.vertex_attrib_pointer_with_i32(col, 4, Gl::FLOAT, false, 24, 8);
        let empty_vao = gl
            .create_vertex_array()
            .ok_or_else(|| JsValue::from_str("vao"))?;
        gl.bind_vertex_array(None);
        let tex_a = make_texture(&gl)?;
        let tex_b = make_texture(&gl)?;
        let max_tex = gl
            .get_parameter(Gl::MAX_TEXTURE_SIZE)?
            .as_f64()
            .unwrap_or(2048.0) as usize;
        Ok(GlState {
            gl,
            wave,
            wu,
            rect,
            rect_res,
            rect_vao,
            rect_buf,
            empty_vao,
            tex_a,
            tex_b,
            max_tex,
        })
    }

    fn upload(&self, a: &[u8], b: &[u8], layout: &Layout) {
        let gl = &self.gl;
        for (tex, data) in [(&self.tex_a, a), (&self.tex_b, b)] {
            gl.bind_texture(Gl::TEXTURE_2D, Some(tex));
            let _ = gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
                Gl::TEXTURE_2D,
                0,
                Gl::RGBA8 as i32,
                layout.width as i32,
                layout.rows as i32,
                0,
                Gl::RGBA,
                Gl::UNSIGNED_BYTE,
                Some(data),
            );
        }
    }
}

fn set4(gl: &Gl, loc: &Option<WebGlUniformLocation>, c: Rgba) {
    gl.uniform4f(loc.as_ref(), c[0], c[1], c[2], c[3]);
}

fn css(c: Rgba, alpha_mul: f32) -> String {
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "rgba({},{},{},{:.3})",
        q(c[0]),
        q(c[1]),
        q(c[2]),
        (c[3] * alpha_mul).clamp(0.0, 1.0)
    )
}

/// Framework-agnostic waveform view.
pub struct WaveformView {
    canvas: HtmlCanvasElement,
    backend: Backend,
    gl: Option<GlState>,
    ctx2d: Option<CanvasRenderingContext2d>,
    data: Option<Rc<Waveform>>,
    pyramid: Option<Pyramid>,
    layout: Option<Layout>,
    refs: Refs,
    markers: Markers,
    theme: WaveTheme,
    view: ViewState,
    css_w: f64,
    css_h: f64,
    dpr: f64,
    /// Sub-rectangle (CSS px) set by `draw_in`; `None` = whole canvas.
    region: Option<Region>,
    lost: Rc<Cell<bool>>,
    restored: Rc<Cell<bool>>,
    _listeners: Vec<Closure<dyn FnMut(Event)>>,
}

impl WaveformView {
    /// Tries WebGL2 first, falls back to Canvas2D.
    pub fn new(canvas: HtmlCanvasElement) -> Result<WaveformView, JsValue> {
        let lost = Rc::new(Cell::new(false));
        let restored = Rc::new(Cell::new(false));
        let mut gl_state = None;
        if let Ok(Some(obj)) = canvas.get_context("webgl2")
            && let Ok(gl) = obj.dyn_into::<Gl>()
        {
            gl_state = GlState::new(gl).ok();
        }
        let (backend, ctx2d, mut listeners) = if gl_state.is_some() {
            (Backend::WebGl2, None, Vec::new())
        } else {
            let ctx = canvas
                .get_context("2d")?
                .ok_or_else(|| JsValue::from_str("no 2d context"))?
                .dyn_into::<CanvasRenderingContext2d>()?;
            (Backend::Canvas2d, Some(ctx), Vec::new())
        };
        if backend == Backend::WebGl2 {
            let l = lost.clone();
            let on_lost = Closure::<dyn FnMut(Event)>::new(move |e: Event| {
                e.prevent_default();
                l.set(true);
            });
            canvas.add_event_listener_with_callback(
                "webglcontextlost",
                on_lost.as_ref().unchecked_ref(),
            )?;
            let (r, l2) = (restored.clone(), lost.clone());
            let on_restored = Closure::<dyn FnMut(Event)>::new(move |_| {
                l2.set(false);
                r.set(true);
            });
            canvas.add_event_listener_with_callback(
                "webglcontextrestored",
                on_restored.as_ref().unchecked_ref(),
            )?;
            listeners.push(on_lost);
            listeners.push(on_restored);
        }
        Ok(WaveformView {
            css_w: canvas.width() as f64,
            css_h: canvas.height() as f64,
            canvas,
            backend,
            gl: gl_state,
            ctx2d,
            data: None,
            pyramid: None,
            layout: None,
            refs: Refs::absolute(),
            markers: Markers::default(),
            theme: WaveTheme::default(),
            view: ViewState::default(),
            dpr: 1.0,
            region: None,
            lost,
            restored,
            _listeners: listeners,
        })
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Replace the waveform; uploads the mip textures once.
    pub fn set_data(&mut self, data: Option<Rc<Waveform>>) {
        self.data = data;
        self.pyramid = self.data.as_deref().map(Pyramid::from_waveform);
        // reference levels from the coarsest level (<= 2048 points): identical for every renderer
        self.refs = self
            .pyramid
            .as_ref()
            .and_then(|p| p.levels.last())
            .map_or(Refs::absolute(), reference_levels);
        self.layout = None;
        self.upload();
    }

    fn upload(&mut self) {
        let (Some(gl), Some(p)) = (&self.gl, &self.pyramid) else {
            return;
        };
        if self.lost.get() {
            return;
        }
        let counts: Vec<usize> = p.levels.iter().map(|l| l.n).collect();
        let layout = plan_layout(&counts, gl.max_tex);
        let (a, b) = pack_textures(p, &layout);
        gl.upload(&a, &b, &layout);
        self.layout = Some(layout);
    }

    pub fn set_markers(&mut self, m: Markers) {
        self.markers = m;
    }
    pub fn set_theme(&mut self, t: WaveTheme) {
        self.theme = t;
    }
    pub fn set_view(&mut self, v: ViewState) {
        self.view = v;
    }

    /// Size in CSS pixels and device pixel ratio; resizes the drawing buffer.
    pub fn resize(&mut self, css_w: f64, css_h: f64, dpr: f64) {
        self.css_w = css_w.max(1.0);
        self.css_h = css_h.max(1.0);
        self.dpr = dpr.max(0.5);
        // Assigning width/height clears the drawing buffer even with the same value, so only
        // touch them when the size really changed.
        let (pw, ph) = (
            (self.css_w * self.dpr).round() as u32,
            (self.css_h * self.dpr).round() as u32,
        );
        if self.canvas.width() != pw {
            self.canvas.set_width(pw);
        }
        if self.canvas.height() != ph {
            self.canvas.set_height(ph);
        }
    }

    /// Width (CSS px) of the area currently drawn into.
    fn view_w(&self) -> f64 {
        self.region.map_or(self.css_w, |r| r.w.max(1.0))
    }
    fn view_h(&self) -> f64 {
        self.region.map_or(self.css_h, |r| r.h.max(1.0))
    }

    /// Draw into a sub-rectangle (CSS px, origin top-left) of the canvas so several views can
    /// share one canvas (WebGL viewport+scissor, Canvas2D clip+translate). Only the rectangle is
    /// painted; `time_at_x`/`x_at_time` are relative to the rectangle afterwards.
    pub fn draw_in(&mut self, rect: Region) {
        self.region = Some(rect);
        self.draw_inner();
    }

    fn duration_s(&self) -> f64 {
        self.data.as_ref().map_or(0.0, |d| d.duration_s())
    }

    pub fn time_at_x(&self, css_x: f64) -> f64 {
        resolve_view(&self.view, self.duration_s(), self.view_w()).time_at_x(css_x)
    }

    pub fn x_at_time(&self, t: f64) -> f64 {
        resolve_view(&self.view, self.duration_s(), self.view_w()).x_at_time(t)
    }

    /// Draw into the whole canvas.
    pub fn draw(&mut self) {
        self.region = None;
        self.draw_inner();
    }

    fn draw_inner(&mut self) {
        if self.restored.replace(false)
            && let Some(Ok(gl)) = self
                .canvas
                .get_context("webgl2")
                .ok()
                .flatten()
                .and_then(|o| o.dyn_into::<Gl>().ok())
                .map(GlState::new)
        {
            self.gl = Some(gl);
            self.upload();
        }
        if self.lost.get() {
            return;
        }
        let rects = match &self.data {
            Some(d) => build_marker_rects(
                &self.markers,
                &self.view,
                &self.theme,
                d.duration_s(),
                self.view_w(),
                self.view_h(),
            ),
            None => vec![],
        };
        match self.backend {
            Backend::WebGl2 => self.draw_gl(&rects),
            Backend::Canvas2d => self.draw_2d(&rects),
        }
    }

    /// Reference levels in force: Bars always normalise, the other styles when `normalise`.
    fn active_refs(&self) -> Refs {
        if self.view.normalise || self.view.style == WaveStyle::Bars {
            self.refs
        } else {
            Refs::absolute()
        }
    }

    /// Bar width and gap in whole device pixels.
    fn bar_px(&self) -> (f32, f32) {
        let bw = (self.theme.bar_w_css as f64 * self.dpr).round().max(1.0);
        let gp = (self.theme.gap_css as f64 * self.dpr).round().max(0.0);
        (bw as f32, gp as f32)
    }

    fn draw_gl(&self, rects: &[Rect]) {
        let Some(s) = &self.gl else { return };
        let gl = &s.gl;
        let (cw, ch) = (self.canvas.width() as f64, self.canvas.height() as f64);
        let (rx, ry, rw, rh) = match self.region {
            Some(r) => {
                let x = (r.x * self.dpr).round().clamp(0.0, cw);
                let w = (r.w * self.dpr).round().clamp(1.0, cw - x).max(1.0);
                let h = (r.h * self.dpr).round().max(1.0);
                // GL origin is bottom-left
                let y = (ch - (r.y * self.dpr).round() - h).max(0.0);
                (x as i32, y as i32, w as i32, h as i32)
            }
            None => (0, 0, cw as i32, ch as i32),
        };
        let (w, h) = (rw, rh);
        gl.viewport(rx, ry, rw, rh);
        gl.enable(Gl::SCISSOR_TEST);
        gl.scissor(rx, ry, rw, rh);
        gl.disable(Gl::BLEND);
        let t = &self.theme;
        gl.use_program(Some(&s.wave));
        gl.bind_vertex_array(Some(&s.empty_vao));
        let res = resolve_view(&self.view, self.duration_s(), self.view_w());
        let (nlev, first, dt0, width) = match (&self.layout, &self.pyramid) {
            (Some(l), Some(p)) => (
                l.slots.len() as i32,
                l.first_level as i32,
                p.dt0_s,
                l.width as i32,
            ),
            _ => (0, 0, 1.0, 1),
        };
        gl.active_texture(Gl::TEXTURE0);
        gl.bind_texture(Gl::TEXTURE_2D, Some(&s.tex_a));
        gl.active_texture(Gl::TEXTURE1);
        gl.bind_texture(Gl::TEXTURE_2D, Some(&s.tex_b));
        gl.uniform1i(s.wu.a.as_ref(), 0);
        gl.uniform1i(s.wu.b.as_ref(), 1);
        gl.uniform2f(s.wu.res.as_ref(), w as f32, h as f32);
        gl.uniform2f(s.wu.org.as_ref(), rx as f32, ry as f32);
        gl.uniform1f(s.wu.t_left.as_ref(), res.t_left_s as f32);
        gl.uniform1f(s.wu.spp.as_ref(), (1.0 / (res.px_per_s * self.dpr)) as f32);
        gl.uniform1f(s.wu.dt0.as_ref(), dt0 as f32);
        gl.uniform1f(s.wu.dur.as_ref(), self.duration_s() as f32);
        gl.uniform1i(s.wu.first.as_ref(), first);
        gl.uniform1i(s.wu.nlev.as_ref(), nlev);
        gl.uniform1i(s.wu.width.as_ref(), width);
        gl.uniform1i(
            s.wu.style.as_ref(),
            match self.view.style {
                WaveStyle::RgbSpectral => 0,
                WaveStyle::ThreeBand => 1,
                WaveStyle::Mono => 2,
                WaveStyle::Bars => 3,
            },
        );
        let refs = self.active_refs();
        gl.uniform4f(s.wu.rf.as_ref(), refs.rms, refs.peak, 0.0, 0.0);
        gl.uniform3f(s.wu.bref.as_ref(), refs.low, refs.mid, refs.high);
        let (bw, gp) = self.bar_px();
        gl.uniform2f(s.wu.bar.as_ref(), bw, gp);
        gl.uniform1f(s.wu.hover.as_ref(), self.markers.hover_s.map_or(-1.0, |v| v as f32));
        gl.uniform1f(s.wu.buf.as_ref(), self.markers.buffered_to_s.map_or(-1.0, |v| v as f32));
        let mut lv = vec![0i32; MAX_LEVELS * 2];
        if let Some(l) = &self.layout {
            for (i, sl) in l.slots.iter().enumerate().take(MAX_LEVELS) {
                lv[2 * i] = sl.n as i32;
                lv[2 * i + 1] = sl.row_off as i32;
            }
        }
        gl.uniform2iv_with_i32_array(s.wu.lv.as_ref(), &lv);
        set4(gl, &s.wu.bg, t.background);
        set4(gl, &s.wu.wave, t.wave);
        set4(gl, &s.wu.core, t.core);
        set4(gl, &s.wu.low, t.low);
        set4(gl, &s.wu.mid, t.mid);
        set4(gl, &s.wu.high, t.high);
        set4(gl, &s.wu.llow, t.layer_low);
        set4(gl, &s.wu.lmid, t.layer_mid);
        set4(gl, &s.wu.lhigh, t.layer_high);
        gl.uniform1f(s.wu.play.as_ref(), self.view.playhead_s as f32);
        set4(gl, &s.wu.pcol, t.played);
        set4(gl, &s.wu.ucol, t.unplayed);
        gl.draw_arrays(Gl::TRIANGLES, 0, 3);

        if rects.is_empty() {
            gl.disable(Gl::SCISSOR_TEST);
            return;
        }
        let d = self.dpr as f32;
        let mut v: Vec<f32> = Vec::with_capacity(rects.len() * 36);
        for r in rects {
            let (x0, y0, x1, y1) = (r.x0 * d, r.y0 * d, r.x1 * d, r.y1 * d);
            // at least one device pixel wide
            let (x0, x1) = if x1 - x0 < 1.0 {
                (x0, x0 + 1.0)
            } else {
                (x0, x1)
            };
            for (x, y) in [(x0, y0), (x1, y0), (x0, y1), (x0, y1), (x1, y0), (x1, y1)] {
                v.extend_from_slice(&[x, y, r.color[0], r.color[1], r.color[2], r.color[3]]);
            }
        }
        gl.enable(Gl::BLEND);
        gl.blend_func(Gl::ONE, Gl::ONE_MINUS_SRC_ALPHA);
        gl.use_program(Some(&s.rect));
        gl.uniform2f(s.rect_res.as_ref(), w as f32, h as f32);
        gl.bind_vertex_array(Some(&s.rect_vao));
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&s.rect_buf));
        let arr = js_sys::Float32Array::from(v.as_slice());
        gl.buffer_data_with_array_buffer_view(Gl::ARRAY_BUFFER, &arr, Gl::DYNAMIC_DRAW);
        gl.draw_arrays(Gl::TRIANGLES, 0, (rects.len() * 6) as i32);
        gl.bind_vertex_array(None);
        gl.disable(Gl::SCISSOR_TEST);
    }

    fn draw_2d(&self, rects: &[Rect]) {
        let Some(ctx) = &self.ctx2d else { return };
        let _ = ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
        ctx.save();
        let (w, h) = match self.region {
            Some(rg) => {
                let (x, y) = ((rg.x * self.dpr).round(), (rg.y * self.dpr).round());
                let (w, h) = (
                    (rg.w * self.dpr).round().max(1.0),
                    (rg.h * self.dpr).round().max(1.0),
                );
                ctx.begin_path();
                ctx.rect(x, y, w, h);
                ctx.clip();
                let _ = ctx.translate(x, y);
                (w, h)
            }
            None => (self.canvas.width() as f64, self.canvas.height() as f64),
        };
        let t = &self.theme;
        let _ = ctx.set_global_composite_operation("source-over");
        ctx.set_fill_style_str(&css(t.background, 1.0));
        ctx.clear_rect(0.0, 0.0, w, h);
        ctx.fill_rect(0.0, 0.0, w, h);
        if let Some(p) = &self.pyramid {
            let res = resolve_view(&self.view, self.duration_s(), self.view_w());
            let spp = 1.0 / (res.px_per_s * self.dpr);
            let refs = self.active_refs();
            let mid = h / 2.0;
            let dur = self.duration_s();
            let lin = |v: u8| byte_to_lin(v as f32 / 255.0);
            let rgb = |c: [f32; 3], a: f32| css([c[0], c[1], c[2], a], 1.0);
            let rgb_of = |c: Rgba| [c[0], c[1], c[2]];
            let min_h = 0.75 / mid; // 1.5 device px in total
            let fill = |x: f64, w: f64, h: f32, style: String| {
                let h = h as f64 * mid;
                ctx.set_fill_style_str(&style);
                ctx.fill_rect(x, mid - h, w, h * 2.0);
            };
            if self.view.style == WaveStyle::Bars {
                let (bw, gp) = self.bar_px();
                let (bw, pitch) = (bw as f64, (bw + gp) as f64);
                let play = self.view.playhead_s;
                let hover = self.markers.hover_s;
                let buf = self.markers.buffered_to_s;
                let t = &self.theme;
                let mut idx = 0u32;
                loop {
                    let x = idx as f64 * pitch;
                    if x >= w {
                        break;
                    }
                    idx += 1;
                    let (tb0, tb1) = (res.t_left_s + x * spp, res.t_left_s + (x + pitch) * spp);
                    let Some(hv) = bar_in_pyramid(p, &refs, tb0, tb1) else {
                        if tb0 >= dur {
                            break;
                        }
                        continue;
                    };
                    let half = ((hv as f64 * mid).max(0.75)).min(mid);
                    // split the bar at the playhead pixel; hover tint between playhead and hover
                    let px_of = |tt: f64| ((tt - res.t_left_s) / spp).floor();
                    let split = px_of(play).clamp(x, x + bw);
                    let hov_x = hover.map(|hs| px_of(hs).clamp(x, x + bw));
                    let seg = |x0: f64, x1: f64, col: Rgba| {
                        if x1 <= x0 {
                            return;
                        }
                        let tp = res.t_left_s + (x0 + 0.5) * spp;
                        let a = if buf.is_some_and(|b| tp > b) { 0.45 } else { 1.0 };
                        ctx.set_fill_style_str(&css(col, a));
                        ctx.fill_rect(x0, mid - half, x1 - x0, half * 2.0);
                    };
                    seg(x, split, t.played);
                    match hov_x {
                        Some(hx) if hx > split => {
                            let mixc: Rgba = std::array::from_fn(|k| t.unplayed[k] + (t.played[k] - t.unplayed[k]) * 0.35);
                            seg(split, hx, mixc);
                            seg(hx, x + bw, t.unplayed);
                        }
                        _ => seg(split, x + bw, t.unplayed),
                    }
                }
            } else {
                let pa_of = |t0: f64| {
                    let f = ((self.view.playhead_s - t0) / spp).clamp(0.0, 1.0) as f32;
                    1.0 + (PLAYED_ALPHA - 1.0) * f
                };
                for x in 0..(w as i32) {
                    let t0 = res.t_left_s + x as f64 * spp;
                    let t1 = t0 + spp;
                    if t1 <= 0.0 || t0 >= dur {
                        continue;
                    }
                    let c = p.sample_column_mean(t0, t1);
                    let pa = pa_of(t0);
                    let x = x as f64;
                    let nh = |l: f32, r: f32| norm_height(l, r).max(min_h as f32);
                    match self.view.style {
                        WaveStyle::Mono => {
                            let pk = nh(lin(c[0].max(c[1])), refs.peak);
                            let rm = nh(lin(c[2]), refs.rms * MONO_BODY_HEADROOM);
                            fill(x, 1.0, pk, rgb(rgb_of(t.wave), t.wave[3] * HALO_ALPHA * pa));
                            fill(x, 1.0, rm, rgb(rgb_of(t.core), t.core[3] * pa));
                        }
                        WaveStyle::RgbSpectral => {
                            let n = band_levels(lin(c[3]), lin(c[4]), lin(c[5]), &refs);
                            let h = nh(lin(c[2]), refs.rms);
                            fill(x, 1.0, h, rgb(spectral_color(n, t), pa));
                        }
                        _ => {
                            let mut layers = [
                                (norm_height(lin(c[3]), refs.low), t.layer_low, 1.0),
                                (norm_height(lin(c[4]), refs.mid), t.layer_mid, LAYER_ALPHA),
                                (norm_height(lin(c[5]), refs.high), t.layer_high, LAYER_ALPHA),
                            ];
                            layers.sort_by(|a, b| b.0.total_cmp(&a.0));
                            for (i, (hv, col, am)) in layers.iter().enumerate() {
                                let hv = if i == 0 { hv.max(min_h as f32) } else { *hv };
                                fill(x, 1.0, hv, rgb(rgb_of(*col), col[3] * am * pa));
                            }
                        }
                    }
                }
            }
        }
        let d = self.dpr;
        for r in rects {
            let wpx = ((r.x1 - r.x0) as f64 * d).max(1.0);
            ctx.set_fill_style_str(&css(r.color, 1.0));
            ctx.fill_rect(
                r.x0 as f64 * d,
                r.y0 as f64 * d,
                wpx,
                (r.y1 - r.y0) as f64 * d,
            );
        }
        ctx.restore();
    }
}
