//! Blacklist panel: mounted on Cleanup and in Settings > Library, same query key.
use bc_types::Page;
use bc_types::library::maint::BlacklistOut;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Button, EmptyState, Icon, Skeleton, Variant, confirm, toast_err, toast_ok};
use crate::logic::format::format_count;
use crate::pages::settings::{QueryError, SysCard, qh};

use crate::pages::settings::logic::parse_blacklist_input;

const PREVIEW: usize = 8;

#[component]
pub fn BlacklistPanel() -> impl IntoView {
    let q = qh(use_query::<Page<BlacklistOut>>(|| Some(QuerySpec::new("/blacklist?limit=500", &["blacklist"]))));
    let draft = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let expanded = RwSignal::new(false);
    let filter = RwSignal::new(String::new());

    let entries = Signal::derive(move || {
        let f = filter.get().to_lowercase();
        q.data
            .get()
            .map(|p| {
                p.items
                    .iter()
                    .filter(|e| f.is_empty() || format!("{} {} {}", e.artist_name, e.title, e.url.clone().unwrap_or_default()).to_lowercase().contains(&f))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let shown = Signal::derive(move || {
        let v = entries.get();
        if expanded.get() { v } else { v.into_iter().take(PREVIEW).collect() }
    });

    let add = move || {
        let Some(body) = parse_blacklist_input(&draft.get_untracked()) else { return };
        busy.set(true);
        spawn_local(async move {
            match api::post::<_, BlacklistOut>("/blacklist", &body).await {
                Ok(_) => {
                    draft.set(String::new());
                    toast_ok("Blocked");
                    q.refetch();
                }
                Err(e) => toast_err(&e.message()),
            }
            busy.set(false);
        });
    };
    let add2 = add.clone();
    let remove = move |e: BlacklistOut| {
        spawn_local(async move {
            let label = if e.title.is_empty() { e.url.clone().unwrap_or_default() } else { format!("{} - {}", e.artist_name, e.title) };
            if confirm("Allow this again?", &format!("\"{label}\" can be downloaded and queued again. Inbox items it retired stay ignored until you un-ignore them in Harvest."), "Remove from blacklist", false).await {
                match api::call("DELETE", &format!("/blacklist/{}", e.id)).await {
                    Ok(()) => q.refetch(),
                    Err(err) => toast_err(&err.message()),
                }
            }
        });
    };
    let total = Signal::derive(move || q.data.get().map(|p| p.total).unwrap_or(0));
    let title = Signal::derive(move || format!("Blacklist ({})", format_count(total.get())));
    let _ = title;

    view! {
        <SysCard title="Blacklist" icon="x-circle"
            hint="Never downloaded, never queued: matched by Bandcamp URL and by artist plus title, so a release with no URL is still blocked. Removing an entry allows it again.">
            <div class="row gap wrap">
                <input class="input mono grow" spellcheck="false" aria-label="Block a Bandcamp URL or Artist - Title"
                    placeholder="https://label.bandcamp.com/album/...  or  Artist - Title"
                    prop:value=move || draft.get() on:input=move |ev| draft.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" { add2() } />
                <Button variant=Variant::Primary icon="plus" busy=busy disabled=Signal::derive(move || draft.get().trim().is_empty()) on_click=move |_| add()>"Block"</Button>
            </div>
            <QueryError q=q />
            <Show when=move || q.data.get().is_none() && q.error.get().is_none()><Skeleton height="48px" /></Show>
            <Show when=move || q.data.get().map(|p| p.items.len() > PREVIEW).unwrap_or(false)>
                <input class="input" type="search" placeholder="Filter blocked entries" aria-label="Filter blacklist"
                    prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev)) />
            </Show>
            <Show when=move || q.data.get().map(|p| p.items.is_empty()).unwrap_or(false)>
                <EmptyState icon="x-circle" title="Nothing blocked yet" hint="Deleting albums on the Cleanup page can add them here." />
            </Show>
            <ul class="bl-list">
                <For each=move || shown.get() key=|e| e.id let:e>
                    {
                        let e2 = e.clone();
                        let name = if e.artist_name.is_empty() && e.title.is_empty() {
                            e.url.clone().unwrap_or_default()
                        } else {
                            format!("{} - {}", if e.artist_name.is_empty() { "Unknown" } else { &e.artist_name }, if e.title.is_empty() { "Untitled" } else { &e.title })
                        };
                        view! {
                            <li>
                                <div class="grow bl-main">
                                    <div class="truncate" title=name.clone()>{name.clone()}</div>
                                    {e.url.clone().filter(|_| !(e.artist_name.is_empty() && e.title.is_empty())).map(|u| view! { <div class="mono faint truncate">{u}</div> })}
                                </div>
                                {e.reason.clone().map(|r| view! { <span class="badge">{r}</span> })}
                                <button type="button" class="btn btn-ghost btn-icon btn-sm" title="Allow again" aria-label=format!("Remove {name} from the blacklist")
                                    on:click=move |_| remove(e2.clone())><Icon name="trash" /></button>
                            </li>
                        }
                    }
                </For>
            </ul>
            <Show when={move || entries.get().len() > PREVIEW}>
                <button type="button" class="btn btn-ghost btn-sm" aria-expanded=move || expanded.get().to_string() on:click=move |_| expanded.update(|v| *v = !*v)>
                    {move || if expanded.get() { "Show fewer".to_string() } else { format!("Show all {}", entries.get().len()) }}
                </button>
            </Show>
        </SysCard>
    }
}
