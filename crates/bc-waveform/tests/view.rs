#![allow(clippy::needless_range_loop)]
//! Pure render-math tests: view resolution, marker geometry, texture layout and packing.

use bc_music::beatgrid::GridExt;
use bc_types::analysis::{BeatGrid, CueKind, CuePoint};
use bc_waveform::builder::build_from_mono;
use bc_waveform::mip::Pyramid;
use bc_waveform::view::*;

fn grid(bpm: f64) -> BeatGrid {
    let mut g = BeatGrid::constant(bpm, 0.0, 0.9, "bc-rs-1");
    g.downbeat_phase = Some(0);
    g
}

#[test]
fn overview_fits_width_and_round_trips() {
    let v = ViewState::default();
    let r = resolve_view(&v, 200.0, 1000.0);
    assert_eq!(r.t_left_s, 0.0);
    assert!((r.px_per_s - 5.0).abs() < 1e-12);
    assert!((r.x_at_time(100.0) - 500.0).abs() < 1e-9);
    assert!((r.time_at_x(r.x_at_time(37.5)) - 37.5).abs() < 1e-9);
    // empty track: no division by zero
    assert!(resolve_view(&v, 0.0, 500.0).px_per_s.is_finite());
}

#[test]
fn scrolling_keeps_playhead_centred() {
    let v = ViewState {
        mode: ViewMode::Scrolling,
        playhead_s: 60.0,
        px_per_s: 50.0,
        ..Default::default()
    };
    let r = resolve_view(&v, 300.0, 800.0);
    assert!((r.x_at_time(60.0) - 400.0).abs() < 1e-9);
    assert!((r.t_left_s - 52.0).abs() < 1e-9);
    let (a, b) = r.visible(800.0);
    assert!((b - a - 16.0).abs() < 1e-9);
}

#[test]
fn free_uses_offset() {
    let v = ViewState {
        mode: ViewMode::Free,
        offset_s: 10.0,
        px_per_s: 20.0,
        ..Default::default()
    };
    let r = resolve_view(&v, 300.0, 800.0);
    assert!((r.x_at_time(11.0) - 20.0).abs() < 1e-9);
}

#[test]
fn normalise_offset_maps_peak_to_full() {
    assert_eq!(normalise_offset(255, true), 0.0);
    assert_eq!(normalise_offset(0, true), 0.0);
    assert_eq!(normalise_offset(100, false), 0.0);
    let o = normalise_offset(200, true);
    assert!((200.0 / 255.0 + o - 1.0).abs() < 1e-6);
}

#[test]
fn grid_lines_thin_out_below_4px() {
    let g = grid(120.0); // beat 0.5 s
    let v = ViewState {
        mode: ViewMode::Free,
        px_per_s: 100.0,
        ..Default::default()
    };
    // 50 px per beat: all lines
    let r = resolve_view(&v, 600.0, 1000.0);
    let l = grid_lines(&g, &r, 1000.0);
    assert!(l.iter().any(|x| x.1 == LineKind::Beat));
    assert!(l.iter().any(|x| x.1 == LineKind::Bar));
    assert!(l.iter().any(|x| x.1 == LineKind::Phrase));
    // bars every 2 s over 10 s visible: downbeats at 0,2,4,6,8,10 (0 and 8 are phrase starts)
    assert!(l.iter().filter(|x| x.1 != LineKind::Beat).count() >= 5);
    for w in l.windows(2) {
        assert!(w[1].0 > w[0].0);
    }
    // 2 px per beat (8 per bar): bars only
    let v = ViewState { px_per_s: 4.0, ..v };
    let r = resolve_view(&v, 600.0, 1000.0);
    let l = grid_lines(&g, &r, 1000.0);
    assert!(!l.is_empty() && l.iter().all(|x| x.1 != LineKind::Beat));
    // 0.4 px per beat, phrase (4 bars = 16 beats) = 6.4 px: phrases only
    let v = ViewState { px_per_s: 0.8, ..v };
    let r = resolve_view(&v, 6000.0, 1000.0);
    let l = grid_lines(&g, &r, 1000.0);
    assert!(!l.is_empty() && l.iter().all(|x| x.1 == LineKind::Phrase));
    // tiny: nothing
    let v = ViewState {
        px_per_s: 0.05,
        ..v
    };
    let r = resolve_view(&v, 6000.0, 1000.0);
    assert!(grid_lines(&g, &r, 1000.0).is_empty());
}

