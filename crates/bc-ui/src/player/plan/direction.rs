//! Where the music goes next, as three-way switches: tempo, tags, energy, plus how strict the
//! harmonic filter is. Each is a group of `aria-pressed` buttons so the whole direction reads at a glance.
use bc_types::player::PlanOp;
use bc_types::suggest::{EnergyDir, Harmonic, TagMode, Tempo};
use leptos::prelude::*;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::Planner;
use super::tag_picker::TagPicker;
use super::tag_rules::TagRulesBar;
use crate::ds::Icon;

pub(super) fn ser<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}
fn parse<T: DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.into())).ok()
}

type Opt = (&'static str, &'static str, &'static str, &'static str);

#[component]
fn Seg(name: &'static str, #[prop(into)] value: Signal<String>, options: Vec<Opt>, on_pick: Callback<String>) -> impl IntoView {
    view! {
        <div class="pp-seg">
            <span class="pp-lbl">{name}</span>
            <div class="segmented" role="group" aria-label=name>
                {options.into_iter().map(|(v, label, icon, title)| view! {
                    <button type="button" title=title aria-pressed=move || (value.get() == v).to_string() on:click=move |_| on_pick.run(v.to_string())>
                        {(!icon.is_empty()).then(|| view! { <Icon name=icon size=11 /> })}{label}
                    </button>
                }).collect_view()}
            </div>
        </div>
    }
}

const TEMPO: [Opt; 3] = [
    ("lower", "lower", "arrow-down", "Ease the tempo down a few percent"),
    ("keep", "keep", "minus", "Hold the tempo"),
    ("raise", "raise", "arrow-up", "Push the tempo up a few percent"),
];
const TAGS: [Opt; 3] = [
    ("stick", "stick", "bookmark", "Stay inside the current tags"),
    ("drift", "drift", "shuffle", "Neighbouring tags welcome"),
    ("switch", "switch", "git-merge", "Head for other tags"),
];
const ENERGY: [Opt; 3] = [
    ("down", "down", "arrow-down", "Bring the energy down"),
    ("keep", "keep", "minus", "Hold the energy"),
    ("up", "up", "arrow-up", "Lift the energy"),
];
const HARMONIC: [Opt; 3] = [
    ("strict", "strict", "", "Only keys that mix cleanly"),
    ("loose", "loose", "", "Allow risky key moves"),
    ("off", "off", "", "Ignore key and tempo"),
];

#[component]
pub fn DirectionBar(pl: Planner) -> impl IntoView {
    let tempo = Signal::derive(move || pl.plan.with(|p| ser::<Tempo>(&p.tempo)));
    let energy = Signal::derive(move || pl.plan.with(|p| ser::<EnergyDir>(&p.energy)));
    let tag_mode = Signal::derive(move || pl.plan.with(|p| ser::<TagMode>(&p.tag_mode)));
    let harmonic = Signal::derive(move || pl.plan.with(|p| ser::<Harmonic>(&p.harmonic)));
    let targets = Signal::derive(move || pl.plan.with(|p| p.target_tags.clone()));
    view! {
        <section class="pp-sec" aria-label="Direction">
            <Seg name="tempo" value=tempo options=TEMPO.to_vec() on_pick=Callback::new(move |v: String| if let Some(t) = parse::<Tempo>(&v) { pl.op(PlanOp::SetTempo { tempo: t }) }) />
            <Seg name="tags" value=tag_mode options=TAGS.to_vec() on_pick=Callback::new(move |v: String| if let Some(m) = parse::<TagMode>(&v) { pl.op(PlanOp::SetTagMode { mode: m }) }) />
            <Show when=move || tag_mode.get() == "switch">
                <div class="pp-indent">
                    <TagPicker value=targets on_toggle=Callback::new(move |t: String| pl.op(PlanOp::ToggleTargetTag { tag: t }))
                        placeholder="towards which tags?" label="Target tags" />
                </div>
            </Show>
            <TagRulesBar pl=pl />
            <Seg name="energy" value=energy options=ENERGY.to_vec() on_pick=Callback::new(move |v: String| if let Some(e) = parse::<EnergyDir>(&v) { pl.op(PlanOp::SetEnergy { energy: e }) }) />
            <Seg name="keys" value=harmonic options=HARMONIC.to_vec() on_pick=Callback::new(move |v: String| if let Some(h) = parse::<Harmonic>(&v) { pl.op(PlanOp::SetHarmonic { harmonic: h }) }) />
        </section>
    }
}
