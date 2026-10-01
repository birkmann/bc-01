//! The set's pool sources: removable chips plus an "Add source" dialog (tabs: Tags, Playlists,
//! Labels, Artists, Loved, with search). Picking keeps the dialog open so several sources go in
//! per visit; already-added rows show as checked.
use bc_types::library::{ArtistOut, LabelOut, Page, PlaylistOut, TagOut};
use bc_types::sets::{PoolSource, PoolSourceCountOut, PoolSourceKind};
use leptos::prelude::*;

use crate::data::QuerySpec;
use crate::ds::{Button, Dialog, Icon, SearchInput, Size, Variant, use_debounced};
use crate::logic::format::format_count;
use crate::player::plan::qh::use_qh;
use crate::util::enc;
use crate::widgets::common::Art;

pub fn source_key(s: &PoolSource) -> String {
    match s.kind {
        PoolSourceKind::Tag => format!("tag:{}", s.tag.clone().unwrap_or_default().to_lowercase()),
        PoolSourceKind::Loved => "loved".into(),
        PoolSourceKind::Playlist => format!("playlist:{}", s.playlist_id.unwrap_or(0)),
        PoolSourceKind::Label => format!("label:{}", s.label_id.unwrap_or(0)),
        PoolSourceKind::Artist => format!("artist:{}", s.artist_id.unwrap_or(0)),
        PoolSourceKind::Tracks => format!("tracks:{}", s.track_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")),
    }
}

fn blank(kind: PoolSourceKind) -> PoolSource {
    PoolSource { kind, tag: None, playlist_id: None, label_id: None, artist_id: None, track_ids: vec![], name: None }
}

fn source_label(s: &PoolSource) -> String {
    match s.kind {
        PoolSourceKind::Loved => "Loved".into(),
        PoolSourceKind::Tracks => format!("{} picked", s.track_ids.len()),
        PoolSourceKind::Tag => s.tag.clone().unwrap_or_default(),
        _ => s.name.clone().unwrap_or_else(|| source_key(s)),
    }
}

fn source_icon(k: PoolSourceKind) -> &'static str {
    match k {
        PoolSourceKind::Tag => "tag",
        PoolSourceKind::Loved => "heart",
        PoolSourceKind::Playlist => "list-music",
        PoolSourceKind::Label => "folder",
        PoolSourceKind::Artist => "user",
        PoolSourceKind::Tracks => "disc",
    }
}

