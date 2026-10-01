//! Stray tracks: single tracks held as one-track "albums". Merging asks Bandcamp
//! which album each is from, so it runs as a sweep with progress and a Stop button.
use bc_types::library::maint::{StrayMergeRequest, StraySweepStatus, StraysOut};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::data::{QuerySpec, use_query, use_topic};
use crate::ds::{Button, EmptyState, Icon, Meter, Skeleton, StatusBadge, Tone, Variant, toast_err};
use crate::logic::format::format_count;
use crate::pages::settings::{QueryError, SysCard, qh};

const PREVIEW: usize = 40;

#[component]
pub fn StraysPanel() -> impl IntoView {
    let strays = qh(use_query::<StraysOut>(|| Some(QuerySpec::new(format!("/releases/strays?limit={PREVIEW}"), &["release", "strays"]))));
    let status = qh(use_query::<StraySweepStatus>(|| Some(QuerySpec::new("/releases/strays/merge", &["strays.status"]))));
    use_topic::<StraySweepStatus>("library.strays", move |s| {
        crate::data::cache::patch::<StraySweepStatus>("/releases/strays/merge", |x| *x = s);
    });
    let running = Signal::derive(move || status.data.get().map(|s| s.running).unwrap_or(false));
    let report = RwSignal::new(false);
    let was_running = StoredValue::new(false);
    Effect::new(move |_| {
        let r = running.get();
        if was_running.get_value() && !r {
            report.set(true);
            strays.refetch();
            crate::data::invalidate_entity("release", &[]);
            crate::data::invalidate_entity("label", &[]);
        }
        was_running.set_value(r);
    });
    let busy = RwSignal::new(false);
    let start = move |_| {
        busy.set(true);
        report.set(false);
        spawn_local(async move {
            match api::post::<_, StraySweepStatus>("/releases/strays/merge", &StrayMergeRequest::default()).await {
                Ok(s) => crate::data::cache::patch::<StraySweepStatus>("/releases/strays/merge", |x| *x = s),
                Err(e) => toast_err(&e.message()),
            }
            busy.set(false);
        });
    };
    let stop = move |_| {
        spawn_local(async move {
            match api::send::<(), StraySweepStatus>("DELETE", "/releases/strays/merge", &()).await {
                Ok(s) => crate::data::cache::patch::<StraySweepStatus>("/releases/strays/merge", |x| *x = s),
                Err(e) => toast_err(&e.message()),
            }
        });
    };
    let resolvable = Signal::derive(move || strays.data.get().map(|s| s.resolvable).unwrap_or(0));
    let total = Signal::derive(move || strays.data.get().map(|s| s.total).unwrap_or(0));
    let meter = Signal::derive(move || status.data.get().and_then(|s| s.total.filter(|t| *t > 0).map(|t| s.seen as f64 / t as f64)));

    view! {
        <SysCard title="Stray tracks" icon="layers"
            hint="Single tracks the library holds as records of their own. Downloading one track off a Bandcamp release tags the file with the track's title as its album, so it lands here as a one-track album and the real record is never assembled, which is why a label can show more releases than it has published. Merging asks Bandcamp which album each track is from, files it there and fixes the file's own tags so a rescan cannot undo it.">
            <QueryError q=strays />
            <Show when=move || strays.data.get().is_none() && strays.error.get().is_none()><Skeleton height="80px" /></Show>
            <Show when=move || strays.data.get().map(|s| s.total == 0).unwrap_or(false) && !running.get() && !report.get()>
                <EmptyState icon="check-circle" title="No stray tracks" hint="Every track is filed under a real album." />
            </Show>
            <ul class="stray-list">
                <For each=move || strays.data.get().map(|s| s.items.clone()).unwrap_or_default() key=|s| s.release_id let:s>
                    <li>
                        <span class="truncate strong">{s.title.clone()}</span>
                        {s.artist.clone().map(|a| view! { <span class="truncate faint">{a}</span> })}
                        {s.track_no.map(|n| view! { <span class="mono faint">{format!("track {n}")}</span> })}
                        {(!s.resolvable).then(|| view! { <span class="badge"><Icon name="link" />"no Bandcamp link"</span> })}
                    </li>
                </For>
            </ul>
            <Show when={move || total.get() > PREVIEW as i64}>
                <p class="sys-hint faint mono">{move || format!("...and {} more", format_count(total.get() - PREVIEW as i64))}</p>
            </Show>
            <Show when={move || total.get() > 0 || running.get()}>
                <div class="row gap wrap">
                    <Show when=move || !running.get()>
                        <Button variant=Variant::Primary icon="git-merge" busy=busy disabled=Signal::derive(move || resolvable.get() == 0)
                            title="One page fetch per track; stoppable at any point" on_click=start>
                            {move || format!("Merge {} into their albums", format_count(resolvable.get()))}
                        </Button>
                        {move || (total.get() > resolvable.get()).then(|| view! {
                            <span class="faint">{format!("{} have no Bandcamp link and cannot be resolved.", format_count(total.get() - resolvable.get()))}</span>
                        })}
                    </Show>
                    <Show when=move || running.get()>
                        <StatusBadge tone=Tone::Info label=Signal::derive(move || status.data.get().map(|s| format!("Merging {} / {}", format_count(s.seen), format_count(s.total.unwrap_or(0)))).unwrap_or_default()) />
                        <Button variant=Variant::Danger icon="x" on_click=stop>"Stop"</Button>
                    </Show>
                </div>
                <Show when=move || running.get()>
                    <Meter value=meter label="Merge progress" />
                    <p class="mono faint truncate">{move || status.data.get().and_then(|s| s.current.clone()).unwrap_or_default()}</p>
                </Show>
            </Show>
            {move || (report.get() && !running.get()).then(|| status.data.get().map(|s| view! {
                <p class="sys-notice ok" role="status"><Icon name="check-circle" />
                    <span>{format!("Merged {}{}{}{}{}{}",
                        format_count(s.merged),
                        if s.albums_created > 0 { format!(", built {} albums", format_count(s.albums_created)) } else { String::new() },
                        if s.singles > 0 { format!(", {} really are singles", format_count(s.singles)) } else { String::new() },
                        if s.albums > 0 { format!(", {} were the album already", format_count(s.albums)) } else { String::new() },
                        if s.failed > 0 { format!(", {} could not be read", format_count(s.failed)) } else { String::new() },
                        s.error.clone().map(|e| format!(", {e}")).unwrap_or_default())}
                        {if s.fills_queued > 0 { " The rest of each record is queued: progress on the Downloads page.".to_string() } else if s.albums_created > 0 { " Each new album holds the one track it was built from; Fill missing on the Albums page fetches the rest.".to_string() } else { String::new() }}
                    </span>
                </p>
            }))}
        </SysCard>
    }
}
