//! Settings > Audio: output and cue devices, buffer size, loudness normalisation,
//! key lock default, how DJ mix transitions are shaped, and the waveform style (stored in `ui.prefs`, mirrored to
//! localStorage `bc:wave:style`).
use bc_types::player::{DevicesInfo, KeyLockQuality, MixSettings, MixSettingsPatch, OutputTarget, PlayerCommand};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::common::{SysCard, Stat, QueryError, qh};
use super::logic::{demo_bar_heights, lufs_label, preview_bars};
use super::prefs::PrefsHandle;
use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Badge, Button, SegmentedControl, Select, SelectOption, Skeleton, Switch, Tone, Variant, toast_err, toast_ok};
use crate::player::use_player;

const DEFAULT: &str = "__default__";
const BUFFERS: [u32; 6] = [64, 128, 256, 512, 1024, 2048];
const TARGETS: [(f64, &str); 4] = [(-14.0, "Streaming (-14)"), (-16.0, "Podcast (-16)"), (-18.0, "Quiet (-18)"), (-23.0, "Broadcast (-23)")];

fn opt(v: &str) -> Option<String> {
    (v != DEFAULT && !v.is_empty()).then(|| v.to_string())
}

#[component]
pub fn AudioSection(prefs: PrefsHandle) -> impl IntoView {
    view! {
        <DevicesCard />
        <MixCard />
        <DjMixCard />
        <WaveCard prefs=prefs />
    }
}

