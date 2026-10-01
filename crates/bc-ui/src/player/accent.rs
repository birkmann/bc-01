//! Optional album-art accent (PLAN §10.1): the accent colour follows the dominant colour of
//! the now-playing artwork. Off by default (UiPrefs.album_art_accent). The colour is made
//! readable on the active surface with the same contrast helpers as the theme editor.
use bc_types::theme::{contrast, ensure_contrast, ink_for, resolve_colors, rgb_to_hex, shift_lightness, is_light};
use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::prefs::use_prefs;
use crate::theme::use_theme;
use crate::util::{document, document_element};

use super::store::use_player;

/// Saturation-weighted mean colour of RGBA bytes (alpha ignored): vivid pixels dominate, so a mostly
/// grey sleeve with a red logo yields red. Returns None for an all-grey or empty image.
pub fn dominant_colour(rgba: &[u8]) -> Option<[f64; 3]> {
    let (mut r, mut g, mut b, mut wsum) = (0.0, 0.0, 0.0, 0.0);
    for px in rgba.chunks_exact(4) {
        let (pr, pg, pb) = (px[0] as f64, px[1] as f64, px[2] as f64);
        let (mx, mn) = (pr.max(pg).max(pb), pr.min(pg).min(pb));
        let sat = if mx == 0.0 { 0.0 } else { (mx - mn) / mx };
        let w = sat * sat * (mx / 255.0 + 0.1);
        r += pr * w;
        g += pg * w;
        b += pb * w;
        wsum += w;
    }
    (wsum > 0.5).then(|| [r / wsum, g / wsum, b / wsum])
}

/// Accent token set derived from an art colour for a given surface.
pub fn accent_tokens(art: [f64; 3], surface: &str) -> Vec<(&'static str, String)> {
    let base = rgb_to_hex(art);
    let toward = if is_light(surface) { "#000000" } else { "#ffffff" };
    let accent = ensure_contrast(&base, surface, 4.5, toward);
    let hover = if is_light(surface) { shift_lightness(&accent, -0.08) } else { shift_lightness(&accent, 0.08) };
    vec![
        ("--color-accent", accent.clone()),
        ("--color-accent-hi", shift_lightness(&accent, 0.1)),
        ("--color-accent-lo", shift_lightness(&accent, -0.1)),
        ("--color-accent-hover", hover),
        ("--color-accent-ink", ink_for(&accent, "#0c0c0c", "#ffffff")),
    ]
}

pub fn install() {
    let prefs = use_prefs();
    let theme = use_theme();
    let player = use_player();
    let art = Signal::derive(move || player.state.with(|s| s.current.as_ref().and_then(|c| c.art_url.clone())));
    Effect::new(move |_| {
        let on = prefs.prefs.with(|p| p.album_art_accent);
        let spec = theme.active();
        let url = art.get();
        let root = document_element();
        let style = root.style();
        let clear = |style: &web_sys::CssStyleDeclaration| {
            for k in ["--color-accent", "--color-accent-hi", "--color-accent-lo", "--color-accent-hover", "--color-accent-ink"] {
                // only remove what we set: theme overrides are re-applied by `theme::apply` on change
                let _ = style.remove_property(k);
            }
        };
        if !on || url.is_none() {
            // restore the theme's own accent overrides
            crate::theme::apply(&spec);
            return;
        }
        let surface = resolve_colors(&spec).get("surface-1").cloned().unwrap_or_else(|| "#0c0c0c".into());
        let _ = contrast(&surface, &surface);
        let Ok(img) = web_sys::HtmlImageElement::new() else { return };
        let img2 = img.clone();
        let theme_ctx = theme;
        let onload = Closure::<dyn FnMut()>::new(move || {
            let doc = document();
            let Ok(canvas) = doc.create_element("canvas").and_then(|c| c.dyn_into::<web_sys::HtmlCanvasElement>().map_err(Into::into)) else { return };
            canvas.set_width(24);
            canvas.set_height(24);
            let Some(ctx2d) = canvas.get_context("2d").ok().flatten().and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok()) else { return };
            if ctx2d.draw_image_with_html_image_element_and_dw_and_dh(&img2, 0.0, 0.0, 24.0, 24.0).is_err() {
                return;
            }
            let Ok(data) = ctx2d.get_image_data(0.0, 0.0, 24.0, 24.0) else { return };
            let bytes = data.data().0;
            if let Some(col) = dominant_colour(&bytes) {
                let style = document_element().style();
                let spec = theme_ctx.active_untracked();
                let surface = resolve_colors(&spec).get("surface-1").cloned().unwrap_or_else(|| "#0c0c0c".into());
                for (k, v) in accent_tokens(col, &surface) {
                    let _ = style.set_property(k, &v);
                }
            }
        });
        img.set_cross_origin(Some("anonymous"));
        img.set_onload(Some(onload.as_ref().unchecked_ref()));
        onload.forget();
        clear(&style);
        if let Some(u) = url {
            img.set_src(&u);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(r: u8, g: u8, b: u8, n: usize) -> Vec<u8> {
        (0..n).flat_map(|_| [r, g, b, 255]).collect()
    }

    #[test]
    fn vivid_pixels_dominate_grey_sleeves() {
        let mut img = px(120, 120, 120, 400);
        img.extend(px(220, 30, 40, 60));
        let c = dominant_colour(&img).unwrap();
        assert!(c[0] > 180.0 && c[1] < 80.0, "{c:?}");
    }

    #[test]
    fn all_grey_has_no_accent() {
        assert!(dominant_colour(&px(100, 100, 100, 100)).is_none());
        assert!(dominant_colour(&[]).is_none());
    }

    #[test]
    fn accent_is_readable_on_the_surface() {
        for surface in ["#0c0c0c", "#ffffff", "#f7f2e9"] {
            for art in [[200.0, 20.0, 30.0], [20.0, 40.0, 220.0], [250.0, 250.0, 40.0]] {
                let t = accent_tokens(art, surface);
                let accent = &t[0].1;
                assert!(contrast(accent, surface) >= 4.4, "{surface} {accent}");
            }
        }
    }
}
