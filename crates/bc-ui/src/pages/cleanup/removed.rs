//! Removed panel: tracks taken out of the library with their files kept. Scans skip these paths
//! until they are restored here, which lets them back in and ingests them straight away.
use bc_types::library::maint::{ExcludedOut, RestoreExcludedRequest, RestoredOut};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Button, EmptyState, Icon, Skeleton, Variant, confirm, toast_err, toast_ok};
use crate::logic::format::format_count;
use crate::pages::settings::{QueryError, SysCard, qh};

const PREVIEW: usize = 50;

pub const QUERY: &str = "/library/excluded";

#[component]
pub fn RemovedPanel() -> impl IntoView {
    let q = qh(use_query::<Vec<ExcludedOut>>(|| Some(QuerySpec::new(QUERY, &["excluded", "track"]))));
    let busy = RwSignal::new(false);
    let expanded = RwSignal::new(false);
    let filter = RwSignal::new(String::new());

    let entries = Signal::derive(move || {
        let f = filter.get().to_lowercase();
        q.data
            .get()
            .map(|v| v.iter().filter(|e| f.is_empty() || format!("{} {} {}", e.artist_name, e.title, e.path).to_lowercase().contains(&f)).cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    });
    let shown = Signal::derive(move || {
        let v = entries.get();
        if expanded.get() { v } else { v.into_iter().take(PREVIEW).collect() }
    });

    let restore = move |paths: Vec<String>| {
        busy.set(true);
        spawn_local(async move {
            match api::post::<_, RestoredOut>("/library/excluded/restore", &RestoreExcludedRequest { paths }).await {
                Ok(r) => {
                    let n = r.tracks_added;
                    toast_ok(&format!("{} track{} back in the library", format_count(n), if n == 1 { "" } else { "s" }));
                    if let Some(e) = r.errors.first() {
                        toast_err(e);
                    }
                    crate::data::invalidate_all();
                    q.refetch();
                }
                Err(e) => toast_err(&e.message()),
            }
            busy.set(false);
        });
    };
    let restore_all = move || {
        let paths: Vec<String> = entries.get_untracked().into_iter().map(|e| e.path).collect();
        if paths.is_empty() {
            return;
        }
        spawn_local(async move {
            let n = paths.len();
            if confirm(
                &format!("Restore {} track{}?", format_count(n as i64), if n == 1 { "" } else { "s" }),
                "They are imported again from their files and future scans pick them up.",
                "Restore",
                false,
            )
            .await
            {
                restore(paths);
            }
        });
    };

    view! {
        <SysCard title="Removed from library" icon="eye-off"
            hint="Tracks removed from the library with their files left on disk. Scans skip these files until you restore them.">
            <QueryError q=q />
            <Show when=move || q.data.get().is_none() && q.error.get().is_none()><Skeleton height="48px" /></Show>
            <Show when=move || q.data.get().map(|v| !v.is_empty()).unwrap_or(false)>
                <div class="row gap wrap">
                    <input class="input grow" type="search" placeholder="Filter removed tracks" aria-label="Filter removed tracks"
                        prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev)) />
                    <Button variant=Variant::Ghost icon="refresh" busy=busy on_click=move |_| restore_all()>
                        {move || if filter.get().is_empty() { "Restore all".to_string() } else { format!("Restore {}", format_count(entries.get().len() as i64)) }}
                    </Button>
                </div>
            </Show>
            <Show when=move || q.data.get().map(|v| v.is_empty()).unwrap_or(false)>
                <EmptyState icon="eye-off" title="Nothing removed" hint="Remove from library on a track keeps its file and lists it here." />
            </Show>
            <ul class="bl-list">
                <For each=move || shown.get() key=|e| e.path.clone() let:e>
                    {
                        let path = e.path.clone();
                        let name = format!("{} - {}", if e.artist_name.is_empty() { "Unknown" } else { &e.artist_name }, if e.title.is_empty() { "Untitled" } else { &e.title });
                        view! {
                            <li>
                                <div class="grow bl-main">
                                    <div class="truncate" title=name.clone()>{name.clone()}</div>
                                    <div class="mono faint truncate" title=e.path.clone()>{e.path.clone()}</div>
                                </div>
                                <button type="button" class="btn btn-ghost btn-icon btn-sm" title="Restore to the library" aria-label=format!("Restore {name}")
                                    disabled=move || busy.get() on:click=move |_| restore(vec![path.clone()])><Icon name="refresh" /></button>
                            </li>
                        }
                    }
                </For>
            </ul>
            <Show when={move || entries.get().len() > PREVIEW}>
                <button type="button" class="btn btn-ghost btn-sm" aria-expanded=move || expanded.get().to_string() on:click=move |_| expanded.update(|v| *v = !*v)>
                    {move || if expanded.get() { "Show fewer".to_string() } else { format!("Show all {}", format_count(entries.get().len() as i64)) }}
                </button>
            </Show>
        </SysCard>
    }
}
