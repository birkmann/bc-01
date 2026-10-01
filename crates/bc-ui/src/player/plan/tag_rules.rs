//! The tag-rules row of the direction bar: which tags the next tracks may carry, which they may
//! not, and the saved rule sets a DJ switches between mid-set. The chips are the switch; the
//! editor beneath is where a set is written, saved and updated.
use bc_types::player::{PlanOp, PlayerCommand, TagRulePreset, TagRules};
use leptos::prelude::*;

use super::Planner;
use super::tag_picker::TagPicker;
use crate::ds::Icon;

fn describe(r: &TagRules) -> String {
    let mut parts = vec![];
    if !r.allow.is_empty() {
        parts.push(format!("only: {}", r.allow.join(", ")));
    }
    if !r.deny.is_empty() {
        parts.push(format!("never: {}", r.deny.join(", ")));
    }
    if parts.is_empty() { "no rules".into() } else { parts.join(" — ") }
}

/// One-line inline name prompt: Enter commits, Escape cancels, blur commits a non-empty name.
#[component]
fn NamePrompt(#[prop(optional, into)] initial: String, #[prop(into)] placeholder: String, on_done: Callback<Option<String>>) -> impl IntoView {
    let name = RwSignal::new(initial);
    let done = StoredValue::new(false);
    let finish = move |v: Option<String>| {
        // blur can fire after the prompt is gone (and its signals disposed)
        if done.try_get_value() == Some(false) {
            done.set_value(true);
            on_done.run(v);
        }
    };
    view! {
        <input class="input pp-name-in" autofocus aria-label=placeholder.clone() placeholder=placeholder
            prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev))
            on:keydown=move |ev| match ev.key().as_str() {
                "Enter" => finish(Some(name.get_untracked())),
                "Escape" => { ev.stop_propagation(); finish(None) }
                _ => {}
            }
            on:blur=move |_| { if let Some(n) = name.try_get_untracked() { finish(if n.trim().is_empty() { None } else { Some(n) }) } } />
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Naming {
    Save,
    Rename,
}