#[test]
fn grid_without_downbeat_has_only_beats() {
    let g = BeatGrid::constant(120.0, 0.0, 0.9, "x");
    let v = ViewState {
        mode: ViewMode::Free,
        px_per_s: 100.0,
        ..Default::default()
    };
    let r = resolve_view(&v, 600.0, 1000.0);
    let l = grid_lines(&g, &r, 1000.0);
    assert!(!l.is_empty() && l.iter().all(|x| x.1 == LineKind::Beat));
    // when beats are too dense and there are no downbeats nothing is drawn
    let v = ViewState { px_per_s: 4.0, ..v };
    assert!(grid_lines(&g, &resolve_view(&v, 600.0, 1000.0), 1000.0).is_empty());
}

#[test]
fn grid_line_positions_are_exact() {
    let g = grid(120.0);
    let v = ViewState {
        mode: ViewMode::Free,
        px_per_s: 100.0,
        offset_s: 1.0,
        ..Default::default()
    };
    let r = resolve_view(&v, 600.0, 400.0);
    let l = grid_lines(&g, &r, 400.0);
    // beat at 1.0 s sits at x = 0, 1.5 s at 50
    assert!(l.iter().any(|(x, _)| x.abs() < 1e-6));
    assert!(l.iter().any(|(x, _)| (x - 50.0).abs() < 1e-6));
}

#[test]
fn chapter_ticks_every_16_bars() {
    let g = grid(120.0); // 16 bars = 32 s
    let v = ViewState::default();
    let r = resolve_view(&v, 200.0, 1000.0); // 5 px/s
    let t = chapter_ticks(&g, &r, 1000.0);
    assert_eq!(t.len(), 200 / 32 + 1);
    assert!((t[1] - 32.0 * 5.0).abs() < 1e-6);
    assert!(chapter_ticks(&BeatGrid::constant(120.0, 0.0, 0.9, "x"), &r, 1000.0).is_empty());
}

#[test]
fn marker_rects() {
    let theme = WaveTheme::default();
    let m = Markers {
        grid: Some(grid(120.0)),
        cues: vec![CuePoint {
            id: None,
            track_id: 1,
            kind: CueKind::Hot,
            pos_ms: 30_000.0,
            end_ms: None,
            label: None,
            color: Some("#ff0000".into()),
            slot: Some(0),
            auto: false,
        }],
        mix_in_s: Some(10.0),
        mix_out_s: Some(150.0),
        loop_region: Some((50.0, 60.0)),
        buffered_to_s: Some(100.0),
        hover_s: Some(75.0),
        chapter_ticks: true,
    };
    let v = ViewState {
        playhead_s: 20.0,
        ..Default::default()
    };
    let rects = build_marker_rects(&m, &v, &theme, 200.0, 1000.0, 40.0);
    assert!(rects.len() > 10);
    // played shade first, spans 0..playhead
    assert_eq!(rects[0].color, theme.played_shade);
    assert!((rects[0].x1 - 100.0).abs() < 1e-3);
    // playhead last
    let p = rects.last().unwrap();
    assert_eq!(p.color, theme.playhead);
    assert!((((p.x0 + p.x1) / 2.0) - 100.0).abs() < 1e-3);
    // cue colour parsed
    assert!(rects.iter().any(|r| r.color == [1.0, 0.0, 0.0, 1.0]));
    // loop fill covers 250..300
    let lf = rects.iter().find(|r| r.color == theme.loop_fill).unwrap();
    assert!((lf.x0 - 250.0).abs() < 1e-3 && (lf.x1 - 300.0).abs() < 1e-3);
    // buffered bar 0..500 at the bottom
    let b = rects.iter().find(|r| r.color == theme.buffered).unwrap();
    assert!((b.x1 - 500.0).abs() < 1e-3 && b.y1 == 40.0);
    // everything is clipped
    assert!(
        rects
            .iter()
            .all(|r| r.x1 >= r.x0 && r.y1 >= r.y0 && r.x1 > -4.0 && r.x0 < 1004.0)
    );
}

