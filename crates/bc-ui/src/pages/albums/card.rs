//! The album card: cover with hover play, playing badge, selection tick, badges
//! (snippet / shelf), optional detail lines, context menu. Used by the virtual grid
//! on Albums / Tracks (grid view), the Home shelves and the related shelves.
use std::collections::HashMap;

use bc_types::library::ReleaseOut;
use bc_types::player::PlayerCommand;
use leptos::prelude::*;
use leptos_router::hooks::use_navigate;

use super::host::{fill_release, library_listing, play_release, release_menu};
use super::logic;
use crate::data::{QuerySpec, use_query};
use crate::ds::popover::Rect;
use crate::ds::{Icon, MenuCtx};
use crate::logic::format::{format_count, format_long_duration};
use crate::player::use_player;
use crate::util::enc;
use crate::widgets::common::{Art, artist_link, label_link};

/// Selection mode of the albums grid, provided by the page.
#[derive(Clone, Copy)]
pub struct AlbumSel {
    pub selecting: RwSignal<bool>,
    /// release id -> its track count.
    pub picked: RwSignal<HashMap<i64, i64>>,
    /// (release id, track count, shift held)
    pub toggle: Callback<(i64, i64, bool)>,
}

/// Height of the text area under the square cover (px): title, artist, badges line.
pub const META_H: f64 = 60.0;
/// With detail lines (tags, track count / duration).
pub const META_H_DETAILS: f64 = 94.0;

#[component]
pub fn FanName(fan_id: i64, #[prop(optional)] compact: bool) -> impl IntoView {
    let fans = use_query::<Vec<bc_types::bandcamp::FanOut>>(|| Some(QuerySpec::new("/fans", &["fan"])));
    let name = move || {
        fans.data
            .get()
            .and_then(|f| f.iter().find(|x| x.id == fan_id).map(|x| x.display_name.clone().unwrap_or_else(|| x.username.clone())))
            .unwrap_or_else(|| format!("fan #{fan_id}"))
    };
    view! {
        <span class=if compact { "lib-badge compact" } else { "lib-badge" }
            title=move || format!("On {}'s shelf: downloaded from their wishlist or collection, not in your library until you move it in.", name())>
            <Icon name="users" size=if compact { 10 } else { 12 } /><span class="truncate">{move || format!("from {}", name())}</span>
        </span>
    }
}

#[component]
pub fn SnippetBadge(#[prop(optional)] compact: bool) -> impl IntoView {
    view! {
        <span class=if compact { "lib-badge warn compact" } else { "lib-badge warn" }
            title="A Bandcamp preview clip, not the full track: it stops after a minute or two. Settings > Snippets keeps these out of your library and playlists.">
            <Icon name="scissors" size=if compact { 10 } else { 12 } />"Snippet"
        </span>
    }
}

/// A calm "Pre-order" tag: icon + label, info tone (never colour alone). `title` says when the rest is out.
#[component]
pub fn PreorderBadge(#[prop(optional)] compact: bool, #[prop(into)] title: String) -> impl IntoView {
    view! {
        <span class=if compact { "lib-badge info compact" } else { "lib-badge info" } title=title>
            <Icon name="clock" size=if compact { 10 } else { 12 } />"Pre-order"
        </span>
    }
}

