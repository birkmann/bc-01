//! Command palette (Ctrl/Cmd+K): jump to any page, run an action, or find a track,
//! artist, label or tag (server search, debounced 120 ms, previous results kept).
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;

use crate::app::{Panel, use_app};
use crate::ds::{Icon, use_debounced};
use crate::logic::fuzzy;
use crate::nav;
use crate::theme::use_theme;

#[derive(Clone)]
enum Action {
    Go(String),
    Run(Callback<()>),
    PlayTrack(i64, String),
}

#[derive(Clone)]
struct Item {
    group: &'static str,
    icon: &'static str,
    label: String,
    hint: String,
    action: Action,
}

fn items_of(v: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(a) = v.as_array() {
        return a.clone();
    }
    v.get("items").and_then(|i| i.as_array()).cloned().unwrap_or_default()
}

#[component]
pub fn CommandPalette() -> impl IntoView {
    let app = use_app();
    let theme = use_theme();
    let player = crate::player::use_player();
    let navigate = use_navigate();
    let q = RwSignal::new(String::new());
    let dq = use_debounced(q, 120);
    let active = RwSignal::new(0usize);
    let remote: RwSignal<Vec<Item>> = RwSignal::new(vec![]);
    let input = NodeRef::<leptos::html::Input>::new();

    Effect::new(move |_| {
        if app.palette_open.get() {
            q.set(String::new());
            active.set(0);
            remote.set(vec![]);
            if let Some(el) = input.get() {
                let _ = el.focus();
            }
        }
    });

    // Remote entity search; keeps the previous results visible while typing.
    Effect::new(move |_| {
        let text = dq.get();
        if !app.palette_open.get_untracked() || text.trim().len() < 2 {
            if text.trim().len() < 2 {
                remote.set(vec![]);
            }
            return;
        }
        let enc = crate::util::enc(text.trim());
        spawn_local(async move {
            let mut out: Vec<Item> = vec![];
            if let Ok(v) = crate::api::get::<serde_json::Value>(&format!("/tracks?q={enc}&limit=6")).await {
                for t in items_of(&v) {
                    let id = t.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
                    let title = t.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let artist = t.pointer("/artist/name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    out.push(Item { group: "Tracks", icon: "play", label: title.clone(), hint: artist, action: Action::PlayTrack(id, title) });
                }
            }
            for (path, group, icon, base) in [
                ("artists", "Artists", "user", "/artists/"),
                ("labels", "Labels", "folder", "/labels/"),
            ] {
                if let Ok(v) = crate::api::get::<serde_json::Value>(&format!("/{path}?q={enc}&limit=4")).await {
                    for a in items_of(&v) {
                        let id = a.get("id").and_then(|x| x.as_i64()).unwrap_or(0);
                        let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                        out.push(Item { group, icon, label: name, hint: String::new(), action: Action::Go(format!("{base}{id}")) });
                    }
                }
            }
            if let Ok(v) = crate::api::get::<serde_json::Value>(&format!("/tags?q={enc}&limit=4")).await {
                for a in items_of(&v) {
                    let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    out.push(Item { group: "Tags", icon: "tag", label: name.clone(), hint: String::new(), action: Action::Go(format!("/tracks?tag={}", crate::util::enc(&name))) });
                }
            }
            let _ = remote.try_set(out);
        });
    });

    let local_items = move || -> Vec<Item> {
        let mut v: Vec<Item> = nav::all()
            .into_iter()
            .map(|n| Item { group: "Go to", icon: n.icon, label: n.label.to_string(), hint: n.to.to_string(), action: Action::Go(n.to.to_string()) })
            .collect();
        let run = |label: &str, icon: &'static str, hint: &str, f: Callback<()>| Item { group: "Actions", icon, label: label.into(), hint: hint.into(), action: Action::Run(f) };
        v.push(run("Toggle light / dark theme", "sun", "", Callback::new(move |_| theme.toggle_mode())));
        v.push(run("Toggle planner", "queue", "q", Callback::new(move |_| app.toggle_panel(Panel::Plan))));
        v.push(run("Toggle similar tracks", "similar", "s", Callback::new(move |_| app.toggle_panel(Panel::Similar))));
        v.push(run("Open deck view", "waveform", "v", Callback::new(move |_| app.deck_open.update(|d| *d = !*d))));
        v.push(run("Play / pause", "play", "space", Callback::new(move |_| player.toggle())));
        v.push(run("Next track", "skip-next", "shift+right", Callback::new(move |_| player.next())));
        v.push(run("Previous track", "skip-prev", "shift+left", Callback::new(move |_| player.previous())));
        v
    };
    let results = Signal::derive(move || -> Vec<Item> {
        let text = q.get();
        let mut scored: Vec<(i32, Item)> = local_items()
            .into_iter()
            .filter_map(|i| fuzzy::score(&text, &i.label).map(|s| (s, i)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        let mut out: Vec<Item> = scored.into_iter().map(|(_, i)| i).take(if text.is_empty() { 14 } else { 7 }).collect();
        out.extend(remote.get());
        out
    });
    let run_item: StoredValue<std::rc::Rc<dyn Fn(Item)>, LocalStorage> = StoredValue::new_local(std::rc::Rc::new(move |it: Item| {
        app.palette_open.set(false);
        match it.action {
            Action::Go(p) => navigate(&p, Default::default()),
            Action::Run(cb) => cb.run(()),
            Action::PlayTrack(id, title) => {
                player.cmd(bc_types::player::PlayerCommand::PlayTrack {
                    item: bc_types::player::QueueItem { track_id: id, title, ..Default::default() },
                    queue: None,
                });
            }
        }
    }));
    let run = move |it: Item| run_item.with_value(|f| f(it));
    view! {
        <Show when=move || app.palette_open.get()>
            <div class="dialog-scrim" style="align-items:start;padding-top:12vh" on:mousedown=move |ev| { if ev.target() == ev.current_target() { app.palette_open.set(false) } }>
                <div class="dialog" role="dialog" aria-modal="true" aria-label="Command palette" style="width:min(620px,100%)">
                    <div class="input-wrap" style="padding:10px 12px;border-bottom:1px solid var(--color-line)">
                        <Icon name="search" />
                        <input node_ref=input class="input" style="border:0;background:transparent;height:34px;padding-left:34px" autocomplete="off"
                            placeholder="Jump to a page, run an action, or find a track, artist, label, tag..."
                            prop:value=move || q.get()
                            on:input=move |ev| { q.set(event_target_value(&ev)); active.set(0); }
                            on:keydown={
                                move |ev| {
                                    let n = results.with(|r| r.len());
                                    match ev.key().as_str() {
                                        "ArrowDown" => { ev.prevent_default(); if n > 0 { active.update(|a| *a = (*a + 1) % n) } }
                                        "ArrowUp" => { ev.prevent_default(); if n > 0 { active.update(|a| *a = (*a + n - 1) % n) } }
                                        "Enter" => { ev.prevent_default(); if let Some(it) = results.with(|r| r.get(active.get_untracked()).cloned()) { run(it) } }
                                        "Escape" => app.palette_open.set(false),
                                        _ => {}
                                    }
                                }
                            } />
                    </div>
                    <div style="max-height:min(60vh,440px);overflow:auto;padding:6px" role="listbox">
                        {move || {
                            let mut last = "";
                            results.get().into_iter().enumerate().map(|(i, it)| {
                                let header = (it.group != last).then(|| { last = it.group; view! { <div class="menu-label">{it.group}</div> } });
                                let it2 = it.clone();
                                view! {
                                    {header}
                                    <div class="option" role="option" data-active=move || (active.get() == i).to_string()
                                        on:pointerenter=move |_| active.set(i) on:click=move |_| run(it2.clone())>
                                        <Icon name=it.icon /><span class="truncate">{it.label.clone()}</span>
                                        <span class="faint truncate" style="margin-left:auto;font-size:var(--text-xs)">{it.hint.clone()}</span>
                                    </div>
                                }
                            }).collect_view()
                        }}
                        {move || results.with(|r| r.is_empty()).then(|| view! { <div class="empty" style="padding:24px"><p>"No matches"</p></div> })}
                    </div>
                </div>
            </div>
        </Show>
    }
}
