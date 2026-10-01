//! URL-state filters shared by Albums and Tracks: tag AND filter with scoped
//! suggestions, the "added" window (presets or a custom range), release sort and
//! the missing-tracks bar. Filters live in the query string (`tag=`, `added=`,
//! `sort=`, `order=`) like the legacy app, so a view is shareable and Back works.
use std::sync::Arc;

use bc_types::library::{FillAllResult, Page, ReleaseOut, TagOut, TagsQuery};
use leptos::prelude::*;
use leptos_router::NavigateOptions;
use leptos_router::hooks::{use_location, use_navigate, use_query_map};
use leptos_router::params::ParamsMap;

use super::logic::{self, AddedBounds, AddedResolved};
use crate::api;
use crate::data::{QuerySpec, use_query};
use crate::ds::{Button, Combobox, Dialog, Icon, MenuCtx, MenuItem, SelectOption, Size, Variant};
use crate::ds::popover::Rect;
use crate::logic::format::format_count;
use crate::util::qs_pairs;

// ---- URL state ------------------------------------------------------------------------------

#[derive(Clone)]
pub struct UrlState {
    pub query: Memo<ParamsMap>,
    pub path: Memo<String>,
    nav: Arc<dyn Fn(&str, NavigateOptions) + Send + Sync>,
}

impl UrlState {
    pub fn new() -> Self {
        let query = use_query_map();
        let loc = use_location();
        let path = Memo::new(move |_| loc.pathname.get());
        Self { query, path, nav: Arc::new(use_navigate()) }
    }
    pub fn get(&self, key: &str) -> Option<String> {
        self.query.with(|q| q.get(key)).filter(|v| !v.is_empty())
    }
    pub fn get_untracked(&self, key: &str) -> Option<String> {
        self.query.with_untracked(|q| q.get(key)).filter(|v| !v.is_empty())
    }
    pub fn all(&self, key: &str) -> Vec<String> {
        self.query.with(|q| q.get_all(key)).unwrap_or_default()
    }
    /// Rewrite the query string (replace, not push: filters should not fill the history).
    pub fn update(&self, f: impl FnOnce(&mut ParamsMap)) {
        let mut q = self.query.get_untracked();
        f(&mut q);
        let url = format!("{}{}", self.path.get_untracked(), q.to_query_string());
        (self.nav)(&url, NavigateOptions { replace: true, scroll: false, ..Default::default() });
    }
    pub fn set(&self, key: &'static str, value: Option<String>) {
        self.update(move |q| {
            q.remove(key);
            if let Some(v) = value {
                q.insert(key, v);
            }
        });
    }
}

impl Default for UrlState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn describe_tags(tags: &[String]) -> String {
    tags.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(" and ")
}

// ---- added filter -----------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct AddedFilter {
    pub url_value: Memo<Option<String>>,
    /// When the relative window was last measured from.
    anchor: RwSignal<f64>,
    pub resolved: Memo<Option<AddedResolved>>,
}

fn tz_offset() -> f64 {
    js_sys::Date::new_0().get_timezone_offset()
}

impl AddedFilter {
    pub fn new(url: &UrlState) -> Self {
        let u = url.clone();
        let url_value = Memo::new(move |_| u.get("added"));
        let anchor = RwSignal::new(crate::util::unix_ms());
        let resolved = Memo::new(move |_| url_value.get().and_then(|v| logic::resolve_added(&v, anchor.get(), tz_offset())));
        Self { url_value, anchor, resolved }
    }
    pub fn active(&self) -> bool {
        self.resolved.with(|r| r.is_some())
    }
    pub fn bounds(&self) -> AddedBounds {
        self.resolved.with_untracked(|r| r.as_ref().map(|r| r.bounds.clone()).unwrap_or_default())
    }
    pub fn description(&self) -> String {
        self.resolved.with(|r| r.as_ref().map(|r| format!("added {}", r.description)).unwrap_or_default())
    }
    /// Measure a relative window from now. `true` when the bounds moved.
    pub fn refresh(&self) -> bool {
        let before = self.bounds();
        self.anchor.set(crate::util::unix_ms());
        before != self.bounds()
    }
    /// Stable identity for a listing key (the URL words, not the resolved instants).
    pub fn key(&self) -> String {
        self.url_value.get().unwrap_or_default()
    }
}

