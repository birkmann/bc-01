//! Settings > Appearance: theme gallery, the live theme editor (Quick / Advanced,
//! contrast badges with WCAG labels, fonts, sizes), share (file / clipboard) and
//! display preferences. Everything edits `ThemeCtx.draft`, which `theme.rs` applies
//! to `<html>` on every change: the whole app is the live preview.
use bc_types::theme::*;
use bc_types::ui::UiPrefs;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use super::common::{Notice, SysCard};
use super::logic::{FontChoice, Grade, font_choice, stack_for_preset};
use super::prefs::PrefsHandle;
use crate::ds::{Badge, Button, Icon, SegmentedControl, Select, SelectOption, Size, Switch, Tone, Variant, confirm, toast_err, toast_ok};
use crate::theme::{ThemeCtx, use_theme};
use crate::util::{copy_text, download_text, entropy};

const CUSTOM: &str = "__custom__";

fn edit(draft: RwSignal<Option<ThemeSpec>>, f: impl FnOnce(&mut ThemeSpec)) {
    draft.update(|d| {
        if let Some(d) = d {
            f(d)
        }
    });
}

fn enum_id<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

fn enum_from<T: serde::de::DeserializeOwned>(id: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(id.to_string())).ok()
}

/// A segmented control whose value is pushed into the draft (skipping the first run).
fn seg_bind(draft: RwSignal<Option<ThemeSpec>>, init: String, apply: impl Fn(&mut ThemeSpec, &str) + Send + Sync + 'static) -> RwSignal<String> {
    let sig = RwSignal::new(init);
    Effect::new(move |prev: Option<()>| {
        let v = sig.get();
        if prev.is_some() {
            edit(draft, |d| apply(d, &v));
        }
    });
    sig
}

fn mode_label(m: Mode) -> &'static str {
    if m == Mode::Light { "Light" } else { "Dark" }
}

// ---------------------------------------------------------------------------------------------

#[component]
pub fn AppearanceSection(prefs: PrefsHandle) -> impl IntoView {
    let ctx = use_theme();
    // Leaving Settings mid-edit must not leave the app half-themed.
    on_cleanup(move || ctx.draft.set(None));

    view! {
        <SysCard title="Theme" icon="palette"
            hint="Pick a theme or make your own. Edits apply to the whole app instantly and are only saved when you press Save."
            actions=crate::ds::children(move || view! { <ThemeActions ctx=ctx /> })>
            <ThemeGallery ctx=ctx />
            <Show when=move || ctx.draft.with(|d| d.is_some())>
                <ThemeEditor ctx=ctx />
            </Show>
        </SysCard>
        <ShareThemes ctx=ctx />
        <DisplayPrefs prefs=prefs />
    }
}

/// Header actions: customise / duplicate the active theme.
#[component]
fn ThemeActions(ctx: ThemeCtx) -> impl IntoView {
    view! {
        <Show when=move || ctx.draft.with(|d| d.is_none())>
            <Button size=Size::Sm icon="copy" title="Duplicate this theme as a new one" on_click=move |_| {
                let mut s = ctx.active_untracked();
                s.id = new_theme_id(entropy());
                s.name = format!("{} copy", s.name).chars().take(40).collect();
                ctx.draft.set(Some(s));
            }><span class="hide-sm">"Duplicate"</span></Button>
            <Button size=Size::Sm variant=Variant::Primary icon="sliders" on_click=move |_| ctx.draft.set(Some(ctx.active_untracked()))>
                <span>"Customize"</span>
            </Button>
        </Show>
    }
}

#[component]
fn ThemeGallery(ctx: ThemeCtx) -> impl IntoView {
    let all = Signal::derive(move || {
        let mut v = builtin_themes();
        v.extend(ctx.store.with(|s| s.themes.clone()));
        v
    });
    let selected = Signal::derive(move || {
        ctx.store.with(|s| if s.mode == Mode::Dark { s.dark.clone() } else { s.light.clone() })
    });
    view! {
        <div class="theme-grid" role="list">
            <For each=move || all.get() key=|t| (t.id.clone(), t.name.clone(), t.colors.len()) let:spec>
                <ThemeCard ctx=ctx spec=spec selected=selected />
            </For>
        </div>
        <p class="sys-hint faint">"The sun/moon button in the sidebar switches between your last-used dark and light themes."</p>
    }
}

