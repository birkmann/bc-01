//! Virtualised `CardGrid` for every grid (albums, artists, labels, explore, feed, fans).
//! Rows are virtualised; items load through the same sparse pager as `DataTable`.
use std::sync::Arc;

use leptos::prelude::*;
use leptos::task::spawn_local;
use send_wrapper::SendWrapper;

use crate::api::ApiErr;
use crate::data::use_invalidation;
use crate::ds::Icon;
use crate::logic::paging::{PAGE_SIZE, Pager, visible_range};
use crate::widgets::data_table::{PageFetcher, PageReq};

pub type GridFetcher<R> = PageFetcher<R>;

/// How many columns fit and how wide each card is.
pub fn grid_metrics(width: f64, min_card_w: f64, gap: f64) -> (usize, f64) {
    let cols = (((width + gap) / (min_card_w + gap)).floor() as usize).max(1);
    let card_w = (width - gap * (cols as f64 - 1.0)) / cols as f64;
    (cols, card_w)
}

/// Item index -> (row, col).
pub fn cell_of(index: usize, cols: usize) -> (usize, usize) {
    (index / cols, index % cols)
}

#[component]
pub fn CardGrid<R>(
    fetch: GridFetcher<R>,
    #[prop(into)] source_key: Signal<String>,
    /// Minimum card width in px.
    #[prop(default = 168.0)] min_card_w: f64,
    /// Height of the text area below the (square) artwork.
    #[prop(default = 58.0)] meta_h: f64,
    #[prop(default = 16.0)] gap: f64,
    /// Renders one card. Receives the row and the card width (px).
    render: Callback<(R, f64), AnyView>,
    #[prop(optional)] entities: Vec<&'static str>,
    #[prop(optional)] total_out: Option<RwSignal<Option<usize>>>,
    #[prop(optional, into)] empty: Option<ViewFn>,
    #[prop(optional)] header: Option<ChildrenFn>,
    /// Rows prepended since the last reset (newest-first listings): on the next reset the scroll position is shifted by
    /// that many rows instead of jumping to the top, so "N new" merges in place. Reset to 0 after use.
    #[prop(optional)] prepend: Option<RwSignal<usize>>,
    #[prop(optional)] node_ref: Option<NodeRef<leptos::html::Div>>,
) -> impl IntoView
where
    R: Clone + Send + Sync + 'static,
{
    let pager: StoredValue<Pager<R>> = StoredValue::new(Pager::new(5));
    let rev = RwSignal::new(0u64);
    let total = RwSignal::new(None::<usize>);
    let error = RwSignal::new(None::<ApiErr>);
    let generation = StoredValue::new(0u64);
    let scroller = node_ref.unwrap_or_default();
    let scroll_top = RwSignal::new(0.0f64);
    let size = RwSignal::new((1000.0f64, 600.0f64));
    // Width available to the cards: measured on the body itself, so padding/margins set by pages never double-count.
    let body_ref = NodeRef::<leptos::html::Div>::new();
    let body_w = RwSignal::new(960.0f64);
    // Height of the optional header above the grid inside the scroller (the rows start below it).
    let head_ref = NodeRef::<leptos::html::Div>::new();
    let head_h = RwSignal::new(0.0f64);
    let raf_pending = StoredValue::new(false);

    let metrics = Memo::new(move |_| grid_metrics(body_w.get().max(120.0), min_card_w, gap));
    let row_h = Memo::new(move |_| metrics.get().1 + meta_h + gap);

    let request_page = {
        let fetch = fetch.clone();
        Arc::new(move |page: usize| {
            // Timers and invalidations can call in after the grid is gone.
            let Some(g) = generation.try_get_value() else { return };
            let fut = fetch(PageReq { offset: page * PAGE_SIZE, limit: PAGE_SIZE, sort: String::new(), desc: false });
            spawn_local(async move {
                match fut.await {
                    Ok(res) => {
                        if generation.try_get_value() != Some(g) {
                            return;
                        }
                        error.set(None);
                        pager.update_value(|p| {
                            p.total = Some(res.total);
                            p.put(page, res.rows);
                        });
                        if total.get_untracked() != Some(res.total) {
                            total.set(Some(res.total));
                            if let Some(t) = total_out {
                                t.set(Some(res.total));
                            }
                        }
                        rev.update(|r| *r += 1);
                    }
                    Err(e) => {
                        if generation.try_get_value() == Some(g) {
                            pager.update_value(|p| p.fail(page));
                            error.set(Some(e));
                        }
                    }
                }
            });
        })
    };

    {
        let request_page = request_page.clone();
        Effect::new(move |prev: Option<()>| {
            source_key.track();
            generation.update_value(|g| *g += 1);
            pager.update_value(|p| p.reset());
            total.set(None);
            if prev.is_some() {
                let shift = prepend.map(|p| p.get_untracked()).unwrap_or(0);
                if let Some(el) = scroller.get_untracked() {
                    if shift > 0 {
                        let cols = metrics.get_untracked().0.max(1);
                        let top = el.scroll_top() as f64 + (shift as f64 / cols as f64).ceil() * row_h.get_untracked();
                        el.set_scroll_top(top as i32);
                        scroll_top.set(top);
                    } else {
                        el.set_scroll_top(0);
                        scroll_top.set(0.0);
                    }
                } else {
                    scroll_top.set(0.0);
                }
                if let Some(p) = prepend {
                    p.set(0);
                }
            }
            pager.update_value(|p| p.total = Some(usize::MAX / 4));
            let first = pager.try_update_value(|p| p.need(0, 1)).unwrap_or_default();
            pager.update_value(|p| p.total = None);
            for pg in first {
                request_page(pg);
            }
            rev.update(|r| *r += 1);
        });
    }

    // Visible rows -> item index range.
    let rows_range = Memo::new(move |_| {
        let t = total.get().unwrap_or(0);
        let (cols, _) = metrics.get();
        let nrows = t.div_ceil(cols);
        visible_range((scroll_top.get() - head_h.get()).max(0.0), size.get().1, row_h.get(), nrows, 2)
    });

    {
        let request_page = request_page.clone();
        Effect::new(move |_| {
            let (ra, rb) = rows_range.get();
            let Some(t) = total.get() else { return };
            let (cols, _) = metrics.get();
            let (a, b) = (ra * cols, (rb * cols).min(t));
            let need = pager.try_update_value(|p| {
                let n = p.need(a, b.max(a + 1));
                p.trim(a, b);
                n
            });
            for pg in need.unwrap_or_default() {
                request_page(pg);
            }
        });
    }

    if !entities.is_empty() {
        let request_page = request_page.clone();
        let timer = StoredValue::new(None::<i32>);
        on_cleanup(move || {
            if let Some(Some(id)) = timer.try_get_value() {
                crate::util::window().clear_timeout_with_handle(id);
            }
        });
        use_invalidation(&entities, move |_| {
            use wasm_bindgen::JsCast;
            let w = crate::util::window();
            if let Some(Some(id)) = timer.try_get_value() {
                w.clear_timeout_with_handle(id);
            }
            let request_page = request_page.clone();
            // Fires 250 ms later: the grid may have unmounted by then (navigation), so read with try_.
            let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
                let (Some((ra, rb)), Some((cols, _)), Some(t)) = (rows_range.try_get_untracked(), metrics.try_get_untracked(), total.try_get_untracked()) else { return };
                let t = t.unwrap_or(0);
                let mut pages = crate::logic::paging::pages_for_rows(ra * cols, (rb * cols).max(ra * cols + 1), t.max(1), 1);
                if t == 0 {
                    pages = vec![0];
                }
                pager.update_value(|p| p.keep_only(&pages));
                for pg in pages {
                    request_page(pg);
                }
            });
            timer.set_value(w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), 250).ok());
        });
    }

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
    // Arrow keys move focus between cards (the focused card's first link/button), scrolling the
    // target row into view first when it is virtualised away.
    let on_key = move |ev: web_sys::KeyboardEvent| {
        use wasm_bindgen::JsCast;
        let delta: i64 = match ev.key().as_str() {
            "ArrowRight" => 1,
            "ArrowLeft" => -1,
            "ArrowDown" => metrics.get_untracked().0 as i64,
            "ArrowUp" => -(metrics.get_untracked().0 as i64),
            _ => return,
        };
        let Some(active) = crate::util::document().active_element() else { return };
        // never hijack keys from text fields / selects inside cards
        if matches!(active.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT") {
            return;
        }
        let Ok(Some(cell)) = active.closest(".cg-cell") else { return };
        let Some(i) = cell.get_attribute("data-i").and_then(|s| s.parse::<i64>().ok()) else { return };
        let total = total.get_untracked().unwrap_or(0) as i64;
        let target = (i + delta).clamp(0, (total - 1).max(0));
        if target == i {
            return;
        }
        ev.prevent_default();
        let (cols, _) = metrics.get_untracked();
        let row_top = (target as usize / cols) as f64 * row_h.get_untracked() + head_h.get_untracked();
        if let Some(sc) = scroller.get_untracked() {
            let (top, h) = (sc.scroll_top() as f64, sc.client_height() as f64);
            if row_top < top {
                sc.set_scroll_top(row_top as i32);
            } else if row_top + row_h.get_untracked() > top + h {
                sc.set_scroll_top((row_top + row_h.get_untracked() - h) as i32);
            }
        }
        let focus = move || {
            if let Ok(Some(el)) = crate::util::document().query_selector(&format!(".cg-cell[data-i=\"{target}\"] a[href], .cg-cell[data-i=\"{target}\"] button")) {
                if let Some(h) = el.dyn_ref::<web_sys::HtmlElement>() {
                    let _ = h.focus();
                }
            }
        };
        crate::util::after(60, focus);
    };
    Effect::new(move |_| {
        if let Some(el) = scroller.get() {
            use wasm_bindgen::JsCast;
            let disconnect = crate::util::observe_resize(el.unchecked_ref(), move |w, h| size.set((w, h)));
            let guard = SendWrapper::new(disconnect);
            on_cleanup(move || (guard.take())());
        }
    });
    Effect::new(move |_| {
        if let Some(el) = head_ref.get() {
            use wasm_bindgen::JsCast;
            let disconnect = crate::util::observe_resize(el.unchecked_ref(), move |_, h| head_h.set(h));
            let guard = SendWrapper::new(disconnect);
            on_cleanup(move || (guard.take())());
        }
    });
    Effect::new(move |_| {
        if let Some(el) = body_ref.get() {
            use wasm_bindgen::JsCast;
            let disconnect = crate::util::observe_resize(el.unchecked_ref(), move |w, _| body_w.set(w));
            let guard = SendWrapper::new(disconnect);
            on_cleanup(move || (guard.take())());
        }
    });

    view! {
        <div class="cg">
            <div class="cg-scroll" node_ref=scroller on:scroll=on_scroll on:keydown=on_key>
                {header.map(|h| view! { <div node_ref=head_ref>{h()}</div> })}
                <div class="cg-body" node_ref=body_ref style=move || {
                    let t = total.get().unwrap_or(0);
                    let (cols, _) = metrics.get();
                    format!("height:{}px", t.div_ceil(cols) as f64 * row_h.get())
                }>
                    <For each=move || { let (a, b) = rows_range.get(); a..b } key=|r| *r let:r>
                        {move || {
                            let (cols, cw) = metrics.get();
                            let t = total.get().unwrap_or(0);
                            let start = r * cols;
                            let end = ((r + 1) * cols).min(t);
                            rev.track();
                            let cards = (start..end).map(|i| {
                                let row = pager.with_value(|p| p.row(i).cloned());
                                let inner = match row {
                                    Some(item) => render.run((item, cw)),
                                    None => view! {
                                        <div><div class="art skeleton"></div><div style="padding-top:8px"><div class="skeleton" style="height:11px;width:80%"></div></div></div>
                                    }.into_any(),
                                };
                                // `display: contents` wrapper: carries the index for arrow-key navigation, no layout impact
                                view! { <div class="cg-cell" data-i=i>{inner}</div> }
                            }).collect_view();
                            view! {
                                <div class="cg-row" style=format!("grid-template-columns:repeat({cols}, {cw:.1}px);transform:translateY({}px);gap:{gap}px", r as f64 * row_h.get())>
                                    {cards}
                                </div>
                            }
                        }}
                    </For>
                </div>
                {move || (total.get() == Some(0)).then(|| view! {
                    <div>{match &empty { Some(e) => e.run().into_any(), None => view! { <div class="empty"><Icon name="disc" /><h3>"Nothing here"</h3></div> }.into_any() }}</div>
                })}
                {move || error.get().map(|e| view! { <div class="empty"><Icon name="alert-circle" /><h3>"Could not load"</h3><p>{e.message()}</p></div> })}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metrics_fit_cards_to_width() {
        let (cols, w) = grid_metrics(1000.0, 168.0, 16.0);
        assert_eq!(cols, 5);
        assert!((w - 187.2).abs() < 0.01);
        assert_eq!(grid_metrics(100.0, 168.0, 16.0).0, 1);
        assert_eq!(grid_metrics(375.0 - 28.0, 150.0, 12.0).0, 2);
    }
    #[test]
    fn cell_math() {
        assert_eq!(cell_of(0, 5), (0, 0));
        assert_eq!(cell_of(11, 5), (2, 1));
    }
}