#[component]
pub fn AddedFilterBar(url: UrlState, filter: AddedFilter) -> impl IntoView {
    let menu = expect_context::<MenuCtx>();
    let dialog = RwSignal::new(false);
    let from = RwSignal::new(String::new());
    let to = RwSignal::new(String::new());
    let btn = NodeRef::<leptos::html::Button>::new();
    let url2 = url.clone();

    let set = {
        let url = url.clone();
        move |v: Option<String>| {
            url.set("added", v);
            filter.anchor.set(crate::util::unix_ms());
        }
    };
    let set = Arc::new(set);

    let open_menu = {
        let set = set.clone();
        move |_| {
            use wasm_bindgen::JsCast;
            let Some(el) = btn.get_untracked() else { return };
            let cur = filter.url_value.get_untracked();
            let mut entries: Vec<crate::ds::MenuEntry> = vec![crate::ds::MenuEntry::Label("Added".into())];
            for p in logic::PRESETS.iter() {
                let (set, v) = (set.clone(), p.value.to_string());
                entries.push(MenuItem::new(p.label).checked(cur.as_deref() == Some(p.value)).on(move || set(Some(v.clone()))).into());
            }
            entries.push(crate::ds::MenuEntry::Sep);
            let (f, t) = filter.resolved.with_untracked(|r| r.as_ref().map(|r| (r.from.clone(), r.to.clone())).unwrap_or_default());
            entries.push(
                MenuItem::new("Custom range…")
                    .icon("clock")
                    .on(move || {
                        from.set(f.clone());
                        to.set(t.clone());
                        dialog.set(true);
                    })
                    .into(),
            );
            if cur.is_some() {
                let set = set.clone();
                entries.push(MenuItem::new("Clear date filter").icon("x").on(move || set(None)).into());
            }
            menu.open(Rect::of(el.unchecked_ref()), entries);
        }
    };

    let apply = {
        let set = set.clone();
        move || {
            let (f, t) = (from.get_untracked(), to.get_untracked());
            if f.is_empty() && t.is_empty() {
                return;
            }
            set(Some(format!("{f}..{t}")));
            dialog.set(false);
        }
    };
    let apply = Arc::new(apply);
    let (apply_a, apply_b) = (apply.clone(), apply.clone());
    let clear = {
        let u = url2;
        move |_| {
            u.set("added", None);
        }
    };

    view! {
        {move || if filter.active() {
            view! {
                <span class="lib-pill on">
                    <button type="button" class="lib-pill-main" node_ref=btn on:click=open_menu.clone() title="Change the date filter" aria-haspopup="menu">
                        <Icon name="clock" size=12 />
                        {move || filter.resolved.with(|r| r.as_ref().map(|r| r.label.clone()).unwrap_or_default())}
                    </button>
                    <button type="button" class="lib-pill-x" on:click=clear.clone() aria-label="Clear the date filter" title="Clear the date filter"><Icon name="x" size=12 /></button>
                </span>
            }.into_any()
        } else {
            view! {
                <button type="button" class="lib-pill dashed" node_ref=btn on:click=open_menu.clone() aria-haspopup="menu">
                    <Icon name="clock" size=12 />"Filter by date"
                </button>
            }.into_any()
        }}
        <Dialog open=dialog title="Added between" footer=crate::ds::children(move || {
            let a = apply_a.clone();
            view! {
                <Button variant=Variant::Ghost on_click=move |_| dialog.set(false)>"Cancel"</Button>
                <Button variant=Variant::Primary disabled=Signal::derive(move || from.get().is_empty() && to.get().is_empty()) on_click=move |_| a()>"Apply"</Button>
            }
        })>
            {
                let apply_b = apply_b.clone();
                move || {
                    let ap = apply_b.clone();
                    view! {
                        <div class="lib-dates">
                            <label class="field"><span class="label">"From day"</span>
                                <input class="input" type="date" aria-label="From day" prop:value=move || from.get()
                                    max=move || to.get()
                                    on:input=move |ev| from.set(event_target_value(&ev)) />
                            </label>
                            <label class="field"><span class="label">"To day"</span>
                                <input class="input" type="date" aria-label="To day" prop:value=move || to.get()
                                    min=move || from.get()
                                    on:input=move |ev| to.set(event_target_value(&ev))
                                    on:keydown=move |ev| if ev.key() == "Enter" { ap(); } />
                            </label>
                        </div>
                        <p class="faint" style="margin-top:8px;font-size:var(--text-xs)">"Either end may be left open."</p>
                    }
                }
            }
        </Dialog>
    }
}

