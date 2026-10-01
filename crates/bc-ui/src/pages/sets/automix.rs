//! Automix: arrange the pool into the set (best order for key and tempo flow).
use std::sync::Arc;

use bc_types::sets::{AutomixRequest, DjSetDetail};
use leptos::prelude::*;

use super::mutations::SetMut;
use crate::ds::{Button, Dialog, Icon, Size, Variant};
use crate::logic::format::format_count;

#[component]
pub fn AutomixButton(
    m: SetMut,
    #[prop(into)] detail: Signal<Option<Arc<DjSetDetail>>>,
    #[prop(into)] pool_size: Signal<i64>,
    #[prop(into)] automix_max: Signal<usize>,
    #[prop(optional)] hero: bool,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let target = RwSignal::new(String::new());
    let keep = RwSignal::new(true);
    let n_items = Signal::derive(move || detail.with(|d| d.as_ref().map(|d| d.items.len()).unwrap_or(0)));
    Effect::new(move |_| {
        if open.get() {
            target.set(detail.with_untracked(|d| d.as_ref().and_then(|d| d.set.target_minutes).map(|t| t.to_string()).unwrap_or_default()));
        }
    });
    let disabled = Signal::derive(move || pool_size.get() == 0 || m.automix_busy.get());
    let usable = Signal::derive(move || (pool_size.get() as usize).min(automix_max.get()));
    let too_big = Signal::derive(move || pool_size.get() as usize > automix_max.get());
    let run = move || {
        if too_big.get_untracked() {
            return;
        }
        let minutes = target.get_untracked().trim().parse::<i64>().ok().filter(|m| *m > 0);
        let n = n_items.get_untracked();
        m.automix(AutomixRequest { keep_existing: if n > 0 { keep.get_untracked() } else { true }, target_minutes: minutes, ..Default::default() });
        open.set(false);
    };
    let title = Signal::derive(move || {
        if pool_size.get() == 0 { "Add pool sources first: automix arranges the pool".to_string() } else { "Arrange the pool into a set: best order for key and tempo flow".to_string() }
    });
    view! {
        <button type="button" class=if hero { "btn btn-primary btn-lg" } else { "btn btn-primary" }
            disabled=move || disabled.get() title=move || title.get() on:click=move |_| open.set(true)>
            <Icon name="wand" />
            <span>"Automix"</span>
            {hero.then(|| view! { <span class="mono">{move || if pool_size.get() > 0 { format!("{} tracks", format_count(usable.get() as i64)) } else { String::new() }}</span> })}
        </button>
        <Dialog open=open title="Automix"
            footer=crate::ds::children(move || view! {
                <Button variant=Variant::Ghost on_click=move |_| open.set(false)>"Cancel"</Button>
                <Button variant=Variant::Primary icon="wand" disabled=too_big on_click=move |_| run()>"Automix"</Button>
            })>
            <div class="automix">
                <p class="muted">
                    "Arrange "<b class="mono">{move || format_count(usable.get() as i64)}</b>
                    {move || format!(" pool track{} for key and tempo flow.", if pool_size.get() == 1 { "" } else { "s" })}
                </p>
                {move || too_big.get().then(|| view! {
                    <p class="warn-text"><Icon name="alert" size=13 />{format!(" The pool holds {}; automix takes up to {}. Narrow the pool for full control.", format_count(pool_size.get()), format_count(automix_max.get() as i64))}</p>
                })}
                <label class="field-row">
                    <span class="muted">"Target minutes"</span>
                    <input class="input num-in" type="number" min="1" placeholder="none" prop:value=move || target.get()
                        on:input=move |ev| target.set(event_target_value(&ev)) />
                </label>
                {move || (n_items.get() > 0).then(|| view! {
                    <label class="check">
                        <input type="checkbox" prop:checked=move || keep.get() on:change=move |ev| keep.set(event_target_checked(&ev)) />
                        <span>{format!("Keep the current {} track{} in place", n_items.get(), if n_items.get() == 1 { "" } else { "s" })}</span>
                    </label>
                })}
            </div>
        </Dialog>
    }
}

#[allow(dead_code)]
fn _s(_: Size) {}
