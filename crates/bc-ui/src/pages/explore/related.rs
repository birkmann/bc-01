//! The neighbourhood of a release: the catalogue it came off, and the related feeds.
use bc_types::bandcamp::{BandOut, DiscoverOut, ReleaseCardOut, RelatedOut, RelatedSectionOut};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::cards::{CatalogDownloadButton, GridPlaybackBar, ReleaseGrid};
use super::logic::{DISCOGRAPHY_PREVIEW, band_path, dedupe_cards, release_key, tag_path};
use crate::api;
use super::qh;
use crate::data::QuerySpec;
use crate::ds::{Button, Icon, Size};
use crate::util::{enc, qs_pairs};

const RELATED_TAG_LIMIT: usize = 4;

fn cards_to_sweep(items: Signal<Vec<ReleaseCardOut>>) -> Signal<Vec<(String, Option<i64>)>> {
    Signal::derive(move || items.with(|v| v.iter().map(|c| (c.url.clone(), c.library_release_id)).collect()))
}

#[component]
pub fn Loading(#[prop(into)] text: String) -> impl IntoView {
    view! { <div class="xg-loading faint" role="status"><span class="xg-spin"><Icon name="refresh" /></span>{text}</div> }
}

/// The catalogue behind a release, on the release: the newest few as covers, the rest one
/// press away, and one button that queues everything the shelf is missing.
#[component]
pub fn BandDiscography(#[prop(into)] url: String, #[prop(into)] exclude: String) -> impl IntoView {
    let expanded = RwSignal::new(false);
    let u = url.clone();
    let band = qh::use_q::<BandOut>(move || Some(QuerySpec::new(format!("/explore/band?url={}", enc(&u)), &[])));
    let releases = Signal::derive(move || {
        band.data.with(|b| {
            let items = b.as_ref().map(|b| b.releases.clone()).unwrap_or_default();
            // Matched on a loose key: the URL off a discography grid and the one off the page's own
            // metadata differ by a scheme or a trailing slash often enough to leave a duplicate card.
            let me = release_key(&exclude);
            items.into_iter().filter(|i| release_key(&i.url) != me).collect::<Vec<_>>()
        })
    });
    let shown = Signal::derive(move || {
        let r = releases.get();
        if expanded.get() { r } else { r.into_iter().take(DISCOGRAPHY_PREVIEW).collect() }
    });
    let hidden = move || releases.with(|r| r.len()).saturating_sub(shown.with(|s| s.len()));
    let missing = Signal::derive(move || band.data.with(|b| b.as_ref().map(|b| b.releases.iter().filter(|r| !r.in_library).count()).unwrap_or(0)));
    let exact = Signal::derive(move || band.data.with(|b| b.as_ref().is_some_and(|b| !b.truncated)));
    view! {
        <section class="xg-section">
            {move || {
                if band.first_load() {
                    return view! { <Loading text="Loading the discography\u{2026}" /> }.into_any();
                }
                if let Some(e) = band.failure() {
                    // Said out loud rather than swallowed: an empty space here reads as a label with one record.
                    return view! { <div class="faint xg-quiet">{format!("Discography unavailable ({})", e.message())}</div> }.into_any();
                }
                let Some(b) = band.data.get() else { return ().into_any() };
                if releases.with(|r| r.is_empty()) {
                    return ().into_any();
                }
                let (bu, bname, bkind) = (b.url.clone(), b.name.clone(), b.kind.clone());
                let url_for_catalog = b.url.clone();
                view! {
                    <h2 class="xg-h">
                        <span class="display">{format!("More from {bname}")}</span>
                        <span class="badge">{bkind.clone()}</span>
                        <span class="mono faint">{move || format!("{}{} releases", releases.with(|r| r.len()), if b.truncated { "+" } else { "" })}</span>
                        <a class="xg-more" href=band_path(&bu)>{format!("open the {bkind} page \u{2192}")}</a>
                        <GridPlaybackBar items=cards_to_sweep(shown) />
                        <CatalogDownloadButton url=url_for_catalog missing=missing exact=exact small=true />
                    </h2>
                    <ReleaseGrid items=shown min=118 />
                    {move || (hidden() > 0 || expanded.get()).then(|| view! {
                        <div class="xg-center">
                            <Button size=Size::Sm on_click=move |_| expanded.update(|e| *e = !*e)>
                                {move || if expanded.get() { "Show fewer".to_string() } else { format!("Show all {} releases", releases.with(|r| r.len())) }}
                            </Button>
                        </div>
                    })}
                }.into_any()
            }}
        </section>
    }
}