#[component]
fn DevicesCard() -> impl IntoView {
    let q = qh(use_query::<DevicesInfo>(|| Some(QuerySpec::new("/player/devices", &["player.devices"]))));
    let output = RwSignal::new(DEFAULT.to_string());
    let cue = RwSignal::new(DEFAULT.to_string());
    let buffer = RwSignal::new(DEFAULT.to_string());
    let busy = RwSignal::new(false);
    let seeded = RwSignal::new(None::<OutputTarget>);
    // Seed the controls from the server target (and follow external changes while not dirty).
    Effect::new(move |_| {
        let Some(info) = q.data.get() else { return };
        let t = info.target.clone();
        if seeded.get_untracked().as_ref() != Some(&t) {
            output.set(t.device.clone().unwrap_or_else(|| DEFAULT.into()));
            cue.set(t.cue_device.clone().unwrap_or_else(|| DEFAULT.into()));
            buffer.set(t.buffer_frames.map(|b| b.to_string()).unwrap_or_else(|| DEFAULT.into()));
            seeded.set(Some(t));
        }
    });
    let dirty = Memo::new(move |_| {
        let Some(s) = seeded.get() else { return false };
        opt(&output.get()) != s.device || opt(&cue.get()) != s.cue_device || buffer.get().parse::<u32>().ok() != s.buffer_frames
    });
    let outputs = Signal::derive(move || {
        let mut v = vec![SelectOption::new(DEFAULT, "System default")];
        if let Some(i) = q.data.get() {
            v.extend(i.outputs.iter().map(|d| SelectOption::new(d.name.clone(), if d.is_default { format!("{} (default)", d.label) } else { d.label.clone() })));
        }
        v
    });
    let cues = Signal::derive(move || {
        let mut v = vec![SelectOption::new(DEFAULT, "None")];
        if let Some(i) = q.data.get() {
            v.extend(i.outputs.iter().map(|d| SelectOption::new(d.name.clone(), d.label.clone())));
        }
        v
    });
    let buffers = Signal::derive(move || {
        let mut v = vec![SelectOption::new(DEFAULT, "Device default")];
        v.extend(BUFFERS.iter().map(|b| SelectOption::new(b.to_string(), format!("{b} frames"))));
        v
    });
    let apply = move |_| {
        let target = OutputTarget { device: opt(&output.get_untracked()), cue_device: opt(&cue.get_untracked()), buffer_frames: buffer.get_untracked().parse().ok() };
        busy.set(true);
        spawn_local(async move {
            match api::put::<_, DevicesInfo>("/player/output", &target).await {
                Ok(info) => {
                    seeded.set(Some(info.target.clone()));
                    crate::data::cache::patch::<DevicesInfo>("/player/devices", |d| *d = info);
                    toast_ok("Audio output updated");
                }
                Err(e) => toast_err(&e.message()),
            }
            busy.set(false);
        });
    };
    let lat = Signal::derive(move || q.data.get().map(|i| format!("{:.1} ms", i.latency_ms)).unwrap_or_else(|| "-".into()));
    let rate = Signal::derive(move || q.data.get().map(|i| format!("{} Hz", i.sample_rate)).unwrap_or_else(|| "-".into()));
    let frames = Signal::derive(move || q.data.get().map(|i| i.buffer_frames.to_string()).unwrap_or_else(|| "-".into()));
    let xruns = Signal::derive(move || q.data.get().map(|i| i.xruns.to_string()).unwrap_or_else(|| "-".into()));
    let backend = Signal::derive(move || q.data.get().map(|i| i.backend.clone()).unwrap_or_default());
    // A cue device that was asked for but did not open: previews fall back to the main output.
    let cue_failed = Signal::derive(move || q.data.get().is_some_and(|i| i.target.cue_device.is_some() && i.cue_device.is_none() && i.backend.starts_with("cpal")));

    view! {
        <SysCard title="Output devices" icon="speaker" hint="The desktop engine plays through these devices. A cue device lets you pre-listen on headphones.">
            <QueryError q=q />
            <Show when=move || q.data.get().is_none() && q.error.get().is_none()>
                <Skeleton height="96px" />
            </Show>
            <Show when=move || q.data.get().is_some()>
                <div class="sys-grid">
                    <div class="field"><label>"Main output"</label>
                        <Select options=outputs value=output aria_label="Main output device" /></div>
                    <div class="field"><label>"Cue / headphones"</label>
                        <Select options=cues value=cue aria_label="Cue device" /></div>
                    <div class="field"><label>"Buffer size"</label>
                        <Select options=buffers value=buffer aria_label="Buffer size" /></div>
                </div>
                <Show when=move || cue_failed.get()>
                    <p class="sys-hint">"The cue device could not be opened, so previews play through the main output."</p>
                </Show>
                <p class="sys-hint faint">"Smaller buffers mean lower latency but risk dropouts (xruns). Changing the output restarts the audio stream."</p>
                <div class="row gap wrap">
                    <Button variant=Variant::Primary icon="check" busy=busy disabled=Signal::derive(move || !dirty.get()) on_click=apply>"Apply"</Button>
                    <Button icon="refresh" title="Rescan devices" on_click=move |_| {
                        spawn_local(async move {
                            match api::get::<DevicesInfo>("/player/devices").await {
                                Ok(info) => { crate::data::cache::patch::<DevicesInfo>("/player/devices", |d| *d = info); }
                                Err(e) => toast_err(&e.message()),
                            }
                        });
                    }>"Rescan"</Button>
                    <span class="spacer"></span>
                    <Badge icon="speaker">{move || backend.get()}</Badge>
                </div>
                <div class="sys-stats">
                    <Stat label="Sample rate" value=rate />
                    <Stat label="Buffer" value=frames />
                    <Stat label="Latency" value=lat />
                    <Stat label="Xruns" value=xruns />
                </div>
            </Show>
        </SysCard>
    }
}