#[test]
fn no_markers_only_playhead_shade() {
    let rects = build_marker_rects(
        &Markers::default(),
        &ViewState::default(),
        &WaveTheme::default(),
        100.0,
        500.0,
        30.0,
    );
    assert!(rects.len() <= 1); // playhead at 0 px: line only (half off-screen is kept)
    let v = ViewState {
        playhead_s: 50.0,
        ..Default::default()
    };
    let rects = build_marker_rects(
        &Markers::default(),
        &v,
        &WaveTheme::default(),
        100.0,
        500.0,
        30.0,
    );
    assert_eq!(rects.len(), 2);
}

#[test]
fn hex_colour_parsing() {
    assert_eq!(
        parse_hex_color("#00ff80"),
        Some([0.0, 1.0, 128.0 / 255.0, 1.0])
    );
    assert_eq!(parse_hex_color("00ff80"), None);
    assert_eq!(parse_hex_color("#fff"), None);
    assert_eq!(parse_hex_color("#gg0000"), None);
}

#[test]
fn layout_plan_wraps_long_tracks_and_drops_levels() {
    // 2 h detail at 172 pts/s
    let counts = [
        1_240_000usize,
        620_000,
        310_000,
        155_000,
        77_500,
        38_750,
        19_375,
        9_688,
        4_844,
        2_422,
        1_211,
    ];
    let l = plan_layout(&counts, 4096);
    assert_eq!(l.width, 4096);
    assert_eq!(l.first_level, 0);
    assert!(l.rows <= 4096);
    assert_eq!(l.slots[0].row_off, 0);
    assert_eq!(l.slots[1].row_off, 1_240_000usize.div_ceil(4096));
    // small GPU: finest levels dropped, coarsest always kept
    let l = plan_layout(&counts, 2048);
    assert_eq!(l.width, 2048);
    assert!(l.rows <= 2048);
    assert!(l.first_level > 0 || l.rows <= 2048);
    let l = plan_layout(&counts, 256);
    assert!(l.first_level > 0);
    assert_eq!(l.slots.len(), counts.len() - l.first_level);
    let l = plan_layout(&[100], 4096);
    assert_eq!((l.rows, l.first_level, l.slots.len()), (1, 0, 1));
    // too many levels for the shader
    let many = vec![10usize; 40];
    assert!(plan_layout(&many, 4096).slots.len() <= MAX_LEVELS);
}

#[test]
fn packing_places_texels() {
    let s: Vec<f32> = (0..44100 * 20)
        .map(|i| ((i as f32) * 0.01).sin() * 0.7)
        .collect();
    let w = build_from_mono(44100, &s, [0; 16]);
    let p = Pyramid::from_waveform(&w);
    let counts: Vec<usize> = p.levels.iter().map(|l| l.n).collect();
    let l = plan_layout(&counts, 4096);
    let (a, b) = pack_textures(&p, &l);
    assert_eq!(a.len(), l.width * l.rows * 4);
    for (slot, idx) in [(0usize, 0usize), (0, 3000), (1, 123), (counts.len() - 1, 7)] {
        let (x, y) = texel_of(&l, slot, idx);
        let o = (y * l.width + x) * 4;
        let pt = p.levels[l.first_level + slot].point(idx);
        assert_eq!(&a[o..o + 4], &[pt[0], pt[1], pt[2], 255]);
        assert_eq!(&b[o..o + 4], &[pt[3], pt[4], pt[5], 255]);
    }
    // 20 s = 3446 detail points: wraps? width 4096 -> no; level choice per column
    assert_eq!(level_for_column(0.001, p.dt0_s, &l), 0);
    assert!(level_for_column(p.dt0_s * 4.0, p.dt0_s, &l) <= 2);
    assert_eq!(column_value(&p, 0.0, 20.0), p.sample_column(0.0, 20.0));
}

