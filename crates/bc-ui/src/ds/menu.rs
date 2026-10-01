//! Menu: one global host renders every menu as an anchored popover (desktop) or a
//! bottom sheet (mobile). Full keyboard navigation: arrows, Home/End, Enter/Space,
//! typeahead, Right to drill into a submenu, Left/Backspace to go back, Escape.
use leptos::portal::Portal;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::icon::Icon;
use super::popover::{Rect, place, viewport};
use crate::util::{document, is_mobile};

#[derive(Clone)]
pub struct MenuItem {
    pub label: String,
    pub icon: Option<String>,
    pub kbd: Option<String>,
    pub danger: bool,
    pub disabled: bool,
    pub checked: Option<bool>,
    pub action: Option<Callback<()>>,
    pub sub: Vec<MenuEntry>,
}

#[derive(Clone)]
pub enum MenuEntry {
    Item(MenuItem),
    Sep,
    Label(String),
}

impl MenuItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self { label: label.into(), icon: None, kbd: None, danger: false, disabled: false, checked: None, action: None, sub: vec![] }
    }
    pub fn on(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.action = Some(Callback::new(move |_| f()));
        self
    }
    pub fn icon(mut self, i: &str) -> Self {
        self.icon = Some(i.into());
        self
    }
    pub fn kbd(mut self, k: &str) -> Self {
        self.kbd = Some(k.into());
        self
    }
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
    pub fn disabled(mut self, d: bool) -> Self {
        self.disabled = d;
        self
    }
    pub fn checked(mut self, c: bool) -> Self {
        self.checked = Some(c);
        self
    }
    pub fn sub(mut self, s: Vec<MenuEntry>) -> Self {
        self.sub = s;
        self
    }
}

impl From<MenuItem> for MenuEntry {
    fn from(i: MenuItem) -> Self {
        MenuEntry::Item(i)
    }
}

fn selectable(e: &MenuEntry) -> bool {
    matches!(e, MenuEntry::Item(i) if !i.disabled)
}

/// Next selectable index after `from` in direction `dir` (+1/-1), wrapping.
pub fn step(entries: &[MenuEntry], from: Option<usize>, dir: i32) -> Option<usize> {
    let n = entries.len() as i32;
    if n == 0 {
        return None;
    }
    let mut i = match from {
        Some(f) => f as i32,
        None => {
            if dir > 0 { -1 } else { n }
        }
    };
    for _ in 0..n {
        i = (i + dir).rem_euclid(n);
        if selectable(&entries[i as usize]) {
            return Some(i as usize);
        }
    }
    None
}

/// First selectable item whose label starts with `c`, searching after `from`.
pub fn typeahead(entries: &[MenuEntry], from: Option<usize>, c: char) -> Option<usize> {
    let n = entries.len();
    let start = from.map(|f| f + 1).unwrap_or(0);
    let lc = c.to_ascii_lowercase();
    (0..n).map(|k| (start + k) % n.max(1)).find(|&i| {
        matches!(&entries[i], MenuEntry::Item(it) if !it.disabled && it.label.to_lowercase().starts_with(lc))
    })
}

#[derive(Clone)]
pub struct MenuState {
    pub anchor: Rect,
    pub entries: Vec<MenuEntry>,
    pub title: Option<String>,
    pub min_w: f64,
}

#[derive(Clone, Copy)]
pub struct MenuCtx {
    pub state: RwSignal<Option<MenuState>>,
}

impl MenuCtx {
    pub fn open(&self, anchor: Rect, entries: Vec<MenuEntry>) {
        self.state.set(Some(MenuState { anchor, entries, title: None, min_w: 180.0 }));
    }
    pub fn open_titled(&self, anchor: Rect, title: &str, entries: Vec<MenuEntry>) {
        self.state.set(Some(MenuState { anchor, entries, title: Some(title.into()), min_w: 180.0 }));
    }
    pub fn close(&self) {
        self.state.set(None);
    }
}

pub fn provide_menu() -> MenuCtx {
    let ctx = MenuCtx { state: RwSignal::new(None) };
    provide_context(ctx);
    ctx
}

#[component]
pub fn MenuHost() -> impl IntoView {
    let ctx = expect_context::<MenuCtx>();
    let open = Memo::new(move |_| ctx.state.with(|s| s.is_some()));
    view! { <Show when=move || open.get()><MenuLayer/></Show> }
}

