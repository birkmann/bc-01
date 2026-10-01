//! The row every list in the planner, the set rail and the similar panel is built from: art,
//! title/artist, then the numbers a DJ reads at a glance (BPM, Camelot, energy sliver, length).
use bc_types::library::TrackOut;
use bc_types::analysis::Verdict;
use bc_types::player::{ItemOrigin, PlayerCommand, PlayerStatus, QueueItem};
use bc_types::sets::SetItemOut;
use bc_types::suggest::TrackBrief;
use leptos::prelude::*;

use crate::ds::Icon;
use crate::logic::format::format_duration_ms;
use crate::player::use_player;
use crate::widgets::common::Art;

/// What a row needs, whatever DTO it came from.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct RowTrack {
    pub id: i64,
    pub title: String,
    pub artist: String,
    pub artist_id: Option<i64>,
    pub album: Option<String>,
    pub release_id: Option<i64>,
    pub art_url: Option<String>,
    pub bpm: Option<f64>,
    pub camelot: Option<String>,
    pub energy: Option<f64>,
    pub duration_ms: Option<i64>,
    pub tags: Vec<String>,
    pub loved: bool,
}

impl From<&TrackBrief> for RowTrack {
    fn from(t: &TrackBrief) -> Self {
        Self {
            id: t.id,
            title: t.title.clone(),
            artist: t.artist.as_ref().map(|a| a.name.clone()).unwrap_or_default(),
            artist_id: t.artist.as_ref().map(|a| a.id),
            album: t.release.as_ref().map(|r| r.name.clone()),
            release_id: t.release.as_ref().map(|r| r.id),
            art_url: t.art_url.clone(),
            bpm: t.bpm,
            camelot: t.camelot.clone(),
            energy: t.energy,
            duration_ms: t.duration_ms,
            tags: t.tags.clone(),
            loved: t.loved,
        }
    }
}

impl From<&TrackOut> for RowTrack {
    fn from(t: &TrackOut) -> Self {
        Self {
            id: t.id,
            title: t.title.clone(),
            artist: t.artist.as_ref().map(|a| a.name.clone()).unwrap_or_default(),
            artist_id: t.artist.as_ref().map(|a| a.id),
            album: t.release.as_ref().map(|r| r.title.clone()),
            release_id: t.release.as_ref().map(|r| r.id),
            art_url: t.art_url.clone().or_else(|| t.release.as_ref().and_then(|r| r.art_url.clone())),
            bpm: t.bpm,
            camelot: t.camelot.clone(),
            energy: t.energy,
            duration_ms: t.duration_ms,
            tags: t.tags.clone(),
            loved: t.loved,
        }
    }
}

impl From<&QueueItem> for RowTrack {
    fn from(t: &QueueItem) -> Self {
        Self {
            id: t.track_id,
            title: t.title.clone(),
            artist: t.artist.clone().unwrap_or_default(),
            artist_id: t.artist_id,
            album: t.album.clone(),
            release_id: t.release_id,
            art_url: t.art_url.clone(),
            bpm: t.bpm,
            camelot: t.camelot.clone(),
            energy: t.energy,
            duration_ms: t.duration_ms,
            tags: t.tags.clone(),
            loved: t.loved,
        }
    }
}

impl From<&SetItemOut> for RowTrack {
    fn from(i: &SetItemOut) -> Self {
        Self {
            id: i.track_id.unwrap_or(0),
            title: i.title.clone(),
            artist: i.artist.clone(),
            art_url: i.art_url.clone(),
            bpm: i.effective_bpm.or(i.bpm),
            camelot: i.effective_camelot.clone().or_else(|| i.camelot.clone()),
            duration_ms: i.duration_ms,
            ..Default::default()
        }
    }
}

impl RowTrack {
    pub fn queue_item(&self) -> QueueItem {
        QueueItem {
            track_id: self.id,
            title: self.title.clone(),
            artist: (!self.artist.is_empty()).then(|| self.artist.clone()),
            artist_id: self.artist_id,
            album: self.album.clone(),
            release_id: self.release_id,
            duration_ms: self.duration_ms,
            bpm: self.bpm,
            camelot: self.camelot.clone(),
            energy: self.energy,
            tags: self.tags.clone(),
            loved: self.loved,
            art_url: self.art_url.clone(),
            origin: ItemOrigin::Library,
            ..Default::default()
        }
    }
}

/// Energy is stored 0..1 by some producers and 1..10 by others; show a 0..1 sliver either way.
pub fn energy_frac(e: f64) -> f64 {
    if e > 1.0 { (e / 10.0).clamp(0.0, 1.0) } else { e.clamp(0.0, 1.0) }
}

