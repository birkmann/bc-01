//! The DJ sets shelf: a card per set with a cover collage, rename, status menu and delete, plus a
//! create tile that expands in place.
use bc_types::sets::{DjSetCreate, DjSetDetail, DjSetListOut};
use leptos::prelude::*;
use leptos_router::hooks::use_navigate;

use super::status::StatusPill;
use crate::api;
use crate::data::QuerySpec;
use crate::ds::{Button, Dialog, EmptyState, ErrorPanel, Icon, MenuButton, MenuEntry, MenuItem, PageHeader, Skeleton, Variant, confirm, toast_err, toast_ok};
use crate::logic::format::{format_count, format_long_duration};
use crate::pages::playlists::Collage;
use crate::player::plan::qh::{Qh, use_qh};

#[component]
fn CreateTile() -> impl IntoView {
    let navigate = use_navigate();
    let open = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let target = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let create = move || {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        let n = name.get_untracked();
        let t = target.get_untracked().trim().parse::<i64>().ok().filter(|t| *t > 0);
        let nav = navigate.clone();
        leptos::task::spawn_local(async move {
            let body = DjSetCreate {
                name: if n.trim().is_empty() { "Untitled set".into() } else { n.trim().to_string() },
                venue: None,
                event_date: None,
                target_minutes: t,
                from_playlist_id: None,
                pool_sources: vec![],
            };
            match api::post::<_, DjSetDetail>("/sets", &body).await {
                Ok(d) => nav(&format!("/sets/{}", d.set.id), Default::default()),
                Err(e) => toast_err(&e.message()),
            }
            let _ = busy.try_set(false);
        });
    };
    view! {
        <Show when=move || open.get() fallback=move || view! {
            <button type="button" class="set-tile new" on:click=move |_| open.set(true)>
                <Icon name="plus" /><span>"New set"</span>
            </button>
        }>
            <form class="set-tile form" on:submit={ let create = create.clone(); move |ev| { ev.prevent_default(); create(); } }>
                <input class="input" placeholder="Set name…" aria-label="Set name" autofocus
                    prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Escape" { open.set(false) } />
                <input class="input" type="number" min="1" placeholder="Target minutes (optional)" aria-label="Target minutes"
                    prop:value=move || target.get() on:input=move |ev| target.set(event_target_value(&ev)) />
                <Button variant=Variant::Primary kind="submit" busy=busy on_click={ let create = create.clone(); move |_| create() }>"Create"</Button>
            </form>
        </Show>
    }
}

#[component]
fn SetCard(card: DjSetListOut, list: Qh<Vec<DjSetListOut>>) -> impl IntoView {
    let id = card.id;
    let name = RwSignal::new(card.name.clone());
    let rename_open = RwSignal::new(false);
    let status = Signal::derive({
        let s = card.status.clone();
        move || s.clone()
    });
    let patch = move |body: serde_json::Value| {
        leptos::task::spawn_local(async move {
            match api::patch::<_, DjSetDetail>(&format!("/sets/{id}"), &body).await {
                Ok(_) => list.refetch(),
                Err(e) => toast_err(&e.message()),
            }
        });
    };
    let do_rename = move || {
        let n = name.get_untracked().trim().to_string();
        rename_open.set(false);
        if !n.is_empty() {
            patch(serde_json::json!({ "name": n }));
        }
    };
    let card_name = card.name.clone();
    let menu = Callback::new(move |_| -> Vec<MenuEntry> {
        let cn = card_name.clone();
        vec![
            MenuItem::new("Rename…").icon("edit").on(move || rename_open.set(true)).into(),
            MenuEntry::Sep,
            MenuItem::new("Delete set…").icon("trash").danger().on(move || {
                let cn = cn.clone();
                leptos::task::spawn_local(async move {
                    if !confirm("Delete this set?", &format!("\"{cn}\" and its plan will be deleted. The tracks stay in your library."), "Delete set", true).await {
                        return;
                    }
                    match api::call("DELETE", &format!("/sets/{id}")).await {
                        Ok(()) => { toast_ok("Set deleted"); list.refetch(); }
                        Err(e) => toast_err(&e.message()),
                    }
                });
            }).into(),
        ]
    });
    let urls: Vec<String> = card.art_urls.clone();
    let pool_only = card.track_count == 0 && card.pool_source_count > 0;
    view! {
        <div class="set-card">
            <a class="set-card-link" href=format!("/sets/{id}") aria-label=format!("Open {}", card.name)></a>
            <Collage urls=urls icon="sliders" class="set-collage" />
            <div class="set-card-head">
                <span class="set-card-name truncate">{card.name.clone()}</span>
                <StatusPill status=status on_change=Callback::new(move |s: String| patch(serde_json::json!({ "status": s }))) />
                <MenuButton entries=menu class="btn-sm set-card-menu" title="More actions" />
            </div>
            <div class="set-card-stats mono">
                <span>{format!("{} track{}", format_count(card.track_count), if card.track_count == 1 { "" } else { "s" })}</span>
                {(card.est_duration_ms > 0).then(|| view! { <span>{format_long_duration(card.est_duration_ms as f64)}</span> })}
                {card.avg_bpm.map(|b| view! { <span>{format!("avg {} BPM", b.round() as i64)}</span> })}
                {pool_only.then(|| view! { <span class="faint"><Icon name="alert-circle" size=11 />" pool only"</span> })}
            </div>
            <Dialog open=rename_open title="Rename set"
                footer=crate::ds::children(move || view! {
                    <Button variant=Variant::Ghost on_click=move |_| rename_open.set(false)>"Cancel"</Button>
                    <Button variant=Variant::Primary on_click=move |_| do_rename()>"Rename"</Button>
                })>
                <input class="input" aria-label="Set name" prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" { do_rename() } />
            </Dialog>
        </div>
    }
}

#[component]
pub fn SetsPage() -> impl IntoView {
    let list = use_qh::<Vec<DjSetListOut>>(|| Some(QuerySpec::new("/sets", &["set"])));
    view! {
        <div class="page dj-page">
            <PageHeader title="DJ Sets" subtitle="Build a pool, automix it, fine-tune the transitions" />
            <div class="page-scroll">
                <div class="set-grid">
                    <CreateTile />
                    {move || list.data.get().map(|l| l.iter().cloned().map(|c| view! { <SetCard card=c list=list /> }).collect_view())}
                    {move || (list.data.with(|d| d.is_none()) && list.error.with(|e| e.is_none())).then(|| (0..5).map(|_| view! { <Skeleton height="200px" /> }).collect_view())}
                </div>
                {move || list.data.with(|d| d.as_ref().map(|l| l.is_empty()).unwrap_or(false)).then(|| view! { <EmptyState title="No sets yet" hint="Create one and start planning." icon="sliders" /> })}
                {move || (list.data.with(|d| d.is_none())).then(|| list.error.get().map(|e| view! { <ErrorPanel message=e.message() on_retry=Callback::new(move |_| list.refetch()) /> })).flatten()}
            </div>
        </div>
    }
}