#[component]
fn ThemeCard(ctx: ThemeCtx, spec: ThemeSpec, selected: Signal<String>) -> impl IntoView {
    let c = resolve_colors(&spec);
    let g = |k: &str| c.get(k).cloned().unwrap_or_default();
    let (s1, s2, s3, acc, ink, faint, line) = (g("surface-1"), g("surface-2"), g("surface-3"), g("accent"), g("ink"), g("ink-faint"), g("line"));
    let id = spec.id.clone();
    let id2 = spec.id.clone();
    let id3 = spec.id.clone();
    let name = spec.name.clone();
    let builtin = is_builtin_id(&spec.id);
    let is_active = Signal::derive(move || ctx.draft.with(|d| d.is_none()) && selected.get() == id);
    let label = format!("{} theme ({})", spec.name, mode_label(spec.base));
    let name_del = spec.name.clone();
    view! {
        <div class="theme-card-wrap" role="listitem">
            <button type="button" class="theme-card" aria-pressed=move || is_active.get().to_string() aria-label=label
                on:click=move |_| ctx.select(&id2)>
                // Real colours of THAT theme (not tokens), so each card shows itself.
                <span class="theme-swatch" style=format!("background:{s1};border-color:{line}")>
                    <span class="theme-swatch-card" style=format!("background:{s2};border-color:{line}")>
                        <i class="dot-a" style=format!("background:{acc}")></i>
                        <i class="bar-a" style=format!("background:{ink}")></i>
                        <i class="bar-b" style=format!("background:{faint}")></i>
                    </span>
                    <span class="theme-swatch-card second" style=format!("background:{s3};border-color:{line}")></span>
                </span>
                <span class="theme-name">
                    <span class="truncate">{name}</span>
                    {move || is_active.get().then(|| view! { <Icon name="check" /> })}
                </span>
            </button>
            {(!builtin).then(|| {
                let id3 = id3.clone();
                let nm = name_del.clone();
                view! {
                    <button type="button" class="theme-del" title="Delete theme" aria-label=format!("Delete theme {nm}")
                        on:click=move |_| {
                            let id = id3.clone();
                            let nm = nm.clone();
                            spawn_local(async move {
                                if confirm("Delete this theme?", &format!("\"{nm}\" will be removed from your saved themes. Export it first if you want to keep a copy."), "Delete theme", true).await {
                                    ctx.delete(&id);
                                }
                            });
                        }>
                        <Icon name="trash" />
                    </button>
                }
            })}
        </div>
    }
}

// ---------------------------------------------------------------------------------------------
// Editor
// ---------------------------------------------------------------------------------------------