/// BPM / key chips and an energy sliver; each only when analysis produced it.
#[component]
pub fn Numbers(#[prop(into)] bpm: Option<f64>, #[prop(into)] camelot: Option<String>, #[prop(into)] energy: Option<f64>) -> impl IntoView {
    if bpm.is_none() && camelot.is_none() && energy.is_none() {
        return ().into_any();
    }
    view! {
        <span class="trow-nums">
            {bpm.map(|b| view! { <span class="trow-chip mono" title="BPM">{format!("{}", b.round() as i64)}</span> })}
            {camelot.map(|c| view! { <span class="trow-chip mono" title="Key (Camelot)">{c}</span> })}
            {energy.map(|e| {
                let f = (energy_frac(e) * 100.0).round();
                view! { <span class="trow-energy" title=format!("Energy {f}%")><i style=format!("height:{f}%")></i></span> }
            })}
        </span>
    }
    .into_any()
}

/// The headphones toggle: pre-listen from the intro on the cue deck.
#[component]
pub fn PreviewButton(track: RowTrack) -> impl IntoView {
    let p = use_player();
    let id = track.id;
    let on = Signal::derive(move || p.state.with(|s| s.preview.playing && s.preview.track_id == Some(id)));
    let t = track.title.clone();
    view! {
        <button type="button" class=move || if on.get() { "btn btn-ghost btn-sm btn-icon is-on" } else { "btn btn-ghost btn-sm btn-icon" }
            aria-pressed=move || on.get().to_string()
            title=move || if on.get() { "Stop pre-listen" } else { "Pre-listen from the intro" }
            aria-label=format!("Preview {t}")
            on:click=move |ev| {
                ev.stop_propagation();
                if on.get_untracked() {
                    p.cmd(PlayerCommand::PreviewStop);
                } else {
                    p.cmd(PlayerCommand::PreviewStart { item: track.queue_item(), at_s: None });
                }
            }>
            <Icon name=move || if on.get() { "volume" } else { "headphones" } />
        </button>
    }
}

/// One list row. `lead` sits before the art (grip, position), `trail` after the numbers
/// (buttons), `below` is a second line under title/artist (verdicts, reasons).
#[component]
pub fn TrackRowView(
    track: RowTrack,
    #[prop(optional)] lead: Option<ChildrenFn>,
    #[prop(optional)] trail: Option<ChildrenFn>,
    #[prop(optional)] below: Option<ChildrenFn>,
    #[prop(optional, into)] muted: Signal<bool>,
    #[prop(optional, into)] current: Signal<bool>,
    #[prop(optional, into)] on_click: Option<Callback<()>>,
    #[prop(optional, into)] class: String,
) -> impl IntoView {
    let cls = move || {
        let mut c = format!("trow {class}");
        if current.get() {
            c.push_str(" current");
        }
        if muted.get() {
            c.push_str(" muted");
        }
        if on_click.is_some() {
            c.push_str(" clickable");
        }
        c
    };
    let dur = track.duration_ms.map(|d| format_duration_ms(Some(d as f64)));
    let body = view! {
        {lead.map(|l| l())}
        <Art src=track.art_url.clone() size=32.0 />
        <span class="trow-main">
            <span class="trow-title truncate">{track.title.clone()}</span>
            <span class="trow-artist truncate">{track.artist.clone()}</span>
            {below.map(|b| b())}
        </span>
        <Numbers bpm=track.bpm camelot=track.camelot.clone() energy=track.energy />
        {dur.map(|d| view! { <span class="trow-dur mono">{d}</span> })}
        {trail.map(|t| t())}
    };
    match on_click {
        Some(cb) => view! { <button type="button" class=cls on:click=move |_| cb.run(())>{body}</button> }.into_any(),
        None => view! { <div class=cls>{body}</div> }.into_any(),
    }
}

#[allow(dead_code)]
fn _s(_: PlayerStatus) {}

/// Verdicts are status, never colour alone: an icon and a word.
#[component]
pub fn VerdictChip(verdict: Verdict, #[prop(into)] reason: String) -> impl IntoView {
    let (cls, icon) = match verdict {
        Verdict::Perfect | Verdict::Good | Verdict::Energy => ("vchip ok", "check"),
        Verdict::Risky => ("vchip warn", "alert"),
        Verdict::Clash => ("vchip danger", "x-circle"),
    };
    view! { <span class=cls title=reason><Icon name=icon size=11 />{verdict.as_str()}</span> }
}

/// String-verdict variant for suggestions (the DTO carries `key_verdict: String`).
#[component]
pub fn VerdictText(#[prop(into)] verdict: String, #[prop(into)] reason: String) -> impl IntoView {
    let (cls, icon) = match verdict.as_str() {
        "perfect" | "good" | "energy" => ("vchip ok", "check"),
        "risky" => ("vchip warn", "alert"),
        _ => ("vchip danger", "x-circle"),
    };
    view! { <span class=cls title=reason><Icon name=icon size=11 />{verdict}</span> }
}

