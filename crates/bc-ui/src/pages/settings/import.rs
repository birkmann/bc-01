//! Settings > Import from the old app. The import is a CLI operation today
//! (`bc import`); there is no HTTP route, so this section explains it and shows
//! `library.import.progress` events when a server-side import publishes them.
use leptos::prelude::*;

use super::common::SysCard;
use crate::data::use_topic;
use crate::ds::{Button, Icon, toast_ok};

#[component]
pub fn ImportSection() -> impl IntoView {
    let last = RwSignal::new(None::<String>);
    use_topic::<bc_types::library::ImportProgress>("library.import.progress", move |p| {
        last.set(Some(if p.done { format!("{} ({})", p.message, if p.ok == Some(true) { "ok" } else { "finished with problems" }) } else { p.message }));
    });
    let from = RwSignal::new(String::new());
    let force = RwSignal::new(false);
    let skip = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let start = move |_| {
        let body = bc_types::library::ImportRequest {
            from: Some(from.get_untracked().trim().to_string()).filter(|s| !s.is_empty()),
            force: force.get_untracked(),
            skip_repairs: skip.get_untracked(),
        };
        busy.set(true);
        leptos::task::spawn_local(async move {
            match crate::api::post::<_, bc_types::Accepted>("/library/import", &body).await {
                Ok(_) => toast_ok("Import started; progress shows below"),
                Err(e) => crate::ds::toast_err(&e.message()),
            }
            let _ = busy.try_set(false);
        });
    };
    view! {
        <SysCard title="Import from the old app" icon="download"
            hint="Brings your library, playlists, sets, loved tracks, analysis and settings over from the Python Bandcamp manager. Your music files are not touched.">
            <div class="col gap">
                <label class="field"><span class="label">"Old data folder or library.db (empty = BC_LEGACY_DB)"</span>
                    <input class="input mono" prop:value=move || from.get() on:input=move |ev| from.set(event_target_value(&ev)) placeholder="/path/to/old/data" /></label>
                <label class="check"><input type="checkbox" prop:checked=move || force.get() on:change=move |ev| force.set(event_target_checked(&ev)) />"Replace an existing library (the old one is moved aside)"</label>
                <label class="check"><input type="checkbox" prop:checked=move || skip.get() on:change=move |ev| skip.set(event_target_checked(&ev)) />"Skip the one-off repair passes"</label>
                <div class="row"><Button variant=crate::ds::Variant::Primary icon="download" busy=busy on_click=start>"Start import"</Button>
                    <span class="faint">"Only allowed on an empty library, or with the replace option."</span></div>
            </div>
            {move || last.get().map(|l| view! { <p class="sys-notice info" role="status"><Icon name="info" /><span class="mono truncate">{l}</span></p> })}
            <p class="sys-hint faint">"The verification report (row counts, search-index parity, playlist checksums) is in the task result; the same import is available as `bc import` on the command line."</p>
        </SysCard>
    }
}