#[component]
fn MenuLayer() -> impl IntoView {
    let ctx = expect_context::<MenuCtx>();
    let st = ctx.state.get_untracked().expect("open");
    let mobile = is_mobile();
    let stack: RwSignal<Vec<(String, Vec<MenuEntry>)>> = RwSignal::new(vec![]);
    let root = st.entries.clone();
    let current = {
        let root = root.clone();
        Signal::derive(move || stack.with(|s| s.last().map(|(_, e)| e.clone()).unwrap_or_else(|| root.clone())))
    };
    let active = RwSignal::new(None::<usize>);
    let panel = NodeRef::<leptos::html::Div>::new();
    let prev_focus = document().active_element();

    Effect::new(move |_| {
        if let Some(el) = panel.get() {
            let _ = el.focus();
        }
    });
    on_cleanup(move || {
        if let Some(el) = prev_focus.and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok()) {
            let _ = el.focus();
        }
    });

    let activate = move |idx: usize| {
        let entries = current.get_untracked();
        let Some(MenuEntry::Item(it)) = entries.get(idx).cloned() else { return };
        if it.disabled {
            return;
        }
        if !it.sub.is_empty() {
            stack.update(|s| s.push((it.label.clone(), it.sub.clone())));
            active.set(step(&it.sub, None, 1));
            return;
        }
        ctx.close();
        if let Some(a) = it.action {
            crate::util::after(0, move || {
                let _ = a.try_run(());
            });
        }
    };

    let on_key = move |ev: web_sys::KeyboardEvent| {
        let entries = current.get_untracked();
        let key = ev.key();
        let mut handled = true;
        match key.as_str() {
            "ArrowDown" => active.set(step(&entries, active.get_untracked(), 1)),
            "ArrowUp" => active.set(step(&entries, active.get_untracked(), -1)),
            "Home" => active.set(step(&entries, None, 1)),
            "End" => active.set(step(&entries, None, -1)),
            "Enter" | " " => {
                if let Some(i) = active.get_untracked() {
                    activate(i)
                }
            }
            "ArrowRight" => {
                if let Some(i) = active.get_untracked() {
                    if matches!(entries.get(i), Some(MenuEntry::Item(it)) if !it.sub.is_empty()) {
                        activate(i)
                    }
                }
            }
            "ArrowLeft" | "Backspace" => {
                if stack.with_untracked(|s| !s.is_empty()) {
                    stack.update(|s| {
                        s.pop();
                    });
                    active.set(None);
                } else {
                    ctx.close();
                }
            }
            "Escape" | "Tab" => ctx.close(),
            k if k.chars().count() == 1 && !ev.ctrl_key() && !ev.meta_key() => {
                if let Some(c) = k.chars().next() {
                    if let Some(i) = typeahead(&entries, active.get_untracked(), c) {
                        active.set(Some(i));
                    }
                }
            }
            _ => handled = false,
        }
        if handled {
            ev.prevent_default();
            ev.stop_propagation();
        }
    };

    let title = st.title.clone();
    let list = move || {
        let entries = current.get();
        entries
            .into_iter()
            .enumerate()
            .map(|(i, e)| match e {
                MenuEntry::Sep => view! { <div class="menu-sep" role="separator"></div> }.into_any(),
                MenuEntry::Label(l) => view! { <div class="menu-label">{l}</div> }.into_any(),
                MenuEntry::Item(it) => {
                    let has_sub = !it.sub.is_empty();
                    let cls = if it.danger { "menu-item danger" } else { "menu-item" };
                    let disabled = it.disabled;
                    view! {
                        <div class=cls role="menuitem" aria-disabled=disabled.to_string()
                            data-active=move || (active.get() == Some(i)).to_string()
                            on:pointerenter=move |_| if !disabled { active.set(Some(i)) }
                            on:click=move |ev| { ev.stop_propagation(); activate(i) }>
                            {it.icon.clone().map(|ic| view! { <Icon name=ic /> })}
                            <span class="truncate">{it.label.clone()}</span>
                            {it.checked.map(|c| view! { <span class="chev">{if c { view! { <Icon name="check" /> }.into_any() } else { ().into_any() }}</span> })}
                            {it.kbd.clone().map(|k| view! { <span class="kbd">{k}</span> })}
                            {has_sub.then(|| view! { <Icon name="chevron-right" class="chev" /> })}
                        </div>
                    }
                    .into_any()
                }
            })
            .collect_view()
    };
    let back = move || {
        stack.with(|s| s.last().map(|(l, _)| l.clone())).map(|l| {
            view! {
                <div class="menu-item" on:click=move |_| { stack.update(|s| { s.pop(); }); active.set(None); }>
                    <Icon name="chevron-left" /><span>{l}</span>
                </div>
                <div class="menu-sep"></div>
            }
        })
    };

    if mobile {
        view! {
            <Portal>
                <div class="sheet-scrim" on:click=move |_| ctx.close()></div>
                <div class="sheet sheet-bottom sheet-menu" role="menu" tabindex="-1" node_ref=panel on:keydown=on_key>
                    <div class="sheet-grab"></div>
                    {title.clone().map(|t| view! { <div class="menu-label">{t}</div> })}
                    <div class="sheet-body" style="padding:4px 8px 12px">{back}{list}</div>
                </div>
            </Portal>
        }
        .into_any()
    } else {
        let (vw, vh) = viewport();
        let est_h = 36.0 * root.len() as f64 + 16.0;
        let style = place(st.anchor, st.min_w, 240.0, est_h, vw, vh);
        view! {
            <Portal>
                <div class="popover-scrim" on:click=move |_| ctx.close() on:contextmenu=move |ev| { ev.prevent_default(); ctx.close(); }></div>
                <div class="popover" role="menu" tabindex="-1" node_ref=panel style=style.clone() on:keydown=on_key>
                    {title.clone().map(|t| view! { <div class="menu-label">{t}</div> })}
                    {back}{list}
                </div>
            </Portal>
        }
        .into_any()
    }
}

