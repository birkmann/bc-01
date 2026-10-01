//! Custom Select (listbox via the global menu: keyboard nav and a bottom sheet on
//! mobile for free) and Combobox (input + filtered suggestions).
use leptos::portal::Portal;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::icon::Icon;
use super::menu::{MenuCtx, MenuEntry, MenuItem};
use super::popover::{Rect, place, viewport};

#[derive(Clone, Debug, PartialEq)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

impl SelectOption {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self { value: value.into(), label: label.into() }
    }
}

impl<V: Into<String>, L: Into<String>> From<(V, L)> for SelectOption {
    fn from((v, l): (V, L)) -> Self {
        SelectOption::new(v, l)
    }
}

#[component]
pub fn Select(
    #[prop(into)] options: Signal<Vec<SelectOption>>,
    value: RwSignal<String>,
    #[prop(optional, into)] on_change: Option<Callback<String>>,
    #[prop(optional, into)] placeholder: Option<String>,
    #[prop(optional, into)] class: String,
    #[prop(optional, into)] aria_label: Option<String>,
) -> impl IntoView {
    let menu = expect_context::<MenuCtx>();
    let btn = NodeRef::<leptos::html::Button>::new();
    let label = move || {
        let v = value.get();
        options.with(|o| o.iter().find(|x| x.value == v).map(|x| x.label.clone()))
    };
    let ph = placeholder.unwrap_or_default();
    view! {
        <button node_ref=btn type="button" class=format!("select-trigger {class}") aria-haspopup="listbox" aria-label=aria_label
            on:click=move |_| {
                let Some(el) = btn.get_untracked() else { return };
                let cur = value.get_untracked();
                let entries: Vec<MenuEntry> = options.get_untracked().into_iter().map(|o| {
                    let v = o.value.clone();
                    MenuItem::new(o.label.clone()).checked(o.value == cur).on(move || {
                        value.set(v.clone());
                        if let Some(cb) = on_change { cb.run(v.clone()); }
                    }).into()
                }).collect();
                let r = Rect::of(el.unchecked_ref());
                menu.state.set(Some(super::menu::MenuState { anchor: r, entries, title: None, min_w: r.width.max(160.0) }));
            }>
            <span class="truncate">{move || label().unwrap_or_else(|| ph.clone())}</span>
            <Icon name="chevron-down" />
        </button>
    }
}

fn filter_opts(opts: &[SelectOption], q: &str, limit: usize) -> Vec<SelectOption> {
    let q = q.trim().to_lowercase();
    opts.iter().filter(|o| q.is_empty() || o.label.to_lowercase().contains(&q)).take(limit).cloned().collect()
}

/// Input with suggestions. `value` is the input text. `remote`: the options
/// are already filtered by the caller (server search).
#[component]
pub fn Combobox(
    #[prop(into)] options: Signal<Vec<SelectOption>>,
    value: RwSignal<String>,
    #[prop(optional, into)] on_pick: Option<Callback<SelectOption>>,
    #[prop(optional, into)] placeholder: Option<String>,
    #[prop(optional)] remote: bool,
    #[prop(optional, into)] class: String,
) -> impl IntoView {
    let input = NodeRef::<leptos::html::Input>::new();
    let open = RwSignal::new(false);
    let active = RwSignal::new(0usize);
    let shown = Memo::new(move |_| {
        let o = options.get();
        if remote { o } else { filter_opts(&o, &value.get(), 50) }
    });
    let pick = move |o: SelectOption| {
        value.set(o.label.clone());
        open.set(false);
        if let Some(cb) = on_pick {
            cb.run(o);
        }
    };
    view! {
        <div class=format!("combobox {class}") style="position:relative">
            <input node_ref=input class="input" type="text" role="combobox" aria-expanded=move || open.get().to_string()
                autocomplete="off" placeholder=placeholder
                prop:value=move || value.get()
                on:input=move |ev| { value.set(event_target_value(&ev)); open.set(true); active.set(0); }
                on:focus=move |_| open.set(true)
                on:blur=move |_| crate::util::after(120, move || open.set(false))
                on:keydown=move |ev| {
                    let n = shown.with(|s| s.len());
                    match ev.key().as_str() {
                        "ArrowDown" => { ev.prevent_default(); open.set(true); if n > 0 { active.update(|a| *a = (*a + 1) % n); } }
                        "ArrowUp" => { ev.prevent_default(); if n > 0 { active.update(|a| *a = (*a + n - 1) % n); } }
                        "Enter" => {
                            if open.get_untracked() {
                                if let Some(o) = shown.with(|s| s.get(active.get_untracked()).cloned()) { ev.prevent_default(); pick(o); }
                            }
                        }
                        "Escape" => open.set(false),
                        _ => {}
                    }
                } />
            {move || (open.get() && shown.with(|s| !s.is_empty())).then(|| {
                let r = input.get_untracked().map(|e| Rect::of(e.unchecked_ref())).unwrap_or(Rect::point(0.0, 0.0));
                let (vw, vh) = viewport();
                let style = place(r, r.width, r.width, 220.0, vw, vh);
                view! {
                    <Portal>
                        <div class="popover" role="listbox" style=style.clone() on:pointerdown=|ev| ev.prevent_default()>
                            {move || shown.get().into_iter().enumerate().map(|(i, o)| {
                                let o2 = o.clone();
                                view! {
                                    <div class="option" role="option" data-active=move || (active.get() == i).to_string()
                                        on:pointerenter=move |_| active.set(i)
                                        on:click=move |_| pick(o2.clone())>
                                        <span class="truncate">{o.label.clone()}</span>
                                    </div>
                                }
                            }).collect_view()}
                        </div>
                    </Portal>
                }
            })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_case_insensitively_with_limit() {
        let o: Vec<SelectOption> = ["Techno", "Tech House", "Ambient", "Dub Techno"].iter().map(|s| SelectOption::new(*s, *s)).collect();
        assert_eq!(filter_opts(&o, "TECH", 10).len(), 3);
        assert_eq!(filter_opts(&o, "tech", 2).len(), 2);
        assert_eq!(filter_opts(&o, "", 10).len(), 4);
    }
}