#[component]
pub fn PoolBuilder(
    #[prop(into)] sources: Signal<Vec<PoolSource>>,
    #[prop(into)] counts: Signal<Vec<PoolSourceCountOut>>,
    #[prop(into)] pool_size: Signal<Option<i64>>,
    #[prop(into)] on_change: Callback<Vec<PoolSource>>,
    #[prop(optional)] hero: bool,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let tab = RwSignal::new("tags".to_string());
    let needle = RwSignal::new(String::new());
    let q = use_debounced(needle, 200);
    let added = Memo::new(move |_| sources.with(|s| s.iter().map(source_key).collect::<std::collections::HashSet<_>>()));

    let want = move |t: &'static str| open.get() && tab.get() == t;
    let tags = use_qh::<Vec<TagOut>>(move || want("tags").then(|| QuerySpec::new(format!("/tags?limit=200&q={}", enc(&q.get())), &["tag"])));
    let playlists = use_qh::<Vec<PlaylistOut>>(move || want("playlists").then(|| QuerySpec::new("/playlists", &["playlist"])));
    let labels = use_qh::<Page<LabelOut>>(move || want("labels").then(|| QuerySpec::new(format!("/labels?limit=100&q={}", enc(&q.get())), &["label"])));
    let artists = use_qh::<Page<ArtistOut>>(move || want("artists").then(|| QuerySpec::new(format!("/artists?limit=100&q={}", enc(&q.get())), &["artist"])));

    let pick = move |s: PoolSource| {
        if added.with(|a| a.contains(&source_key(&s))) {
            return;
        }
        let mut cur = sources.get_untracked();
        cur.push(s);
        on_change.run(cur);
    };
    let remove = move |i: usize| {
        let mut cur = sources.get_untracked();
        if i < cur.len() {
            cur.remove(i);
            on_change.run(cur);
        }
    };

    let tabs = [("tags", "Tags"), ("playlists", "Playlists"), ("labels", "Labels"), ("artists", "Artists"), ("loved", "Loved")];

    let row = move |key: String, name: String, count: Option<i64>, icon: &'static str, art: Option<String>, src: PoolSource| {
        let is_added = Memo::new({
            let key = key.clone();
            move |_| added.with(|a| a.contains(&key))
        });
        view! {
            <button type="button" role="option" class="pick-row" aria-selected=move || is_added.get().to_string() disabled=move || is_added.get()
                on:click=move |_| pick(src.clone())>
                {match art {
                    Some(a) => view! { <Art src=a size=24.0 /> }.into_any(),
                    None => view! { <span class="pick-ico"><Icon name=icon size=14 /></span> }.into_any(),
                }}
                <span class="truncate pick-name">{name}</span>
                {count.map(|c| view! { <span class="mono faint">{format_count(c)}</span> })}
                {move || is_added.get().then(|| view! { <Icon name="check" size=14 class="pick-check" /> })}
            </button>
        }
    };

    view! {
        <div class=if hero { "pools hero" } else { "pools" }>
            {move || sources.get().into_iter().enumerate().map(|(i, s)| {
                let label = source_label(&s);
                let count = counts.with(|c| c.get(i).map(|c| c.track_count));
                let title = format!("Remove {label} from the pool");
                view! {
                    <button type="button" class="pchip" title=title.clone() aria-label=title on:click=move |_| remove(i)>
                        <Icon name=source_icon(s.kind) size=11 />
                        <span class="truncate">{label}</span>
                        {count.map(|c| view! { <span class="mono faint">{format_count(c)}</span> })}
                        <Icon name="x" size=11 />
                    </button>
                }
            }).collect_view()}
            <button type="button" class="pchip add" aria-haspopup="dialog" on:click=move |_| open.set(true)>
                <Icon name="plus" size=12 />"Add source"
            </button>
            {move || {
                let n = pool_size.get();
                (n.is_some() && !sources.with(|s| s.is_empty())).then(|| view! { <span class="mono faint pool-size">{format!("{} in pool", format_count(n.unwrap_or(0)))}</span> })
            }}
            <Dialog open=open title="Add pool source" wide=true
                footer=crate::ds::children(move || view! { <Button variant=Variant::Primary on_click=move |_| open.set(false)>"Done"</Button> })>
                <div class="pick">
                    <div class="pick-tabs" role="tablist">
                        {tabs.into_iter().map(|(id, label)| view! {
                            <button type="button" role="tab" class="chip" aria-pressed=move || (tab.get() == id).to_string()
                                on:click=move |_| { tab.set(id.to_string()); needle.set(String::new()); }>{label}</button>
                        }).collect_view()}
                    </div>
                    <Show when=move || tab.get() != "loved">
                        <SearchInput value=needle placeholder="Find…" />
                    </Show>
                    <div class="pick-list" role="listbox">
                        {move || match tab.get().as_str() {
                            "tags" => tags.data.get().map(|l| l.iter().map(|t| {
                                let mut s = blank(PoolSourceKind::Tag);
                                s.tag = Some(t.name.clone());
                                s.name = Some(t.name.clone());
                                row(source_key(&s), t.name.clone(), Some(t.track_count), "tag", None, s)
                            }).collect_view().into_any()),
                            "playlists" => playlists.data.get().map(|l| {
                                let nq = q.get().to_lowercase();
                                l.iter().filter(|p| nq.is_empty() || p.name.to_lowercase().contains(&nq)).map(|p| {
                                    let mut s = blank(PoolSourceKind::Playlist);
                                    s.playlist_id = Some(p.id);
                                    s.name = Some(p.name.clone());
                                    row(source_key(&s), p.name.clone(), Some(p.track_count), "list-music", None, s)
                                }).collect_view().into_any()
                            }),
                            "labels" => labels.data.get().map(|l| l.items.iter().map(|x| {
                                let mut s = blank(PoolSourceKind::Label);
                                s.label_id = Some(x.id);
                                s.name = Some(x.name.clone());
                                row(source_key(&s), x.name.clone(), Some(x.track_count), "folder", x.art_urls.first().cloned(), s)
                            }).collect_view().into_any()),
                            "artists" => artists.data.get().map(|l| l.items.iter().map(|x| {
                                let mut s = blank(PoolSourceKind::Artist);
                                s.artist_id = Some(x.id);
                                s.name = Some(x.name.clone());
                                row(source_key(&s), x.name.clone(), Some(x.track_count), "user", x.art_url.clone(), s)
                            }).collect_view().into_any()),
                            _ => Some(row("loved".into(), "All loved tracks".into(), None, "heart", None, blank(PoolSourceKind::Loved)).into_any()),
                        }}
                    </div>
                </div>
            </Dialog>
        </div>
    }
}

#[allow(dead_code)]
fn _s(_: Size) {}
