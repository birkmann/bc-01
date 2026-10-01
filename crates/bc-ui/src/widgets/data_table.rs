//! Virtualised `DataTable`: fixed row height, server-side sort and filter, sparse
//! index-addressed paging (200-row pages around the viewport +/-1; the scrollbar
//! can jump to any row of 190k), column prefs, selection as a tagged union with
//! Shift-range, pointer drag-and-drop, context menu (right click / long press).
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use bc_types::ui::{ColumnPrefs, Selection};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;

use crate::api::ApiErr;
use crate::data::use_invalidation;
use crate::ds::{Icon, MenuCtx, MenuEntry};
use crate::ds::popover::Rect;
use crate::logic::paging::{PAGE_SIZE, Pager, visible_range};
use crate::logic::selection::SelectionOps;
use crate::widgets::dnd::{DragPayload, begin_drag};

#[derive(Clone, Debug)]
pub struct PageReq {
    pub offset: usize,
    pub limit: usize,
    /// Server sort key of the active column (empty = default order).
    pub sort: String,
    pub desc: bool,
}

pub struct PageRes<R> {
    pub rows: Vec<R>,
    pub total: usize,
}

pub type PageFuture<R> = Pin<Box<dyn Future<Output = Result<PageRes<R>, ApiErr>>>>;
pub type PageFetcher<R> = Arc<dyn Fn(PageReq) -> PageFuture<R> + Send + Sync>;
pub type CellFn<R> = Arc<dyn Fn(&R) -> AnyView + Send + Sync>;

#[derive(Clone)]
pub struct Column<R: 'static> {
    pub id: &'static str,
    pub label: &'static str,
    /// Fixed px width, or the minimum when `grow`.
    pub width: f64,
    pub grow: bool,
    /// Server sort key; `None` = not sortable.
    pub sort: Option<&'static str>,
    pub right: bool,
    pub default_hidden: bool,
    /// Hide below this viewport width (px) to keep phones readable.
    pub min_viewport: f64,
    pub render: CellFn<R>,
}

impl<R: 'static> Column<R> {
    pub fn new(id: &'static str, label: &'static str, width: f64, render: impl Fn(&R) -> AnyView + Send + Sync + 'static) -> Self {
        Self { id, label, width, grow: false, sort: None, right: false, default_hidden: false, min_viewport: 0.0, render: Arc::new(render) }
    }
    pub fn grow(mut self) -> Self {
        self.grow = true;
        self
    }
    pub fn sortable(mut self, key: &'static str) -> Self {
        self.sort = Some(key);
        self
    }
    pub fn right(mut self) -> Self {
        self.right = true;
        self
    }
    pub fn hidden(mut self) -> Self {
        self.default_hidden = true;
        self
    }
    pub fn hidden_unless(mut self, show: bool) -> Self {
        self.default_hidden = !show;
        self
    }
    pub fn from_width(mut self, px: f64) -> Self {
        self.min_viewport = px;
        self
    }
}

/// CSS `grid-template-columns` for the visible columns.
pub fn grid_template(widths: &[(f64, bool)], with_select: bool) -> String {
    let mut parts: Vec<String> = vec![];
    if with_select {
        parts.push("40px".into());
    }
    for (w, grow) in widths {
        parts.push(if *grow { format!("minmax({w}px, 1fr)") } else { format!("{w}px") });
    }
    parts.join(" ")
}

fn viewport_w() -> f64 {
    crate::util::window().inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1200.0)
}

