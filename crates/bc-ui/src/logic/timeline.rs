//! Pure geometry for the Arrange timeline (port of `timelineGeometry.ts`).
//! `start_ms`, `played_ms` and `overlap_ms` are server truth; this converts
//! ms to px, assigns lanes and applies the one live drag override.

pub const ZOOM_LADDER: [f64; 6] = [2.0, 4.0, 8.0, 15.0, 30.0, 60.0];
pub const LANE_H: f64 = 96.0;
pub const LANE_GAP: f64 = 8.0;
pub const RULER_H: f64 = 24.0;
pub const TIMELINE_H: f64 = RULER_H + LANE_H * 2.0 + LANE_GAP;
/// The musical overlap lengths a transition handle snaps to.
pub const BEAT_CHOICES: [u32; 6] = [0, 4, 8, 16, 32, 64];

/// The slice of a set item the geometry needs (the page maps `SetItem` into this).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GeomItem {
    pub id: i64,
    pub start_ms: f64,
    pub played_ms: f64,
    pub duration_ms: Option<f64>,
    pub cue_in_ms: Option<f64>,
    pub cue_out_ms: Option<f64>,
    pub tempo_adjust_pct: f64,
    pub transition_beats: Option<u32>,
    pub effective_bpm: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DragField {
    CueIn,
    CueOut,
    TransitionBeats,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DragOverride {
    pub item_id: i64,
    pub field: DragField,
    /// cue fields: track-time ms. transition beats: the beat count.
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClipRect {
    pub index: usize,
    pub lane: u8,
    pub left_px: f64,
    pub width_px: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BandRect {
    /// transitions[index] joins into items[index + 1].
    pub index: usize,
    pub left_px: f64,
    pub width_px: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tick {
    pub left_px: f64,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Geometry {
    pub clips: Vec<ClipRect>,
    pub bands: Vec<BandRect>,
    pub total_px: f64,
    pub ticks: Vec<Tick>,
}

fn x(ms: f64, px_per_sec: f64) -> f64 {
    ms / 1000.0 * px_per_sec
}

pub fn tempo_multiplier(i: &GeomItem) -> f64 {
    1.0 + i.tempo_adjust_pct / 100.0
}

pub fn overlap_ms(beats: Option<u32>, bpm: Option<f64>) -> f64 {
    match (beats, bpm) {
        (Some(b), Some(bpm)) if b > 0 && bpm > 0.0 => (b as f64 * 60_000.0 / bpm).round(),
        _ => 0.0,
    }
}

pub fn quantize_beats(ms: f64, bpm: f64) -> u32 {
    let mut best = 0;
    let mut best_d = f64::INFINITY;
    for b in BEAT_CHOICES {
        let d = (overlap_ms(Some(b), Some(bpm)) - ms).abs();
        if d < best_d {
            best = b;
            best_d = d;
        }
    }
    best
}

fn label(ms: f64) -> String {
    let total = (ms / 1000.0).round() as i64;
    format!("{}:{:02}", total / 60, total % 60)
}

pub fn choose_tick_interval(px_per_sec: f64, min_spacing: f64) -> f64 {
    for s in [1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0] {
        if s * px_per_sec >= min_spacing {
            return s;
        }
    }
    600.0
}

/// `overlaps[i]` = overlap_ms of the transition joining item i into i+1.
pub fn compute_geometry(items: &[GeomItem], overlaps: &[f64], px_per_sec: f64, ov: Option<DragOverride>) -> Geometry {
    let mut clips: Vec<ClipRect> = items
        .iter()
        .enumerate()
        .map(|(i, it)| ClipRect {
            index: i,
            lane: (i % 2) as u8,
            left_px: x(it.start_ms, px_per_sec),
            width_px: x(it.played_ms, px_per_sec).max(2.0),
        })
        .collect();
    let mut bands: Vec<BandRect> = overlaps
        .iter()
        .enumerate()
        .filter_map(|(i, &o)| {
            let inc = items.get(i + 1)?;
            (o > 0.0).then(|| BandRect { index: i, left_px: x(inc.start_ms, px_per_sec), width_px: x(o, px_per_sec) })
        })
        .collect();

    if let Some(ov) = ov {
        if let Some(at) = items.iter().position(|i| i.id == ov.item_id) {
            let item = &items[at];
            let m = tempo_multiplier(item);
            match ov.field {
                DragField::CueIn => {
                    let dpx = x((ov.value - item.cue_in_ms.unwrap_or(0.0)) / m, px_per_sec);
                    let c = &mut clips[at];
                    c.left_px += dpx;
                    c.width_px = (c.width_px - dpx).max(2.0);
                }
                DragField::CueOut => {
                    let old = item.cue_out_ms.or(item.duration_ms).unwrap_or(0.0);
                    let dpx = x((ov.value - old) / m, px_per_sec);
                    clips[at].width_px = (clips[at].width_px + dpx).max(2.0);
                    for c in clips.iter_mut().skip(at + 1) {
                        c.left_px += dpx;
                    }
                    for b in bands.iter_mut().filter(|b| b.index >= at) {
                        b.left_px += dpx;
                    }
                }
                DragField::TransitionBeats => {
                    let old = overlap_ms(item.transition_beats, item.effective_bpm);
                    let new = overlap_ms(Some(ov.value.round() as u32), item.effective_bpm);
                    let dpx = x(new - old, px_per_sec);
                    for c in clips.iter_mut().skip(at) {
                        c.left_px -= dpx;
                    }
                    for b in bands.iter_mut() {
                        if at > 0 && b.index == at - 1 {
                            b.left_px -= dpx;
                            b.width_px = x(new, px_per_sec).max(0.0);
                        } else if b.index >= at {
                            b.left_px -= dpx;
                        }
                    }
                }
            }
        }
    }

    let total_px = clips.last().map(|c| c.left_px + c.width_px).unwrap_or(0.0);
    let mut ticks = vec![];
    let interval = choose_tick_interval(px_per_sec, 80.0);
    let minor = interval / 5.0;
    let total_s = total_px / px_per_sec;
    let show_minor = minor * px_per_sec >= 14.0;
    let step = if show_minor { minor } else { interval };
    let mut s = 0.0;
    while s <= total_s + interval {
        let major = !show_minor || ((s / minor).round() as i64) % 5 == 0;
        ticks.push(Tick { left_px: s * px_per_sec, label: major.then(|| label(s * 1000.0)) });
        s += step;
    }
    Geometry { clips, bands, total_px, ticks }
}

/// Track-time ms at a pixel offset within a clip.
pub fn track_ms_at(item: &GeomItem, clip_offset_px: f64, px_per_sec: f64) -> f64 {
    item.cue_in_ms.unwrap_or(0.0) + clip_offset_px / px_per_sec * 1000.0 * tempo_multiplier(item)
}

/// Peak-array index range a clip's cue window covers over `n` points.
pub fn peak_slice(item: &GeomItem, n: usize) -> (usize, usize) {
    let dur = item.duration_ms.unwrap_or(0.0);
    if dur <= 0.0 || n == 0 {
        return (0, n);
    }
    let from = item.cue_in_ms.unwrap_or(0.0);
    let to = item.cue_out_ms.unwrap_or(dur);
    let nf = n as f64;
    let start = ((from / dur * nf).floor().max(0.0) as usize).min(n - 1);
    let end = ((to / dur * nf).ceil().max(1.0) as usize).min(n);
    (start, end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, start: f64) -> GeomItem {
        GeomItem {
            id,
            start_ms: start,
            played_ms: 300_000.0,
            duration_ms: Some(300_000.0),
            effective_bpm: Some(128.0),
            ..Default::default()
        }
    }

    #[test]
    fn contiguous_clips_edge_to_edge() {
        let items = [item(1, 0.0), item(2, 300_000.0)];
        let g = compute_geometry(&items, &[0.0], 10.0, None);
        assert_eq!(g.clips[0].left_px, 0.0);
        assert_eq!(g.clips[0].width_px, 3000.0);
        assert_eq!(g.clips[1].left_px, 3000.0);
        assert!(g.bands.is_empty());
        assert_eq!(g.total_px, 6000.0);
    }

    #[test]
    fn overlap_band_aligns_with_incoming_and_outgoing_tail() {
        let mut b = item(2, 285_000.0);
        b.transition_beats = Some(32);
        let g = compute_geometry(&[item(1, 0.0), b], &[15_000.0], 10.0, None);
        let band = &g.bands[0];
        assert_eq!(band.left_px, g.clips[1].left_px);
        assert_eq!(band.width_px, 150.0);
        assert_eq!(g.clips[0].left_px + g.clips[0].width_px, band.left_px + band.width_px);
    }

    #[test]
    fn alternates_lanes() {
        let items: Vec<_> = (0..4).map(|i| item(i + 1, i as f64 * 300_000.0)).collect();
        let lanes: Vec<_> = compute_geometry(&items, &[], 10.0, None).clips.iter().map(|c| c.lane).collect();
        assert_eq!(lanes, [0, 1, 0, 1]);
    }

    #[test]
    fn cue_out_shifts_downstream() {
        let items = [item(1, 0.0), item(2, 300_000.0), item(3, 600_000.0)];
        let ov = DragOverride { item_id: 1, field: DragField::CueOut, value: 290_000.0 };
        let g = compute_geometry(&items, &[0.0, 0.0], 10.0, Some(ov));
        assert_eq!(g.clips[0].left_px, 0.0);
        assert_eq!(g.clips[0].width_px, 2900.0);
        assert_eq!(g.clips[1].left_px, 2900.0);
        assert_eq!(g.clips[2].left_px, 5900.0);
    }

    #[test]
    fn cue_in_moves_only_dragged_clip_honouring_pitch() {
        let mut a = item(1, 0.0);
        a.tempo_adjust_pct = 100.0;
        let ov = DragOverride { item_id: 1, field: DragField::CueIn, value: 20_000.0 };
        let g = compute_geometry(&[a, item(2, 150_000.0)], &[0.0], 10.0, Some(ov));
        assert_eq!(g.clips[0].left_px, 100.0);
        assert_eq!(g.clips[1].left_px, 1500.0);
    }

    #[test]
    fn transition_beats_override_pulls_incoming_and_rest() {
        let items = [item(1, 0.0), item(2, 300_000.0), item(3, 600_000.0)];
        let ov = DragOverride { item_id: 2, field: DragField::TransitionBeats, value: 32.0 };
        let g = compute_geometry(&items, &[0.0, 0.0], 10.0, Some(ov));
        assert_eq!(g.clips[1].left_px, 3000.0 - 150.0);
        assert_eq!(g.clips[2].left_px, 6000.0 - 150.0);
    }

    #[test]
    fn degenerates() {
        assert_eq!(compute_geometry(&[], &[], 10.0, None).total_px, 0.0);
        let mut n = item(1, 0.0);
        n.duration_ms = None;
        n.played_ms = 0.0;
        assert!(compute_geometry(&[n], &[], 10.0, None).clips[0].width_px > 0.0);
    }

    #[test]
    fn helpers() {
        assert_eq!(quantize_beats(0.0, 128.0), 0);
        assert_eq!(quantize_beats(7_400.0, 128.0), 16);
        assert_eq!(quantize_beats(15_200.0, 128.0), 32);
        assert_eq!(quantize_beats(40_000.0, 128.0), 64);
        assert_eq!(overlap_ms(Some(32), Some(128.0)), 15_000.0);
        assert_eq!(overlap_ms(None, Some(128.0)), 0.0);
        assert_eq!(overlap_ms(Some(32), None), 0.0);
        assert!(choose_tick_interval(60.0, 80.0) <= 5.0);
        assert!(choose_tick_interval(2.0, 80.0) >= 60.0);
        for px in ZOOM_LADDER {
            assert!(choose_tick_interval(px, 80.0) * px >= 80.0);
        }
        let mut i = item(1, 0.0);
        i.cue_in_ms = Some(30_000.0);
        i.tempo_adjust_pct = 100.0;
        assert_eq!(track_ms_at(&i, 10.0, 10.0), 32_000.0);
        let mut j = item(1, 0.0);
        j.cue_in_ms = Some(30_000.0);
        j.cue_out_ms = Some(270_000.0);
        assert_eq!(peak_slice(&j, 2000), (200, 1800));
        assert_eq!(peak_slice(&item(2, 0.0), 2000), (0, 2000));
    }
}
