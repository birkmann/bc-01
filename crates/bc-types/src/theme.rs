//! ThemeSpec v1 (compatible with the legacy JSON), built-in palettes, colour
//! math and CSS-variable generation. One Rust source feeds both the CSS
//! variables and the WebGL waveform colours (PLAN §10.1): the UI applies
//! [`css_vars`] inline on `<html>` and the renderer reads [`resolve_colors`].
//! wasm-safe: no I/O.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const COLOR_TOKENS: &[&str] = &[
    "surface-0", "surface-1", "surface-2", "surface-3", "surface-4", "line-subtle", "line", "line-strong",
    "ink", "ink-muted", "ink-faint", "ink-invert", "accent", "accent-hi", "accent-lo", "accent-ink",
    "accent-hover", "danger-hover", "danger-ink", "ok", "warn", "danger", "info", "series-1", "series-2",
    "series-3", "wave-played", "wave-unplayed",
];

/// Groups for the advanced editor, in display order.
pub const COLOR_GROUPS: &[(&str, &[&str])] = &[
    ("Surfaces", &["surface-0", "surface-1", "surface-2", "surface-3", "surface-4"]),
    ("Borders", &["line-subtle", "line", "line-strong"]),
    ("Text", &["ink", "ink-muted", "ink-faint", "ink-invert"]),
    ("Accent", &["accent", "accent-hi", "accent-lo", "accent-hover", "accent-ink"]),
    ("Status", &["ok", "warn", "danger", "danger-hover", "danger-ink", "info"]),
    ("Charts", &["series-1", "series-2", "series-3"]),
    ("Waveform", &["wave-played", "wave-unplayed"]),
];

