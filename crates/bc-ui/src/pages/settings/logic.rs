//! Pure settings logic: contrast grading, font choice matching, unit conversion,
//! blacklist input parsing, move-plan warning split. Native-testable.
use bc_types::library::BlacklistAdd;
use bc_types::theme::FontPreset;

pub const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// WCAG grade of a contrast ratio. Status is never colour alone: the label is always shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Grade {
    Aaa,
    Aa,
    AaLarge,
    Fail,
}

impl Grade {
    pub fn of(ratio: f64) -> Grade {
        if ratio >= 7.0 {
            Grade::Aaa
        } else if ratio >= 4.5 {
            Grade::Aa
        } else if ratio >= 3.0 {
            Grade::AaLarge
        } else {
            Grade::Fail
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Grade::Aaa => "AAA",
            Grade::Aa => "AA",
            Grade::AaLarge => "AA large",
            Grade::Fail => "Fail",
        }
    }
    pub fn hint(self) -> &'static str {
        match self {
            Grade::Aaa => "Passes WCAG AAA for body text (7:1)",
            Grade::Aa => "Passes WCAG AA for body text (4.5:1)",
            Grade::AaLarge => "Only passes for large text or icons (3:1)",
            Grade::Fail => "Below 3:1: hard to read",
        }
    }
}

/// What a font slot's select shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FontChoice {
    Preset(String),
    Custom,
}

/// `value` is the theme's stack for the slot (`None` = default = first preset).
pub fn font_choice(presets: &[FontPreset], value: Option<&str>) -> FontChoice {
    match value {
        None => FontChoice::Preset(presets.first().map(|p| p.id.to_string()).unwrap_or_default()),
        Some(v) => match presets.iter().find(|p| p.stack == v) {
            Some(p) => FontChoice::Preset(p.id.to_string()),
            None => FontChoice::Custom,
        },
    }
}

/// The stack to store when a preset is picked (`None` for the default = remove the override).
pub fn stack_for_preset(presets: &[FontPreset], id: &str) -> Option<String> {
    let first = presets.first()?;
    if first.id == id {
        return None;
    }
    presets.iter().find(|p| p.id == id).map(|p| p.stack.to_string())
}

pub fn gb_to_bytes(gb: f64) -> i64 {
    (gb * GB).round() as i64
}

pub fn bytes_to_gb_text(bytes: i64) -> String {
    let gb = (bytes as f64 / GB * 10.0).round() / 10.0;
    if gb.fract() == 0.0 { format!("{}", gb as i64) } else { format!("{gb}") }
}

