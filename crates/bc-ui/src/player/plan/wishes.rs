//! What the DJ wants to get to: tracks, artists, labels, tags. Wishes steer suggestions. One
//! search box finds all four kinds; a track can go straight into the queue from here too.
use bc_types::library::{ArtistOut, LabelOut, Page, TagOut, TrackOut, TrackPage};
use bc_types::player::{PlanOp, PlayerCommand, Wish};
use leptos::prelude::*;

use super::Planner;
use super::suggest_body::wish_label;
use super::track_row::{Numbers, RowTrack};
use crate::data::QuerySpec;
use crate::ds::{Icon, SearchInput, use_debounced};
use crate::player::plan::qh::use_qh;
use crate::util::enc;
use crate::widgets::common::queue_item;

fn wish_icon(w: &Wish) -> &'static str {
    match w {
        Wish::Track { .. } => "disc",
        Wish::Artist { .. } => "user",
        Wish::Label { .. } => "folder",
        Wish::Tag { .. } => "tag",
    }
}

fn icon_btn(icon: &'static str, title: String, f: Box<dyn Fn() + Send + Sync>, disabled: Signal<bool>) -> impl IntoView {
    view! { <button type="button" class="btn btn-ghost btn-sm btn-icon" title=title.clone() aria-label=title disabled=move || disabled.get() on:click=move |_| f()><Icon name=icon size=13 /></button> }
}

#[component]
fn ResultRow(icon: &'static str, #[prop(into)] title: String, #[prop(optional, into)] subtitle: Option<String>, #[prop(optional)] numbers: Option<ChildrenFn>, actions: ChildrenFn) -> impl IntoView {
    view! {
        <div class="pp-res">
            <span class="pp-res-ico"><Icon name=icon size=13 /></span>
            <span class="pp-res-main"><span class="truncate pp-res-t">{title}</span>{subtitle.map(|s| view! { <span class="truncate muted pp-res-s">{s}</span> })}</span>
            {numbers.map(|n| n())}
            <span class="pp-res-act">{actions()}</span>
        </div>
    }
}