#[component]
fn ThemeEditor(ctx: ThemeCtx) -> impl IntoView {
    let draft = ctx.draft;
    let init = draft.get_untracked().unwrap_or_else(|| builtin_for_dark());
    let colors: Memo<ColorMap> = Memo::new(move |_| draft.with(|d| d.as_ref().map(resolve_colors).unwrap_or_default()));
    let name = RwSignal::new(init.name.clone());
    Effect::new(move |prev: Option<()>| {
        let n = name.get();
        if prev.is_some() {
            edit(draft, |d| d.name = n.chars().take(40).collect());
        }
    });
    let base = seg_bind(draft, enum_id(&init.base), |d, v| {
        if let Some(m) = enum_from::<Mode>(v) {
            d.base = m;
        }
    });
    let mode = RwSignal::new("quick".to_string());
    let is_builtin = Memo::new(move |_| draft.with(|d| d.as_ref().map(|d| is_builtin_id(&d.id)).unwrap_or(true)));
    let can_save = Memo::new(move |_| !name.get().trim().is_empty());

    let quick = Memo::new(move |_| quick_from(&colors.get()));
    let set_quick = move |patch: Box<dyn FnOnce(&mut QuickColors)>| {
        let mut q = quick.get_untracked();
        patch(&mut q);
        edit(draft, |d| {
            let derived = derive_quick(&q, d.base);
            d.colors.extend(derived);
        });
    };
    let set_quick = std::sync::Arc::new(set_quick);
    let (sq1, sq2, sq3) = (set_quick.clone(), set_quick.clone(), set_quick.clone());

    let below_aa = Memo::new(move |_| {
        let c = colors.get();
        COLOR_TOKENS.iter().filter(|t| contrast_against_surface(t, &c).map(|r| r < 4.5).unwrap_or(false)).map(|t| t.to_string()).collect::<Vec<_>>()
    });

    let ink_pairs = Signal::derive(move || {
        let c = colors.get();
        let r = |a: &str, b: &str| contrast(c.get(a).map(String::as_str).unwrap_or("#000000"), c.get(b).map(String::as_str).unwrap_or("#000000"));
        vec![
            ("Text on background", r("ink", "surface-1")),
            ("Muted text", r("ink-muted", "surface-1")),
            ("Accent on background", r("accent", "surface-1")),
            ("Text on accent buttons", r("accent-ink", "accent")),
        ]
    });

    let scale = seg_bind(draft, format!("{}", init.sizes.type_scale), |d, v| {
        if let Ok(x) = v.parse::<f64>() {
            d.sizes.type_scale = x;
        }
    });
    let radius = seg_bind(draft, enum_id(&init.sizes.radius), |d, v| {
        if let Some(r) = enum_from::<Radius>(v) {
            d.sizes.radius = r;
        }
    });
    let density = seg_bind(draft, enum_id(&init.sizes.density), |d, v| {
        if let Some(r) = enum_from::<Density>(v) {
            d.sizes.density = r;
        }
    });

    let save = move |as_new: bool| {
        if ctx.save_draft(as_new).is_some() {
            toast_ok("Theme saved");
        }
    };
    let delete = move |_| {
        let Some(d) = draft.get_untracked() else { return };
        spawn_local(async move {
            if confirm("Delete this theme?", &format!("\"{}\" will be removed from your saved themes. Export it first if you want to keep a copy.", d.name), "Delete theme", true).await {
                ctx.delete(&d.id);
            }
        });
    };

    view! {
        <div class="theme-editor" role="group" aria-label="Theme editor">
            <div class="te-top">
                <div class="field grow">
                    <label for="theme-name">"Theme name"</label>
                    <input id="theme-name" class="input" maxlength="40" placeholder="My theme"
                        prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                </div>
                <div class="field">
                    <label>"Base palette"</label>
                    <SegmentedControl options=vec![("dark", "Dark"), ("light", "Light")] value=base />
                </div>
            </div>
            <p class="sys-hint faint">"The base is the fallback for every colour you leave untouched."</p>

            <div class="te-mode">
                <SegmentedControl options=vec![("quick", "Quick"), ("advanced", "Advanced")] value=mode />
                <span class="spacer"></span>
                {move || {
                    let v = below_aa.get();
                    let n = v.len();
                    (n > 0).then(|| view! { <span title=format!("Below 4.5:1 against their surface: {}", v.join(", "))><Badge tone=Tone::Warn icon="alert">{format!("{n} below AA")}</Badge></span> })
                }}
            </div>

            <Show when=move || mode.get() == "quick">
                <div class="te-quick">
                    <ColorRow label="Background" token="surface-1" colors=colors desc="Page and panels"
                        on_set=Callback::new({ let s = sq1.clone(); move |h: String| s(Box::new(move |q| q.bg = h)) }) />
                    <ColorRow label="Text" token="ink" colors=colors desc="Body copy, titles"
                        on_set=Callback::new({ let s = sq2.clone(); move |h: String| s(Box::new(move |q| q.text = h)) }) />
                    <ColorRow label="Accent" token="accent" colors=colors desc="Buttons, focus, now playing"
                        on_set=Callback::new({ let s = sq3.clone(); move |h: String| s(Box::new(move |q| q.accent = h)) }) />
                </div>
                <p class="sys-hint faint">"Three colours derive every surface, border and muted text. Switch to Advanced to fine-tune single tokens."</p>
                <div class="contrast-table" role="table" aria-label="Contrast summary">
                    {move || ink_pairs.get().into_iter().map(|(label, ratio)| view! {
                        <div class="contrast-row" role="row">
                            <span role="cell">{label}</span>
                            <ContrastBadge ratio=ratio />
                        </div>
                    }).collect_view()}
                </div>
            </Show>

            <Show when=move || mode.get() == "advanced">
                <AdvancedColors draft=draft colors=colors />
            </Show>

            <ThemePreview />

            <h3 class="te-h">"Fonts"</h3>
            <div class="te-grid3">
                <FontPicker draft=draft label="Body" presets=SANS_PRESETS
                    get={|f: &ThemeFonts| f.sans.clone()} set={|f: &mut ThemeFonts, v: Option<String>| f.sans = v} />
                <FontPicker draft=draft label="Numbers and code" presets=MONO_PRESETS
                    get={|f: &ThemeFonts| f.mono.clone()} set={|f: &mut ThemeFonts, v: Option<String>| f.mono = v} />
                <FontPicker draft=draft label="Headings" presets=DISPLAY_PRESETS
                    get={|f: &ThemeFonts| f.display.clone()} set={|f: &mut ThemeFonts, v: Option<String>| f.display = v} />
            </div>
            <p class="sys-hint faint">"Only fonts installed on this device are used; a theme never downloads fonts."</p>

            <h3 class="te-h">"Sizes"</h3>
            <div class="te-grid3">
                <div class="field"><label>"Text size"</label>
                    <SegmentedControl options=vec![("0.9", "S"), ("1", "M"), ("1.1", "L"), ("1.2", "XL")] value=scale /></div>
                <div class="field"><label>"Corners"</label>
                    <SegmentedControl options=vec![("none", "Square"), ("small", "Subtle"), ("default", "Default"), ("large", "Soft"), ("round", "Round")] value=radius /></div>
                <div class="field"><label>"Row density"</label>
                    <SegmentedControl options=vec![("compact", "Compact"), ("default", "Default"), ("comfortable", "Roomy")] value=density /></div>
            </div>

            <div class="te-actions">
                <Button variant=Variant::Primary icon="check" disabled=Signal::derive(move || !can_save.get()) on_click=move |_| save(false)>
                    {move || if is_builtin.get() { "Save as new theme" } else { "Save" }}
                </Button>
                <Show when=move || !is_builtin.get()>
                    <Button icon="copy" disabled=Signal::derive(move || !can_save.get()) on_click=move |_| save(true)>"Save as copy"</Button>
                </Show>
                <Button icon="refresh" title="Drop the changes" on_click=move |_| draft.set(None)>"Revert"</Button>
                <span class="spacer"></span>
                <Show when=move || !is_builtin.get()>
                    <Button variant=Variant::Danger icon="trash" on_click=delete>"Delete"</Button>
                </Show>
            </div>
        </div>
    }
}

