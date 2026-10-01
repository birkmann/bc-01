//! Where the mix draws from: the whole library, the loved tracks, or a playlist, and, chained,
//! what comes after. The first chip is the pool being mixed; the rest take over as each runs dry.
use bc_types::library::PlaylistOut;
use bc_types::player::{PlanOp, Pool};
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::Planner;
use super::suggest_body::pool_label;
use crate::data::QuerySpec;
use crate::ds::popover::Rect;
use crate::ds::{Icon, MenuCtx, MenuEntry, MenuItem};
use crate::player::plan::qh::use_qh;

fn pool_icon(p: &Pool) -> &'static str {
    match p {
        Pool::Library => "music",
        Pool::Loved => "heart",
        Pool::Playlist { .. } => "list-music",
    }
}

#[component]
pub fn PoolBar(pl: Planner) -> impl IntoView {
    let pools = Signal::derive(move || pl.plan.with(|p| p.pools.clone()));
    let spent = Signal::derive(move || pl.plan.with(|p| p.spent_pool.clone()));
    let playlists = use_qh::<Vec<PlaylistOut>>(|| Some(QuerySpec::new("/playlists", &["playlist"])));
    let menu = expect_context::<MenuCtx>();
    let btn = NodeRef::<leptos::html::Button>::new();
    let open_picker = move |_| {
        let Some(el) = btn.get_untracked() else { return };
        let mut opts: Vec<Pool> = vec![Pool::Library, Pool::Loved];
        if let Some(l) = playlists.data.get_untracked() {
            opts.extend(l.iter().map(|p| Pool::Playlist { id: p.id, name: p.name.clone() }));
        }
        let entries: Vec<MenuEntry> = opts
            .into_iter()
            .map(|p| {
                let (a, b) = (p.clone(), p.clone());
                MenuItem::new(pool_label(&p))
                    .icon(pool_icon(&p))
                    .sub(vec![
                        MenuItem::new("Mix from this now").icon("mix").on(move || pl.op(PlanOp::MixFrom { pool: a.clone() })).into(),
                        MenuItem::new("Then (after the current pool)").icon("chevron-right").on(move || pl.op(PlanOp::ChainPool { pool: b.clone() })).into(),
                    ])
                    .into()
            })
            .collect();
        menu.open(Rect::of(el.unchecked_ref()), entries);
    };
    view! {
        <section class="pp-sec" aria-label="Pools">
            <div class="pp-seg top">
                <span class="pp-lbl">"from"</span>
                <div class="pp-chips">
                    <Show when=move || pools.with(|p| p.is_empty())>
                        <span class="pp-chip static"><Icon name="music" size=10 />"Library"</span>
                    </Show>
                    {move || pools.get().into_iter().enumerate().map(|(i, p)| {
                        let key = p.key();
                        let label = pool_label(&p);
                        let p2 = p.clone();
                        let rm = format!("Remove {label}");
                        view! {
                            {(i > 0).then(|| view! { <Icon name="chevron-right" size=10 class="faint" /> })}
                            <span class=if i == 0 { "pp-chip on static" } else { "pp-chip static" }
                                title=if i == 0 { "Mixing from this now" } else { "Next in the chain; click to mix from it now" }>
                                {if i > 0 {
                                    view! { <button type="button" class="pp-bare" on:click=move |_| pl.op(PlanOp::MixFrom { pool: p2.clone() })><Icon name=pool_icon(&p) size=10 /><span class="truncate">{label.clone()}</span></button> }.into_any()
                                } else {
                                    view! { <Icon name=pool_icon(&p) size=10 /><span class="truncate">{label.clone()}</span> }.into_any()
                                }}
                                <button type="button" class="pp-bare" aria-label=rm on:click=move |_| pl.op(PlanOp::RemovePool { key: key.clone() })><Icon name="x" size=10 /></button>
                            </span>
                        }
                    }).collect_view()}
                    <button node_ref=btn type="button" class="pp-chip add" aria-haspopup="menu" on:click=open_picker>
                        <Icon name="plus" size=10 />{move || if pools.with(|p| p.is_empty()) { "playlist…" } else { "add" }}
                    </button>
                </div>
            </div>
            {move || {
                let first = pools.with(|p| p.first().cloned());
                let s = spent.get();
                first.and_then(|f| (s.as_deref() == Some(f.key().as_str())).then(|| view! {
                    <div class="pp-indent pp-warn"><Icon name="alert" size=12 />{format!("Nothing left to draw from {}. Add another pool to keep going.", pool_label(&f))}</div>
                }))
            }}
        </section>
    }
}
