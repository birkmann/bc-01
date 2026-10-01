//! The label folder card of the shelf, play helpers and the shared label menu.
use std::collections::HashSet;

use bc_types::library::LabelOut;
use bc_types::player::{LabelMode, PlayerCommand, QueueSource};
use leptos::prelude::*;

use super::bandcamp::{Actions, Entity};
use super::edit::{EditTarget, remove_label};
use super::logic::{count_of, thumb};
use super::shared::{Collage, FavButton, Favs, Kind, open_tab};
use crate::ds::popover::Rect;
use crate::ds::{Icon, MenuCtx, MenuEntry, MenuItem};
use crate::logic::format::{format_bytes, format_count};
use crate::player::use_player;

/// A label with its position in the current listing (shift-click ranges are by position).
#[derive(Clone, PartialEq)]
pub struct LabelRow {
    pub idx: usize,
    pub label: LabelOut,
}

/// Start a label's catalogue: in order, or a shuffled draw. `listing` is the shelf it sits in, so
/// playback carries on into the next folder of that shelf once the label runs out.
pub fn play_label(id: i64, listing: serde_json::Value, mode: LabelMode) {
    let player = use_player();
    player.cmd(PlayerCommand::StartSource { source: QueueSource::Label { label_id: id, listing, mode }, shuffle: mode == LabelMode::Shuffle });
}

/// The label currently playing, if the queue came from a label folder.
pub fn current_label() -> Signal<Option<i64>> {
    let player = use_player();
    Signal::derive(move || player.state.with(|s| match &s.source {
        Some(QueueSource::Label { label_id, .. }) => Some(*label_id),
        _ => None,
    }))
}

/// Everything the label menu / edit / remove need from the hosting page.
#[derive(Clone, Copy)]
pub struct LabelOps {
    pub favs: Favs,
    pub actions: Actions,
    pub edit: RwSignal<Option<EditTarget>>,
    /// Called after a label was removed.
    pub on_removed: Callback<i64>,
    /// The shelf the folders sit in (the `listing` playback carries on into).
    pub listing: Signal<serde_json::Value>,
}

impl LabelOps {
    pub fn entries(&self, l: &LabelOut) -> Vec<MenuEntry> {
        let (a, b, c, d) = (l.clone(), l.clone(), l.clone(), l.clone());
        let ops = *self;
        let fav = self.favs.is(Kind::Label, l.id);
        let (p1, p2) = (l.clone(), l.clone());
        let mut v: Vec<MenuEntry> = vec![
            MenuItem::new("Play").icon("play").disabled(l.track_count == 0).on(move || play_label(p1.id, ops.listing.get_untracked(), LabelMode::All)).into(),
            MenuItem::new("Shuffle").icon("shuffle").disabled(l.track_count == 0).on(move || play_label(p2.id, ops.listing.get_untracked(), LabelMode::Shuffle)).into(),
            MenuEntry::Sep,
            MenuItem::new("Find new releases").icon("rss").disabled(self.actions.busy().get_untracked()).on(move || {
                ops.actions.find_new(Entity { kind: Kind::Label, id: a.id, name: a.name.clone(), url: a.bandcamp_url.clone() });
            }).into(),
            MenuItem::new(if fav { "Remove from favourites" } else { "Add to favourites" }).icon(if fav { "heart-fill" } else { "heart" }).on(move || ops.favs.toggle(Kind::Label, b.id)).into(),
            MenuItem::new("Edit label\u{2026}").icon("edit").on(move || {
                ops.edit.set(Some(EditTarget { kind: Kind::Label, id: c.id, name: c.name.clone(), url: c.bandcamp_url.clone() }));
            }).into(),
        ];
        if let Some(u) = l.bandcamp_url.clone().filter(|u| !u.is_empty()) {
            v.push(MenuItem::new("Open on Bandcamp").icon("external").on(move || open_tab(&u)).into());
        }
        v.push(MenuEntry::Sep);
        v.push(MenuItem::new("Remove label\u{2026}").icon("trash").danger().on(move || remove_label(d.clone(), Callback::new({ let id = d.id; move |_| ops.on_removed.run(id) }))).into());
        v
    }

    pub fn open_at(&self, l: &LabelOut, anchor: Rect) {
        use_context::<MenuCtx>().map(|m| m.open_titled(anchor, &l.name, self.entries(l)));
    }
}