// ---- tag filter --------------------------------------------------------------------------------

#[derive(Clone, Default, PartialEq)]
pub struct TagScope {
    pub q: Option<String>,
    pub loved: Option<bool>,
    pub added: AddedBounds,
}

const PICKER_LIMIT: i64 = 1000;

#[component]
pub fn TagFilterBar(url: UrlState, #[prop(into)] scope: Signal<TagScope>) -> impl IntoView {
    let tags = {
        let u = url.clone();
        Memo::new(move |_| u.all("tag"))
    };
    let adding = RwSignal::new(false);
    let needle = RwSignal::new(String::new());

    let remove = {
        let url = url.clone();
        move |t: String| {
            url.update(move |q| {
                let rest: Vec<String> = q.get_all("tag").unwrap_or_default().into_iter().filter(|x| *x != t).collect();
                q.remove("tag");
                for r in rest {
                    q.insert("tag", r);
                }
            });
        }
    };
    let remove = Arc::new(remove);
    let add = {
        let url = url.clone();
        move |t: String| {
            url.update(move |q| {
                let have = q.get_all("tag").unwrap_or_default();
                if !have.iter().any(|x| x.eq_ignore_ascii_case(&t)) {
                    q.insert("tag", t);
                }
            });
        }
    };
    let add = Arc::new(add);
    let clear = {
        let url = url.clone();
        move |_| url.update(|q| {
            q.remove("tag");
        })
    };

    // Suggestions, narrowed to what would still leave something on screen. Fetched only while picking.
    let cloud = use_query::<Vec<TagOut>>(move || {
        if !adding.get() {
            return None;
        }
        let s = scope.get();
        let tg = tags.get();
        let tq = TagsQuery {
            tags: tg,
            q: s.q,
            loved: s.loved,
            added_after: s.added.after,
            added_before: s.added.before,
            limit: Some(PICKER_LIMIT),
            ..Default::default()
        };
        let url = format!("/tags{}", qs_pairs(&tags_pairs(&tq)));
        Some(QuerySpec::new(url, &["tag", "track"]))
    });
    let options = Signal::derive(move || {
        let have: Vec<String> = tags.get().iter().map(|t| t.to_lowercase()).collect();
        cloud
            .data
            .get()
            .map(|d| {
                d.iter()
                    .filter(|t| !have.contains(&t.name.to_lowercase()))
                    .map(|t| SelectOption::new(t.name.clone(), format!("{}  ({})", t.name, format_count(t.track_count))))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    });
    let narrowed = move || !tags.get().is_empty() || scope.with(|s| s.q.is_some() || s.loved.is_some() || s.added != AddedBounds::default());

    let add2 = add.clone();
    view! {
        {
            let remove = remove.clone();
            move || {
                let remove = remove.clone();
                tags.get().into_iter().map(move |t| {
                    let (r, t2) = (remove.clone(), t.clone());
                    view! {
                        <button type="button" class="lib-pill on" title=format!("Remove #{t} from the filter") aria-label=format!("Remove tag {t} from the filter")
                            on:click=move |_| r(t2.clone())>
                            "#"{t.clone()}<Icon name="x" size=12 />
                        </button>
                    }
                }).collect_view()
            }
        }
        {move || if adding.get() {
            let add = add2.clone();
            view! {
                <span class="lib-tagpick">
                    <Combobox options=options value=needle placeholder=if narrowed() { "Narrow by tag…" } else { "Filter by tag…" }
                        on_pick=Callback::new(move |o: SelectOption| { add(o.value); needle.set(String::new()); adding.set(false); }) />
                    <Button variant=Variant::Ghost size=Size::Sm icon="x" title="Cancel" on_click=move |_| { needle.set(String::new()); adding.set(false); } />
                </span>
            }.into_any()
        } else {
            view! {
                <button type="button" class="lib-pill dashed" on:click=move |_| adding.set(true) aria-haspopup="listbox">
                    <Icon name="plus" size=12 />{move || if tags.get().is_empty() { "Filter by tag" } else { "Add tag" }}
                </button>
            }.into_any()
        }}
        {move || (tags.get().len() > 1).then(|| view! { <button type="button" class="lib-link" on:click=clear.clone()>"Clear"</button> })}
    }
}

fn tags_pairs(q: &TagsQuery) -> Vec<(String, String)> {
    let mut o = vec![];
    if let Some(v) = &q.q {
        o.push(("q".to_string(), v.clone()));
    }
    for t in &q.tags {
        o.push(("tags".into(), t.clone()));
    }
    if let Some(v) = q.loved {
        o.push(("loved".into(), v.to_string()));
    }
    if let Some(v) = &q.added_after {
        o.push(("added_after".into(), v.clone()));
    }
    if let Some(v) = &q.added_before {
        o.push(("added_before".into(), v.clone()));
    }
    if let Some(v) = q.min_count {
        o.push(("min_count".into(), v.to_string()));
    }
    if let Some(v) = q.limit {
        o.push(("limit".into(), v.to_string()));
    }
    o
}

// ---- release sort --------------------------------------------------------------------------------

/// Sort field + direction in the URL. `sort`/`order` absent = newest added first.
#[derive(Clone)]
pub struct ReleaseSortState {
    url: UrlState,
}

impl ReleaseSortState {
    pub fn new(url: &UrlState) -> Self {
        Self { url: url.clone() }
    }
    pub fn current(&self) -> (logic::SortSpec, bc_types::library::SortDir, bool) {
        let (s, o) = (self.url.get("sort"), self.url.get("order"));
        logic::effective_sort(s.as_deref(), o.as_deref())
    }
    pub fn describe(&self) -> Option<String> {
        let (spec, order, active) = self.current();
        active.then(|| format!("by {}, {}", spec.label.to_lowercase(), logic::describe_order(spec, order).to_lowercase()))
    }
    fn write(&self, sort: &str, order: bc_types::library::SortDir) {
        let spec = logic::sort_spec(sort);
        self.url.update(move |q| {
            q.remove("sort");
            q.remove("order");
            if spec.value != "added" {
                q.insert("sort", spec.value.to_string());
            }
            if order != spec.default_order {
                q.insert("order", logic::order_name(order).to_string());
            }
        });
    }
}

#[component]
pub fn ReleaseSortBar(state: ReleaseSortState) -> impl IntoView {
    let menu = expect_context::<MenuCtx>();
    let btn = NodeRef::<leptos::html::Button>::new();
    let st = state.clone();
    let open = move |_| {
        use wasm_bindgen::JsCast;
        let Some(el) = btn.get_untracked() else { return };
        let (cur, _, _) = st.current();
        let entries = logic::RELEASE_SORTS
            .iter()
            .map(|s| {
                let (st, v, d) = (st.clone(), s.value, s.default_order);
                MenuItem::new(s.label).checked(cur.value == s.value).on(move || st.write(v, d)).into()
            })
            .collect();
        menu.open(Rect::of(el.unchecked_ref()), entries);
    };
    let st2 = state.clone();
    let flip = move |_| {
        let (spec, order, _) = st2.current();
        st2.write(spec.value, if order == bc_types::library::SortDir::Asc { bc_types::library::SortDir::Desc } else { bc_types::library::SortDir::Asc });
    };
    let (st3, st4, st5) = (state.clone(), state.clone(), state);
    view! {
        <div class="lib-sort">
            <span class="faint lib-sort-label">"Sort"</span>
            <button type="button" class="select-trigger" node_ref=btn on:click=open aria-haspopup="listbox" aria-label="Sort by">
                <span class="truncate">{move || st3.current().0.label}</span>
                <Icon name="chevron-down" />
            </button>
            <button type="button"
                class=move || { let (s, o, _) = st4.current(); if o == s.default_order { "btn btn-outline btn-icon" } else { "btn btn-outline btn-icon is-on" } }
                aria-label="Reverse the order" title=move || { let (s, o, _) = st5.current(); format!("{} (click to reverse)", logic::describe_order(s, o)) } on:click=flip>
                <Icon name="sort" />
            </button>
        </div>
    }
}

// ---- fill missing -------------------------------------------------------------------------------

fn fill_label(r: &FillAllResult) -> String {
    if r.queued > 0 {
        format!("Queued {}", format_count(r.queued))
    } else if r.already_queued > 0 {
        "Already queued".into()
    } else if r.unfillable > 0 {
        format!("{} unlinked", format_count(r.unfillable))
    } else {
        "Nothing to fill".into()
    }
}

fn fill_title(r: &FillAllResult) -> String {
    let mut parts = vec![];
    if r.queued > 0 {
        parts.push(format!("{} queued (progress on the Downloads page)", r.queued));
    }
    if r.already_queued > 0 {
        parts.push(format!("{} already being fetched", r.already_queued));
    }
    if r.unfillable > 0 {
        parts.push(format!("{} have no Bandcamp link recorded, so there is no page to re-download", r.unfillable));
    }
    if parts.is_empty() { "Every album is complete.".into() } else { format!("{} album(s) short of tracks: {}.", r.missing, parts.join("; ")) }
}

/// How many albums are short of tracks, a view of just those, and the one click that queues them all.
#[component]
pub fn FillMissingBar(url: UrlState) -> impl IntoView {
    let active = {
        let u = url.clone();
        Memo::new(move |_| u.get("missing").as_deref() == Some("1"))
    };
    let count = use_query::<Page<ReleaseOut>>(|| Some(QuerySpec::keyed("releases:missing-count", "/releases?missing=true&limit=1", &["release", "job"])));
    let total = Memo::new(move |_| count.data.get().map(|p| p.total).unwrap_or(0));
    let fill = RwSignal::new(None::<Result<FillAllResult, String>>);
    let busy = RwSignal::new(false);
    let toggle = {
        let url = url.clone();
        move |on: bool| url.set("missing", on.then(|| "1".to_string()))
    };
    let toggle = Arc::new(toggle);
    let (t1, t2) = (toggle.clone(), toggle);
    let run = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        leptos::task::spawn_local(async move {
            let r = api::post::<_, FillAllResult>("/releases/fill", &serde_json::json!({})).await;
            fill.set(Some(r.map_err(|e| e.message())));
            busy.set(false);
        });
    };
    view! {
        {move || (total.get() > 0 || active.get()).then(|| {
            let (t1, t2) = (t1.clone(), t2.clone());
            view! {
                {if active.get() {
                    view! {
                        <span class="lib-pill on">
                            <span class="lib-pill-main"><Icon name="disc" size=12 />{move || format!("{} missing tracks", format_count(total.get()))}</span>
                            <button type="button" class="lib-pill-x" aria-label="Show the whole library again" title="Show the whole library again" on:click=move |_| t1(false)><Icon name="x" size=12 /></button>
                        </span>
                    }.into_any()
                } else {
                    view! {
                        <button type="button" class="lib-pill dashed warn" aria-pressed="false" title=move || format!("Show only the {} album(s) with missing tracks", format_count(total.get())) on:click=move |_| t2(true)>
                            <Icon name="disc" size=12 />{move || format!("{} missing tracks", format_count(total.get()))}
                        </button>
                    }.into_any()
                }}
                <button type="button" class="lib-pill warn" on:click=run.clone() disabled=move || busy.get()
                    title=move || match fill.get() { Some(Ok(r)) => fill_title(&r), Some(Err(e)) => e, None => "Queue every album with missing tracks for download".into() }>
                    <Icon name=move || if matches!(fill.get(), Some(Ok(_))) { "check" } else { "download" } size=12 />
                    {move || match fill.get() { Some(Ok(r)) => fill_label(&r), Some(Err(_)) => "Retry fill all".into(), None => "Fill all".to_string() }}
                </button>
            }
        })}
    }
}