#[component]
fn MixCard() -> impl IntoView {
    let player = use_player();
    let normalise = RwSignal::new(player.state.with_untracked(|s| s.mix_settings.normalise));
    let key_lock = RwSignal::new(player.state.with_untracked(|s| s.mix_settings.key_lock));
    let klq = RwSignal::new(String::from("balanced"));
    Effect::new(move |_| klq.set(match player.state.with(|s| s.mix_settings.key_lock_quality) { KeyLockQuality::Fast => "fast", KeyLockQuality::Balanced => "balanced", KeyLockQuality::High => "high" }.to_string()));
    let target = RwSignal::new(format!("{}", player.state.with_untracked(|s| s.mix_settings.target_lufs)));
    Effect::new(move |_| {
        let (n, k, t) = player.state.with(|s| (s.mix_settings.normalise, s.mix_settings.key_lock, s.mix_settings.target_lufs));
        normalise.set(n);
        key_lock.set(k);
        target.set(format!("{t}"));
    });
    let patch = move |p: MixSettingsPatch| player.cmd(PlayerCommand::SetMixSettings { patch: p });
    let opts: Vec<SelectOption> = TARGETS.iter().map(|(v, l)| SelectOption::new(format!("{v}"), *l)).collect();
    let current = Signal::derive(move || target.get().parse::<f64>().map(lufs_label).unwrap_or_default());
    view! {
        <SysCard title="Playback" icon="sliders" hint="Applies to the desktop engine; changes take effect immediately.">
            <div class="pref-row">
                <div class="grow"><div class="name">"Loudness normalisation"</div>
                    <div class="desc faint">"Trim each track towards the target using its measured loudness, so volume stays even between tracks."</div></div>
                <Switch value=normalise label="Loudness normalisation" on_change=Callback::new(move |v: bool| patch(MixSettingsPatch { normalise: Some(v), ..Default::default() })) />
            </div>
            <div class="pref-row">
                <div class="grow"><div class="name">"Normalisation target"</div>
                    <div class="desc faint">{move || format!("Currently {}. Streaming services use about -14 LUFS.", current.get())}</div></div>
                <div style="width:190px">
                    <Select options=opts value=target aria_label="Normalisation target"
                        on_change=Callback::new(move |v: String| if let Ok(t) = v.parse::<f64>() { patch(MixSettingsPatch { target_lufs: Some(t), ..Default::default() }) }) />
                </div>
            </div>
            <div class="pref-row">
                <div class="grow"><div class="name">"Key lock when changing tempo"</div>
                    <div class="desc faint">"Keep the pitch when a track is sped up or slowed down to match the mix. The engine uses its high-quality time stretcher."</div></div>
                <Switch value=key_lock label="Key lock" on_change=Callback::new(move |v: bool| patch(MixSettingsPatch { key_lock: Some(v), ..Default::default() })) />
            </div>
            <div class="pref-row">
                <div class="grow"><div class="name">"Key-lock quality"</div>
                    <div class="desc faint">"Fast is light on the CPU, High sounds best on sustained material."</div></div>
                <div style="width:190px">
                    <Select options=Signal::derive(|| vec![SelectOption::new("fast", "Fast"), SelectOption::new("balanced", "Balanced"), SelectOption::new("high", "High")])
                        value=klq aria_label="Key-lock quality"
                        on_change=Callback::new(move |v: String| {
                            let q = match v.as_str() { "fast" => KeyLockQuality::Fast, "high" => KeyLockQuality::High, _ => KeyLockQuality::Balanced };
                            patch(MixSettingsPatch { key_lock_quality: Some(q), ..Default::default() })
                        }) />
                </div>
            </div>
        </SysCard>
    }
}

const STYLES: [(&str, &str); 6] = [
    ("eq_blend", "DJ blend (EQ)"),
    ("blend", "Crossfade"),
    ("bass_swap", "Bass swap"),
    ("filter", "Filter sweep"),
    ("echo_out", "Echo out"),
    ("cut", "Cut"),
];
const RETURNS: [(&str, &str); 5] = [
    ("15", "Glide back over 15 s"),
    ("30", "Glide back over 30 s"),
    ("60", "Glide back over 1 min"),
    ("120", "Glide back over 2 min"),
    ("hold", "Stay at the matched tempo"),
];

/// A serde enum as its stored name (`eq_blend`, `phrase`, ...).
fn tag<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn untag<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

/// A control value bound to one mix setting: follows the player state, and sends
/// a patch when the user changes it (only then: the state echoing back is a no-op).
fn bind_mix(read: fn(&MixSettings) -> String, write: fn(&str) -> Option<MixSettingsPatch>) -> RwSignal<String> {
    let player = use_player();
    let v = RwSignal::new(player.state.with_untracked(|s| read(&s.mix_settings)));
    Effect::new(move |_| v.set(player.state.with(|s| read(&s.mix_settings))));
    Effect::new(move |_| {
        let cur = v.get();
        if player.state.with_untracked(|s| read(&s.mix_settings)) != cur
            && let Some(p) = write(&cur) {
                player.cmd(PlayerCommand::SetMixSettings { patch: p });
            }
    });
    v
}