#[component]
pub fn LabelCard(
    row: LabelRow,
    width: f64,
    ops: LabelOps,
    picked: RwSignal<HashSet<i64>>,
    on_pick: Callback<(usize, i64, bool)>,
) -> impl IntoView {
    let _ = width;
    let listing = ops.listing;
    let l = row.label;
    let idx = row.idx;
    let id = l.id;
    let player = use_player();
    let current = current_label();
    let is_current = move || current.get() == Some(id);
    let is_playing = move || is_current() && player.is_playing();
    let is_picked = move || picked.with(|p| p.contains(&id));
    let selecting = move || picked.with(|p| !p.is_empty());
    let arts: Vec<String> = l.art_urls.iter().take(4).map(|u| thumb(u)).collect();
    let size = if l.size_bytes > 0 { format_bytes(l.size_bytes as f64) } else { "\u{a0}".to_string() };
    let has_tracks = l.track_count > 0;
    let name = l.name.clone();
    let l_menu = l.clone();
    let l_ctx = l.clone();
    let name_play = name.clone();
    let name_shuffle = name.clone();
    let name_sel = name.clone();
    let name_fav = name.clone();
    let name_more = name.clone();
    view! {
        <div class="pp-card pp-folder" class:cur=is_current class:sel=is_picked
            on:contextmenu={
                let l = l_ctx.clone();
                move |ev: web_sys::MouseEvent| { ev.prevent_default(); ops.open_at(&l, Rect::point(ev.client_x() as f64, ev.client_y() as f64)); }
            }>
            <span class="pp-folder-tab" aria-hidden="true"></span>
            <div class="pp-card-art pp-folder-art">
                <Collage urls=arts />
                {move || is_current().then(|| view! {
                    <span class="pp-nowplaying"><span class=move || if is_playing() { "pp-bars on" } else { "pp-bars" } aria-hidden="true"><i></i><i></i><i></i></span>{if is_playing() { "Playing" } else { "Paused" }}</span>
                })}
                {has_tracks.then(|| view! {
                    <div class="pp-play-pair">
                        <button type="button" class="pp-chipbtn" aria-label=format!("Shuffle {name_shuffle}") title="Shuffle"
                            on:click=move |ev| { ev.stop_propagation(); play_label(id, listing.get_untracked(), LabelMode::Shuffle); }><Icon name="shuffle" /></button>
                        <button type="button" class="pp-chipbtn pp-chipbtn-primary" class:show=is_current
                            aria-label=move || format!("{} {name_play}", if is_playing() { "Pause" } else if is_current() { "Resume" } else { "Play" })
                            on:click=move |ev| {
                                ev.stop_propagation();
                                if is_current() { player.toggle(); } else { play_label(id, listing.get_untracked(), LabelMode::All); }
                            }>
                            <Icon name=move || if is_playing() { "pause".to_string() } else { "play".to_string() } />
                        </button>
                    </div>
                })}
            </div>
            <div class="pp-card-meta">
                <div class="pp-card-title truncate">{name.clone()}</div>
                <div class="pp-card-sub num truncate">{count_of(l.release_count, "release")}</div>
                <div class="pp-card-sub num truncate">{count_of(l.track_count, "track")}</div>
                <div class="pp-card-sub num truncate">{size.trim().to_string()}</div>
            </div>
            <a class="pp-card-link" href=format!("/labels/{id}") aria-label=name.clone()></a>
            <button type="button" role="checkbox" aria-checked=move || is_picked().to_string() aria-label=format!("Select {name_sel}")
                class=move || if is_picked() { "pp-check pp-check-btn on" } else if selecting() { "pp-check pp-check-btn always" } else { "pp-check pp-check-btn" }
                on:click=move |ev: web_sys::MouseEvent| { ev.prevent_default(); ev.stop_propagation(); on_pick.run((idx, id, ev.shift_key())); }>
                <Icon name="check" />
            </button>
            <div class="pp-card-tools">
                <FavButton kind=Kind::Label id=id favs=ops.favs overlay=true name=name_fav />
                <button type="button" class="pp-chipbtn" aria-label=format!("More options for {name_more}") aria-haspopup="menu"
                    on:click={
                        let l = l_menu.clone();
                        move |ev: web_sys::MouseEvent| {
                            ev.stop_propagation(); ev.prevent_default();
                            use wasm_bindgen::JsCast;
                            if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) { ops.open_at(&l, Rect::of(&el)); }
                        }
                    }><Icon name="more" /></button>
            </div>
        </div>
    }
}

#[allow(dead_code)]
fn _k() {
    let _ = format_count;
}