#[component]
pub fn TagRulesBar(pl: Planner) -> impl IntoView {
    let rules = Signal::derive(move || pl.plan.with(|p| p.tag_rules.clone()));
    let presets = Signal::derive(move || pl.plan.with(|p| p.tag_presets.clone()));
    let active_id = Signal::derive(move || pl.plan.with(|p| p.active_preset_id.clone()));
    let empty = Signal::derive(move || rules.with(|r| r.is_empty()));
    let active = Signal::derive(move || {
        let id = active_id.get()?;
        presets.with(|p| p.iter().find(|x| x.id == id).cloned())
    });
    let unsaved = Signal::derive(move || !empty.get() && active.get().is_none());
    let editing = RwSignal::new(false);
    let naming = RwSignal::new(None::<Naming>);
    let open = Signal::derive(move || editing.get() || (unsaved.get() && presets.with(|p| p.is_empty())));
    let allow = Signal::derive(move || rules.with(|r| r.allow.clone()));
    let deny = Signal::derive(move || rules.with(|r| r.deny.clone()));

    // upcoming rows that break the rules in force
    let offenders = Memo::new(move |_| {
        let r = rules.get();
        if r.is_empty() {
            return vec![];
        }
        pl.player.state.with(|s| {
            ((s.queue_index + 1).max(0) as usize..s.queue.len()).filter(|i| s.queue.get(*i).map(|t| r.breaks(&t.tags)).unwrap_or(false)).collect::<Vec<_>>()
        })
    });
    let drop_offenders = move |_| {
        // highest index first: every removal shifts the rows after it
        for i in offenders.get_untracked().into_iter().rev() {
            pl.player.cmd(PlayerCommand::RemoveAt { index: i });
        }
    };
    let offender_view = move || {
        let n = offenders.with(|o| o.len());
        (n > 0).then(|| view! {
            <span class="pp-off faint">
                <span class="mono">{format!("{n} up next break{} these", if n == 1 { "s" } else { "" })}</span>
                <button type="button" class="pp-link danger-h" on:click=drop_offenders>"drop"</button>
            </span>
        })
    };
    let offender_view2 = offender_view;

    view! {
        <div class="pp-rules">
            <div class="pp-seg top">
                <span class="pp-lbl">"rules"</span>
                <div class="pp-chips" role="group" aria-label="Tag rules">
                    <button type="button" class=move || if empty.get() { "pp-chip on" } else { "pp-chip" } aria-pressed=move || empty.get().to_string()
                        title="No tag rules: any tag may come up" on:click=move |_| pl.op(PlanOp::ClearTagRules)>"none"</button>
                    {move || presets.get().into_iter().map(|p: TagRulePreset| {
                        let id = p.id.clone();
                        let id2 = p.id.clone();
                        let on = Signal::derive(move || active_id.get().as_deref() == Some(id2.as_str()));
                        view! {
                            <button type="button" class=move || if on.get() { "pp-chip on" } else { "pp-chip" } aria-pressed=move || on.get().to_string()
                                title=describe(&p.rules) on:click=move |_| pl.op(PlanOp::ApplyPreset { id: id.clone() })>
                                {move || on.get().then(|| view! { <Icon name="check" size=10 /> })}{p.name.clone()}
                            </button>
                        }
                    }).collect_view()}
                    {move || unsaved.get().then(|| view! { <span class="pp-chip dashed" title=describe(&rules.get())>"unsaved"</span> })}
                    <button type="button" class="pp-link grow-end" aria-pressed=move || open.get().to_string()
                        title="Edit the rules: allowed and forbidden tags" on:click=move |_| editing.update(|e| *e = !*e)>
                        <Icon name="edit" size=10 />"edit"<Icon name=move || if open.get() { "chevron-up" } else { "chevron-down" } size=10 />
                    </button>
                </div>
            </div>
            <Show when=move || open.get()>
                <div class="pp-indent pp-editor">
                    <div class="pp-seg top"><span class="pp-lbl narrow">"allow"</span>
                        <TagPicker value=allow on_toggle=Callback::new(move |t: String| pl.op(PlanOp::ToggleAllowTag { tag: t }))
                            placeholder="only these tags (any of them)…" label="Allowed tags" /></div>
                    <div class="pp-seg top"><span class="pp-lbl narrow">"forbid"</span>
                        <TagPicker value=deny on_toggle=Callback::new(move |t: String| pl.op(PlanOp::ToggleDenyTag { tag: t }))
                            placeholder="never these tags…" label="Forbidden tags" danger=true /></div>
                    <div class="pp-actions">
                        {move || if naming.get() == Some(Naming::Save) {
                            view! { <NamePrompt placeholder="name this rule set" on_done=Callback::new(move |n: Option<String>| {
                                if let Some(n) = n { pl.op(PlanOp::SavePreset { name: n }); }
                                naming.set(None);
                            }) /> }.into_any()
                        } else {
                            view! { <button type="button" class="pp-link" disabled=move || empty.get() title="Save these rules as a new set you can switch to later"
                                on:click=move |_| naming.set(Some(Naming::Save))><Icon name="bookmark" size=10 />"save as…"</button> }.into_any()
                        }}
                        {move || active.get().map(|a| {
                            let (id1, id2, name) = (a.id.clone(), a.id.clone(), a.name.clone());
                            if naming.get() == Some(Naming::Rename) {
                                view! { <NamePrompt initial=name placeholder="rename" on_done=Callback::new(move |n: Option<String>| {
                                    if let Some(n) = n { pl.op(PlanOp::RenamePreset { id: id1.clone(), name: n }); }
                                    naming.set(None);
                                }) /> }.into_any()
                            } else {
                                view! {
                                    <button type="button" class="pp-link" on:click=move |_| naming.set(Some(Naming::Rename))>"rename"</button>
                                    <button type="button" class="pp-link danger-h" title="Delete this set (the rules stay in force until changed)"
                                        on:click=move |_| pl.op(PlanOp::DeletePreset { id: id2.clone() })><Icon name="trash" size=10 />"delete"</button>
                                }.into_any()
                            }
                        })}
                        {move || {
                            // edited away from a preset: offer to write the edits back to one
                            let cands: Vec<TagRulePreset> = if unsaved.get() { let r = rules.get(); presets.get().into_iter().filter(|p| !p.rules.same_as(&r)).collect() } else { vec![] };
                            (!cands.is_empty()).then(|| view! {
                                <label class="pp-link">"update "
                                    <select class="input pp-sel" aria-label="Overwrite a saved rule set with these rules"
                                        on:change=move |ev| { let v = event_target_value(&ev); if !v.is_empty() { pl.op(PlanOp::UpdatePreset { id: v }); } }>
                                        <option value="">"which set?"</option>
                                        {cands.into_iter().map(|p| view! { <option value=p.id.clone()>{p.name.clone()}</option> }).collect_view()}
                                    </select></label>
                            })
                        }}
                        {move || (!empty.get()).then(|| view! {
                            <button type="button" class="pp-link" title="Clear the rules in force (saved sets are kept)" on:click=move |_| pl.op(PlanOp::ClearTagRules)><Icon name="x" size=10 />"clear"</button>
                        })}
                        {offender_view()}
                    </div>
                </div>
            </Show>
            <Show when=move || !open.get() && !empty.get()>
                <div class="pp-indent pp-summary faint">
                    {move || { let a = allow.get(); (!a.is_empty()).then(|| view! { <span class="truncate"><Icon name="check" size=10 class="ok" />{a.join(", ")}</span> }) }}
                    {move || { let d = deny.get(); (!d.is_empty()).then(|| view! { <span class="truncate"><Icon name="x-circle" size=10 class="bad" />{d.join(", ")}</span> }) }}
                    {offender_view2()}
                </div>
            </Show>
        </div>
    }
}
