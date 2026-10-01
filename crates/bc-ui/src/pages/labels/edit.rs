//! Edit dialog (rename + Bandcamp page) and the remove-label flow, shared by the label shelf and
//! the label page. Artists reuse the dialog with renaming off.
use bc_types::library::{DeleteLabelResult, LabelOut};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::logic::count_of;
use super::shared::Kind;
use crate::api;
use crate::ds::{Button, Dialog, Variant, children, confirm, toast_err, toast_ok};
use crate::logic::format::format_bytes;

/// What the dialog edits.
#[derive(Clone, PartialEq, Debug)]
pub struct EditTarget {
    pub kind: Kind,
    pub id: i64,
    pub name: String,
    pub url: Option<String>,
}

/// Rename a label or pin the Bandcamp page an artist/label lives at.
/// `on_saved` gets the id of the surviving row (a rename onto an existing label merges).
#[component]
pub fn EditDialog(target: RwSignal<Option<EditTarget>>, on_saved: Callback<i64>) -> impl IntoView {
    let open = RwSignal::new(false);
    let name = RwSignal::new(String::new());
    let url = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        if let Some(t) = target.get() {
            name.set(t.name.clone());
            url.set(t.url.clone().unwrap_or_default());
            error.set(None);
            open.set(true);
        }
    });
    Effect::new(move |prev: Option<bool>| {
        let o = open.get();
        if prev == Some(true) && !o {
            target.set(None);
        }
        o
    });
    let save = move |_| {
        let Some(t) = target.get_untracked() else { return };
        let mut body = serde_json::json!({ "bandcamp_url": url.get_untracked().trim() });
        {
            let n = name.get_untracked().trim().to_string();
            if n.is_empty() {
                return;
            }
            body["name"] = serde_json::json!(n);
        }
        busy.set(true);
        spawn_local(async move {
            let r = api::patch::<_, serde_json::Value>(&format!("/{}/{}", t.kind.path(), t.id), &body).await;
            let _ = busy.try_set(false);
            match r {
                Ok(v) => {
                    let survivor = v.get("id").and_then(|i| i.as_i64()).unwrap_or(t.id);
                    let _ = open.try_set(false);
                    on_saved.run(survivor);
                    toast_ok("Saved");
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.message()));
                }
            }
        });
    };
    let title = Signal::derive(move || target.get().map(|t| format!("Edit {}", t.kind.noun())).unwrap_or_default());
    view! {
        <Dialog open=open title=title footer=children(move || view! {
            <Button variant=Variant::Ghost on_click=move |_| open.set(false)>"Cancel"</Button>
            <Button variant=Variant::Primary busy=busy on_click=save>"Save"</Button>
        })>
            {move || {
                let kind = target.get().map(|t| t.kind).unwrap_or(Kind::Artist);
                view! {
                    <form class="pp-form" on:submit=move |ev| { ev.prevent_default(); save(()); }>
                        <p class="pp-hint">
                            {match kind {
                                Kind::Label => "Renaming onto a name that already exists merges the two folders into one.",
                                Kind::Artist => "The Bandcamp page is what the bio, \u{201c}Find new releases\u{201d} and the catalogue download all read from.",
                            }}
                        </p>
                        {true.then(|| view! {
                            <label class="pp-field"><span>"Name"</span>
                                <input class="input" prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) spellcheck="false" />
                            </label>
                        })}
                        <label class="pp-field"><span>"Bandcamp page"</span>
                            <input class="input mono" placeholder=format!("https://the{}.bandcamp.com", kind.noun()) prop:value=move || url.get()
                                on:input=move |ev| url.set(event_target_value(&ev)) spellcheck="false" />
                        </label>
                        {move || error.get().map(|e| view! { <p class="pp-error" role="alert">{e}</p> })}
                        <button type="submit" class="pp-hidden-submit" tabindex="-1" aria-hidden="true"></button>
                    </form>
                }
            }}
        </Dialog>
    }
}

/// Confirm, delete the label with its releases and files, and report. `done` runs on success.
pub fn remove_label(label: LabelOut, done: Callback<DeleteLabelResult>) {
    spawn_local(async move {
        let size = if label.size_bytes > 0 { format!(" ({})", format_bytes(label.size_bytes as f64)) } else { String::new() };
        let body = format!(
            "Deletes \u{201c}{}\u{201d} together with its {} and {}{size} \u{2014} the files are removed from disk. This cannot be undone.",
            label.name,
            count_of(label.release_count, "release"),
            count_of(label.track_count, "track")
        );
        if !confirm("Remove label", &body, "Delete", true).await {
            return;
        }
        match api::send::<_, DeleteLabelResult>("DELETE", &format!("/labels/{}", label.id), &serde_json::json!({})).await {
            Ok(res) => {
                toast_ok(&format!("Removed {} \u{2014} {}, {} and {} deleted.", label.name, count_of(res.releases, "release"), count_of(res.tracks, "track"), count_of(res.files, "file")));
                done.run(res);
            }
            Err(e) => toast_err(&e.message()),
        }
    });
}
