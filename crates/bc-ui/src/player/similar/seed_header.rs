//! What the list is similar *to*: the seed track and the facts it was matched on. The pin freezes
//! it, so several rows can be queued without the list moving when the next track starts.
use bc_types::player::QueueItem;
use leptos::prelude::*;

use super::SimilarCtx;
use crate::ds::{Button, Icon, Variant};
use crate::util::enc;
use crate::widgets::common::Art;

#[component]
pub(crate) fn SeedHeader(ctx: SimilarCtx, seed: QueueItem) -> impl IntoView {
    let pinned = Signal::derive(move || ctx.pinned.with(|p| p.is_some()));
    let label = Signal::derive(move || ctx.data.with(|d| d.as_ref().and_then(|d| d.seed_label.clone())));
    let pool_tags = Signal::derive(move || ctx.data.with(|d| d.as_ref().map(|d| d.pool_tags.clone()).unwrap_or_default()));
    let s2 = seed.clone();
    view! {
        <section class="pp-sec" aria-label="Seed track">
            <div class="pp-row-flex">
                <Art src=seed.art_url.clone() size=40.0 />
                <span class="pp-seed-main">
                    <span class="truncate pp-seed-t">{seed.title.clone()}</span>
                    <span class="truncate muted pp-seed-a">
                        {seed.artist.clone().unwrap_or_default()}
                        {move || label.get().map(|l| view! { <span class="faint">{format!(" · {l}")}</span> })}
                    </span>
                </span>
                <Button variant=Variant::Ghost icon="bookmark" pressed=pinned
                    title=Signal::derive(move || if pinned.get() { "Pinned: the list stays on this track" } else { "Pin this track so the list stops following the player" }).get_untracked()
                    on_click=move |_| if pinned.get_untracked() { ctx.pinned.set(None); ctx.limit.set(super::PAGE) } else { ctx.pinned.set(Some(s2.clone())); ctx.limit.set(super::PAGE) } />
            </div>
            {move || { let t = pool_tags.get(); (!t.is_empty()).then(|| view! {
                <p class="pp-simby">
                    <span class="pp-lbl wide">"Similar by"</span>
                    {t.into_iter().map(|tag| { let href = format!("/tracks?tag={}", enc(&tag)); view! { <a class="pp-chip" href=href>{tag}</a> } }).collect_view()}
                </p>
            }) }}
        </section>
    }
}

#[allow(dead_code)]
fn _i() -> impl IntoView {
    view! { <Icon name="x" /> }
}
