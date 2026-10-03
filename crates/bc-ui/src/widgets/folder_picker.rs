//! Folder picker for library roots: walks folders on the machine running bc (`GET
//! /library/browse`), so it works the same from a paired phone as in the app window.
use bc_types::library::BrowseOut;
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::ds::{Button, Dialog, Icon, Switch, Variant};
use crate::logic::format::format_count;
use crate::util::enc;

/// `open` shows it, starting at `start` (a typed path; the music folder when empty or missing).
/// `on_pick` gets the chosen folder's full path.
#[component]
pub fn FolderPicker(
    open: RwSignal<bool>,
    #[prop(into)] start: Signal<String>,
    on_pick: Callback<String>,
    #[prop(optional, into)] title: Option<String>,
    #[prop(optional, into)] confirm_label: Option<String>,
) -> impl IntoView {
    let st = Browser {
        listing: RwSignal::new(None),
        error: RwSignal::new(None),
        loading: RwSignal::new(false),
        hidden: RwSignal::new(false),
        seq: StoredValue::new(0),
    };
    let Browser { listing, error, loading, hidden, .. } = st;
    Effect::new(move |_| {
        if open.get() {
            let s = start.get_untracked().trim().to_string();
            listing.set(None);
            error.set(None);
            st.load((!s.is_empty()).then_some(s), true);
        }
    });
    // long paths keep the deepest folders in view
    let crumbs_ref = NodeRef::<leptos::html::Ol>::new();
    Effect::new(move |_| {
        listing.track();
        if let Some(el) = crumbs_ref.get() {
            el.set_scroll_left(el.scroll_width());
        }
    });
    let go = move |p: String| st.load(Some(p), false);
    let reload = move || st.load(listing.get_untracked().map(|b| b.path), false);

    let confirm_label = confirm_label.unwrap_or_else(|| "Use this folder".into());
    let pick = move || {
        if let Some(b) = listing.get_untracked() {
            on_pick.run(b.path);
            open.set(false);
        }
    };
    let footer = crate::ds::children(move || {
        let label = confirm_label.clone();
        view! {
            <label class="fp-hidden">
                <Switch value=hidden label="Show hidden folders" on_change=Callback::new(move |_| reload()) />
                <span>"Hidden folders"</span>
            </label>
            <span class="grow"></span>
            <Button variant=Variant::Ghost on_click=move |_| open.set(false)>"Cancel"</Button>
            <Button variant=Variant::Primary icon="check" disabled=Signal::derive(move || listing.with(|l| l.is_none()))
                on_click=move |_| pick()>{label}</Button>
        }
    });

    view! {
        <Dialog open=open title=title.unwrap_or_else(|| "Choose a folder".into()) wide=true footer=footer>
            <div class="fp">
                <nav class="fp-places" aria-label="Places">
                    {move || listing.get().map(|b| {
                        let active = active_place(&b);
                        b.places.into_iter().enumerate().map(|(i, p)| {
                            let icon = match p.kind.as_str() { "home" => "home", "music" => "music", "drive" => "hdd", _ => "monitor" };
                            let here = active == Some(i);
                            let path = p.path.clone();
                            view! {
                                <button type="button" class="fp-place" class:active=here title=p.path.clone() on:click=move |_| go(path.clone())>
                                    <Icon name=icon size=14 /><span class="truncate">{p.label}</span>
                                </button>
                            }
                        }).collect_view()
                    })}
                </nav>
                <div class="fp-main">
                    <div class="fp-bar">
                        <button type="button" class="fp-up" title="Up one folder" aria-label="Up one folder"
                            disabled=move || listing.with(|l| l.as_ref().and_then(|b| b.parent.as_ref()).is_none())
                            on:click=move |_| if let Some(p) = listing.get_untracked().and_then(|b| b.parent) { go(p) }>
                            <Icon name="arrow-up" size=14 />
                        </button>
                        <ol class="fp-crumbs" aria-label="Current folder" node_ref=crumbs_ref>
                            {move || listing.get().map(|b| {
                                let crumbs = crumbs(&b.path);
                                let last = crumbs.len().saturating_sub(1);
                                crumbs.into_iter().enumerate().map(|(i, (label, path))| view! {
                                    <li>
                                        <button type="button" class="fp-crumb mono" aria-current=(i == last).then_some("location")
                                            on:click=move |_| go(path.clone())>{label}</button>
                                    </li>
                                }).collect_view()
                            })}
                        </ol>
                    </div>
                    <div class="fp-list" role="list" aria-busy=move || loading.get().to_string()>
                        {move || match (error.get(), listing.get()) {
                            (Some(e), _) => view! { <p class="fp-empty sys-notice danger" role="alert"><Icon name="alert" />{e}</p> }.into_any(),
                            (None, None) => view! { <p class="fp-empty faint">"Loading\u{2026}"</p> }.into_any(),
                            (None, Some(b)) if b.dirs.is_empty() => view! { <p class="fp-empty faint">"No folders in here."</p> }.into_any(),
                            (None, Some(b)) => b.dirs.into_iter().map(|d| {
                                let path = d.path.clone();
                                view! {
                                    <button type="button" role="listitem" class="fp-dir" title=d.path.clone() on:click=move |_| go(path.clone())>
                                        <Icon name="folder" size=15 /><span class="truncate">{d.name}</span><Icon name="chevron-right" size=13 />
                                    </button>
                                }
                            }).collect_view().into_any(),
                        }}
                    </div>
                    <p class="fp-status faint" role="status">{move || listing.get().map(|b| status_line(&b))}</p>
                </div>
            </div>
        </Dialog>
    }
}