#[test]
fn display_mapping_is_normalised_and_expanded() {
    // byte scale is dBFS -60..0
    assert_eq!(byte_to_lin(0.0), 0.0);
    assert!((byte_to_lin(1.0) - 1.0).abs() < 1e-6);
    assert!((byte_to_lin(0.5) - 10f32.powf(-1.5)).abs() < 1e-6);
    let at_db = |db: f32| byte_to_lin((db + 60.0) / 60.0);
    // against a -5.6 dBFS RMS reference a loud passage fills the height and a -13 dBFS
    // breakdown is clearly lower (the old fixed gain gave ~0.9 for both)
    let r = at_db(-5.6);
    let loud = norm_height(at_db(-5.6), r);
    let breakdown = norm_height(at_db(-13.0), r);
    assert!((loud - 1.0).abs() < 1e-5, "{loud}");
    assert!(breakdown < 0.4, "{breakdown}");
    assert_eq!(norm_height(5.0, r), 1.0);
    assert_eq!(norm_height(0.0, r), 0.0);
}

#[test]
fn spectral_colour_is_saturated_and_follows_the_dominant_band() {
    let t = WaveTheme::default();
    let sat = |c: [f32; 3]| (c[0].max(c[1]).max(c[2]) - c[0].min(c[1]).min(c[2])) / c[0].max(c[1]).max(c[2]).max(1e-4);
    // bass only: the low anchor family (red-ish), mids only: green, highs only: blue
    let lo = spectral_color([1.0, 0.0, 0.0], &t);
    assert!(lo[0] > lo[1] && lo[0] > lo[2], "{lo:?}");
    let mi = spectral_color([0.0, 1.0, 0.0], &t);
    assert!(mi[1] > mi[0] && mi[1] > mi[2], "{mi:?}");
    let hi = spectral_color([0.0, 0.0, 1.0], &t);
    assert!(hi[2] > hi[0] && hi[2] > hi[1], "{hi:?}");
    // a balanced mix never washes out to grey or white, and the brightest channel stays high
    let bal = spectral_color([0.9, 0.7, 0.6], &t);
    assert!(sat(bal) > 0.3, "{bal:?}");
    assert!(bal[0].max(bal[1]).max(bal[2]) > 0.7);
    // silence falls back to the low anchor
    assert_eq!(spectral_color([0.0; 3], &t), [t.low[0], t.low[1], t.low[2]]);
}

#[test]
fn bars_theme_defaults_and_markers() {
    let t = WaveTheme::default();
    assert_eq!(t.bar_w_css, 2.0);
    assert_eq!(t.gap_css, 1.0);
    // white played, cyan unplayed
    assert!(t.played[0] > 0.85 && t.played[1] > 0.85 && t.played[2] > 0.85);
    assert!(t.unplayed[2] > t.unplayed[0] + 0.4 && t.unplayed[1] > t.unplayed[0] + 0.3);
    // Bars: no shade, no grid, no hover line; a cue is a thin line plus a flag; the playhead
    // is a line over a wider edge in `played_to`
    let v = ViewState { style: Style::Bars, playhead_s: 50.0, ..Default::default() };
    let m = Markers {
        grid: Some(grid(128.0)),
        hover_s: Some(60.0),
        mix_in_s: Some(20.0),
        cues: vec![CuePoint { id: None, track_id: 1, kind: CueKind::Hot, pos_ms: 30_000.0, end_ms: None, label: None, color: None, slot: None, auto: false }],
        ..Default::default()
    };
    let rects = build_marker_rects(&m, &v, &t, 100.0, 500.0, 56.0);
    assert!(rects.iter().all(|r| r.color != t.hover && r.color != t.played_shade));
    assert_eq!(rects.len(), 6, "two markers x (line + flag), playhead edge + line");
    let (edge, line) = (&rects[4], &rects[5]);
    assert_eq!((edge.color, line.color), (t.played_to, t.playhead));
    assert!(edge.x0 < line.x0 && edge.x1 > line.x1);
    assert!((line.x0 + line.x1) / 2.0 == 250.0, "playhead at 50 s of 100 s on 500 px");
}

#[test]
fn played_shade_rect_is_optional() {
    let mut theme = WaveTheme::default();
    theme.played_shade[3] = 0.0;
    let v = ViewState {
        playhead_s: 50.0,
        ..Default::default()
    };
    let rects = build_marker_rects(&Markers::default(), &v, &theme, 100.0, 500.0, 30.0);
    assert_eq!(rects.len(), 1); // playhead only
    assert_eq!(rects[0].color, theme.playhead);
}