pub type ColorMap = BTreeMap<String, String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Radius {
    None,
    Small,
    #[default]
    Default,
    Large,
    Round,
}
impl Radius {
    pub const ALL: [Radius; 5] = [Radius::None, Radius::Small, Radius::Default, Radius::Large, Radius::Round];
    pub fn scale(self) -> f64 {
        match self {
            Radius::None => 0.0,
            Radius::Small => 0.5,
            Radius::Default => 1.0,
            Radius::Large => 1.75,
            Radius::Round => 2.5,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Radius::None => "none",
            Radius::Small => "small",
            Radius::Default => "default",
            Radius::Large => "large",
            Radius::Round => "round",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    Compact,
    #[default]
    Default,
    Comfortable,
}
impl Density {
    pub const ALL: [Density; 3] = [Density::Compact, Density::Default, Density::Comfortable];
    pub fn label(self) -> &'static str {
        match self {
            Density::Compact => "compact",
            Density::Default => "default",
            Density::Comfortable => "comfortable",
        }
    }
    /// Fixed row height in px for virtualised lists.
    pub fn row_px(self) -> f64 {
        match self {
            Density::Compact => 28.0,
            Density::Default => 36.0,
            Density::Comfortable => 44.0,
        }
    }
}

pub const TYPE_SCALES: [f64; 4] = [0.9, 1.0, 1.1, 1.2];

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThemeSizes {
    #[serde(rename = "typeScale", default = "one")]
    pub type_scale: f64,
    #[serde(default)]
    pub radius: Radius,
    #[serde(default)]
    pub density: Density,
}
fn one() -> f64 {
    1.0
}
impl Default for ThemeSizes {
    fn default() -> Self {
        Self { type_scale: 1.0, radius: Radius::Default, density: Density::Default }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThemeFonts {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub sans: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub mono: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub display: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeSpec {
    pub schema: u8,
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub base: Mode,
    #[serde(default)]
    pub colors: ColorMap,
    #[serde(default)]
    pub fonts: ThemeFonts,
    #[serde(default)]
    pub sizes: ThemeSizes,
}

const DARK: &[(&str, &str)] = &[
    ("surface-0", "#080808"), ("surface-1", "#0c0c0c"), ("surface-2", "#141414"), ("surface-3", "#1a1a1a"),
    ("surface-4", "#252525"), ("line-subtle", "#1f1f1f"), ("line", "#2b2b2b"), ("line-strong", "#454545"),
    ("ink", "#f0f0f0"), ("ink-muted", "#a8a8a8"), ("ink-faint", "#7c7c7c"), ("ink-invert", "#0c0c0c"),
    ("accent", "#01ff95"), ("accent-hi", "#66ffbe"), ("accent-lo", "#00c974"), ("accent-ink", "#001a10"),
    ("accent-hover", "#4dffb1"), ("danger-hover", "#e03a3a"), ("danger-ink", "#ffffff"), ("ok", "#21c07a"),
    ("warn", "#c98500"), ("danger", "#ff4444"), ("info", "#3987e5"), ("series-1", "#3987e5"),
    ("series-2", "#d95926"), ("series-3", "#199e70"), ("wave-played", "#f0f0f0"), ("wave-unplayed", "#2ec7e6"),
];
const LIGHT: &[(&str, &str)] = &[
    ("surface-0", "#e6e6e6"), ("surface-1", "#ffffff"), ("surface-2", "#f6f6f6"), ("surface-3", "#fcfcfc"),
    ("surface-4", "#e4e4e4"), ("line-subtle", "#e8e8e8"), ("line", "#d5d5d5"), ("line-strong", "#a8a8a8"),
    ("ink", "#1a1a1a"), ("ink-muted", "#4f4f4f"), ("ink-faint", "#757575"), ("ink-invert", "#ffffff"),
    ("accent", "#007a4d"), ("accent-hi", "#009862"), ("accent-lo", "#005c3a"), ("accent-ink", "#ffffff"),
    ("accent-hover", "#005c3a"), ("danger-hover", "#b31f29"), ("danger-ink", "#ffffff"), ("ok", "#147a57"),
    ("warn", "#9a6700"), ("danger", "#d1242f"), ("info", "#0969da"), ("series-1", "#3987e5"),
    ("series-2", "#d95926"), ("series-3", "#199e70"), ("wave-played", "#1a1a1a"), ("wave-unplayed", "#0891b2"),
];

pub fn base_colors(mode: Mode) -> ColorMap {
    let src = if mode == Mode::Dark { DARK } else { LIGHT };
    src.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

pub const BUILTIN_PREFIX: &str = "builtin:";
pub fn is_builtin_id(id: &str) -> bool {
    id.starts_with(BUILTIN_PREFIX)
}

fn builtin(id: &str, name: &str, base: Mode, over: &[(&str, &str)]) -> ThemeSpec {
    ThemeSpec {
        schema: 1,
        id: format!("{BUILTIN_PREFIX}{id}"),
        name: name.into(),
        base,
        colors: over.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        fonts: ThemeFonts::default(),
        sizes: ThemeSizes::default(),
    }
}

pub fn builtin_themes() -> Vec<ThemeSpec> {
    vec![
        builtin("dark", "Dark", Mode::Dark, &[]),
        builtin("light", "Light", Mode::Light, &[]),
        builtin("oled", "OLED", Mode::Dark, &[
            ("surface-0", "#000000"), ("surface-1", "#000000"), ("surface-2", "#0a0a0a"), ("surface-3", "#101010"),
            ("surface-4", "#1c1c1c"), ("line-subtle", "#161616"), ("line", "#242424"), ("line-strong", "#3c3c3c"),
            ("wave-played", "#ffffff"), ("wave-unplayed", "#22d3ee"),
        ]),
        builtin("midnight", "Midnight", Mode::Dark, &[
            ("surface-0", "#070b14"), ("surface-1", "#0b111d"), ("surface-2", "#111a2b"), ("surface-3", "#162135"),
            ("surface-4", "#1f2d47"), ("line-subtle", "#1a2438"), ("line", "#253248"), ("line-strong", "#3b4d6b"),
            ("ink", "#e8eefb"), ("ink-muted", "#a3b1cc"), ("ink-faint", "#75849f"), ("ink-invert", "#0b111d"),
            ("accent", "#5ea1ff"), ("accent-hi", "#8dbcff"), ("accent-lo", "#3b7fe0"), ("accent-ink", "#04162e"),
            ("accent-hover", "#7fb3ff"), ("info", "#8dbcff"), ("series-1", "#5ea1ff"), ("series-2", "#ff9d5c"),
            ("series-3", "#3fd0a4"),
            ("wave-played", "#e8eefb"), ("wave-unplayed", "#38bdf8"),
        ]),
        builtin("paper", "Paper", Mode::Light, &[
            ("surface-0", "#e4ddd0"), ("surface-1", "#f7f2e9"), ("surface-2", "#efe8dc"), ("surface-3", "#faf6ef"),
            ("surface-4", "#e2d9c8"), ("line-subtle", "#e6dece"), ("line", "#d3c8b3"), ("line-strong", "#a99c84"),
            ("ink", "#2b2620"), ("ink-muted", "#5c5347"), ("ink-faint", "#7d7366"), ("ink-invert", "#f7f2e9"),
            ("accent", "#8a4b1f"), ("accent-hi", "#a3602e"), ("accent-lo", "#6b3814"), ("accent-ink", "#ffffff"),
            ("accent-hover", "#6b3814"), ("ok", "#3d7a3a"), ("warn", "#8f6400"), ("danger", "#b8322b"),
            ("danger-hover", "#962820"), ("info", "#2b63a3"),
            ("wave-played", "#2b2620"), ("wave-unplayed", "#0e7490"),
        ]),
        builtin("contrast", "High contrast", Mode::Dark, &[
            ("surface-0", "#000000"), ("surface-1", "#000000"), ("surface-2", "#0e0e0e"), ("surface-3", "#141414"),
            ("surface-4", "#2a2a2a"), ("line-subtle", "#3a3a3a"), ("line", "#5a5a5a"), ("line-strong", "#8a8a8a"),
            ("ink", "#ffffff"), ("ink-muted", "#d6d6d6"), ("ink-faint", "#b0b0b0"), ("ink-invert", "#000000"),
            ("accent", "#ffd60a"), ("accent-hi", "#ffe45c"), ("accent-lo", "#e6bd00"), ("accent-ink", "#1a1500"),
            ("accent-hover", "#ffe45c"), ("ok", "#3ddc84"), ("warn", "#ffb020"), ("danger", "#ff5c5c"),
            ("info", "#5eb1ff"),
            ("wave-played", "#ffffff"), ("wave-unplayed", "#00e5ff"),
        ]),
    ]
}

pub fn find_builtin(id: &str) -> Option<ThemeSpec> {
    builtin_themes().into_iter().find(|t| t.id == id)
}

/// Full palette: base defaults with the spec's overrides applied.
pub fn resolve_colors(spec: &ThemeSpec) -> ColorMap {
    let mut m = base_colors(spec.base);
    for (k, v) in &spec.colors {
        m.insert(k.clone(), v.clone());
    }
    m
}

// ---- colour math ------------------------------------------------------------

pub type Rgb = [f64; 3];

/// Normalise `#abc` / `abc` / `#aabbcc` to lower-case `#aabbcc`.
pub fn normalize_hex(input: &str) -> Option<String> {
    let s = input.trim().trim_start_matches('#');
    if !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match s.len() {
        6 => Some(format!("#{}", s.to_ascii_lowercase())),
        3 => Some(format!("#{}", s.chars().flat_map(|c| [c, c]).collect::<String>().to_ascii_lowercase())),
        _ => None,
    }
}

pub fn hex_to_rgb(hex: &str) -> Rgb {
    let n = normalize_hex(hex).unwrap_or_else(|| "#000000".into());
    let p = |i: usize| u8::from_str_radix(&n[i..i + 2], 16).unwrap_or(0) as f64;
    [p(1), p(3), p(5)]
}

pub fn rgb_to_hex(c: Rgb) -> String {
    let h = |v: f64| v.round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", h(c[0]), h(c[1]), h(c[2]))
}

fn to_linear(v: f64) -> f64 {
    let c = v / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}
fn to_srgb(v: f64) -> f64 {
    let c = if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 };
    c * 255.0
}

pub fn luminance(hex: &str) -> f64 {
    let [r, g, b] = hex_to_rgb(hex);
    0.2126 * to_linear(r) + 0.7152 * to_linear(g) + 0.0722 * to_linear(b)
}

pub fn contrast(a: &str, b: &str) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

pub fn is_light(hex: &str) -> bool {
    luminance(hex) > 0.4
}

fn to_oklab(c: Rgb) -> [f64; 3] {
    let (lr, lg, lb) = (to_linear(c[0]), to_linear(c[1]), to_linear(c[2]));
    let l = (0.4122214708 * lr + 0.5363325363 * lg + 0.0514459929 * lb).cbrt();
    let m = (0.2119034982 * lr + 0.6806995451 * lg + 0.1073969566 * lb).cbrt();
    let s = (0.0883024619 * lr + 0.2817188376 * lg + 0.6299787005 * lb).cbrt();
    [
        0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
    ]
}
fn from_oklab(c: [f64; 3]) -> Rgb {
    let l = (c[0] + 0.3963377774 * c[1] + 0.2158037573 * c[2]).powi(3);
    let m = (c[0] - 0.1055613458 * c[1] - 0.0638541728 * c[2]).powi(3);
    let s = (c[0] - 0.0894841775 * c[1] - 1.291485548 * c[2]).powi(3);
    [
        to_srgb(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
        to_srgb(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
        to_srgb(-0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s),
    ]
}

/// Perceptual OKLab mix: t=0 -> a, t=1 -> b.
pub fn mix(a: &str, b: &str, t: f64) -> String {
    let (la, lb) = (to_oklab(hex_to_rgb(a)), to_oklab(hex_to_rgb(b)));
    let k = t.clamp(0.0, 1.0);
    rgb_to_hex(from_oklab([la[0] + (lb[0] - la[0]) * k, la[1] + (lb[1] - la[1]) * k, la[2] + (lb[2] - la[2]) * k]))
}

pub fn shift_lightness(hex: &str, delta: f64) -> String {
    let mut lab = to_oklab(hex_to_rgb(hex));
    lab[0] = (lab[0] + delta).clamp(0.0, 1.0);
    rgb_to_hex(from_oklab(lab))
}

pub fn ensure_contrast(fg: &str, bg: &str, min: f64, towards: &str) -> String {
    let mut out = fg.to_string();
    for i in 1..=20 {
        if contrast(&out, bg) >= min {
            break;
        }
        out = mix(fg, towards, i as f64 / 20.0);
    }
    out
}

pub fn ink_for(fill: &str, dark: &str, light: &str) -> String {
    if contrast(fill, dark) >= contrast(fill, light) { dark.into() } else { light.into() }
}

// ---- quick derivation ---------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct QuickColors {
    pub bg: String,
    pub text: String,
    pub accent: String,
}

pub fn quick_from(c: &ColorMap) -> QuickColors {
    let g = |k: &str| c.get(k).cloned().unwrap_or_default();
    QuickColors { bg: g("surface-1"), text: g("ink"), accent: g("accent") }
}

pub fn derive_quick(q: &QuickColors, base: Mode) -> ColorMap {
    let reff = base_colors(base);
    let bg = normalize_hex(&q.bg).unwrap_or_else(|| reff["surface-1"].clone());
    let text = normalize_hex(&q.text).unwrap_or_else(|| reff["ink"].clone());
    let accent = normalize_hex(&q.accent).unwrap_or_else(|| reff["accent"].clone());
    let dark = !is_light(&bg);
    let ink = ensure_contrast(&text, &bg, 7.0, if dark { "#ffffff" } else { "#000000" });
    let step = |t: f64| mix(&bg, &ink, t);
    let mut m = ColorMap::new();
    let mut put = |k: &str, v: String| {
        m.insert(k.to_string(), v);
    };
    put("surface-0", if dark { shift_lightness(&bg, -0.02) } else { shift_lightness(&bg, -0.06) });
    put("surface-1", bg.clone());
    put("surface-2", step(0.035));
    put("surface-3", step(if dark { 0.06 } else { 0.015 }));
    put("surface-4", step(0.1));
    put("line-subtle", step(0.08));
    put("line", step(0.13));
    put("line-strong", step(0.25));
    put("ink-muted", ensure_contrast(&step(0.68), &bg, 7.0, &ink));
    put("ink-faint", ensure_contrast(&step(0.48), &bg, 4.5, &ink));
    put("ink", ink);
    put("ink-invert", bg);
    put("accent-hi", shift_lightness(&accent, 0.12));
    put("accent-lo", shift_lightness(&accent, -0.12));
    put("accent-hover", if dark { shift_lightness(&accent, 0.1) } else { shift_lightness(&accent, -0.1) });
    put("accent-ink", ink_for(&accent, "#0c0c0c", "#ffffff"));
    put("danger-ink", ink_for(&reff["danger"], "#0c0c0c", "#ffffff"));
    put("accent", accent);
    m
}

// ---- fonts ----------------------------------------------------------------------

/// Sanitise a user font-family list for injection as a custom property value.
pub fn sanitize_font_stack(input: &str) -> Option<String> {
    let raw = input.trim();
    if raw.is_empty() || raw.len() > 200 {
        return None;
    }
    if raw == "var(--font-sans)" {
        return Some(raw.into());
    }
    let lower = raw.to_ascii_lowercase();
    if raw.contains([';', '{', '}', '\\', '/']) || lower.contains("url(") {
        return None;
    }
    let mut fams = vec![];
    for part in raw.split(',') {
        let name = part.trim().trim_matches(|c| c == '\'' || c == '"').trim();
        if name.is_empty() {
            continue;
        }
        if name.len() > 64 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '_' || c == '-') {
            return None;
        }
        fams.push(if name.contains(' ') { format!("\"{name}\"") } else { name.to_string() });
    }
    if fams.is_empty() { None } else { Some(fams.join(", ")) }
}

pub struct FontPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub stack: &'static str,
}
pub const SANS_PRESETS: &[FontPreset] = &[
    FontPreset { id: "inter", label: "Inter (default)", stack: "'Inter Variable', Inter, ui-sans-serif, system-ui, sans-serif" },
    FontPreset { id: "system", label: "System UI", stack: "system-ui, -apple-system, \"Segoe UI\", Roboto, sans-serif" },
    FontPreset { id: "humanist", label: "Humanist", stack: "Seravek, \"Gill Sans Nova\", Ubuntu, Calibri, \"DejaVu Sans\", sans-serif" },
    FontPreset { id: "grotesk", label: "Grotesque", stack: "\"Helvetica Neue\", Helvetica, Arial, sans-serif" },
    FontPreset { id: "serif", label: "Serif", stack: "Charter, \"Iowan Old Style\", Georgia, \"Times New Roman\", serif" },
    FontPreset { id: "mono", label: "Monospace", stack: "'JetBrains Mono Variable', ui-monospace, monospace" },
];
pub const MONO_PRESETS: &[FontPreset] = &[
    FontPreset { id: "jetbrains", label: "JetBrains Mono (default)", stack: "'JetBrains Mono Variable', 'JetBrains Mono', ui-monospace, monospace" },
    FontPreset { id: "system", label: "System monospace", stack: "ui-monospace, \"SF Mono\", Menlo, Consolas, \"Liberation Mono\", monospace" },
    FontPreset { id: "courier", label: "Courier", stack: "\"Courier New\", Courier, monospace" },
];
pub const DISPLAY_PRESETS: &[FontPreset] = &[
    FontPreset { id: "body", label: "Same as body (default)", stack: "var(--font-sans)" },
    FontPreset { id: "serif", label: "Serif", stack: "Charter, \"Iowan Old Style\", Georgia, serif" },
    FontPreset { id: "mono", label: "Monospace", stack: "'JetBrains Mono Variable', ui-monospace, monospace" },
    FontPreset { id: "wide", label: "Wide", stack: "\"Avenir Next\", \"Futura\", \"Century Gothic\", \"Trebuchet MS\", sans-serif" },
];

// ---- validation / serialisation -------------------------------------------------

const NAME_MAX: usize = 40;
const JSON_MAX: usize = 20_000;

pub fn new_theme_id(entropy: u64) -> String {
    format!("user:{:08x}", (entropy ^ (entropy >> 29)) as u32)
}

/// Validate an untrusted JSON value into a ThemeSpec. Unknown keys dropped,
/// colours must be hex, fonts sanitised. `keep_id` preserves `id`; imports mint a new one.
pub fn parse_theme_value(v: &serde_json::Value, keep_id: bool, entropy: u64) -> Result<ThemeSpec, String> {
    let obj = v.as_object().ok_or("Theme must be a JSON object.")?;
    if obj.get("schema").and_then(|s| s.as_u64()) != Some(1) {
        return Err("Unsupported theme schema (expected 1).".into());
    }
    let name: String = obj.get("name").and_then(|n| n.as_str()).unwrap_or("").trim().chars().take(NAME_MAX).collect();
    if name.is_empty() {
        return Err("Theme needs a name.".into());
    }
    let base = match obj.get("base").and_then(|b| b.as_str()) {
        Some("dark") => Mode::Dark,
        Some("light") => Mode::Light,
        _ => return Err("base must be \"dark\" or \"light\".".into()),
    };
    let mut colors = ColorMap::new();
    if let Some(c) = obj.get("colors") {
        let c = c.as_object().ok_or("colors must be an object.")?;
        for (k, val) in c {
            if !COLOR_TOKENS.contains(&k.as_str()) {
                continue;
            }
            let s = val.as_str().ok_or_else(|| format!("Color \"{k}\" must be a hex string."))?;
            let hex = normalize_hex(s).ok_or_else(|| format!("Color \"{k}\" is not a valid hex color."))?;
            colors.insert(k.clone(), hex);
        }
    }
    let mut fonts = ThemeFonts::default();
    if let Some(f) = obj.get("fonts") {
        let f = f.as_object().ok_or("fonts must be an object.")?;
        for slot in ["sans", "mono", "display"] {
            match f.get(slot) {
                None | Some(serde_json::Value::Null) => {}
                Some(serde_json::Value::String(s)) if s.is_empty() => {}
                Some(serde_json::Value::String(s)) => {
                    let clean = sanitize_font_stack(s).ok_or_else(|| format!("Font \"{slot}\" is not a valid font-family list."))?;
                    match slot {
                        "sans" => fonts.sans = Some(clean),
                        "mono" => fonts.mono = Some(clean),
                        _ => fonts.display = Some(clean),
                    }
                }
                _ => return Err(format!("Font \"{slot}\" must be a string.")),
            }
        }
    }
    let mut sizes = ThemeSizes::default();
    if let Some(s) = obj.get("sizes") {
        let s = s.as_object().ok_or("sizes must be an object.")?;
        if let Some(ts) = s.get("typeScale").and_then(|x| x.as_f64()) {
            sizes.type_scale = *TYPE_SCALES.iter().min_by(|a, b| ((**a - ts).abs()).total_cmp(&(**b - ts).abs())).unwrap_or(&1.0);
        }
        if let Some(r) = s.get("radius").and_then(|x| serde_json::from_value::<Radius>(x.clone()).ok()) {
            sizes.radius = r;
        }
        if let Some(d) = s.get("density").and_then(|x| serde_json::from_value::<Density>(x.clone()).ok()) {
            sizes.density = d;
        }
    }
    let id = match obj.get("id").and_then(|i| i.as_str()) {
        Some(i) if keep_id && !i.is_empty() => i.to_string(),
        _ => new_theme_id(entropy),
    };
    Ok(ThemeSpec { schema: 1, id, name, base, colors, fonts, sizes })
}

pub fn parse_theme_json(json: &str, entropy: u64) -> Result<ThemeSpec, String> {
    if json.len() > JSON_MAX {
        return Err("Theme file is too large.".into());
    }
    let v: serde_json::Value = serde_json::from_str(json).map_err(|_| "Not valid JSON.".to_string())?;
    parse_theme_value(&v, false, entropy)
}

/// Shareable JSON; the id is local bookkeeping and omitted.
pub fn serialize_theme(spec: &ThemeSpec) -> String {
    let mut v = serde_json::to_value(spec).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.remove("id");
    }
    serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
}

pub fn theme_slug(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    let t = out.trim_end_matches('-');
    if t.is_empty() { "theme".into() } else { t.chars().take(40).collect() }
}

/// Custom properties to write inline on `<html>`: only the overrides plus the
/// scale factors. Empty values mean "remove". Returns (name, value) pairs.
pub fn css_vars(spec: &ThemeSpec) -> Vec<(String, Option<String>)> {
    let mut out = vec![];
    for t in COLOR_TOKENS {
        out.push((format!("--color-{t}"), spec.colors.get(*t).cloned()));
    }
    out.push(("--font-sans".into(), spec.fonts.sans.clone()));
    out.push(("--font-mono".into(), spec.fonts.mono.clone()));
    out.push(("--font-display".into(), spec.fonts.display.clone()));
    out.push(("--type-scale".into(), (spec.sizes.type_scale != 1.0).then(|| spec.sizes.type_scale.to_string())));
    out.push(("--radius-scale".into(), (spec.sizes.radius != Radius::Default).then(|| spec.sizes.radius.scale().to_string())));
    let surface = resolve_colors(spec).get("surface-1").cloned().unwrap_or_default();
    let scheme = if is_light(&surface) { "light" } else { "dark" };
    let base = if spec.base == Mode::Light { "light" } else { "dark" };
    out.push(("color-scheme".into(), (scheme != base).then(|| scheme.to_string())));
    out
}

/// Contrast of a token against the surface it usually sits on (for badges).
pub fn contrast_against_surface(token: &str, c: &ColorMap) -> Option<f64> {
    let bg = match token {
        "ink" | "ink-muted" | "ink-faint" | "accent" | "accent-hi" | "accent-lo" | "ok" | "warn" | "danger" | "info" => "surface-1",
        "accent-ink" => "accent",
        "danger-ink" => "danger",
        _ => return None,
    };
    Some(contrast(c.get(token)?, c.get(bg)?))
}

/// The user theme library persisted in `ui_state['themes']`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThemeStore {
    #[serde(default)]
    pub dark: String,
    #[serde(default)]
    pub light: String,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub themes: Vec<ThemeSpec>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_normalise() {
        assert_eq!(normalize_hex("ABC").as_deref(), Some("#aabbcc"));
        assert_eq!(normalize_hex("#AABBCC").as_deref(), Some("#aabbcc"));
        assert_eq!(normalize_hex("#12"), None);
        assert_eq!(normalize_hex("zzzzzz"), None);
    }