fn builtin_for_dark() -> ThemeSpec {
    find_builtin("builtin:dark").expect("builtin")
}

/// Contrast ratio + WCAG grade. Icon and label always accompany the colour.
#[component]
pub fn ContrastBadge(ratio: f64) -> impl IntoView {
    let g = Grade::of(ratio);
    let (cls, icon) = match g {
        Grade::Aaa | Grade::Aa => ("ok", "check-circle"),
        Grade::AaLarge => ("warn", "alert"),
        Grade::Fail => ("danger", "x-circle"),
    };
    view! {
        <span class=format!("contrast {cls}") title=g.hint() aria-label=format!("Contrast {ratio:.1} to 1, {}", g.label())>
            <Icon name=icon />
            <span class="mono">{format!("{ratio:.1}:1")}</span>
            <b>{g.label()}</b>
        </span>
    }
}

/// Swatch (native colour input, live) + hex field (committed on Enter/blur) + contrast badge.
#[component]
fn ColorRow(
    #[prop(into)] label: String,
    #[prop(into)] token: String,
    colors: Memo<ColorMap>,
    on_set: Callback<String>,
    #[prop(optional, into)] desc: String,
    #[prop(optional, into)] overridden: MaybeProp<bool>,
    #[prop(optional, into)] on_reset: Option<Callback<()>>,
) -> impl IntoView {
    let tok = token.clone();
    let value = Signal::derive(move || colors.with(|c| c.get(&tok).cloned().unwrap_or_else(|| "#000000".into())));
    let text = RwSignal::new(value.get_untracked());
    Effect::new(move |_| text.set(value.get()));
    let bad = RwSignal::new(false);
    let tok2 = token.clone();
    let ratio = Signal::derive(move || colors.with(|c| contrast_against_surface(&tok2, c)));
    let id = format!("c-{token}-{}", label.to_lowercase().replace(' ', "-"));
    let commit = move || {
        let t = text.get_untracked();
        match normalize_hex(&t) {
            Some(h) => {
                bad.set(false);
                if h != value.get_untracked() {
                    on_set.run(h);
                }
            }
            None => {
                bad.set(true);
            }
        }
    };
    let commit2 = commit.clone();
    view! {
        <div class="color-row">
            <input type="color" class="swatch" aria-label=format!("{label} colour") prop:value=move || value.get()
                on:input=move |ev| { let v = event_target_value(&ev); if let Some(h) = normalize_hex(&v) { if h != value.get_untracked() { on_set.run(h); } } } />
            <div class="meta">
                <label for=id.clone() class="name">{label.clone()}{move || overridden.get().unwrap_or(false).then(|| view! { <i class="ovr" title="Overrides the base palette"></i> })}</label>
                {(!desc.is_empty()).then(|| view! { <span class="desc faint">{desc.clone()}</span> })}
            </div>
            <input id=id class="input mono hex" spellcheck="false" maxlength="9" aria-invalid=move || bad.get().to_string()
                prop:value=move || text.get()
                on:input=move |ev| { text.set(event_target_value(&ev)); bad.set(false); }
                on:change=move |_| commit()
                on:blur=move |_| { commit2(); if bad.get_untracked() { text.set(value.get_untracked()); bad.set(false); } }
                on:keydown=move |ev| if ev.key() == "Enter" { ev.prevent_default(); } />
            <span class="badge-slot">{move || ratio.get().map(|r| view! { <ContrastBadge ratio=r /> })}</span>
            {on_reset.map(|cb| view! {
                <button type="button" class="btn btn-ghost btn-sm btn-icon" title="Reset to base" aria-label=format!("Reset {token}")
                    disabled=move || !overridden.get().unwrap_or(false) on:click=move |_| cb.run(())>
                    <Icon name="refresh" />
                </button>
            })}
        </div>
    }
}

