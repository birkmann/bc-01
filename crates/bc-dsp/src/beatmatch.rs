//! Planning a DJ blend: how long, at what tempo, with what on the outgoing.
//!
//! Exact port of `web/frontend/src/player/beatmatch.ts` (same numbers, same
//! tolerances) plus the transition kind / quantise / phase-lock fields of the
//! native engine. Pure arithmetic, so the shape of every transition is testable
//! without audio; the mixer executes whatever [`BlendSpec`] comes out of here.
//!
//! Beatmatching is a *rate*: the incoming deck plays at `outgoing_bpm /
//! incoming_bpm` so the two grids run at the same speed. Only tempos within a
//! few percent are matched (half and double time count as the same tempo, as
//! they do to a DJ); beyond that a plain echo-out sounds better than a warped record.

use bc_types::player::{Entry, MixSettings, Quantise, TransitionKind};

/// Tempos further apart than this are not matched: ~1 semitone un-locked.
pub const SYNC_TOLERANCE: f64 = 0.06;
/// A matched blend never runs shorter or longer than this, whatever the beats say.
pub const BLEND_MIN_S: f64 = 8.0;
pub const BLEND_MAX_S: f64 = 96.0;
/// The plain (unmatched) end-of-track blend.
pub const PLAIN_LENGTH_S: f64 = 4.0;
/// An unmatched DJ blend: four bars of the outgoing, within these bounds.
pub const PLAIN_EQ_MIN_S: f64 = 4.0;
pub const PLAIN_EQ_MAX_S: f64 = 8.0;
/// A blend never starts before this share of the outgoing track has played.
pub const EARLIEST_START_PCT: f64 = 0.5;
/// How long the echo keeps ringing after the send closes before the deck is parked.
pub const ECHO_TAIL_S: f64 = 4.0;
pub const CUT_TAIL_S: f64 = 0.2;
/// How fast "cut now" finishes a running blend.
pub const CUT_S: f64 = 0.3;
/// Seconds the incoming's tempo takes to glide back to 1.0 after a matched blend (default).
pub const GLIDE_BACK_S: f64 = 30.0;
/// A nudge / phase correction bends the rate by this much...
pub const BEND_PCT: f64 = 0.04;
/// ...for at most this long, however big the residual.
pub const BEND_MAX_S: f64 = 2.0;
/// The continuous phase-lock loop never moves the rate more than this.
pub const PLL_MAX_TRIM: f64 = 0.003;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EchoSpec {
    /// Send level into the echo bus.
    pub send: f64,
    /// Seconds before the end of the blend the send opens (and stays open).
    pub hold_s: f64,
    /// Ramp-up time of the send.
    pub up_s: f64,
    /// Tempo the echo repeats at (a dotted eighth of it).
    pub bpm: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyncSpec {
    /// Playback rate for the incoming deck.
    pub rate: f64,
    pub key_lock: bool,
    pub from_bpm: f64,
    pub to_bpm: f64,
    /// Keep the rate after the blend rather than gliding back to 1.
    pub hold: bool,
    /// The outgoing deck's rate when this was planned. Should the outgoing still be
    /// gliding when the blend starts, the mixer scales `rate` by how far it moved;
    /// 0 = `rate` is authoritative.
    pub out_rate: f64,
    /// Seconds the glide back to 1.0 takes after the blend.
    pub glide_s: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    Linear,
    EqualPower,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlendSpec {
    pub kind: TransitionKind,
    /// Crossfade length in seconds.
    pub length_s: f64,
    pub curve: Curve,
    /// Where the incoming track starts, in its own seconds.
    pub incoming_start_s: f64,
    pub echo: Option<EchoSpec>,
    pub sync: Option<SyncSpec>,
    /// How long after the crossfade the outgoing deck is left before parking.
    pub park_tail_s: f64,
    pub quantise: Quantise,
    /// Continuous phase-lock loop after the initial bend.
    pub phase_lock: bool,
    /// Bass swap / EQ blend: seconds (wall) after the start the basslines change
    /// over -- the incoming's drop when it is known. `None`: the bar nearest the middle.
    pub swap_s: Option<f64>,
}

impl BlendSpec {
    /// This blend with a tempo match at `rate` (no key lock), for tests and tools.
    pub fn with_sync(mut self, rate: f64) -> Self {
        self.sync = Some(SyncSpec { rate, key_lock: false, from_bpm: 120.0, to_bpm: 120.0 * rate, hold: false, out_rate: 0.0, glide_s: GLIDE_BACK_S });
        self
    }
}

/// A user skip: fast, but still an echo rather than a cut, and straight in at the drop.
pub fn short_blend(out_bpm: Option<f64>, echo: bool, incoming_start_s: f64) -> BlendSpec {
    BlendSpec {
        kind: TransitionKind::Blend,
        length_s: 1.2,
        curve: Curve::Linear,
        incoming_start_s,
        echo: echo.then_some(EchoSpec { send: 0.7, hold_s: 0.6, up_s: 0.05, bpm: out_bpm }),
        sync: None,
        park_tail_s: if echo { ECHO_TAIL_S } else { CUT_TAIL_S },
        quantise: Quantise::Off,
        phase_lock: false,
        swap_s: None,
    }
}

/// A hard cut (declick only): used for track starts and previous/jump.
pub fn cut_spec(incoming_start_s: f64) -> BlendSpec {
    BlendSpec {
        kind: TransitionKind::Cut,
        length_s: 0.012,
        curve: Curve::Linear,
        incoming_start_s,
        echo: None,
        sync: None,
        park_tail_s: CUT_TAIL_S,
        quantise: Quantise::Off,
        phase_lock: false,
        swap_s: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Multiplier {
    pub m: f64,
    pub ratio: f64,
}

/// Straight, double or half time -- whichever brings the two tempos closest.
pub fn choose_multiplier(out_bpm: f64, in_bpm: f64) -> Multiplier {
    let mut best = Multiplier { m: 1.0, ratio: out_bpm / in_bpm };
    for m in [2.0, 0.5] {
        let ratio = out_bpm / (in_bpm * m);
        if (ratio - 1.0).abs() < (best.ratio - 1.0).abs() {
            best = Multiplier { m, ratio };
        }
    }
    best
}

/// The rate the incoming deck should play at to run alongside the outgoing,
/// or `None` when the tempos are too far apart to match.
pub fn sync_rate(out_bpm_eff: Option<f64>, in_bpm: Option<f64>, tolerance: f64) -> Option<f64> {
    let (o, i) = (out_bpm_eff?, in_bpm?);
    if !(o > 0.0 && i > 0.0) {
        return None;
    }
    let m = choose_multiplier(o, i);
    if (m.ratio - 1.0).abs() > tolerance {
        return None;
    }
    Some(js_round(m.ratio * 1e5) / 1e5)
}

/// `Math.round` (ties toward +inf), to stay bit-identical with the browser planner.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

/// How long a matched blend should run: the chosen number of beats, but never
/// past the end of the outgoing track or the end of the incoming's intro, and
/// always long enough to hear and short enough to still be a transition.
pub fn blend_length(length_beats: f64, out_bpm_eff: Option<f64>, out_remaining_s: f64, in_s: f64) -> f64 {
    let beats_s = match out_bpm_eff {
        Some(b) if b > 0.0 => length_beats * 60.0 / b,
        _ => BLEND_MAX_S,
    };
    let wanted = beats_s.min(out_remaining_s).min(if in_s > 0.0 { in_s } else { BLEND_MAX_S });
    wanted.clamp(BLEND_MIN_S, BLEND_MAX_S)
}

/// A blend length cut short by its bounds, rounded down to whole bars at
/// `bpm` so it still ends on a bar line (at least one bar).
pub fn whole_bars(length_s: f64, bpm: Option<f64>) -> f64 {
    match bpm.filter(|b| *b > 0.0) {
        Some(b) => {
            let bar = 4.0 * 60.0 / b;
            ((length_s / bar + 1e-9).floor() * bar).max(bar.min(length_s))
        }
        None => length_s,
    }
}

/// Seconds the outgoing has room for a blend: from its outro, or earlier when the
/// outro is too short for `wanted_s`, but never before half-way.
pub fn blend_room(dur_s: f64, out_start_s: f64, wanted_s: f64) -> f64 {
    let earliest = out_start_s.min((dur_s * EARLIEST_START_PCT).max(dur_s - 1.0 - wanted_s));
    0f64.max(dur_s - 1.0 - earliest)
}

/// When the blend must start so it fits before the outgoing ends: at the outro
/// if that leaves room, else early enough for the whole length plus `slack_s`
/// (time to load the incoming and wait for the quantised start).
pub fn trigger_point(out_s: f64, dur_s: f64, length_s: f64, slack_s: f64) -> f64 {
    0f64.max(out_s.min(dur_s - length_s - 1.0 - slack_s.max(0.0)))
}

/// Room a trigger leaves for the incoming to load and the start to wait for its
/// quantise boundary at `bpm_eff`.
pub fn trigger_slack_s(q: Quantise, bpm_eff: Option<f64>) -> f64 {
    let beat = bpm_eff.filter(|b| *b > 0.0).map(|b| 60.0 / b).unwrap_or(0.5);
    let beats = match q {
        Quantise::Off => 0.0,
        Quantise::Beat => 1.0,
        Quantise::Bar => 4.0,
        Quantise::Phrase => 16.0,
    };
    1.5 + (beats * beat).min(10.0)
}

#[derive(Debug, Clone, Copy)]
pub struct OutgoingFacts {
    pub bpm: Option<f64>,
    /// The deck's current playback rate: a matched track carries its tempo on.
    pub rate: f64,
    pub dur_s: f64,
    /// Where its outro starts (mix-out point); `None` = unknown.
    pub out_s: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
pub struct IncomingFacts {
    pub bpm: Option<f64>,
    /// Where its intro ends (mix-in point).
    pub in_s: f64,
}

/// The end-of-track blend between these two tracks under these settings.
pub fn plan_blend(out: &OutgoingFacts, inc: &IncomingFacts, s: &MixSettings) -> BlendSpec {
    let out_bpm_eff = match out.bpm {
        Some(b) if b > 0.0 => Some(b * if out.rate != 0.0 { out.rate } else { 1.0 }),
        _ => None,
    };
    let rate = if s.sync { sync_rate(out_bpm_eff, inc.bpm, SYNC_TOLERANCE) } else { None };
    let out_start = out.out_s.unwrap_or_else(|| 0f64.max(out.dur_s - PLAIN_LENGTH_S - 1.0));
    let intro = s.entry == Entry::Intro;
    let eq = s.transition == TransitionKind::EqBlend;
    let wanted_s = out_bpm_eff.map(|b| s.length_beats as f64 * 60.0 / b).unwrap_or(BLEND_MAX_S);
    // The DJ blend may begin before a short outro to run its full length; the
    // others keep to the outro, as the browser player does.
    let out_remaining_s = if eq { blend_room(out.dur_s, out_start, wanted_s) } else { 0f64.max(out.dur_s - out_start) };

    // Coming in at the drop, the incoming's intro does not bound the blend. The DJ
    // blend runs only half of itself over the intro (the drop is the bass swap).
    let intro_bound = match (intro, eq) {
        (false, _) => 0.0,
        (true, false) => inc.in_s,
        (true, true) => 0.0,
    };
    let length_s = match rate {
        Some(_) if eq => whole_bars(blend_length(s.length_beats as f64, out_bpm_eff, out_remaining_s, intro_bound), out_bpm_eff),
        Some(_) => blend_length(s.length_beats as f64, out_bpm_eff, out_remaining_s, intro_bound),
        None if eq => out_bpm_eff.map(|b| 16.0 * 60.0 / b).unwrap_or(6.0).clamp(PLAIN_EQ_MIN_S, PLAIN_EQ_MAX_S),
        None => PLAIN_LENGTH_S,
    };

    // In the DJ blend the EQs do the leaving: the echo is for tempos that cannot be matched.
    let echo = (s.echo && !(eq && rate.is_some())).then_some(EchoSpec { send: 0.65, hold_s: 3.0, up_s: 0.3, bpm: out_bpm_eff });
    let sync = match (rate, out_bpm_eff, inc.bpm) {
        (Some(rate), Some(o), Some(i)) => Some(SyncSpec {
            rate,
            key_lock: s.key_lock,
            from_bpm: i,
            to_bpm: o,
            hold: s.hold_tempo,
            out_rate: if out.rate > 0.0 { out.rate } else { 1.0 },
            glide_s: s.glide_back_s.max(0.0),
        }),
        _ => None,
    };
    // From the intro, the DJ blend lines the incoming's drop up with the bass swap
    // half-way through: the intro plays under the outgoing's outro, the drop lands as it leaves.
    let (eq_start, swap_s) = match (eq, intro, rate) {
        (true, true, Some(r)) if inc.in_s > 0.0 => {
            let start = 0f64.max(inc.in_s - length_s * 0.5 * r);
            let swap = (inc.in_s - start) / r;
            (Some(start), (swap >= length_s * 0.25).then_some(swap))
        }
        _ => (None, None),
    };

    let mut spec = BlendSpec {
        kind: s.transition,
        length_s,
        curve: if rate.is_some() { Curve::EqualPower } else { Curve::Linear },
        // At the drop: straight in where the main sounds start, matched or not.
        // From the intro (matched only): the intro runs under the outgoing's
        // outro and ends as the outgoing goes.
        incoming_start_s: match eq_start {
            Some(st) => st,
            None if rate.is_some() && intro => 0f64.max(inc.in_s - length_s),
            None => inc.in_s,
        },
        echo,
        sync,
        park_tail_s: if echo.is_some() { ECHO_TAIL_S } else { CUT_TAIL_S },
        quantise: s.quantise,
        phase_lock: s.phase_lock,
        swap_s,
    };
    shape_for_kind(&mut spec, out_bpm_eff);
    spec
}

/// Adjust a planned blend for the real transition types of the native engine.
/// `Blend` is left exactly as the browser planner made it.
fn shape_for_kind(spec: &mut BlendSpec, out_bpm_eff: Option<f64>) {
    match spec.kind {
        TransitionKind::Blend | TransitionKind::BassSwap | TransitionKind::Filter | TransitionKind::EqBlend => {}
        TransitionKind::EchoOut => {
            let beat = out_bpm_eff.map(|b| 60.0 / b).unwrap_or(0.5);
            // Two bars of the outgoing, bounded: the echo does the leaving.
            spec.length_s = (8.0 * beat).clamp(2.0, 6.0);
            spec.curve = Curve::EqualPower;
            let bpm = out_bpm_eff;
            spec.echo = Some(EchoSpec { send: 0.7, hold_s: spec.length_s, up_s: 0.05, bpm });
            spec.park_tail_s = ECHO_TAIL_S;
        }
        TransitionKind::Cut => {
            spec.length_s = 0.012;
            spec.curve = Curve::Linear;
            spec.park_tail_s = if spec.echo.is_some() { ECHO_TAIL_S } else { CUT_TAIL_S };
            if let Some(e) = spec.echo.as_mut() {
                e.hold_s = 0.4;
                e.up_s = 0.02;
                e.send = 0.7;
            }
        }
    }
}

/// How long a track plays when it starts at `start_s` and is moved on after
/// `max_play_s` (`None` = when it ends) -- `effectiveLengthMs` of the planner.
pub fn effective_length_ms(duration_ms: Option<f64>, start_s: f64, max_play_s: Option<f64>) -> f64 {
    let Some(d) = duration_ms else { return 0.0 };
    let left = 0f64.max(d - start_s * 1000.0);
    match max_play_s {
        None => left,
        Some(m) => left.min(m * 1000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() < eps, "{a} != {b}");
    }

    #[test]
    fn choose_multiplier_cases() {
        assert_eq!(choose_multiplier(128.0, 64.0), Multiplier { m: 2.0, ratio: 1.0 });
        assert_eq!(choose_multiplier(70.0, 140.0).m, 0.5);
        assert_eq!(choose_multiplier(128.0, 127.0).m, 1.0);
    }

    #[test]
    fn sync_rate_matches_within_tolerance() {
        close(sync_rate(Some(128.0), Some(126.0), SYNC_TOLERANCE).unwrap(), 1.01587, 1e-4);
        close(sync_rate(Some(126.0), Some(128.0), SYNC_TOLERANCE).unwrap(), 0.98438, 1e-4);
    }

    #[test]
    fn sync_rate_refuses_far_tempos() {
        assert_eq!(sync_rate(Some(128.0), Some(140.0), SYNC_TOLERANCE), None);
        assert_eq!(sync_rate(Some(128.0), Some(118.0), SYNC_TOLERANCE), None);
    }

    #[test]
    fn sync_rate_half_and_double_time() {
        assert_eq!(sync_rate(Some(128.0), Some(64.0), SYNC_TOLERANCE), Some(1.0));
        assert_eq!(sync_rate(Some(87.0), Some(174.0), SYNC_TOLERANCE), Some(1.0));
    }

    #[test]
    fn sync_rate_needs_both() {
        assert_eq!(sync_rate(None, Some(128.0), SYNC_TOLERANCE), None);
        assert_eq!(sync_rate(Some(128.0), None, SYNC_TOLERANCE), None);
        assert_eq!(sync_rate(Some(0.0), Some(128.0), SYNC_TOLERANCE), None);
    }

    #[test]
    fn blend_length_is_chosen_beats_at_outgoing_tempo() {
        assert_eq!(blend_length(32.0, Some(128.0), 60.0, 40.0), 15.0);
    }

    #[test]
    fn blend_length_never_outruns_outgoing_or_intro() {
        assert_eq!(blend_length(64.0, Some(128.0), 12.0, 40.0), 12.0);
        assert_eq!(blend_length(64.0, Some(128.0), 60.0, 10.0), 10.0);
    }

    #[test]
    fn blend_length_clamps() {
        assert_eq!(blend_length(16.0, Some(200.0), 60.0, 40.0), BLEND_MIN_S);
        assert_eq!(blend_length(64.0, Some(40.0), 300.0, 200.0), BLEND_MAX_S);
        assert_eq!(blend_length(32.0, Some(128.0), 3.0, 40.0), BLEND_MIN_S);
    }

    #[test]
    fn blend_length_ignores_unknown_intro() {
        assert_eq!(blend_length(32.0, Some(128.0), 60.0, 0.0), 15.0);
    }

    #[test]
    fn trigger_point_cases() {
        assert_eq!(trigger_point(300.0, 330.0, 15.0, 0.0), 300.0);
        assert_eq!(trigger_point(320.0, 330.0, 15.0, 0.0), 314.0);
        assert_eq!(trigger_point(320.0, 330.0, 15.0, 4.0), 310.0);
    }

    #[test]
    fn blend_length_rounds_a_bounded_blend_to_bars() {
        // 64 beats at 128 = 30 s wanted, 20 s of room: 10 whole bars (18.75 s)
        close(whole_bars(blend_length(64.0, Some(128.0), 20.0, 0.0), Some(128.0)), 18.75, 1e-9);
        close(whole_bars(30.0, Some(128.0)), 30.0, 1e-9);
        close(whole_bars(1.0, Some(128.0)), 1.0, 1e-9);
    }

    #[test]
    fn blend_room_reaches_before_a_short_outro() {
        // outro at 340 of 360, 30 s wanted: the blend may start at 329
        close(blend_room(360.0, 340.0, 30.0), 30.0, 1e-9);
        // never before half-way
        close(blend_room(100.0, 95.0, 90.0), 49.0, 1e-9);
        // a long outro is room enough as it is
        close(blend_room(360.0, 280.0, 30.0), 79.0, 1e-9);
    }

    fn eq_settings() -> MixSettings {
        MixSettings { transition: TransitionKind::EqBlend, length_beats: 64, ..Default::default() }
    }

    #[test]
    fn eq_blend_runs_its_length_past_a_short_outro_without_echo() {
        let out = OutgoingFacts { bpm: Some(128.0), rate: 1.0, dur_s: 360.0, out_s: Some(345.0) };
        let spec = plan_blend(&out, &IncomingFacts { bpm: Some(126.0), in_s: 30.0 }, &eq_settings());
        assert_eq!(spec.kind, TransitionKind::EqBlend);
        close(spec.length_s, 30.0, 1e-9);
        assert!(spec.echo.is_none(), "a matched DJ blend leaves with the EQs");
        let sy = spec.sync.unwrap();
        assert_eq!((sy.out_rate, sy.glide_s), (1.0, 30.0));
        assert_eq!(spec.incoming_start_s, 30.0);
        assert_eq!(spec.swap_s, None);
    }

    #[test]
    fn eq_blend_from_the_intro_swaps_the_bass_on_the_drop() {
        let s = MixSettings { entry: Entry::Intro, ..eq_settings() };
        let spec = plan_blend(&out_facts(), &IncomingFacts { bpm: Some(128.0), in_s: 60.0 }, &s);
        close(spec.length_s, 30.0, 1e-9);
        close(spec.incoming_start_s, 45.0, 1e-9);
        close(spec.swap_s.unwrap(), 15.0, 1e-9);
        // a short intro starts the track from the top, the drop still sets the swap
        let short = plan_blend(&out_facts(), &IncomingFacts { bpm: Some(128.0), in_s: 10.0 }, &s);
        assert_eq!(short.incoming_start_s, 0.0);
        close(short.swap_s.unwrap(), 10.0, 1e-9);
    }

    #[test]
    fn eq_blend_unmatched_is_four_bars_and_echoes() {
        let spec = plan_blend(&out_facts(), &IncomingFacts { bpm: Some(100.0), in_s: 30.0 }, &eq_settings());
        assert!(spec.sync.is_none());
        close(spec.length_s, 7.5, 1e-9);
        assert!(spec.echo.is_some());
    }

    /// The browser player's settings: an equal-power blend of 32 beats on the bar.
    fn blend() -> MixSettings {
        MixSettings { transition: TransitionKind::Blend, length_beats: 32, quantise: Quantise::Bar, ..Default::default() }
    }

    fn out_facts() -> OutgoingFacts {
        OutgoingFacts { bpm: Some(128.0), rate: 1.0, dur_s: 360.0, out_s: Some(330.0) }
    }

    #[test]
    fn plan_blend_matches_tempo_and_comes_in_at_drop() {
        let spec = plan_blend(&out_facts(), &IncomingFacts { bpm: Some(126.0), in_s: 30.0 }, &blend());
        close(spec.sync.unwrap().rate, 1.01587, 1e-4);
        assert!(!spec.sync.unwrap().key_lock);
        assert_eq!(spec.curve, Curve::EqualPower);
        assert_eq!(spec.length_s, 15.0);
        assert_eq!(spec.incoming_start_s, 30.0);
        assert_eq!(spec.echo.unwrap().bpm, Some(128.0));
    }

    #[test]
    fn plan_blend_can_run_the_intro_under_the_outro() {
        let s = MixSettings { entry: Entry::Intro, ..blend() };
        let spec = plan_blend(&out_facts(), &IncomingFacts { bpm: Some(126.0), in_s: 30.0 }, &s);
        assert_eq!(spec.length_s, 15.0);
        assert_eq!(spec.incoming_start_s, 15.0);
    }

    #[test]
    fn plan_blend_falls_back_to_plain_when_far_apart() {
        let spec = plan_blend(&out_facts(), &IncomingFacts { bpm: Some(100.0), in_s: 30.0 }, &blend());
        assert!(spec.sync.is_none());
        assert_eq!(spec.curve, Curve::Linear);
        assert_eq!(spec.length_s, 4.0);
        assert_eq!(spec.incoming_start_s, 30.0);
    }

    #[test]
    fn plan_blend_honours_switches_independently() {
        let inc = IncomingFacts { bpm: Some(126.0), in_s: 30.0 };
        let no_echo = plan_blend(&out_facts(), &inc, &MixSettings { echo: false, ..blend() });
        assert!(no_echo.echo.is_none());
        assert!(no_echo.sync.is_some());
        assert!(no_echo.park_tail_s < 1.0);

        let no_sync = plan_blend(&out_facts(), &inc, &MixSettings { sync: false, ..blend() });
        assert!(no_sync.sync.is_none());
        assert!(no_sync.echo.is_some());

        let locked = plan_blend(
            &out_facts(),
            &inc,
            &MixSettings { key_lock: true, hold_tempo: true, ..blend() },
        );
        assert!(locked.sync.unwrap().key_lock);
        assert!(locked.sync.unwrap().hold);
    }

    #[test]
    fn plan_blend_carries_matched_tempo_to_next_plan() {
        let out = OutgoingFacts { rate: 1.02, ..out_facts() };
        let spec = plan_blend(&out, &IncomingFacts { bpm: Some(130.56), in_s: 30.0 }, &blend());
        close(spec.sync.unwrap().rate, 1.0, 1e-3);
        close(spec.sync.unwrap().to_bpm, 130.56, 1e-2);
    }

    #[test]
    fn plan_blend_intro_shorter_than_blend_starts_at_zero() {
        let s = MixSettings { entry: Entry::Intro, ..blend() };
        let spec = plan_blend(&out_facts(), &IncomingFacts { bpm: Some(128.0), in_s: 10.0 }, &s);
        assert_eq!(spec.length_s, 10.0);
        assert_eq!(spec.incoming_start_s, 0.0);
    }

    #[test]
    fn plan_blend_at_drop_short_intro_does_not_shorten() {
        let spec = plan_blend(&out_facts(), &IncomingFacts { bpm: Some(128.0), in_s: 10.0 }, &blend());
        assert_eq!(spec.length_s, 15.0);
        assert_eq!(spec.incoming_start_s, 10.0);
    }

    #[test]
    fn short_blend_is_a_quick_echo_out() {
        let s = short_blend(Some(128.0), true, 0.0);
        assert!(s.length_s < 2.0);
        assert_eq!(s.echo.unwrap().bpm, Some(128.0));
        assert!(short_blend(Some(128.0), false, 0.0).echo.is_none());
        assert_eq!(short_blend(Some(128.0), true, 32.0).incoming_start_s, 32.0);
    }

    #[test]
    fn effective_length_counts_start_and_pace_limit() {
        assert_eq!(effective_length_ms(Some(360_000.0), 0.0, None), 360_000.0);
        assert_eq!(effective_length_ms(Some(360_000.0), 30.0, None), 330_000.0);
        assert_eq!(effective_length_ms(Some(360_000.0), 30.0, Some(240.0)), 240_000.0);
        assert_eq!(effective_length_ms(Some(200_000.0), 30.0, Some(240.0)), 170_000.0);
        assert_eq!(effective_length_ms(None, 30.0, Some(240.0)), 0.0);
    }

    #[test]
    fn kinds_shape_the_plan() {
        let inc = IncomingFacts { bpm: Some(128.0), in_s: 30.0 };
        let cut = plan_blend(&out_facts(), &inc, &MixSettings { transition: TransitionKind::Cut, ..blend() });
        assert!(cut.length_s < 0.05);
        let eo = plan_blend(&out_facts(), &inc, &MixSettings { transition: TransitionKind::EchoOut, ..blend() });
        assert!(eo.length_s >= 2.0 && eo.length_s <= 6.0);
        assert!(eo.echo.is_some());
        let bs = plan_blend(&out_facts(), &inc, &MixSettings { transition: TransitionKind::BassSwap, ..blend() });
        assert_eq!(bs.length_s, 15.0);
    }
}
