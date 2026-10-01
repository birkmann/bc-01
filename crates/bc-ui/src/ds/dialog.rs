//! Dialog, Sheet and the global `confirm()` helper.
use futures::channel::oneshot;
use leptos::portal::Portal;
use leptos::prelude::*;
use std::cell::RefCell;

use super::button::{Button, Variant};
use super::icon::Icon;

fn focusables(root: &web_sys::HtmlElement) -> Vec<web_sys::HtmlElement> {
    use wasm_bindgen::JsCast;
    let sel = "button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex='-1'])";
    let mut out = vec![];
    if let Ok(list) = root.query_selector_all(sel) {
        for i in 0..list.length() {
            if let Some(n) = list.item(i).and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok()) {
                out.push(n);
            }
        }
    }
    out
}

/// Focus trap: Tab cycles inside `root`.
fn trap_tab(ev: &web_sys::KeyboardEvent, root: &web_sys::HtmlElement) {
    use wasm_bindgen::JsCast;
    if ev.key() != "Tab" {
        return;
    }
    let f = focusables(root);
    if f.is_empty() {
        ev.prevent_default();
        return;
    }
    let active = crate::util::document().active_element();
    let first = &f[0];
    let last = &f[f.len() - 1];
    let on_first = active.as_ref().map(|a| a == first.unchecked_ref::<web_sys::Element>()).unwrap_or(false);
    let on_last = active.as_ref().map(|a| a == last.unchecked_ref::<web_sys::Element>()).unwrap_or(false);
    if ev.shift_key() && on_first {
        ev.prevent_default();
        let _ = last.focus();
    } else if !ev.shift_key() && on_last {
        ev.prevent_default();
        let _ = first.focus();
    }
}

#[component]
pub fn Dialog(
    open: RwSignal<bool>,
    #[prop(into)] title: Signal<String>,
    #[prop(optional)] wide: bool,
    #[prop(optional)] footer: Option<ChildrenFn>,
    children: ChildrenFn,
) -> impl IntoView {
    let children = StoredValue::new(children);
    let footer = StoredValue::new(footer);
    view! {
        <Show when=move || open.get()>
            {move || {
                let panel = NodeRef::<leptos::html::Div>::new();
                Effect::new(move |_| {
                    if let Some(p) = panel.get() {
                        let f = focusables(&p);
                        // prefer the first input, else the first control that is not the header close
                        if let Some(el) = f.iter().find(|e| e.tag_name() == "INPUT").or(f.first()) { let _ = el.focus(); }
                    }
                });
                view! {
                    <Portal>
                        <div class="dialog-scrim" on:mousedown=move |ev| {
                            if ev.target() == ev.current_target() { open.set(false); }
                        }>
                            <div class=if wide { "dialog dialog-lg" } else { "dialog" } role="dialog" aria-modal="true" node_ref=panel
                                on:keydown=move |ev| {
                                    if ev.key() == "Escape" { ev.stop_propagation(); open.set(false); }
                                    if let Some(p) = panel.get_untracked() { trap_tab(&ev, &p); }
                                }>
                                <div class="dialog-head">{move || title.get()}</div>
                                <div class="dialog-body">{children.with_value(|c| c())}</div>
                                {footer.with_value(|f| f.as_ref().map(|f| view! { <div class="dialog-foot">{f()}</div> }))}
                            </div>
                        </div>
                    </Portal>
                }
            }}
        </Show>
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum SheetSide {
    #[default]
    Right,
    Bottom,
}

/// Slide-in panel (right on desktop) / bottom sheet.
#[component]
pub fn Sheet(
    open: RwSignal<bool>,
    #[prop(into)] title: String,
    #[prop(optional)] side: SheetSide,
    children: ChildrenFn,
) -> impl IntoView {
    let title = StoredValue::new(title);
    let children = StoredValue::new(children);
    view! {
        <Show when=move || open.get()>
            <Portal>
                <div class="sheet-scrim" on:click=move |_| open.set(false)></div>
                <div class=if side == SheetSide::Right { "sheet sheet-right" } else { "sheet sheet-bottom" } role="dialog" aria-modal="true"
                    on:keydown=move |ev| if ev.key() == "Escape" { open.set(false) }>
                    {(side == SheetSide::Bottom).then(|| view! { <div class="sheet-grab"></div> })}
                    <div class="sheet-head"><span class="grow">{title.get_value()}</span>
                        <Button variant=Variant::Ghost icon="x" title="Close" on_click=move |_| open.set(false) /></div>
                    <div class="sheet-body">{children.with_value(|c| c())}</div>
                </div>
            </Portal>
        </Show>
    }
}

// ---- confirm() ---------------------------------------------------------------

struct Pending {
    tx: oneshot::Sender<bool>,
}

thread_local! {
    static CONFIRM: RefCell<Option<RwSignal<Option<ConfirmView>>>> = const { RefCell::new(None) };
    static PENDING: RefCell<Option<Pending>> = const { RefCell::new(None) };
}

#[derive(Clone)]
struct ConfirmView {
    title: String,
    body: String,
    confirm_label: String,
    danger: bool,
}

/// Ask for confirmation. Resolves to `true` when confirmed.
pub async fn confirm(title: &str, body: &str, confirm_label: &str, danger: bool) -> bool {
    let (tx, rx) = oneshot::channel();
    let sig = CONFIRM.with(|c| *c.borrow());
    let Some(sig) = sig else { return false };
    // a previous unanswered confirm is cancelled
    PENDING.with(|p| {
        if let Some(old) = p.borrow_mut().take() {
            let _ = old.tx.send(false);
        }
        *p.borrow_mut() = Some(Pending { tx });
    });
    sig.set(Some(ConfirmView { title: title.into(), body: body.into(), confirm_label: confirm_label.into(), danger }));
    rx.await.unwrap_or(false)
}

fn resolve(ok: bool) {
    PENDING.with(|p| {
        if let Some(pen) = p.borrow_mut().take() {
            let _ = pen.tx.send(ok);
        }
    });
    CONFIRM.with(|c| {
        if let Some(sig) = *c.borrow() {
            sig.set(None);
        }
    });
}

#[component]
pub fn ConfirmHost() -> impl IntoView {
    let sig: RwSignal<Option<ConfirmView>> = RwSignal::new(None);
    CONFIRM.with(|c| *c.borrow_mut() = Some(sig));
    let open = RwSignal::new(false);
    Effect::new(move |_| open.set(sig.with(|s| s.is_some())));
    Effect::new(move |_| {
        if !open.get() && sig.get_untracked().is_some() {
            resolve(false);
        }
    });
    view! {
        <Dialog open=open title=Signal::derive(move || sig.get().map(|s| s.title).unwrap_or_default())
            footer=super::children(move || view! {
                <Button variant=Variant::Ghost on_click=move |_| resolve(false)>"Cancel"</Button>
                <Button variant=move_variant(sig) on_click=move |_| resolve(true)>
                    {move || sig.get().map(|s| s.confirm_label).unwrap_or_else(|| "OK".into())}
                </Button>
            })>
            {move || {
                let s = sig.get();
                view! { <p>{s.map(|s| s.body)}</p> }
            }}
        </Dialog>
    }
}

fn move_variant(sig: RwSignal<Option<ConfirmView>>) -> Variant {
    if sig.with_untracked(|s| s.as_ref().map(|s| s.danger).unwrap_or(false)) { Variant::Danger } else { Variant::Primary }
}

#[allow(dead_code)]
fn _icon_used() -> impl IntoView {
    view! { <Icon name="x" /> }
}