#[component]
pub fn AlbumCard(
    release: ReleaseOut,
    #[prop(optional)] details: bool,
    #[prop(optional)] listing: Option<serde_json::Value>,
    /// Cards on a shelf pass a fixed width; grids size the cell.
    #[prop(optional)] index: usize,
) -> impl IntoView {
    let player = use_player();
    let menu = expect_context::<MenuCtx>();
    let sel = use_context::<AlbumSel>();
    let listing = listing.unwrap_or_else(library_listing);
    let id = release.id;
    let title = release.title.clone();
    let missing = logic::missing_tracks(&release);
    let fillable = logic::fillable_tracks(&release);
    let warn_short = logic::shortfall_is_warning(&release);
    let preorder = release.is_preorder.then(|| logic::out_badge(release.release_date.as_deref()));

    let is_current = Memo::new(move |_| player.state.with(|s| s.current.as_ref().and_then(|c| c.release_id) == Some(id)));
    let is_playing = Memo::new(move |_| is_current.get() && player.is_playing());
    let picked = move || sel.map(|s| s.picked.with(|p| p.contains_key(&id))).unwrap_or(false);
    let selecting = move || sel.map(|s| s.selecting.get()).unwrap_or(false);
    let filled = RwSignal::new(false);

    let navigate = use_navigate();
    let nav = Callback::new(move |p: String| navigate(&p, Default::default()));

    let r_menu = release.clone();
    let l_menu = listing.clone();
    let open_at = move |anchor: Rect| {
        let entries = release_menu(&r_menu, l_menu.clone(), player, nav, None);
        menu.open_titled(anchor, &r_menu.title, entries);
    };
    let open_at = std::sync::Arc::new(open_at);
    let open_ctx = open_at.clone();
    let open_btn = open_at;

    let l_play = listing.clone();
    let on_play = move |ev: leptos::ev::MouseEvent| {
        ev.stop_propagation();
        if is_current.get_untracked() {
            player.cmd(PlayerCommand::Toggle);
        } else {
            play_release(player, id, l_play.clone(), false);
        }
    };

    let href = format!("/albums/{id}");
    let (tc, tit2) = (release.track_count, title.clone());
    let click_select = move |ev: leptos::ev::MouseEvent| {
        if let Some(s) = sel {
            if s.selecting.get_untracked() {
                ev.prevent_default();
                s.toggle.run((id, tc, ev.shift_key()));
            }
        }
    };
    let artist = match &release.artist {
        Some(a) => artist_link(Some(a)).into_any(),
        None => view! { "Unknown artist" }.into_any(),
    };
    let label = release.label.clone().filter(|_| details).map(|l| view! { {label_link(Some(&l), release.label_id)}" · " });
    let year = release.year;
    let tags: Vec<String> = release.tags.iter().take(3).cloned().collect();
    let tracks_line = {
        let n = release.track_count;
        let base = if missing > 0 { format!("{}/{} tracks", n, release.expected_track_count.unwrap_or(n)) } else { format!("{} {}", n, if n == 1 { "track" } else { "tracks" }) };
        let dur = if release.duration_ms > 0 { format!(" · {}", format_long_duration(release.duration_ms as f64)) } else { String::new() };
        format!("{}{}{}", year.map(|y| format!("{y} · ")).unwrap_or_default(), base, dur)
    };
    let (snippet, fan) = (release.snippet_only, release.source_fan_id);
    let art = release.art_url.as_deref().map(|u| logic::art_size(u, "medium"));
    let aria = title.clone();
    let more_label = format!("More options for {title}");

    view! {
        <div class=move || {
            let mut c = String::from("alb");
            if is_current.get() { c.push_str(" cur"); }
            if picked() { c.push_str(" picked"); }
            if selecting() { c.push_str(" selecting"); }
            c
        }
            data-release-id=id data-index=index
            on:contextmenu=move |ev: leptos::ev::MouseEvent| {
                ev.prevent_default();
                open_ctx(Rect::point(ev.client_x() as f64, ev.client_y() as f64));
            }>
            <div class="alb-cover">
                <Art src=art class="alb-art" />
                {move || is_current.get().then(|| view! {
                    <span class="alb-now" class:paused=move || !is_playing.get()>
                        <span class="eq" aria-hidden="true"><i></i><i></i><i></i></span>
                        {move || if is_playing.get() { "Playing" } else { "Paused" }}
                    </span>
                })}
                {move || selecting().then(|| view! {
                    <span class="alb-tick" class:on=picked aria-hidden="true">{move || picked().then(|| view! { <Icon name="check" size=14 /> })}</span>
                })}
                <button type="button" class="alb-play" aria-label=move || format!("{} {}", if is_playing.get() { "Pause" } else { "Play" }, aria) on:click=on_play>
                    <Icon name=move || if is_playing.get() { "pause" } else { "play" } size=18 />
                </button>
            </div>
            <div class="alb-meta" style=if details { format!("height:{}px", META_H_DETAILS) } else { format!("height:{}px", META_H) }>
                <div class="alb-title truncate">{title}</div>
                <div class="alb-artist truncate">{artist}{(!details).then(|| year.map(|y| format!(" · {y}"))).flatten()}</div>
                {(snippet || fan.is_some() || preorder.is_some()).then(|| view! {
                    <div class="alb-badges">
                        {preorder.clone().map(|d| view! { <PreorderBadge compact=true title=d /> })}
                        {snippet.then(|| view! { <SnippetBadge compact=true /> })}
                        {fan.map(|f| view! { <FanName fan_id=f compact=true /> })}
                    </div>
                })}
                {details.then(|| view! {
                    <div class="alb-tags truncate">
                        {if tags.is_empty() { view! { " " }.into_any() } else {
                            tags.iter().enumerate().map(|(i, t)| view! {
                                {(i > 0).then_some(" · ")}
                                <a class="alb-taglink" href=format!("/tracks?tag={}", enc(t))>{t.clone()}</a>
                            }).collect_view().into_any()
                        }}
                    </div>
                    <div class="alb-info mono truncate">
                        {label}
                        {if warn_short {
                            view! { <span class="warn-text" title=format!("{missing} of the record's tracks are missing")>{tracks_line.clone()}</span> }.into_any()
                        } else { view! { {tracks_line.clone()} }.into_any() }}
                    </div>
                    {(fillable > 0).then(|| view! {
                        <button type="button" class="lib-pill warn tiny" disabled=move || filled.get()
                            on:click=move |ev| { ev.stop_propagation(); if !filled.get_untracked() { filled.set(true); fill_release(id); } }>
                            <Icon name=move || if filled.get() { "check" } else { "download" } size=11 />
                            {move || if filled.get() { "Queued" } else { "Fill missing" }}
                        </button>
                    })}
                })}
            </div>
            <a class="alb-link" href=href aria-label=tit2 on:click=click_select></a>
            <button type="button" class="alb-more" aria-label=more_label aria-haspopup="menu" on:click=move |ev: leptos::ev::MouseEvent| {
                use wasm_bindgen::JsCast;
                ev.stop_propagation();
                if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                    open_btn(Rect::of(&el));
                }
            }>
                <Icon name="more" size=16 />
            </button>
        </div>
    }
}

/// A horizontally scrolling shelf of cards (Home, related shelves).
#[component]
pub fn CardRow(children: Children) -> impl IntoView {
    view! { <div class="lib-row" role="list">{children()}</div> }
}

pub fn count_label(n: i64, one: &str, many: &str) -> String {
    format!("{} {}", format_count(n), if n == 1 { one } else { many })
}
