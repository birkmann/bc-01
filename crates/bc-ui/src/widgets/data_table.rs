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
    // Every column sized by hand: an empty track takes the rest so rows still span the table.
    if !widths.iter().any(|(_, grow)| *grow) {
        parts.push("minmax(0, 1fr)".into());
    }
    parts.join(" ")
}

/// Smallest width a column can be dragged to.
const MIN_COL_W: f64 = 32.0;
/// Pointer travel before a header press becomes a column move instead of a click.
const MOVE_THRESHOLD: f64 = 5.0;

/// Pin the implicit defaults into `p` before editing it: the default hidden set (an empty
/// `hidden` + `order` means "defaults") and a complete `order` (columns added since keep
/// their place after the known ones).
fn materialize(p: &mut ColumnPrefs, cols: &[(&'static str, bool)]) {
    if p.hidden.is_empty() && p.order.is_empty() {
        p.hidden = cols.iter().filter(|c| c.1).map(|c| c.0.to_string()).collect();
    }
    let mut ids: Vec<&str> = cols.iter().map(|c| c.0).collect();
    ids.sort_by_key(|id| p.order.iter().position(|o| o == id).unwrap_or(usize::MAX));
    p.order = ids.into_iter().map(String::from).collect();
}

/// Move column `id` to slot `to` of the `visible` columns (`to == visible.len()`: after the
/// last), editing the full `order`, which also holds the hidden columns.
fn move_column(order: &mut Vec<String>, visible: &[&str], id: &str, to: usize) {
    order.retain(|o| o != id);
    let at = match visible.get(to) {
        Some(before) => order.iter().position(|o| o == before),
        None => visible.iter().rev().find(|v| **v != id).and_then(|last| order.iter().position(|o| o == last)).map(|i| i + 1),
    };
    order.insert(at.unwrap_or(order.len()), id.to_string());
}

/// The slot a column dragged from `from` drops into, or `None` when it would stay put.
fn drop_slot(from: usize, to: usize) -> Option<usize> {
    (to != from && to != from + 1).then_some(to)
}

/// A header press that may turn into a column move.
#[derive(Clone, Copy, PartialEq)]
struct HeadDrag {
    id: &'static str,
    from: usize,
    x0: f64,
    dx: f64,
    /// Past the threshold: moving, not clicking.
    active: bool,
    drop: Option<usize>,
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
    // Ids of the shown columns, in order. A memo, so a width change (every pointer move of a
    // resize) re-lays the grid without re-rendering the rows.
    let visible_ids = Memo::new(move |_| -> Vec<&'static str> {
        let vw = viewport_w();
        let p = prefs.get();
        let mut cols: Vec<(&'static str, bool, f64)> = columns.with_value(|c| c.iter().map(|c| (c.id, c.default_hidden, c.min_viewport)).collect());
        if !p.order.is_empty() {
            cols.sort_by_key(|c| p.order.iter().position(|o| o == c.0).unwrap_or(usize::MAX));
        }
        cols.into_iter()
            .filter(|&(id, default_hidden, min_viewport)| {
                let hidden = if p.hidden.is_empty() && p.order.is_empty() { default_hidden } else { p.hidden.iter().any(|h| h == id) };
                !hidden && vw >= min_viewport
            })
            .map(|c| c.0)
            .collect()
    });
    let visible_cols = Signal::derive(move || -> Vec<Column<R>> {
        visible_ids.with(|ids| columns.with_value(|cols| ids.iter().filter_map(|id| cols.iter().find(|c| c.id == *id).cloned()).collect()))
    });
    let col_defaults = move || -> Vec<(&'static str, bool)> { columns.with_value(|c| c.iter().map(|c| (c.id, c.default_hidden)).collect()) };
    // Column prefs are saved per table in ui_state.
    let persist = {
        let k = prefs_key.clone();
        Callback::new(move |_: ()| {
            let Some(p) = prefs.try_get_untracked() else { return };
            let k = k.clone();
            spawn_local(async move {
                let _ = crate::api::call_json("PUT", &format!("/ui-state/{k}"), &p).await;
            });
        })
    };
    // Column chooser: visibility, plus a reset of the dragged widths and order.
    let chooser = Callback::new(move |_: ()| -> Vec<MenuEntry> {
        let mut entries: Vec<MenuEntry> = columns.with_value(|cols| {
            cols.iter()
                .filter(|c| !c.label.is_empty())
                .map(|c| {
                    let id = c.id;
                    let hidden_now = {
                        let p = prefs.get_untracked();
                        if p.hidden.is_empty() && p.order.is_empty() { c.default_hidden } else { p.hidden.iter().any(|h| h == id) }
                    };
                    crate::ds::MenuItem::new(c.label).checked(!hidden_now).on(move || {
                        prefs.update(|p| {
                            materialize(p, &col_defaults());
                            if let Some(i) = p.hidden.iter().position(|h| h == id) { p.hidden.remove(i); } else { p.hidden.push(id.to_string()); }
                        });
                        persist.run(());
                    }).into()
                })
                .collect()
        });
        let p = prefs.get_untracked();
        let custom_order = !p.order.is_empty() && p.order.iter().map(String::as_str).ne(col_defaults().iter().map(|c| c.0));
        if !p.widths.is_empty() || custom_order {
            entries.push(MenuEntry::Sep);
            entries.push(crate::ds::MenuItem::new("Reset widths and order").icon("refresh").on(move || {
                prefs.update(|p| {
                    materialize(p, &col_defaults());
                    p.order = col_defaults().iter().map(|c| c.0.to_string()).collect();
                    p.widths.clear();
                });
                persist.run(());
            }).into());
        }
        entries
    });
    let template = Memo::new(move |_| {
        let p = prefs.get();
        // A column the user sized keeps that width; the others keep growing into the free space.
        let w: Vec<(f64, bool)> = visible_cols.get().iter().map(|c| match p.widths.get(c.id) { Some(w) => (*w, false), None => (c.width, c.grow) }).collect();
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

    // ---- header drags: resize (right edge) and move (the cell) ---------------------
    let head_ref = NodeRef::<leptos::html::Div>::new();
    // column id, pointer x and column width at grab
    let resizing = StoredValue::new(None::<(&'static str, f64, f64)>);
    let head_drag = RwSignal::new(None::<HeadDrag>);
    // The click that ends a column move must not also sort.
    let swallow_click = StoredValue::new(false);
    let root = crate::util::document_element;
    on_cleanup(move || {
        let _ = root().class_list().remove_2("col-resizing", "col-moving");
    });
    let set_width = move |id: &'static str, w: Option<f64>| {
        prefs.update(|p| match w {
            Some(w) => {
                p.widths.insert(id.to_string(), w);
            }
            None => {
                p.widths.remove(id);
            }
        });
    };
    let end_resize = move || {
        if resizing.get_value().is_some() {
            resizing.set_value(None);
            let _ = root().class_list().remove_1("col-resizing");
            persist.run(());
        }
    };
    // Slot under pointer x among the header cells (the dragged one counted where it sits).
    let slot_at = move |x: f64, dragged: &'static str, dx: f64| -> Option<usize> {
        let head = head_ref.get_untracked()?;
        let cells = head.query_selector_all(".dt-th[data-col]").ok()?;
        let n = cells.length();
        for k in 0..n {
            let Some(el) = cells.item(k).and_then(|e| e.dyn_into::<web_sys::Element>().ok()) else { continue };
            let r = el.get_bounding_client_rect();
            let left = r.left() - if el.get_attribute("data-col").as_deref() == Some(dragged) { dx } else { 0.0 };
            if x < left + r.width() / 2.0 {
                return Some(k as usize);
            }
        }
        Some(n as usize)
    };
    let end_move = move |commit: bool| {
        let Some(d) = head_drag.get_untracked() else { return };
        head_drag.set(None);
        if !d.active {
            return;
        }
        let _ = root().class_list().remove_1("col-moving");
        swallow_click.set_value(true);
        if let (true, Some(to)) = (commit, d.drop) {
            let visible = visible_ids.get_untracked();
            prefs.update(|p| {
                materialize(p, &col_defaults());
                move_column(&mut p.order, &visible, d.id, to);
            });
            persist.run(());
        }
    };

    let all_selected = move || selection.map(|s| matches!(s.get(), Selection::All { .. })).unwrap_or(false);

    view! {
        <div class=format!("dt dt-{}", table_id.get_value()) role="grid" aria-rowcount=move || total.get().map(|t| t as i64).unwrap_or(-1)>
            <div class="dt-cols"><crate::ds::MenuButton entries=chooser icon="sliders" title="Columns" /></div>
            <div class="dt-scroll" node_ref=scroller on:scroll=on_scroll>
                <div class="dt-inner" style=move || format!("min-width:{}px", min_w.get())>
                    <div class="dt-head" role="row" node_ref=head_ref style=move || format!("grid-template-columns:{}", template.get())>
                        {selection.map(|sel| view! {
                            <div class="dt-cell dt-check" role="columnheader">
                                <input type="checkbox" aria-label="Select all" prop:checked=all_selected
                                    on:change=move |ev| {
                                        let on = event_target_checked(&ev);
                                        if on { sel.update(|s| s.select_all(select_filter.map(|f| f.get_untracked()).unwrap_or(serde_json::Value::Null))) } else { sel.update(|s| s.clear()) }
                                    } />
                            </div>
                        })}
                        {move || { let cols = visible_cols.get(); let n = cols.len(); cols.into_iter().enumerate().map(|(idx, c)| {
                            let key = c.sort;
                            let id = c.id;
                            let active = move || key.map(|k| sort.with(|(s, _)| s == k)).unwrap_or(false);
                            let arrow = move || {
                                if active() { if sort.with(|(_, d)| *d) { "arrow-down" } else { "arrow-up" } } else { "" }
                            };
                            let base = format!("dt-cell dt-th{}{}", if c.right { " r" } else { "" }, if key.is_some() { " sortable" } else { "" });
                            let class = move || {
                                let (moving, drop) = head_drag.with(|d| match d {
                                    Some(d) if d.active => (d.id == id, d.drop),
                                    _ => (false, None),
                                });
                                let marker = match drop {
                                    Some(k) if k == idx => " drop-before",
                                    Some(k) if k == n && idx + 1 == n => " drop-after",
                                    _ => "",
                                };
                                format!("{base}{}{marker}", if moving { " moving" } else { "" })
                            };
                            view! {
                                <div class=class data-col=id
                                    role="columnheader" aria-sort=move || if active() { Some(if sort.with(|(_, d)| *d) { "descending" } else { "ascending" }) } else { None }
                                    style=move || head_drag.with(|d| match d { Some(d) if d.active && d.id == id => format!("transform:translateX({}px)", d.dx), _ => String::new() })
                                    title="Click to sort · drag to move"
                                    on:click=move |_| {
                                        if swallow_click.get_value() { swallow_click.set_value(false); return; }
                                        if let Some(k) = key { header_click(k) }
                                    }
                                    on:pointerdown=move |ev: web_sys::PointerEvent| {
                                        swallow_click.set_value(false);
                                        if ev.button() != 0 { return; }
                                        if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                                            let _ = el.set_pointer_capture(ev.pointer_id());
                                        }
                                        head_drag.set(Some(HeadDrag { id, from: idx, x0: ev.client_x() as f64, dx: 0.0, active: false, drop: None }));
                                    }
                                    on:pointermove=move |ev: web_sys::PointerEvent| {
                                        let Some(mut d) = head_drag.get_untracked().filter(|d| d.id == id) else { return };
                                        let x = ev.client_x() as f64;
                                        d.dx = x - d.x0;
                                        if !d.active {
                                            if d.dx.abs() < MOVE_THRESHOLD { return; }
                                            d.active = true;
                                            let _ = root().class_list().add_1("col-moving");
                                        }
                                        d.drop = slot_at(x, id, d.dx).and_then(|to| drop_slot(d.from, to));
                                        head_drag.set(Some(d));
                                    }
                                    on:pointerup=move |_| end_move(true)
                                    on:pointercancel=move |_| end_move(false)
                                    on:lostpointercapture=move |_| end_move(false)>
                                    <span class="truncate">{c.label}</span>
                                    {move || { let a = arrow(); (!a.is_empty()).then(|| view! { <Icon name=a size=12 /> }) }}
                                    <span class="dt-resize" aria-hidden="true" title="Drag to resize · double-click to reset"
                                        on:click=|ev: web_sys::MouseEvent| ev.stop_propagation()
                                        on:dblclick=move |ev: web_sys::MouseEvent| {
                                            ev.stop_propagation();
                                            set_width(id, None);
                                            persist.run(());
                                        }
                                        on:pointerdown=move |ev: web_sys::PointerEvent| {
                                            ev.stop_propagation();
                                            if ev.button() != 0 { return; }
                                            ev.prevent_default();
                                            let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
                                            let _ = el.set_pointer_capture(ev.pointer_id());
                                            // Start from the rendered width: a growing column is wider than its minimum.
                                            let w = el.parent_element().map(|p| p.get_bounding_client_rect().width()).unwrap_or(c.width);
                                            resizing.set_value(Some((id, ev.client_x() as f64, w)));
                                            let _ = root().class_list().add_1("col-resizing");
                                        }
                                        on:pointermove=move |ev: web_sys::PointerEvent| {
                                            ev.stop_propagation();
                                            if let Some((_, x0, w0)) = resizing.get_value().filter(|r| r.0 == id) {
                                                set_width(id, Some((w0 + ev.client_x() as f64 - x0).max(MIN_COL_W).round()));
                                            }
                                        }
                                        on:pointerup=move |ev: web_sys::PointerEvent| { ev.stop_propagation(); end_resize() }
                                        on:pointercancel=move |ev: web_sys::PointerEvent| { ev.stop_propagation(); end_resize() }>
                                    </span>
                                </div>
                            }
                        }).collect_view() }}
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

    #[test]
    fn grid_template_fills_when_no_column_grows() {
        assert_eq!(grid_template(&[(60.0, false), (80.0, false)], false), "60px 80px minmax(0, 1fr)");
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn move_column_keeps_hidden_columns_in_place() {
        // "plays" is hidden: moves are among a, b, c only.
        let visible = ["a", "b", "c"];
        let mut o = ids(&["a", "plays", "b", "c"]);
        move_column(&mut o, &visible, "c", 0);
        assert_eq!(o, ids(&["c", "a", "plays", "b"]));
        let mut o = ids(&["a", "plays", "b", "c"]);
        move_column(&mut o, &visible, "a", 3);
        assert_eq!(o, ids(&["plays", "b", "c", "a"]));
        let mut o = ids(&["a", "plays", "b", "c"]);
        move_column(&mut o, &visible, "a", 2);
        assert_eq!(o, ids(&["plays", "b", "a", "c"]));
    }

    #[test]
    fn drop_next_to_itself_is_no_move() {
        assert_eq!(drop_slot(2, 2), None);
        assert_eq!(drop_slot(2, 3), None);
        assert_eq!(drop_slot(2, 0), Some(0));
        assert_eq!(drop_slot(2, 4), Some(4));
    }

    #[test]
    fn materialize_pins_defaults_and_appends_new_columns() {
        let cols = [("a", false), ("b", true), ("c", false)];
        let mut p = ColumnPrefs::default();
        materialize(&mut p, &cols);
        assert_eq!(p.hidden, ids(&["b"]));
        assert_eq!(p.order, ids(&["a", "b", "c"]));
        // a saved order from before "b" existed
        let mut p = ColumnPrefs { order: ids(&["c", "a"]), hidden: vec![], widths: Default::default() };
        materialize(&mut p, &cols);
        assert!(p.hidden.is_empty());
        assert_eq!(p.order, ids(&["c", "a", "b"]));
    }
}