#[component]
fn DjMixCard() -> impl IntoView {
    let player = use_player();
    let style = bind_mix(|m| tag(&m.transition), |v| Some(MixSettingsPatch { transition: Some(untag(v)?), ..Default::default() }));
    let length = bind_mix(|m| m.length_beats.to_string(), |v| Some(MixSettingsPatch { length_beats: Some(v.parse().ok()?), ..Default::default() }));
    let entry = bind_mix(|m| tag(&m.entry), |v| Some(MixSettingsPatch { entry: Some(untag(v)?), ..Default::default() }));
    let quantise = bind_mix(|m| tag(&m.quantise), |v| Some(MixSettingsPatch { quantise: Some(untag(v)?), ..Default::default() }));
    let ret = bind_mix(
        |m| if m.hold_tempo { "hold".into() } else { format!("{}", m.glide_back_s.round() as i64) },
        |v| Some(match v {
            "hold" => MixSettingsPatch { hold_tempo: Some(true), ..Default::default() },
            s => MixSettingsPatch { hold_tempo: Some(false), glide_back_s: Some(s.parse().ok()?), ..Default::default() },
        }),
    );
    let flag = |f: fn(&MixSettings) -> bool| {
        let v = RwSignal::new(player.state.with_untracked(|s| f(&s.mix_settings)));
        Effect::new(move |_| v.set(player.state.with(|s| f(&s.mix_settings))));
        v
    };
    let sync = flag(|m| m.sync);
    let phase_lock = flag(|m| m.phase_lock);
    let echo = flag(|m| m.echo);
    let patch = move |p: MixSettingsPatch| player.cmd(PlayerCommand::SetMixSettings { patch: p });
    let styles = Signal::derive(|| STYLES.iter().map(|(v, l)| SelectOption::new(*v, *l)).collect::<Vec<_>>());
    let returns = Signal::derive(|| RETURNS.iter().map(|(v, l)| SelectOption::new(*v, *l)).collect::<Vec<_>>());
    let length_hint = Signal::derive(move || {
        let beats = length.get().parse::<f64>().unwrap_or(64.0);
        format!("Measured in beats of the playing track: about {:.0} s at 128 BPM. A blend never starts before half-way through a track.", beats * 60.0 / 128.0)
    });
    view! {
        <SysCard title="DJ mix" icon="mix" hint="How the player blends one track into the next when Mix is on.">
            <div class="pref-row">
                <div class="grow"><div class="name">"Transition style"</div>
                    <div class="desc faint">"DJ blend brings the next track in with its bass cut, swaps the basslines on a bar and then lets the old track's mids and highs go."</div></div>
                <div style="width:190px"><Select options=styles value=style aria_label="Transition style" /></div>
            </div>
            <div class="pref-row">
                <div class="grow"><div class="name">"Blend length"</div>
                    <div class="desc faint">{move || length_hint.get()}</div></div>
                <SegmentedControl options=vec![("16", "4 bars"), ("32", "8 bars"), ("64", "16 bars"), ("128", "32 bars")] value=length />
            </div>
            <div class="pref-row">
                <div class="grow"><div class="name">"Bring the next track in"</div>
                    <div class="desc faint">"Over the intro lines its drop up with the bass swap; at the drop starts it where the main part begins."</div></div>
                <SegmentedControl options=vec![("intro", "Over the intro"), ("drop", "At the drop")] value=entry />
            </div>
            <div class="pref-row">
                <div class="grow"><div class="name">"Start the blend on"</div>
                    <div class="desc faint">"Waits for the next beat, bar or 4-bar phrase of the playing track. Falls back to a bar when a phrase would come too late."</div></div>
                <SegmentedControl options=vec![("beat", "Beat"), ("bar", "Bar"), ("phrase", "Phrase")] value=quantise />
            </div>
            <div class="pref-row">
                <div class="grow"><div class="name">"Beatmatch"</div>
                    <div class="desc faint">"Play the next track at the current tempo when the two are within 6 % (half and double time count)."</div></div>
                <Switch value=sync label="Beatmatch" on_change=Callback::new(move |v: bool| patch(MixSettingsPatch { sync: Some(v), ..Default::default() })) />
            </div>
            <div class="pref-row">
                <div class="grow"><div class="name">"Phase lock"</div>
                    <div class="desc faint">"Keep the beats of both tracks locked together for the whole blend."</div></div>
                <Switch value=phase_lock label="Phase lock" on_change=Callback::new(move |v: bool| patch(MixSettingsPatch { phase_lock: Some(v), ..Default::default() })) />
            </div>
            <div class="pref-row">
                <div class="grow"><div class="name">"After the blend"</div>
                    <div class="desc faint">"A beatmatched track eases back to its own tempo once the old one is gone."</div></div>
                <div style="width:190px"><Select options=returns value=ret aria_label="Tempo after the blend" /></div>
            </div>
            <div class="pref-row">
                <div class="grow"><div class="name">"Echo out"</div>
                    <div class="desc faint">"Throw the old track into an echo as it leaves. The DJ blend only echoes when the tempos can't be matched."</div></div>
                <Switch value=echo label="Echo out" on_change=Callback::new(move |v: bool| patch(MixSettingsPatch { echo: Some(v), ..Default::default() })) />
            </div>
        </SysCard>
    }
}

