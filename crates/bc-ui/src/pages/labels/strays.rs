//! Single tracks the library holds as records of their own ("strays"), shown on a label page as a
//! chip with the count that opens an explanation and the merge. Merging asks Bandcamp which album
//! each track belongs to, so it runs as a sweep with live progress and a stop button.
use bc_types::library::{StrayOut, StraysOut};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::sweep::use_stray_merge;
use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Button, Dialog, Icon, Size, Variant, children};
use crate::logic::format::format_count;

#[component]
pub fn StraysTrigger(label_id: i64) -> impl IntoView {
    let open = RwSignal::new(false);
    let whole = RwSignal::new(false);
    let merge = use_stray_merge(Callback::new(move |_| {
        crate::data::invalidate_entity("release", &[]);
        crate::data::invalidate_entity("label", &[]);
    }));
    let q = use_query::<StraysOut>(move || {
        let url = if whole.get() { "/releases/strays?limit=6".to_string() } else { format!("/releases/strays?label_id={label_id}&limit=6") };
        Some(QuerySpec::new(url, &["release"]))
    });
    let total = move || q.data.get().map(|d| d.total).unwrap_or(0);
    let resolvable = move || q.data.get().map(|d| d.resolvable).unwrap_or(0);
    let running = move || merge.status.with(|s| s.as_ref().map(|s| s.running).unwrap_or(false));
    let show = move || total() > 0 || running() || merge.report.get();
    let start = move |_| {
        merge.starting.set(true);
        merge.error.set(None);
        let body = if whole.get_untracked() { serde_json::json!({}) } else { serde_json::json!({ "label_id": label_id }) };
        spawn_local(async move {
            let r = api::post::<_, bc_types::library::StraySweepStatus>("/releases/strays/merge", &body).await;
            let _ = merge.starting.try_set(false);
            match r {
                Ok(s) => {
                    let _ = merge.status.try_set(Some(s));
                    let _ = merge.report.try_set(false);
                }
                Err(e) => {
                    let _ = merge.error.try_set(Some(e.message()));
                }
            }
        });
    };
    let stop = move |_| {
        spawn_local(async move {
            if let Ok(s) = api::send::<_, bc_types::library::StraySweepStatus>("DELETE", "/releases/strays/merge", &serde_json::json!({})).await {
                let _ = merge.status.try_set(Some(s));
            }
        });
    };
    view! {
        {move || show().then(|| view! {
            <button type="button" class="chip pp-strays-chip" on:click=move |_| open.set(true) title="Single tracks filed as records of their own">
                <Icon name="layers" />
                {move || if running() { "Merging strays\u{2026}".to_string() } else { format!("{} stray {}", format_count(total()), if total() == 1 { "track" } else { "tracks" }) }}
            </button>
        })}
        <Dialog open=open wide=true title="Stray tracks" footer=children(move || view! {
            <Button variant=Variant::Ghost on_click=move |_| open.set(false)>"Close"</Button>
        })>
            {move || view! {
                <div class="pp-strays">
                    <p class="pp-hint">
                        "Single tracks the library holds as records of their own. Downloading one track off a Bandcamp release tags the file with the track's own title as its album, so it lands here as a one-track \u{201c}album\u{201d} and the record it belongs to is never assembled \u{2014} which is why a label can show more releases than it has published. Merging asks Bandcamp which album each track is from, files it there, and fixes the file's own tags so a rescan cannot undo it."
                    </p>
                    <div class="pp-chiprow">
                        <button type="button" class="chip" aria-pressed=move || (!whole.get()).to_string() on:click=move |_| whole.set(false)><Icon name="layers" />"This label"</button>
                        <button type="button" class="chip" aria-pressed=move || whole.get().to_string() on:click=move |_| whole.set(true)><Icon name="folder" />"Whole library"</button>
                        {move || q.loading.get().then(|| view! { <span class="pp-hint"><span class="pp-spin"></span>" counting\u{2026}"</span> })}
                    </div>
                    <ul class="pp-stray-list">
                        {move || q.data.get().map(|d| {
                            let more = d.total - d.items.len() as i64;
                            let rows = d.items.iter().map(|s: &StrayOut| view! {
                                <li>
                                    <span class="truncate">{s.title.clone()}</span>
                                    {s.artist.clone().map(|a| view! { <span class="truncate faint">{a}</span> })}
                                    {s.track_no.map(|n| view! { <span class="num faint">{format!("track {n}")}</span> })}
                                    {(!s.resolvable).then(|| view! { <span class="badge">"no Bandcamp link"</span> })}
                                </li>
                            }).collect_view();
                            view! { {rows}{(more > 0).then(|| view! { <li class="faint">{format!("\u{2026}and {} more", format_count(more))}</li> })} }
                        })}
                    </ul>
                    {move || {
                        let s = merge.status.get();
                        match s {
                            Some(s) if s.running => view! {
                                <div class="pp-notice">
                                    <span class="pp-spin"></span>
                                    <span class="pp-notice-text"><span class="num">{format!("{}/{}", format_count(s.seen), format_count(s.total.unwrap_or(0)))}</span>{s.current.clone().map(|c| format!(" \u{b7} {c}"))}</span>
                                    <Button size=Size::Sm variant=Variant::Outline on_click=stop>"Stop"</Button>
                                </div>
                            }.into_any(),
                            Some(s) if merge.report.get() && s.phase != "idle" => view! {
                                <div class="pp-notice" role="status"><span class="pp-notice-text">
                                    {format!("Merged {} into {} ({} created). {} unresolved, {} failed.", format_count(s.merged), format_count(s.albums), format_count(s.albums_created), format_count(s.unresolved), format_count(s.failed))}
                                    {s.error.clone().map(|e| format!(" {e}"))}
                                </span></div>
                            }.into_any(),
                            _ => view! {
                                <Button variant=Variant::Primary icon="git-merge" busy=merge.starting disabled=Signal::derive(move || resolvable() == 0 || q.loading.get()) on_click=start
                                    title="One page fetch per track; stoppable at any point">
                                    {move || format!("Merge {} into their albums", format_count(resolvable()))}
                                </Button>
                            }.into_any(),
                        }
                    }}
                    {move || merge.error.get().map(|e| view! { <p class="pp-error" role="alert">{e}</p> })}
                </div>
            }}
        </Dialog>
    }
}