#[component]
fn AdvancedColors(draft: RwSignal<Option<ThemeSpec>>, colors: Memo<ColorMap>) -> impl IntoView {
    let n_over = Memo::new(move |_| draft.with(|d| d.as_ref().map(|d| d.colors.len()).unwrap_or(0)));
    view! {
        <div class="te-adv">
            <p class="sys-hint faint">{move || format!("{} overrides on top of the base palette. Contrast is measured against the surface the token usually sits on.", n_over.get())}</p>
            {COLOR_GROUPS.iter().map(|(group, tokens)| view! {
                <div class="te-group">
                    <h4>{*group}</h4>
                    {tokens.iter().map(|&token| {
                        let overridden = Signal::derive(move || draft.with(|d| d.as_ref().map(|d| {
                            d.colors.get(token).map(|v| v != &base_colors(d.base)[token]).unwrap_or(false)
                        }).unwrap_or(false)));
                        view! {
                            <ColorRow label=token token=token colors=colors overridden=overridden
                                on_set=Callback::new(move |h: String| edit(draft, |d| { d.colors.insert(token.to_string(), h); }))
                                on_reset=Callback::new(move |_| edit(draft, |d| { d.colors.remove(token); })) />
                        }
                    }).collect_view()}
                </div>
            }).collect_view()}
        </div>
    }
}