#[component]
pub fn DataTable<R>(
    columns: Vec<Column<R>>,
    fetch: PageFetcher<R>,
    /// Identity of the filter. Changing it resets the table to the top.
    #[prop(into)] source_key: Signal<String>,
    sort: RwSignal<(String, bool)>,
    #[prop(into)] row_id: Callback<R, i64>,
    /// Column-prefs key (ui_state) - also the CSS hook.
    #[prop(into)] table_id: String,
    #[prop(optional)] selection: Option<RwSignal<Selection>>,
    #[prop(optional, into)] select_filter: Option<Signal<serde_json::Value>>,
    #[prop(optional, into)] on_row_click: Option<Callback<R>>,
    #[prop(optional, into)] on_row_dblclick: Option<Callback<R>>,
    #[prop(optional, into)] row_menu: Option<Callback<R, Vec<MenuEntry>>>,
    #[prop(optional, into)] row_class: Option<Callback<R, String>>,
    /// Make rows draggable: payload for a row (`kind`, ids, label).
    #[prop(optional, into)] drag: Option<Callback<R, DragPayload>>,
    /// Entities whose `invalidate` events refresh the loaded pages in place.
    #[prop(optional)] entities: Vec<&'static str>,
    #[prop(optional)] total_out: Option<RwSignal<Option<usize>>>,
    #[prop(optional, into)] empty: Option<ViewFn>,
    #[prop(optional)] node_ref: Option<NodeRef<leptos::html::Div>>,
) -> impl IntoView
where
    R: Clone + Send + Sync + 'static,
{
    let menu = use_context::<MenuCtx>();
    let row_h = Signal::derive(|| {
        let v = crate::util::document_element();
        let d = v.get_attribute("data-density").unwrap_or_default();
        match d.as_str() {
            "compact" => 28.0,
            "comfortable" => 44.0,
            _ => 36.0,
        }
    });
    let table_id = StoredValue::new(table_id);
    let columns = StoredValue::new(columns);

    // ---- column prefs (server-side, per table) ---------------------------------
    let prefs = RwSignal::new(ColumnPrefs::default());
    let prefs_key = format!("{}.{}", bc_types::ui::KEY_COLUMNS, table_id.get_value());
    {
        let k = prefs_key.clone();
        spawn_local(async move {
            if let Ok(p) = crate::api::get::<ColumnPrefs>(&format!("/ui-state/{k}")).await {
                prefs.set(p);
            }
        });
    }
    let visible_cols = Signal::derive(move || -> Vec<Column<R>> {
        let vw = viewport_w();
        let p = prefs.get();
        let mut cols: Vec<Column<R>> = columns.with_value(|c| c.clone());
        if !p.order.is_empty() {
            cols.sort_by_key(|c| p.order.iter().position(|o| o == c.id).unwrap_or(usize::MAX));
        }
        cols.into_iter()
            .filter(|c| {
                let hidden = if p.hidden.is_empty() && p.order.is_empty() { c.default_hidden } else { p.hidden.iter().any(|h| h == c.id) };
                !hidden && vw >= c.min_viewport
            })
            .collect()
    });
    // Column chooser (visibility only; saved per table in ui_state).
    let save_prefs = {
        let k = prefs_key.clone();
        move |p: &ColumnPrefs| {
            let (k, p) = (k.clone(), p.clone());
            spawn_local(async move {
                let _ = crate::api::call_json("PUT", &format!("/ui-state/{k}"), &p).await;
            });
        }
    };
    let chooser = Callback::new(move |_: ()| -> Vec<MenuEntry> {
        columns.with_value(|cols| {
            cols.iter()
                .filter(|c| !c.label.is_empty())
                .map(|c| {
                    let id = c.id;
                    let default_hidden: Vec<String> = columns.with_value(|all| all.iter().filter(|x| x.default_hidden).map(|x| x.id.to_string()).collect());
                    let all_ids: Vec<String> = columns.with_value(|all| all.iter().map(|x| x.id.to_string()).collect());
                    let hidden_now = {
                        let p = prefs.get_untracked();
                        if p.hidden.is_empty() && p.order.is_empty() { c.default_hidden } else { p.hidden.iter().any(|h| h == id) }
                    };
                    let save_prefs = save_prefs.clone();
                    crate::ds::MenuItem::new(c.label).checked(!hidden_now).on(move || {
                        prefs.update(|p| {
                            if p.hidden.is_empty() && p.order.is_empty() {
                                p.hidden = default_hidden.clone();
                                p.order = all_ids.clone();
                            }
                            if let Some(i) = p.hidden.iter().position(|h| h == id) { p.hidden.remove(i); } else { p.hidden.push(id.to_string()); }
                        });
                        save_prefs(&prefs.get_untracked());
                    }).into()
                })
                .collect()
        })
    });
    let template = Memo::new(move |_| {
        let p = prefs.get();
        let w: Vec<(f64, bool)> = visible_cols.get().iter().map(|c| (p.widths.get(c.id).copied().unwrap_or(c.width), c.grow)).collect();
        grid_template(&w, selection.is_some())
    });
    let min_w = Memo::new(move |_| {
        let p = prefs.get();
        visible_cols.get().iter().map(|c| p.widths.get(c.id).copied().unwrap_or(c.width)).sum::<f64>() + if selection.is_some() { 40.0 } else { 0.0 }
    });

    // ---- paging --------------------------------------------------------------
    let pager: StoredValue<Pager<R>> = StoredValue::new(Pager::new(6));
    let rev = RwSignal::new(0u64);
    let total = RwSignal::new(None::<usize>);
    let error = RwSignal::new(None::<ApiErr>);
    let generation = StoredValue::new(0u64);
    let scroller = node_ref.unwrap_or_default();
    let scroll_top = RwSignal::new(0.0f64);
    let viewport_h = RwSignal::new(600.0f64);
    let raf_pending = StoredValue::new(false);

    let request_page = {
        let fetch = fetch.clone();
        move |page: usize| {
            // Timers and invalidations can call in after the table is gone.
            let (Some(g), Some((s, desc))) = (generation.try_get_value(), sort.try_get_untracked()) else { return };
            let req = PageReq { offset: page * PAGE_SIZE, limit: PAGE_SIZE, sort: s, desc };
            let fut = fetch(req);
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
                        if generation.try_get_value() != Some(g) {
                            return;
                        }
                        pager.update_value(|p| p.fail(page));
                        error.set(Some(e));
                    }
                }
            });
        }
    };
    let request_page = Arc::new(request_page);

    // Reset on filter / sort change.
    {
        let request_page = request_page.clone();
        Effect::new(move |prev: Option<()>| {
            source_key.track();
            sort.track();
            generation.update_value(|g| *g += 1);
            pager.update_value(|p| p.reset());
            total.set(None);
            if prev.is_some() {
                if let Some(el) = scroller.get_untracked() {
                    el.set_scroll_top(0);
                }
                scroll_top.set(0.0);
            }
            pager.update_value(|p| p.total = Some(usize::MAX / 4)); // allow need() before the count is known
            let first = pager.try_update_value(|p| p.need(0, 1)).unwrap_or_default();
            pager.update_value(|p| p.total = None);
            for pg in first {
                request_page(pg);
            }
            rev.update(|r| *r += 1);
        });
    }

    let range = Memo::new(move |_| {
        let t = total.get().unwrap_or(0);
        visible_range(scroll_top.get(), viewport_h.get(), row_h.get(), t, 6)
    });

    // Fetch what the viewport needs.
    {
        let request_page = request_page.clone();
        Effect::new(move |_| {
            let (a, b) = range.get();
            if total.get().is_none() {
                return;
            }
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

    // In-place refresh when entities change on the server (coalesced, visible +/-1 pages only).
    {
        let request_page = request_page.clone();
        let timer = StoredValue::new(None::<i32>);
        // Runs from a 250 ms timer: the table may have unmounted by then (navigation), so read with try_.
        let refresh = move || {
            let (Some((a, b)), Some(t)) = (range.try_get_untracked(), total.try_get_untracked()) else { return };
            let t = t.unwrap_or(0);
            let mut pages = crate::logic::paging::pages_for_rows(a, b.max(a + 1), t.max(1), 1);
            if t == 0 {
                pages = vec![0];
            }
            pager.update_value(|p| p.keep_only(&pages));
            for pg in pages {
                request_page(pg);
            }
        };
        let refresh = Arc::new(refresh);
        if !entities.is_empty() {
            on_cleanup(move || {
                if let Some(Some(id)) = timer.try_get_value() {
                    crate::util::window().clear_timeout_with_handle(id);
                }
            });
            use_invalidation(&entities, move |_ids| {
                let refresh = refresh.clone();
                let w = crate::util::window();
                if let Some(Some(id)) = timer.try_get_value() {
                    w.clear_timeout_with_handle(id);
                }
                let cb = wasm_bindgen::closure::Closure::once_into_js(move || refresh());
                timer.set_value(w.set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), 250).ok());
            });
        }
    }

    // Scroll: coalesce into one rAF.
    let on_scroll = move |_| {
        if raf_pending.try_get_value() != Some(false) {
            return;
        }
        raf_pending.set_value(true);
        crate::util::raf(move || {
            raf_pending.set_value(false);
            if let Some(Some(el)) = scroller.try_get_untracked() {
                scroll_top.set(el.scroll_top() as f64);
                viewport_h.set(el.client_height() as f64);
            }
        });
    };
    Effect::new(move |_| {
        if let Some(el) = scroller.get() {
            viewport_h.set(el.client_height() as f64);
        }
    });

    // ---- selection -----------------------------------------------------------
    let anchor = StoredValue::new(None::<usize>);
    let id_at = move |i: usize| -> Option<i64> { pager.with_value(|p| p.row(i).cloned()).map(|r| row_id.run(r)) };
    let toggle_at = move |i: usize, shift: bool| {
        let Some(sel) = selection else { return };
        if shift {
            if let Some(a) = anchor.get_value() {
                let (lo, hi) = if a <= i { (a, i) } else { (i, a) };
                let ids: Vec<i64> = (lo..=hi).filter_map(id_at).collect();
                sel.update(|s| s.add_many(&ids));
                return;
            }
        }
        if let Some(id) = id_at(i) {
            sel.update(|s| s.toggle(id));
            anchor.set_value(Some(i));
        }
    };

    let open_menu = move |ev: &web_sys::MouseEvent, row: R| {
        if let (Some(cb), Some(menu)) = (row_menu, menu) {
            ev.prevent_default();
            let entries = cb.run(row);
            if !entries.is_empty() {
                menu.open(Rect::point(ev.client_x() as f64, ev.client_y() as f64), entries);
            }
        }
    };

    let header_click = move |key: &'static str| {
        sort.update(|(s, d)| {
            if s == key {
                *d = !*d;
            } else {
                *s = key.to_string();
                *d = false;
            }
        });
    };

    let all_selected = move || selection.map(|s| matches!(s.get(), Selection::All { .. })).unwrap_or(false);

    view! {
        <div class=format!("dt dt-{}", table_id.get_value()) role="grid" aria-rowcount=move || total.get().map(|t| t as i64).unwrap_or(-1)>
            <div class="dt-cols"><crate::ds::MenuButton entries=chooser icon="sliders" title="Columns" /></div>
            <div class="dt-scroll" node_ref=scroller on:scroll=on_scroll>
                <div class="dt-inner" style=move || format!("min-width:{}px", min_w.get())>
                    <div class="dt-head" role="row" style=move || format!("grid-template-columns:{}", template.get())>
                        {selection.map(|sel| view! {
                            <div class="dt-cell dt-check" role="columnheader">
                                <input type="checkbox" aria-label="Select all" prop:checked=all_selected
                                    on:change=move |ev| {
                                        let on = event_target_checked(&ev);
                                        if on { sel.update(|s| s.select_all(select_filter.map(|f| f.get_untracked()).unwrap_or(serde_json::Value::Null))) } else { sel.update(|s| s.clear()) }
                                    } />
                            </div>
                        })}
                        {move || visible_cols.get().into_iter().map(|c| {
                            let key = c.sort;
                            let active = move || key.map(|k| sort.with(|(s, _)| s == k)).unwrap_or(false);
                            let arrow = move || {
                                if active() { if sort.with(|(_, d)| *d) { "arrow-down" } else { "arrow-up" } } else { "" }
                            };
                            view! {
                                <div class=format!("dt-cell dt-th{}{}", if c.right { " r" } else { "" }, if key.is_some() { " sortable" } else { "" })
                                    role="columnheader" aria-sort=move || if active() { Some(if sort.with(|(_, d)| *d) { "descending" } else { "ascending" }) } else { None }
                                    on:click=move |_| if let Some(k) = key { header_click(k) }>
                                    <span class="truncate">{c.label}</span>
                                    {move || { let a = arrow(); (!a.is_empty()).then(|| view! { <Icon name=a size=12 /> }) }}
                                </div>
                            }
                        }).collect_view()}
                    </div>
                    <div class="dt-body" style=move || format!("height:{}px", total.get().unwrap_or(0) as f64 * row_h.get())>
                        <For each=move || { let (a, b) = range.get(); a..b } key=|i| *i let:i>
                            {
                                let row = Signal::derive(move || { rev.track(); pager.with_value(|p| p.row(i).cloned()) });
                                let selected = move || selection.map(|s| row.get().map(|r| s.with(|s| s.is_selected(row_id.run(r)))).unwrap_or(false)).unwrap_or(false);
                                let long_press = StoredValue::new(None::<i32>);
                                view! {
                                    <div class=move || {
                                        let extra = row.get().and_then(|r| row_class.map(|f| f.run(r))).unwrap_or_default();
                                        format!("dt-row{}{} {extra}", if selected() { " sel" } else { "" }, if i % 2 == 1 { " odd" } else { "" })
                                    }
                                        role="row" data-index=i
                                        style=move || format!("height:{}px;transform:translateY({}px);grid-template-columns:{}", row_h.get(), i as f64 * row_h.get(), template.get())
                                        on:click=move |ev: web_sys::MouseEvent| {
                                            if ev.shift_key() && selection.is_some() { toggle_at(i, true); return; }
                                            if (ev.ctrl_key() || ev.meta_key()) && selection.is_some() { toggle_at(i, false); return; }
                                            if let (Some(cb), Some(r)) = (on_row_click, row.get_untracked()) { cb.run(r); }
                                        }
                                        on:dblclick=move |_| if let (Some(cb), Some(r)) = (on_row_dblclick, row.get_untracked()) { cb.run(r); }
                                        on:contextmenu=move |ev| if let Some(r) = row.get_untracked() { open_menu(&ev, r) }
                                        on:pointerdown=move |ev: web_sys::PointerEvent| {
                                            if let (Some(d), Some(r)) = (drag, row.get_untracked()) {
                                                begin_drag(&ev, d.run(r));
                                            }
                                            // long-press opens the menu on touch
                                            if ev.pointer_type() == "touch" && row_menu.is_some() && drag.is_none() {
                                                let (x, y) = (ev.client_x() as f64, ev.client_y() as f64);
                                                let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
                                                    if let (Some(cb), Some(menu), Some(Some(r))) = (row_menu, menu, row.try_get_untracked()) {
                                                        let Some(entries) = cb.try_run(r) else { return };
                                                        if !entries.is_empty() { menu.open(Rect::point(x, y), entries); }
                                                    }
                                                });
                                                long_press.set_value(crate::util::window().set_timeout_with_callback_and_timeout_and_arguments_0(cb.unchecked_ref(), 520).ok());
                                            }
                                        }
                                        on:pointerup=move |_| if let Some(Some(t)) = long_press.try_get_value() { crate::util::window().clear_timeout_with_handle(t) }
                                        on:pointermove=move |_| if let Some(Some(t)) = long_press.try_get_value() { crate::util::window().clear_timeout_with_handle(t); long_press.set_value(None) }
                                    >
                                        {selection.map(|_| view! {
                                            <div class="dt-cell dt-check" on:click=move |ev: web_sys::MouseEvent| { ev.stop_propagation(); toggle_at(i, ev.shift_key()); }>
                                                <input type="checkbox" tabindex="-1" prop:checked=selected aria-label="Select row" />
                                            </div>
                                        })}
                                        {move || match row.get() {
                                            Some(r) => visible_cols.get().into_iter().map(|c| {
                                                let cell = (c.render)(&r);
                                                view! { <div class=format!("dt-cell{}", if c.right { " r num" } else { "" }) role="gridcell">{cell}</div> }
                                            }).collect_view().into_any(),
                                            None => visible_cols.get().into_iter().enumerate().map(|(k, c)| view! {
                                                <div class="dt-cell"><div class="skeleton" style=format!("height:10px;width:{}%", if c.grow { 60 } else { 70 - (k % 3) * 12 })></div></div>
                                            }).collect_view().into_any(),
                                        }}
                                    </div>
                                }
                            }
                        </For>
                    </div>
                </div>
                {move || (total.get() == Some(0)).then(|| view! {
                    <div class="dt-empty">{match &empty { Some(e) => e.run().into_any(), None => view! { <div class="empty"><Icon name="music" /><h3>"Nothing here"</h3></div> }.into_any() }}</div>
                })}
                {move || error.get().map(|e| view! { <div class="dt-empty"><div class="empty"><Icon name="alert-circle" /><h3>"Could not load"</h3><p>{e.message()}</p></div></div> })}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_template_mixes_fixed_and_flexible() {
        assert_eq!(grid_template(&[(60.0, false), (240.0, true), (80.0, false)], true), "40px 60px minmax(240px, 1fr) 80px");
        assert_eq!(grid_template(&[(100.0, true)], false), "minmax(100px, 1fr)");
    }
}