#[component]
fn Results(#[prop(into)] q: Signal<String>, pl: Planner, on_done: Callback<()>) -> impl IntoView {
    let enabled = move || q.get().trim().chars().count() >= 2;
    let tracks = use_qh::<TrackPage>(move || enabled().then(|| QuerySpec::new(format!("/tracks?limit=8&q={}", enc(q.get().trim())), &["track"])));
    let artists = use_qh::<Page<ArtistOut>>(move || enabled().then(|| QuerySpec::new(format!("/artists?limit=5&q={}", enc(q.get().trim())), &["artist"])));
    let labels = use_qh::<Page<LabelOut>>(move || enabled().then(|| QuerySpec::new(format!("/labels?limit=5&q={}", enc(q.get().trim())), &["label"])));
    let tags = use_qh::<Vec<TagOut>>(move || enabled().then(|| QuerySpec::new(format!("/tags?limit=6&q={}", enc(q.get().trim())), &["tag"])));
    let has_queue = Signal::derive(move || pl.player.state.with(|s| !s.queue.is_empty()));
    let wish = move |w: Wish| pl.op(PlanOp::AddWish { wish: w });

    view! {
        <Show when=enabled>
            <div class="pp-results">
                {move || tracks.data.get().map(|p| p.page.items.iter().cloned().map(|t: TrackOut| {
                    let rt: RowTrack = (&t).into();
                    let (a, b, c) = (t.clone(), t.clone(), t.clone());
                    let title = t.title.clone();
                    let artist = t.artist.as_ref().map(|a| a.name.clone()).unwrap_or_default();
                    let (bpm, camelot, energy) = (rt.bpm, rt.camelot.clone(), rt.energy);
                    view! {
                        <ResultRow icon="disc" title=t.title.clone() subtitle=artist.clone()
                            numbers=crate::ds::children(move || view! { <Numbers bpm=bpm camelot=camelot.clone() energy=energy /> })
                            actions=crate::ds::children(move || {
                                let (a, b, c) = (a.clone(), b.clone(), c.clone());
                                let player = pl.player;
                                let artist = artist.clone();
                                let (t1, t2, t3) = (format!("Play {title} next"), format!("Add {title} to the end"), format!("Wish for {title}"));
                                view! {
                                    {icon_btn("skip-next", t1, Box::new(move || { player.cmd(PlayerCommand::PlayNext { items: vec![queue_item(&a)] }); on_done.run(()); }), has_queue)}
                                    {icon_btn("queue", t2, Box::new(move || { player.cmd(PlayerCommand::AddToQueue { items: vec![queue_item(&b)] }); on_done.run(()); }), Signal::derive(|| false))}
                                    {icon_btn("plus", t3, Box::new(move || wish(Wish::Track { id: c.id, title: c.title.clone(), artist: artist.clone() })), Signal::derive(|| false))}
                                }
                            }) />
                    }
                }).collect_view())}
                {move || artists.data.get().map(|p| p.items.iter().cloned().map(|a| {
                    let (id, name) = (a.id, a.name.clone());
                    let t = format!("Wish for {name}");
                    view! { <ResultRow icon="user" title=a.name.clone() subtitle=format!("{} tracks", a.track_count)
                        actions=crate::ds::children(move || { let (name, t) = (name.clone(), t.clone()); icon_btn("plus", t, Box::new(move || wish(Wish::Artist { id, name: name.clone() })), Signal::derive(|| false)) }) /> }
                }).collect_view())}
                {move || labels.data.get().map(|p| p.items.iter().cloned().map(|l| {
                    let (id, name) = (l.id, l.name.clone());
                    let t = format!("Wish for {name}");
                    view! { <ResultRow icon="folder" title=l.name.clone() subtitle=format!("{} tracks", l.track_count)
                        actions=crate::ds::children(move || { let (name, t) = (name.clone(), t.clone()); icon_btn("plus", t, Box::new(move || wish(Wish::Label { id, name: name.clone() })), Signal::derive(|| false)) }) /> }
                }).collect_view())}
                {move || tags.data.get().map(|l| l.iter().cloned().map(|g| {
                    let name = g.name.clone();
                    let t = format!("Wish for {name}");
                    view! { <ResultRow icon="tag" title=g.name.clone() subtitle=format!("{} tracks", g.track_count)
                        actions=crate::ds::children(move || { let (name, t) = (name.clone(), t.clone()); icon_btn("plus", t, Box::new(move || wish(Wish::Tag { name: name.clone() })), Signal::derive(|| false)) }) /> }
                }).collect_view())}
                {move || {
                    let none = tracks.data.get().map(|p| p.page.items.is_empty()).unwrap_or(true)
                        && artists.data.get().map(|p| p.items.is_empty()).unwrap_or(true)
                        && labels.data.get().map(|p| p.items.is_empty()).unwrap_or(true)
                        && tags.data.get().map(|p| p.is_empty()).unwrap_or(true);
                    (none && !tracks.loading.get()).then(|| view! { <div class="pp-empty">"Nothing in the library matches."</div> })
                }}
            </div>
        </Show>
    }
}

#[component]
pub fn WishBar(pl: Planner) -> impl IntoView {
    let q = RwSignal::new(String::new());
    let dq = use_debounced(q, 200);
    let wishes = Signal::derive(move || pl.plan.with(|p| p.wishes.clone()));
    view! {
        <section class="pp-sec" aria-label="Wishes">
            <div class="pp-seg">
                <span class="pp-lbl">"wishes"</span>
                <SearchInput value=q placeholder="track, artist, label or tag…" class="grow" />
            </div>
            <Results q=dq pl=pl on_done=Callback::new(move |_| q.set(String::new())) />
            <Show when=move || !wishes.with(|w| w.is_empty())>
                <div class="pp-indent pp-chips">
                    {move || wishes.get().into_iter().map(|w| {
                        let key = w.key();
                        let label = wish_label(&w);
                        view! {
                            <button type="button" class="pp-chip on" title=format!("Drop wish: {label}") on:click=move |_| pl.op(PlanOp::RemoveWish { key: key.clone() })>
                                <Icon name=wish_icon(&w) size=10 /><span class="truncate">{label.clone()}</span><Icon name="x" size=10 />
                            </button>
                        }
                    }).collect_view()}
                    <Show when=move || { wishes.with(|w| w.len()) > 1 }>
                        <button type="button" class="pp-link" on:click=move |_| pl.op(PlanOp::ClearWishes)>"clear"</button>
                    </Show>
                </div>
            </Show>
        </section>
    }
}