#[derive(Clone, Copy)]
struct Browser {
    listing: RwSignal<Option<BrowseOut>>,
    error: RwSignal<Option<String>>,
    loading: RwSignal<bool>,
    hidden: RwSignal<bool>,
    /// The newest request wins when folders are clicked faster than they load.
    seq: StoredValue<u32>,
}

impl Browser {
    /// List `path` (the server's default, the music folder, for `None`). With `fallback`, a path
    /// that does not exist (yet) lists the default instead of an error.
    fn load(self, path: Option<String>, fallback: bool) {
        self.seq.update_value(|s| *s += 1);
        let mine = self.seq.get_value();
        self.loading.set(true);
        spawn_local(async move {
            let mut url = format!("/library/browse?hidden={}", self.hidden.get_untracked());
            if let Some(p) = &path {
                url.push_str(&format!("&path={}", enc(p)));
            }
            let res = api::get::<BrowseOut>(&url).await;
            if self.seq.get_value() != mine {
                return;
            }
            self.loading.set(false);
            match res {
                Ok(b) => {
                    self.error.set(None);
                    self.listing.set(Some(b));
                }
                Err(e) if fallback && path.is_some() && e.status == 404 => self.load(None, false),
                Err(e) => self.error.set(Some(e.message())),
            }
        });
    }
}

/// The place the current folder is in: the deepest one whose path contains it (`/` only when
/// nothing else does).
fn active_place(b: &BrowseOut) -> Option<usize> {
    let inside = |place: &str| {
        let place = place.trim_end_matches('/');
        b.path == place || b.path.starts_with(&format!("{place}/")) || place.is_empty()
    };
    b.places.iter().enumerate().filter(|(_, p)| inside(&p.path)).max_by_key(|(_, p)| p.path.trim_end_matches('/').len()).map(|(i, _)| i)
}

/// What the current folder holds, under the list.
fn status_line(b: &BrowseOut) -> String {
    let folders = match b.dirs.len() {
        1 => "1 folder".to_string(),
        n => format!("{} folders", format_count(n as i64)),
    };
    let audio = match b.audio_files {
        0 => String::new(),
        1 => " \u{b7} 1 audio file right here".to_string(),
        n => format!(" \u{b7} {} audio files right here", format_count(n as i64)),
    };
    let root = if b.is_root { " \u{b7} already in your library" } else { "" };
    format!("{folders}{audio}{root}")
}

/// Breadcrumbs for an absolute path: `/` first, then each folder with the path up to it.
fn crumbs(path: &str) -> Vec<(String, String)> {
    let mut out = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        acc.push('/');
        acc.push_str(part);
        out.push((part.to_string(), acc.clone()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bc_types::library::BrowsePlace;

    #[test]
    fn the_deepest_place_is_active() {
        let place = |path: &str, kind: &str| BrowsePlace { label: path.into(), path: path.into(), kind: kind.into() };
        let at = |path: &str| BrowseOut {
            path: path.into(),
            places: vec![place("/home/u", "home"), place("/home/u/Music", "music"), place("/mnt/disk", "drive"), place("/", "root")],
            ..Default::default()
        };
        assert_eq!(active_place(&at("/home/u/Music/Ambient")), Some(1));
        assert_eq!(active_place(&at("/home/u/Musicals")), Some(0));
        assert_eq!(active_place(&at("/mnt/disk")), Some(2));
        assert_eq!(active_place(&at("/etc")), Some(3));
    }

    #[test]
    fn crumbs_walk_down_from_the_root() {
        assert_eq!(crumbs("/"), [("/".into(), "/".into())]);
        assert_eq!(
            crumbs("/home/u/Music"),
            [
                ("/".into(), "/".into()),
                ("home".into(), "/home".into()),
                ("u".into(), "/home/u".into()),
                ("Music".into(), "/home/u/Music".into()),
            ]
        );
    }
}