    #[test]
    fn wcag_contrast_extremes() {
        assert!((contrast("#000000", "#ffffff") - 21.0).abs() < 0.01);
        assert!((contrast("#777777", "#777777") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn builtins_keep_text_readable() {
        for t in builtin_themes() {
            let c = resolve_colors(&t);
            assert!(contrast(&c["ink"], &c["surface-1"]) >= 7.0, "{}", t.name);
            assert!(contrast(&c["ink-muted"], &c["surface-1"]) >= 4.5, "{}", t.name);
            assert!(contrast(&c["accent"], &c["surface-1"]) >= 3.0, "{}", t.name);
            assert_eq!(c.len(), COLOR_TOKENS.len());
        }
        assert_eq!(builtin_themes().len(), 6);
    }

    #[test]
    fn wave_tokens_default_and_stay_visible() {
        let t = parse_theme_json(r#"{"schema":1,"name":"Old","base":"dark"}"#, 1).unwrap();
        let c = resolve_colors(&t);
        assert_eq!(c["wave-played"], "#f0f0f0");
        assert_eq!(c["wave-unplayed"], "#2ec7e6");
        assert!(COLOR_GROUPS.iter().any(|(g, ts)| *g == "Waveform" && ts.contains(&"wave-played")));
        for t in builtin_themes() {
            let c = resolve_colors(&t);
            assert!(contrast(&c["wave-played"], &c["surface-1"]) >= 3.0, "{} played", t.name);
            assert!(contrast(&c["wave-unplayed"], &c["surface-1"]) >= 3.0, "{} unplayed", t.name);
        }
    }

    #[test]
    fn legacy_json_roundtrip() {
        let json = r##"{"schema":1,"name":"Mine","base":"dark","colors":{"accent":"#FF0000","bogus":"#fff"},"fonts":{"sans":"Helvetica Neue, Arial"},"sizes":{"typeScale":1.12,"radius":"large","density":"compact"}}"##;
        let t = parse_theme_json(json, 7).unwrap();
        assert_eq!(t.colors.get("accent").map(String::as_str), Some("#ff0000"));
        assert!(!t.colors.contains_key("bogus"));
        assert_eq!(t.sizes.type_scale, 1.1);
        assert_eq!(t.sizes.radius, Radius::Large);
        assert!(t.id.starts_with("user:"));
        let out = serialize_theme(&t);
        assert!(!out.contains("\"id\""));
        let back = parse_theme_json(&out, 7).unwrap();
        assert_eq!(back.colors, t.colors);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse_theme_json("{", 1).is_err());
        assert!(parse_theme_json(r#"{"schema":2,"name":"x","base":"dark"}"#, 1).is_err());
        assert!(parse_theme_json(r##"{"schema":1,"name":"x","base":"dark","colors":{"ink":"red"}}"##, 1).is_err());
        assert!(parse_theme_json(r#"{"schema":1,"name":"x","base":"dark","fonts":{"sans":"a;b"}}"#, 1).is_err());
    }

    #[test]
    fn font_sanitising() {
        assert_eq!(sanitize_font_stack("'Fira Code', monospace").as_deref(), Some("\"Fira Code\", monospace"));
        assert!(sanitize_font_stack("x} body{").is_none());
        assert!(sanitize_font_stack("url(x)").is_none());
    }

    #[test]
    fn quick_derivation_is_readable() {
        let q = QuickColors { bg: "#101820".into(), text: "#cccccc".into(), accent: "#ff8800".into() };
        let c = derive_quick(&q, Mode::Dark);
        assert!(contrast(&c["ink"], &c["surface-1"]) >= 7.0);
        assert!(contrast(&c["ink-faint"], &c["surface-1"]) >= 4.4);
    }

    #[test]
    fn css_vars_only_overrides() {
        let t = find_builtin("builtin:oled").unwrap();
        let v = css_vars(&t);
        let accent = v.iter().find(|(k, _)| k == "--color-accent").unwrap();
        assert!(accent.1.is_none());
        let s0 = v.iter().find(|(k, _)| k == "--color-surface-0").unwrap();
        assert_eq!(s0.1.as_deref(), Some("#000000"));
    }
}