/// Icon button that opens a menu below itself. `entries` is re-read on open.
#[component]
pub fn MenuButton(
    #[prop(into)] entries: Callback<(), Vec<MenuEntry>>,
    #[prop(optional, into)] icon: Option<String>,
    #[prop(optional, into)] title: Option<String>,
    #[prop(optional, into)] label: Option<String>,
    #[prop(optional, into)] class: String,
) -> impl IntoView {
    let ctx = expect_context::<MenuCtx>();
    let btn = NodeRef::<leptos::html::Button>::new();
    let icon = icon.unwrap_or_else(|| "more".into());
    let has_label = label.is_some();
    let t = title.clone().unwrap_or_else(|| "More actions".into());
    view! {
        <button node_ref=btn type="button" class=format!("btn btn-ghost {} {class}", if has_label { "" } else { "btn-icon" })
            title=t.clone() aria-label=t aria-haspopup="menu"
            on:click=move |ev| {
                ev.stop_propagation();
                if let Some(el) = btn.get_untracked() {
                    ctx.open(Rect::of(el.unchecked_ref()), entries.run(()));
                }
            }>
            <Icon name=icon />
            {label.map(|l| view! { <span>{l}</span> })}
        </button>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<MenuEntry> {
        vec![
            MenuItem::new("Play").into(),
            MenuEntry::Sep,
            MenuItem::new("Queue").into(),
            MenuItem::new("Delete").disabled(true).into(),
            MenuEntry::Label("More".into()),
            MenuItem::new("Share").into(),
        ]
    }

    #[test]
    fn arrows_skip_separators_labels_and_disabled_and_wrap() {
        let e = entries();
        assert_eq!(step(&e, None, 1), Some(0));
        assert_eq!(step(&e, Some(0), 1), Some(2));
        assert_eq!(step(&e, Some(2), 1), Some(5));
        assert_eq!(step(&e, Some(5), 1), Some(0));
        assert_eq!(step(&e, Some(0), -1), Some(5));
        assert_eq!(step(&e, None, -1), Some(5));
        assert_eq!(step(&[], None, 1), None);
    }

    #[test]
    fn typeahead_cycles_and_ignores_disabled() {
        let e = entries();
        assert_eq!(typeahead(&e, None, 'q'), Some(2));
        assert_eq!(typeahead(&e, None, 'd'), None);
        assert_eq!(typeahead(&e, Some(0), 'p'), Some(0));
        assert_eq!(typeahead(&e, None, 's'), Some(5));
    }
}