/// A sample of real components, themed live like everything else.
#[component]
fn ThemePreview() -> impl IntoView {
    view! {
        <div class="te-preview" aria-hidden="true">
            <div class="row" style="gap:8px;flex-wrap:wrap">
                <span class="btn btn-primary btn-sm">"Primary"</span>
                <span class="btn btn-outline btn-sm">"Outline"</span>
                <span class="btn btn-danger btn-sm">"Danger"</span>
                <span class="badge badge-ok"><Icon name="check-circle" />"Ok"</span>
                <span class="badge badge-warn"><Icon name="alert" />"Warn"</span>
                <span class="badge badge-danger"><Icon name="x-circle" />"Failed"</span>
                <span class="chip">"Techno"</span><span class="chip on">"House"</span>
            </div>
            <div class="te-prev-row">
                <span class="mono faint">"128"</span>
                <span class="grow"><b>"Night Drive"</b><span class="muted">" - Some Artist"</span></span>
                <span class="camelot">"8A"</span>
                <span class="mono muted">"6:42"</span>
            </div>
            <div class="meter"><i style="width:62%"></i></div>
        </div>
    }
}

#[component]
fn FontPicker(
    draft: RwSignal<Option<ThemeSpec>>,
    #[prop(into)] label: String,
    presets: &'static [FontPreset],
    get: fn(&ThemeFonts) -> Option<String>,
    set: fn(&mut ThemeFonts, Option<String>),
) -> impl IntoView {
    let cur = draft.with_untracked(|d| d.as_ref().and_then(|d| get(&d.fonts)));
    let choice = font_choice(presets, cur.as_deref());
    let custom = RwSignal::new(choice == FontChoice::Custom);
    let text = RwSignal::new(if custom.get_untracked() { cur.clone().unwrap_or_default() } else { String::new() });
    let error = RwSignal::new(None::<String>);
    let selected = RwSignal::new(match choice {
        FontChoice::Preset(id) => id,
        FontChoice::Custom => CUSTOM.to_string(),
    });
    let mut opts: Vec<SelectOption> = presets.iter().map(|p| SelectOption::new(p.id, p.label)).collect();
    opts.push(SelectOption::new(CUSTOM, "Custom..."));
    let commit = move || {
        let raw = text.get_untracked();
        if raw.trim().is_empty() {
            error.set(None);
            edit(draft, |d| set(&mut d.fonts, None));
            return;
        }
        match sanitize_font_stack(&raw) {
            Some(clean) => {
                error.set(None);
                edit(draft, |d| set(&mut d.fonts, Some(clean)));
            }
            None => error.set(Some("Use comma-separated font names (letters, digits, spaces).".into())),
        }
    };
    let commit2 = commit.clone();
    let lbl = label.clone();
    view! {
        <div class="field">
            <label>{label}</label>
            <Select options=opts value=selected aria_label=lbl
                on_change=Callback::new(move |v: String| {
                    if v == CUSTOM {
                        custom.set(true);
                    } else {
                        custom.set(false);
                        error.set(None);
                        let stack = stack_for_preset(presets, &v);
                        edit(draft, |d| set(&mut d.fonts, stack));
                    }
                }) />
            <Show when=move || custom.get()>
                <input class="input" placeholder="Font name, another font" spellcheck="false"
                    aria-invalid=move || error.get().is_some().to_string()
                    prop:value=move || text.get()
                    on:input=move |ev| text.set(event_target_value(&ev))
                    on:change=move |_| commit()
                    on:blur=move |_| commit2() />
            </Show>
            <Notice text=error />
        </div>
    }
}

// ---------------------------------------------------------------------------------------------
// Share
// ---------------------------------------------------------------------------------------------