/// Parse the GB field (accepts a decimal comma). `None` when not a non-negative number.
pub fn parse_gb(text: &str) -> Option<f64> {
    let v: f64 = text.trim().replace(',', ".").parse().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// One field, two shapes: a Bandcamp URL, or "Artist - Title" for records without one.
pub fn parse_blacklist_input(raw: &str) -> Option<BlacklistAdd> {
    let value = raw.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with("http") {
        return Some(BlacklistAdd { url: Some(value.to_string()), ..Default::default() });
    }
    let seps = [" \u{2014} ", " \u{2013} ", " - "];
    let mut best: Option<(usize, &str)> = None;
    for s in seps {
        if let Some(i) = value.find(s) {
            if best.map(|(b, _)| i < b).unwrap_or(true) {
                best = Some((i, s));
            }
        }
    }
    match best {
        Some((i, s)) => Some(BlacklistAdd { artist_name: value[..i].trim().to_string(), title: value[i + s.len()..].trim().to_string(), ..Default::default() }),
        None => Some(BlacklistAdd { title: value.to_string(), ..Default::default() }),
    }
}

/// Move-plan warnings: `BLOCK: ...` entries stop the move, the rest are notes.
pub fn split_warnings(warnings: &[String]) -> (Vec<String>, Vec<String>) {
    let mut blockers = vec![];
    let mut notes = vec![];
    for w in warnings {
        match w.strip_prefix("BLOCK:") {
            Some(rest) => blockers.push(rest.trim().to_string()),
            None => notes.push(w.clone()),
        }
    }
    (blockers, notes)
}

pub fn lufs_label(v: f64) -> String {
    format!("{v:.0} LUFS")
}

/// A deterministic fake waveform for the style preview (no randomness: stable screenshots).
pub fn preview_bars(n: usize) -> Vec<(f64, f64, f64)> {
    (0..n)
        .map(|i| {
            let t = i as f64 / n as f64;
            let env = 0.35 + 0.55 * (t * 9.0).sin().abs() * (0.6 + 0.4 * (t * 31.0).cos().abs());
            let low = (env * (0.9 - 0.3 * (t * 5.0).sin().abs())).clamp(0.05, 1.0);
            let mid = (env * 0.7 * (0.6 + 0.4 * (t * 17.0).sin().abs())).clamp(0.05, 1.0);
            let high = (env * 0.45 * (0.5 + 0.5 * (t * 43.0).cos().abs())).clamp(0.03, 1.0);
            (low, mid, high)
        })
        .collect()
}

/// Bar heights (0..=1) for the "Bars" preview: a built-in demo envelope with an intro, a drop,
/// a breakdown and a second drop, run through the same `compute_bars` the player uses.
pub fn demo_bar_heights(n_bars: usize) -> Vec<f32> {
    use bc_waveform::bars::{compute_bars, reference_levels};
    use bc_waveform::format::Levels;
    use bc_waveform::scale::lin_to_u8;
    const N: usize = 512;
    let mut l = Levels::zeros(N);
    for i in 0..N {
        let t = i as f32 / N as f32;
        let section = if t < 0.12 { 0.25 + 1.5 * t } else if t < 0.42 { 0.85 } else if t < 0.58 { 0.22 } else if t < 0.62 { 0.22 + 16.0 * (t - 0.58) } else if t < 0.92 { 0.88 } else { 0.88 - 8.0 * (t - 0.92) };
        let wob = 0.9 + 0.1 * (t * 220.0).sin().abs();
        let rms = (section * 0.38 * wob).clamp(0.01, 1.0);
        let peak = (rms * 1.8).min(1.0);
        l.planes[0][i] = lin_to_u8(peak);
        l.planes[1][i] = lin_to_u8(peak);
        l.planes[2][i] = lin_to_u8(rms);
        l.planes[3][i] = lin_to_u8(rms * 0.8);
        l.planes[4][i] = lin_to_u8(rms * 0.5);
        l.planes[5][i] = lin_to_u8(rms * 0.3);
    }
    let refs = reference_levels(&l);
    compute_bars(&l, &refs, 0.0, 1.0, n_bars).into_iter().map(|b| b.h).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_types::theme::{DISPLAY_PRESETS, SANS_PRESETS};

    #[test]
    fn grades() {
        assert_eq!(Grade::of(21.0), Grade::Aaa);
        assert_eq!(Grade::of(7.0), Grade::Aaa);
        assert_eq!(Grade::of(4.6), Grade::Aa);
        assert_eq!(Grade::of(3.2), Grade::AaLarge);
        assert_eq!(Grade::of(2.9), Grade::Fail);
        assert_eq!(Grade::Fail.label(), "Fail");
    }

    #[test]
    fn font_choices() {
        assert_eq!(font_choice(SANS_PRESETS, None), FontChoice::Preset("inter".into()));
        assert_eq!(font_choice(SANS_PRESETS, Some(SANS_PRESETS[2].stack)), FontChoice::Preset("humanist".into()));
        assert_eq!(font_choice(SANS_PRESETS, Some("Comic Sans")), FontChoice::Custom);
        assert_eq!(stack_for_preset(DISPLAY_PRESETS, "body"), None);
        assert_eq!(stack_for_preset(DISPLAY_PRESETS, "serif").as_deref(), Some(DISPLAY_PRESETS[1].stack));
    }

    #[test]
    fn gb_conversion() {
        assert_eq!(gb_to_bytes(1.5), 1_610_612_736);
        assert_eq!(bytes_to_gb_text(5 * 1024 * 1024 * 1024), "5");
        assert_eq!(bytes_to_gb_text(1_610_612_736), "1.5");
        assert_eq!(parse_gb("2,5"), Some(2.5));
        assert_eq!(parse_gb("-1"), None);
        assert_eq!(parse_gb("abc"), None);
    }

    #[test]
    fn blacklist_inputs() {
        let u = parse_blacklist_input(" https://x.bandcamp.com/album/y ").unwrap();
        assert_eq!(u.url.as_deref(), Some("https://x.bandcamp.com/album/y"));
        let a = parse_blacklist_input("Some Artist \u{2014} Some - Title").unwrap();
        assert_eq!((a.artist_name.as_str(), a.title.as_str()), ("Some Artist", "Some - Title"));
        let b = parse_blacklist_input("A - B").unwrap();
        assert_eq!((b.artist_name.as_str(), b.title.as_str()), ("A", "B"));
        assert_eq!(parse_blacklist_input("   "), None);
    }

    #[test]
    fn warnings_split() {
        let (b, n) = split_warnings(&["BLOCK: target inside source".into(), "slow drive".into()]);
        assert_eq!(b, vec!["target inside source"]);
        assert_eq!(n, vec!["slow drive"]);
    }

    #[test]
    fn demo_bars_show_a_breakdown() {
        let h = demo_bar_heights(60);
        assert_eq!(h.len(), 60);
        let drop = h[20..24].iter().sum::<f32>() / 4.0;
        let brk = h[30..33].iter().sum::<f32>() / 3.0;
        assert!(brk <= 0.4 * drop, "break {brk} drop {drop}");
    }

    #[test]
    fn bars_stable() {
        let a = preview_bars(8);
        assert_eq!(a.len(), 8);
        assert_eq!(a, preview_bars(8));
        assert!(a.iter().all(|(l, m, h)| (0.0..=1.0).contains(l) && (0.0..=1.0).contains(m) && (0.0..=1.0).contains(h)));
    }
}