#[component]
fn WaveCard(prefs: PrefsHandle) -> impl IntoView {
    let _ = prefs;
    let style = crate::prefs::wave_style_pref();
    let player_style = crate::prefs::bind_pref(|p| p.player_wave_style.clone(), |p, v| p.player_wave_style = v);
    let bars = preview_bars(72);
    let bars2 = bars.clone();
    let demo = demo_bar_heights(48);
    let theme = crate::theme::use_theme();
    view! {
        <SysCard title="Waveform style" icon="waveform" hint="How waveforms are coloured in the player, decks and set planner.">
            <div class="pref-row">
                <div class="grow"><div class="name">"Player bar"</div></div>
                <SegmentedControl options=vec![("bars", "Bars"), ("rgb", "Spectral")] value=player_style />
            </div>
            <div class="pref-row">
                <div class="grow"><div class="name">"Deck and set planner"</div></div>
                <SegmentedControl options=vec![("rgb", "RGB spectral"), ("bands", "Three bands"), ("mono", "Mono")] value=style />
            </div>
            <div class="pref-row"><div class="grow"><div class="name faint">"Player bar preview"</div></div></div>
            <svg class="wave-preview" viewBox="0 0 144 40" preserveAspectRatio="none" role="img" aria-label="Preview of the bars waveform">
                {move || {
                    let colors = bc_types::theme::resolve_colors(&theme.active());
                    let played = colors.get("wave-played").cloned().unwrap_or_else(|| "#f0f0f0".into());
                    let unplayed = colors.get("wave-unplayed").cloned().unwrap_or_else(|| "#2ec7e6".into());
                    demo.iter().enumerate().map(|(i, h)| {
                        let hh = (*h as f64 * 36.0).max(1.0);
                        let fill = if i < 18 { played.clone() } else { unplayed.clone() };
                        view! { <rect x=i as f64 * 3.0 + 0.2 y=20.0 - hh / 2.0 width="2" height=hh fill=fill /> }
                    }).collect_view()
                }}
            </svg>
            <div class="pref-row"><div class="grow"><div class="name faint">"Deck preview"</div></div></div>
            <svg class="wave-preview" viewBox="0 0 144 40" preserveAspectRatio="none" role="img"
                aria-label=move || format!("Preview of the {} waveform style", style.get())>
                {move || {
                    let s = style.get();
                    bars2.iter().enumerate().map(|(i, (lo, mid, hi))| {
                        let x = i as f64 * 2.0 + 0.2;
                        match s.as_str() {
                            "bands" => view! {
                                <g>
                                    <rect x=x y=20.0 - lo * 18.0 width="1.4" height=lo * 18.0 class="wv-lo" />
                                    <rect x=x y=20.0 width="1.4" height=mid * 12.0 class="wv-mid" />
                                    <rect x=x y=20.0 - hi * 6.0 width="1.4" height=hi * 6.0 class="wv-hi" />
                                </g>
                            }.into_any(),
                            "mono" => {
                                let h = (lo + mid + hi) / 3.0 * 36.0;
                                view! { <rect x=x y=20.0 - h / 2.0 width="1.4" height=h class="wv-mono" /> }.into_any()
                            }
                            _ => {
                                let h = (lo * 0.6 + mid * 0.3 + hi * 0.1) * 36.0;
                                let (a, b, c) = (lo / (lo + mid + hi), mid / (lo + mid + hi), hi / (lo + mid + hi));
                                view! {
                                    <g>
                                        <rect x=x y=20.0 - h / 2.0 width="1.4" height=h * a class="wv-lo" />
                                        <rect x=x y=20.0 - h / 2.0 + h * a width="1.4" height=h * b class="wv-mid" />
                                        <rect x=x y=20.0 - h / 2.0 + h * (a + b) width="1.4" height=h * c class="wv-hi" />
                                    </g>
                                }.into_any()
                            }
                        }
                    }).collect_view()
                }}
            </svg>
            <p class="sys-hint faint">"Saved with your other preferences and shared across devices."</p>
        </SysCard>
    }
}

#[allow(dead_code)]
fn _t(_: Tone) {}