#[component]
fn ShareThemes(ctx: ThemeCtx) -> impl IntoView {
    let pasted = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let file = NodeRef::<leptos::html::Input>::new();
    let json = move || serialize_theme(&ctx.active_untracked());

    let do_import = move |text: String| match ctx.import(&text) {
        Ok(_) => {
            error.set(None);
            pasted.set(String::new());
            toast_ok("Theme imported and applied");
        }
        Err(e) => error.set(Some(e)),
    };
    let on_file = move |ev: leptos::ev::Event| {
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        let Some(f) = input.files().and_then(|l| l.get(0)) else { return };
        input.set_value("");
        spawn_local(async move {
            match JsFuture::from(f.text()).await.ok().and_then(|v| v.as_string()) {
                Some(t) => do_import(t),
                None => error.set(Some("Could not read that file.".into())),
            }
        });
    };

    view! {
        <SysCard title="Share themes" icon="upload" hint="A theme is a small JSON file. Move it between machines like any other file.">
            <div class="row wrap gap">
                <Button icon="download" on_click=move |_| {
                    let s = ctx.active_untracked();
                    download_text(&format!("crate-theme-{}.json", theme_slug(&s.name)), "application/json", &serialize_theme(&s));
                }>{move || format!("Export \"{}\"", ctx.active().name)}</Button>
                <Button icon="copy" on_click=move |_| { copy_text(&json()); toast_ok("Theme JSON copied"); }>"Copy JSON"</Button>
                <Button icon="upload" on_click=move |_| if let Some(i) = file.get_untracked() { i.click(); }>"Import file"</Button>
                <input node_ref=file type="file" accept="application/json,.json" class="hidden" on:change=on_file />
            </div>
            <div class="row gap" style="margin-top:12px;align-items:flex-start">
                <textarea class="input mono grow" rows="2" spellcheck="false" aria-label="Theme JSON"
                    placeholder="Or paste a theme's JSON here"
                    prop:value=move || pasted.get() on:input=move |ev| pasted.set(event_target_value(&ev))></textarea>
                <Button variant=Variant::Primary disabled=Signal::derive(move || pasted.get().trim().is_empty())
                    on_click=move |_| do_import(pasted.get_untracked())>"Import"</Button>
            </div>
            <Notice text=error />
        </SysCard>
    }
}

// ---------------------------------------------------------------------------------------------
// Display preferences (ui.prefs)
// ---------------------------------------------------------------------------------------------

#[component]
fn PrefSwitch(
    prefs: PrefsHandle,
    #[prop(into)] label: String,
    #[prop(into)] desc: String,
    get: fn(&UiPrefs) -> bool,
    set: fn(&mut UiPrefs, bool),
) -> impl IntoView {
    let local = RwSignal::new(get(&prefs.prefs.get_untracked()));
    Effect::new(move |_| local.set(prefs.prefs.with(get)));
    view! {
        <div class="pref-row">
            <div class="grow"><div class="name">{label.clone()}</div><div class="desc faint">{desc}</div></div>
            <Switch value=local label=label on_change=Callback::new(move |v: bool| prefs.prefs.update(|p| set(p, v))) />
        </div>
    }
}

#[component]
fn DisplayPrefs(prefs: PrefsHandle) -> impl IntoView {
    view! {
        <SysCard title="Display" icon="monitor" hint="Synced to your other devices.">
            <PrefSwitch prefs=prefs label="Album-art accent" desc="Tint the player with the colour of the current cover."
                get={|p: &UiPrefs| p.album_art_accent} set={|p: &mut UiPrefs, v: bool| p.album_art_accent = v} />
            <PrefSwitch prefs=prefs label="Reduce motion" desc="Fewer animations and transitions."
                get={|p: &UiPrefs| p.reduce_motion} set={|p: &mut UiPrefs, v: bool| p.reduce_motion = v} />
            <PrefSwitch prefs=prefs label="Tag colours" desc="Tint tag chips with a stable colour per tag. The colour never carries meaning on its own."
                get={|p: &UiPrefs| p.tag_colors} set={|p: &mut UiPrefs, v: bool| p.tag_colors = v} />
            <PrefSwitch prefs=prefs label="Waveforms in track rows" desc="Show a small waveform in track lists (uses more GPU)."
                get={|p: &UiPrefs| p.row_waveforms} set={|p: &mut UiPrefs, v: bool| p.row_waveforms = v} />
        </SysCard>
    }
}

#[allow(dead_code)]
fn _toast() {
    let _ = toast_err;
}