/// One related grid, with its own paging. Tag sections page through `/discover`: the cursor
/// the section came back with is a discover cursor, so "more like this" is that feed continued.
#[component]
fn RelatedSection(section: RelatedSectionOut) -> impl IntoView {
    let extra = RwSignal::new(Vec::<ReleaseCardOut>::new());
    let next = RwSignal::new(section.cursor.clone());
    let loading = RwSignal::new(false);
    let base = section.items.clone();
    let items = Signal::derive(move || dedupe_cards(base.iter().cloned().chain(extra.get())));
    let tag = section.tag.clone();
    let more = {
        let tag = tag.clone();
        move |_| {
            let (Some(tag), Some(cursor)) = (tag.clone(), next.get_untracked()) else { return };
            loading.set(true);
            let q = qs_pairs(&[("tags".into(), tag), ("slice".into(), "top".into()), ("cursor".into(), cursor), ("size".into(), "48".into())]);
            spawn_local(async move {
                match api::get::<DiscoverOut>(&format!("/explore/discover{q}")).await {
                    Ok(d) => {
                        let _ = extra.try_update(|e| e.extend(d.items));
                        let _ = next.try_set(d.cursor);
                    }
                    Err(e) => crate::ds::toast_err(&e.message()),
                }
                let _ = loading.try_set(false);
            });
        }
    };
    let title = if section.source == "tag" { format!("Tagged {}", section.title) } else { section.title.clone() };
    let is_band = section.source == "band";
    let band_url = section.url.clone();
    let total = section.total;
    let missing = Signal::derive(move || items.with(|v| v.iter().filter(|i| !i.in_library).count()));
    let exact = Signal::derive(move || total.is_none_or(|t| t <= items.with(|v| v.len()) as i64));
    view! {
        <section class="xg-section">
            <h2 class="xg-h">
                <span class="display">{title}</span>
                {total.map(|t| view! { <span class="mono faint">{format!("{} releases", crate::logic::format::format_count(t))}</span> })}
                {tag.clone().map(|t| view! { <a class="xg-more" href=tag_path(&t)>"browse the whole feed \u{2192}"</a> })}
                {move || items.with(|v| !v.is_empty()).then(|| view! { <GridPlaybackBar items=cards_to_sweep(items) /> })}
                {(is_band).then(|| band_url.clone()).flatten().map(|u| view! { <CatalogDownloadButton url=u missing=missing exact=exact small=true /> })}
            </h2>
            <ReleaseGrid items=items min=118 />
            {move || next.get().is_some().then(|| view! {
                <div class="xg-center"><Button size=Size::Sm busy=loading on_click=more.clone()>"More like this"</Button></div>
            })}
        </section>
    }
}

/// Every grid of neighbours for one release, fetched as one call. `tags` seeds which tag feeds
/// are shown; the chips let the person change them. Mount with a fresh component per release.
#[component]
pub fn BandcampRelated(#[prop(into)] url: String, tags: Vec<String>, #[prop(default = true)] include_band: bool) -> impl IntoView {
    let chosen = RwSignal::new(tags.iter().take(3).cloned().collect::<Vec<_>>());
    let u = url.clone();
    let related = qh::use_q::<RelatedOut>(move || {
        let mut pairs: Vec<(String, String)> = vec![("url".into(), u.clone())];
        pairs.extend(chosen.get().into_iter().map(|t| ("tags".to_string(), t)));
        pairs.push(("size".into(), "48".into()));
        pairs.push(("tag_limit".into(), RELATED_TAG_LIMIT.to_string()));
        pairs.push(("include_band".into(), include_band.to_string()));
        Some(QuerySpec::new(format!("/explore/related{}", qs_pairs(&pairs)), &[]))
    });
    // The oldest choice drops out at the cap: a click always shows you something.
    let toggle = move |t: String| {
        chosen.update(|c| {
            if let Some(i) = c.iter().position(|x| *x == t) {
                c.remove(i);
            } else {
                c.push(t);
                let over = c.len().saturating_sub(RELATED_TAG_LIMIT);
                c.drain(..over);
            }
        })
    };
    let seed = tags.clone();
    view! {
        <div class="xg-related">
            {(seed.len() > 1).then(|| view! {
                <div class="xg-feeds">
                    <span class="faint xg-feeds-l">"Feeds"</span>
                    {seed.iter().cloned().map(|t| {
                        let (t1, t2) = (t.clone(), t.clone());
                        view! { <button type="button" class="chip" aria-pressed=move || chosen.with(|c| c.contains(&t1)).to_string() on:click=move |_| toggle(t2.clone())>{t}</button> }
                    }).collect_view()}
                </div>
            })}
            {move || {
                if related.first_load() || (related.loading.get() && related.data.with(|d| d.is_none())) {
                    return view! { <Loading text="Finding related music\u{2026}" /> }.into_any();
                }
                if let Some(e) = related.failure() {
                    // A failed related lookup is not a failed page: the release above is still readable and playable.
                    return view! { <div class="faint xg-quiet">{format!("Related releases unavailable ({})", e.message())}</div> }.into_any();
                }
                let sections = related.data.with(|d| d.as_ref().map(|r| r.sections.clone()).unwrap_or_default());
                sections
                    .into_iter()
                    .filter(|s| include_band || s.source != "band")
                    .map(|s| view! { <RelatedSection section=s /> })
                    .collect_view()
                    .into_any()
            }}
        </div>
    }
}
