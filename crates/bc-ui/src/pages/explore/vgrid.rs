//! A virtualised grid over an append-only list, for feeds that arrive a cursor page at a
//! time (Bandcamp discover): infinite scroll, no "Load more". Only the visible rows are in
//! the DOM; reaching the last rows asks the owner for more. Rows are keyed by index and
//! only re-render when their own contents change, so appending a page never makes the
//! covers already on screen flash.
use leptos::prelude::*;

use crate::logic::paging::visible_range;
use crate::widgets::card_grid::grid_metrics;

#[component]
pub fn VGrid<R>(
    #[prop(into)] items: Signal<Vec<R>>,
    /// Changes whenever the list is replaced (new query): resets scroll and row caches.
    #[prop(into)]
    epoch: Signal<u64>,
    #[prop(into)] has_more: Signal<bool>,
    #[prop(into)] loading: Signal<bool>,
    on_more: Callback<()>,
    render: Callback<(R, f64), AnyView>,
    #[prop(default = 168.0)] min_card_w: f64,
    #[prop(default = 52.0)] meta_h: f64,
    #[prop(default = 16.0)] gap: f64,
    #[prop(optional)] header: Option<ChildrenFn>,
    #[prop(optional)] footer: Option<ChildrenFn>,
    /// Shown instead of the body when the list is empty and nothing is loading.
    #[prop(optional, into)]
    empty: Option<ViewFn>,
    #[prop(optional, into)] class: String,
) -> impl IntoView
where
    R: Clone + Send + Sync + 'static,
{
    let scroller = NodeRef::<leptos::html::Div>::new();
    let scroll_top = RwSignal::new(0.0f64);
    let size = RwSignal::new((1000.0f64, 600.0f64));
    let raf_pending = StoredValue::new(false);

    let len = Memo::new(move |_| items.with(|v| v.len()));
    let metrics = Memo::new(move |_| grid_metrics(size.get().0.max(120.0), min_card_w, gap));
    let row_h = Memo::new(move |_| metrics.get().1 + meta_h + gap);
    let nrows = Memo::new(move |_| len.get().div_ceil(metrics.get().0));
    let rows_range = Memo::new(move |_| visible_range(scroll_top.get(), size.get().1, row_h.get(), nrows.get(), 2));

    Effect::new(move |prev: Option<()>| {
        epoch.track();
        if prev.is_some() {
            if let Some(el) = scroller.get_untracked() {
                el.set_scroll_top(0);
            }
            scroll_top.set(0.0);
        }
    });

    // Ask for more when the viewport gets within a few rows of the end (also fills a tall
    // screen whose first page does not reach the bottom).
    Effect::new(move |_| {
        let (_, rb) = rows_range.get();
        let rows = nrows.get();
        if has_more.get() && !loading.get() && rb + 3 >= rows {
            on_more.run(());
        }
    });

    let on_scroll = move |_| {
        if raf_pending.try_get_value() != Some(false) {
            return;
        }
        raf_pending.set_value(true);
        crate::util::raf(move || {
            raf_pending.set_value(false);
            if let Some(Some(el)) = scroller.try_get_untracked() {
                scroll_top.set(el.scroll_top() as f64);
            }
        });
    };
    Effect::new(move |_| {
        if let Some(el) = scroller.get() {
            use wasm_bindgen::JsCast;
            let disconnect = crate::util::observe_resize(el.unchecked_ref(), move |w, h| size.set((w, h)));
            let guard = send_wrapper::SendWrapper::new(disconnect);
            on_cleanup(move || (guard.take())());
        }
    });

    view! {
        <div class=format!("cg xg {class}")>
            <div class="cg-scroll" node_ref=scroller on:scroll=on_scroll>
                {header.map(|h| h())}
                <div class="cg-body" style=move || format!("height:{}px", nrows.get() as f64 * row_h.get())>
                    <For each=move || { let (a, b) = rows_range.get(); a..b } key=|r| *r let:r>
                        {
                            // One memo per row: the row only re-renders when ITS cards change.
                            let end = Memo::new(move |_| ((r + 1) * metrics.get().0).min(len.get()));
                            move || {
                                epoch.track();
                                let (cols, cw) = metrics.get();
                                let end = end.get();
                                let start = (r * cols).min(end);
                                let cards = items.with_untracked(|v| v[start..end].to_vec());
                                let cards = cards.into_iter().map(|c| render.run((c, cw))).collect_view();
                                view! {
                                    <div class="cg-row" style=format!("grid-template-columns:repeat({cols}, {cw:.1}px);transform:translateY({}px);gap:{gap}px", r as f64 * row_h.get_untracked())>
                                        {cards}
                                    </div>
                                }
                            }
                        }
                    </For>
                </div>
                {footer.map(|f| f())}
                {move || {
                    let show = len.get() == 0 && !loading.get();
                    show.then(|| empty.as_ref().map(|e| e.run()))
                }}
            </div>
        </div>
    }
}
