//! A search that found nothing in the library: the same words on Bandcamp.
use bc_types::bandcamp::SearchHitOut;
use leptos::prelude::*;

use crate::data::{QuerySpec, use_query};
use crate::ds::{EmptyState, Icon, Skeleton};
use crate::pages::albums::related::release_path;
use crate::util::enc;
use crate::widgets::common::Art;

#[component]
pub fn BandcampSearchShelf(#[prop(into)] q: String, #[prop(into)] message: String) -> impl IntoView {
    let term = q.clone();
    let hits = use_query::<Vec<SearchHitOut>>(move || Some(QuerySpec::new(format!("/explore/search?q={}&kind=all&limit=12", enc(&term)), &[])));
    let data = hits.data;
    let err = hits.error;
    let explore = format!("/explore?q={}", enc(&q));
    view! {
        <div class="tm-miss">
            <EmptyState title=message hint="Try other words, or look on Bandcamp." icon="search" />
            <div class="tm-miss-bc">
                <h2 class="section-title"><Icon name="compass" />" On Bandcamp"</h2>
                {move || match (data.get(), err.get()) {
                    (Some(d), _) if !d.is_empty() => view! {
                        <div class="tm-hits">
                            {d.iter().cloned().map(|h| {
                                let href = match h.library_release_id { Some(id) => format!("/albums/{id}"), None => if h.kind == "artist" || h.kind == "label" { format!("/explore/band?url={}", enc(&h.url)) } else { release_path(&h.url) } };
                                view! {
                                    <a class="tm-hit" href=href>
                                        <span class="tm-hit-art"><Art src=h.art_url.clone() class="alb-art" /></span>
                                        <span class="tm-hit-text"><span class="truncate tm-hit-name">{h.name.clone()}</span><span class="truncate faint tm-hit-sub">{format!("{} · {}", h.kind, h.subtitle)}{h.in_library.then_some(" · in library")}</span></span>
                                    </a>
                                }
                            }).collect_view()}
                        </div>
                    }.into_any(),
                    (Some(_), _) => view! { <p class="faint">"Nothing on Bandcamp either."</p> }.into_any(),
                    (None, Some(_)) => ().into_any(),
                    (None, None) => view! { <Skeleton height="60px" /> }.into_any(),
                }}
                <a class="btn btn-outline tm-more" href=explore>"Search Bandcamp for this"</a>
            </div>
        </div>
    }
}
