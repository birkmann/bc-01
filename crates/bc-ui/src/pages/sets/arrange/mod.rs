//! Arrange timeline of DJ sets (two lanes, GPU waveforms, beat/bar lines, continuous zoom, trim,
//! transition bands, inspector). `ArrangeView` is the contract SetDetailPage embeds in its Arrange tab.
//!
//! Layout of the code: `logic` (pure, tested) / `engine` (viewport, gestures, frame loop) /
//! `lanes` (GL waveform lanes) / `paint` (2D overlay) / `inspector` (DOM editor) / this file (wiring).

mod engine;
mod inspector;
mod lanes;
mod logic;
mod paint;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use bc_types::player::{ItemOrigin, PlayerCommand, QueueItem, QueueSource};
use bc_types::sets::DjSetDetail;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;

use self::engine::{Engine, Load, Mods, Out, PlayerSnap};
use self::inspector::Inspector;
use crate::api;
use crate::ds::{Button, EmptyState, Icon, SegmentedControl, Variant, confirm, toast_err};
use crate::player::store::{position_now, use_player};
use crate::player::waveform::style_of;
use crate::theme::use_theme;
use crate::util;

type EngineRc = Rc<RefCell<Engine>>;

/// Everything the event handlers need; all `Copy` so it can move into any closure.
#[derive(Clone, Copy)]
struct Ui {
    eng: StoredValue<EngineRc, LocalStorage>,
    set_id: i64,
    cur: RwSignal<Option<Arc<DjSetDetail>>>,
    sel: RwSignal<Option<i64>>,
    sel_band: RwSignal<bool>,
    zoom: RwSignal<String>,
    announce: RwSignal<String>,
    tick: RwSignal<u32>,
    on_changed: Callback<()>,
    last_seek: StoredValue<f64>,
    player: crate::player::store::PlayerCtx,
}

impl Ui {
    /// Run `f` on the engine, then execute what it queued (outside the engine borrow).
    fn run<R>(&self, f: impl FnOnce(&mut Engine) -> R) -> R {
        let rc = self.eng.get_value();
        let (r, outs) = {
            let mut e = rc.borrow_mut();
            let r = f(&mut e);
            (r, std::mem::take(&mut e.outbox))
        };
        self.handle(&rc, outs);
        r
    }

    fn handle(&self, rc: &EngineRc, outs: Vec<Out>) {
        let mut loads: Vec<Load> = vec![];
        for o in outs {
            match o {
                Out::Select(id, band) => {
                    self.sel.set(id);
                    self.sel_band.set(band);
                }
                Out::Commit(ov) => self.commit(ov),
                Out::Scrub { index, track_s, live } => self.scrub(index, track_s, live),
                Out::PlayFrom(i) => {
                    let cue = self.cur.with_untracked(|d| d.as_ref().and_then(|d| d.items.get(i)).and_then(|it| it.cue_in_ms)).unwrap_or(0);
                    self.scrub(i, cue as f64 / 1000.0, false);
                }
                Out::Remove(id) => self.remove(id),
                Out::Zoom(s) => self.zoom.set(s),
                Out::Announce(s) => self.announce.set(s),
                Out::Load(l) => loads.push(l),
            }
        }
        if !loads.is_empty() {
            engine::spawn_loads(rc, loads);
        }
    }

    fn commit(&self, ov: crate::logic::timeline::DragOverride) {
        let (item, ty) = self.run(|e| (e.geom_item(ov.item_id).cloned(), e.transition_type_of(ov.item_id)));
        let Some(item) = item else {
            self.run(|e| e.clear_override());
            return;
        };
        match logic::override_patch(&ov, &item, ty.as_deref()) {
            Some(body) => self.patch(ov.item_id, body),
            None => self.run(|e| e.clear_override()),
        }
    }

    /// PATCH one item; the response is authoritative and replaces the local state at once.
    fn patch(&self, item_id: i64, body: serde_json::Value) {
        let ui = *self;
        spawn_local(async move {
            let path = format!("/sets/{}/items/{item_id}", ui.set_id);
            match api::patch::<_, DjSetDetail>(&path, &body).await {
                Ok(d) => {
                    let d = Arc::new(d);
                    ui.cur.set(Some(d.clone()));
                    ui.run(|e| e.set_detail(Some(d)));
                    ui.on_changed.run(());
                }
                Err(e) => {
                    toast_err(&e.message());
                    ui.run(|e| e.clear_override());
                    ui.tick.update(|t| *t += 1);
                }
            }
        });
    }

    fn remove(&self, id: i64) {
        let ui = *self;
        let title = self.cur.with_untracked(|d| d.as_ref().and_then(|d| d.items.iter().find(|i| i.id == id)).map(|i| i.title.clone())).unwrap_or_default();
        spawn_local(async move {
            if !confirm("Remove from set", &format!("Remove \u{201c}{title}\u{201d} from this set? The track stays in your library."), "Remove", true).await {
                return;
            }
            match api::call("DELETE", &format!("/sets/{}/items/{id}", ui.set_id)).await {
                Ok(()) => ui.on_changed.run(()),
                Err(e) => toast_err(&e.message()),
            }
        });
    }

    /// Click/drag on the ruler: play the set from there (`PlayQueue` of the set's items, then seek).
    fn scrub(&self, index: usize, track_s: f64, live: bool) {
        let player = self.player;
        let Some(d) = self.cur.get_untracked() else { return };
        let Some(item) = d.items.get(index) else { return };
        let Some(tid) = item.track_id.filter(|_| !item.missing) else { return };
        let now = util::perf_now();
        if live {
            if now - self.last_seek.get_value() < 45.0 {
                return;
            }
            self.last_seek.set_value(now);
        }
        let (cur_track, from_set) = player.state.with_untracked(|s| (s.current.as_ref().map(|c| c.track_id), matches!(&s.source, Some(QueueSource::Set { id, .. }) if *id == self.set_id)));
        if cur_track == Some(tid) && (from_set || live) {
            player.cmd(PlayerCommand::Seek { seconds: track_s });
            return;
        }
        let playable: Vec<_> = d.items.iter().filter(|i| i.track_id.is_some() && !i.missing).collect();
        let start = playable.iter().position(|i| i.id == item.id).unwrap_or(0);
        let items: Vec<QueueItem> = playable
            .iter()
            .map(|i| QueueItem {
                track_id: i.track_id.unwrap_or_default(),
                title: i.title.clone(),
                artist: (!i.artist.is_empty()).then(|| i.artist.clone()),
                duration_ms: i.duration_ms,
                bpm: i.bpm,
                camelot: i.camelot.clone(),
                energy: i.energy.map(|e| e as f64),
                art_url: i.art_url.clone(),
                origin: ItemOrigin::Library,
                ..Default::default()
            })
            .collect();
        player.cmd(PlayerCommand::PlayQueue { items, start_index: start, source: Some(QueueSource::Set { id: self.set_id, name: d.set.name.clone() }) });
        if track_s > 0.5 {
            util::after(450, move || player.cmd(PlayerCommand::Seek { seconds: track_s }));
            // the first seek can race the track load; repeat once
            util::after(1100, move || {
                let pos = player.clock.with_untracked(|c| c.position_s);
                if (pos - track_s).abs() > 3.0 {
                    player.cmd(PlayerCommand::Seek { seconds: track_s });
                }
            });
        }
    }

    fn player_snap(&self) -> PlayerSnap {
        let player = self.player;
        let (cur_track, qi, from_set) = player.state.with_untracked(|s| (s.current.as_ref().map(|c| c.track_id), s.queue_index, matches!(&s.source, Some(QueueSource::Set { id, .. }) if *id == self.set_id)));
        let (pos, playing) = player.clock.with_untracked(|c| (position_now(c, player.clock_at.get_untracked()), c.playing));
        let item = self.cur.with_untracked(|d| {
            let d = d.as_ref()?;
            let ids: Vec<Option<i64>> = d.items.iter().map(|i| if i.missing { None } else { i.track_id }).collect();
            logic::pick_playing_item(cur_track, qi, from_set, &ids)
        });
        PlayerSnap { item, track_pos_s: pos, playing: playing && item.is_some() }
    }
}

fn mods_of(ev: &web_sys::PointerEvent) -> Mods {
    Mods { shift: ev.shift_key(), alt: ev.alt_key(), ctrl: ev.ctrl_key() || ev.meta_key() }
}

/// `detail`: the live set (None while loading). `on_changed`: call after a PATCH/mutation so the
/// parent refreshes `detail` (the server response is authoritative).
#[component]
pub fn ArrangeView(
    set_id: i64,
    #[prop(into)] detail: Signal<Option<Arc<DjSetDetail>>>,
    #[prop(into)] on_changed: Callback<()>,
) -> impl IntoView {
    let theme = use_theme();
    let host = NodeRef::<leptos::html::Div>::new();
    let c0 = NodeRef::<leptos::html::Canvas>::new();
    let c1 = NodeRef::<leptos::html::Canvas>::new();
    let ov = NodeRef::<leptos::html::Canvas>::new();
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));

    let ui = Ui {
        eng: StoredValue::new_local(Rc::new(RefCell::new(Engine::new()))),
        set_id,
        cur: RwSignal::new(None),
        sel: RwSignal::new(None),
        sel_band: RwSignal::new(false),
        zoom: RwSignal::new(String::new()),
        announce: RwSignal::new(String::new()),
        tick: RwSignal::new(0),
        on_changed,
        last_seek: StoredValue::new(0.0),
        player: use_player(),
    };
    let follow = RwSignal::new(true);
    let snap = RwSignal::new(true);
    let style = crate::prefs::wave_style_pref();
    let has_items = Memo::new(move |_| ui.cur.with(|d| d.as_ref().is_some_and(|d| !d.items.is_empty())));

    // parent detail -> local state -> engine
    Effect::new(move |_| {
        let d = detail.get();
        ui.cur.set(d.clone());
        ui.run(|e| {
            let same = match (&e.detail, &d) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            };
            if !same {
                e.set_detail(d);
            }
        });
    });

    // theme -> colours
    Effect::new(move |_| {
        let map = bc_types::theme::resolve_colors(&theme.active());
        ui.run(|e| e.set_colors(map));
    });
    Effect::new(move |_| {
        let s = style.get();
        ui.run(|e| e.set_style(style_of(&s)));
    });
    Effect::new(move |_| {
        let (f, s) = (follow.get(), snap.get());
        ui.run(|e| {
            e.follow = f;
            e.snap = s;
        });
    });

    // mount: attach canvases, observe the size, start the frame loop
    Effect::new(move |started: Option<bool>| {
        if started == Some(true) {
            return true;
        }
        let (Some(h), Some(a), Some(b), Some(o)) = (host.get(), c0.get(), c1.get(), ov.get()) else { return false };
        let (a, b, o): (web_sys::HtmlCanvasElement, web_sys::HtmlCanvasElement, web_sys::HtmlCanvasElement) = (a.unchecked_into(), b.unchecked_into(), o.unchecked_into());
        ui.run(|e| e.attach(a, b, o));
        let el: web_sys::HtmlElement = h.unchecked_into();
        let w0 = el.client_width() as f64;
        let coarse = || util::media_matches("(pointer: coarse)");
        ui.run(|e| e.resize(w0, util::window().device_pixel_ratio(), coarse()));
        let disc = util::observe_resize(el.unchecked_ref(), move |w, _| {
            ui.run(|e| e.resize(w, util::window().device_pixel_ratio(), util::media_matches("(pointer: coarse)")));
        });
        let guard = send_wrapper::SendWrapper::new(disc);
        on_cleanup(move || (guard.take())());
        util::raf_loop(move |now| {
            if !alive.try_get_value().unwrap_or(false) {
                return false;
            }
            let snap = ui.player_snap();
            ui.run(|e| {
                e.set_player(snap);
                e.frame(now);
            });
            true
        });
        true
    });

    // ---- event handlers ---------------------------------------------------------------------
    let local = move |client_x: f64, client_y: f64| -> (f64, f64) {
        ov.get_untracked()
            .map(|c| {
                let r = c.get_bounding_client_rect();
                (client_x - r.left(), client_y - r.top())
            })
            .unwrap_or((0.0, 0.0))
    };
    let on_down = move |ev: web_sys::PointerEvent| {
        if ev.button() != 0 && ev.pointer_type() == "mouse" {
            return;
        }
        if let Some(c) = ov.get_untracked() {
            let _ = c.set_pointer_capture(ev.pointer_id());
        }
        if let Some(h) = host.get_untracked() {
            let _ = h.focus();
        }
        let (x, y) = local(ev.client_x() as f64, ev.client_y() as f64);
        let m = mods_of(&ev);
        ui.run(|e| e.pointer_down(ev.pointer_id(), x, y, m));
    };
    let on_move = move |ev: web_sys::PointerEvent| {
        let (x, y) = local(ev.client_x() as f64, ev.client_y() as f64);
        let m = mods_of(&ev);
        ui.run(|e| e.pointer_move(ev.pointer_id(), x, y, m, util::perf_now()));
    };
    let on_up = move |ev: web_sys::PointerEvent| {
        let (x, y) = local(ev.client_x() as f64, ev.client_y() as f64);
        ui.run(|e| e.pointer_up(ev.pointer_id(), x, y, util::perf_now()));
    };
    let on_cancel = move |ev: web_sys::PointerEvent| ui.run(|e| e.pointer_cancel(ev.pointer_id()));
    let on_leave = move |_: web_sys::PointerEvent| ui.run(|e| e.pointer_leave());
    let on_wheel = move |ev: web_sys::WheelEvent| {
        ev.prevent_default();
        let (x, _) = local(ev.client_x() as f64, ev.client_y() as f64);
        let m = Mods { shift: ev.shift_key(), alt: ev.alt_key(), ctrl: ev.ctrl_key() || ev.meta_key() };
        ui.run(|e| e.wheel(x, ev.delta_x(), ev.delta_y(), ev.delta_mode(), m));
    };
    let on_key = move |ev: web_sys::KeyboardEvent| {
        if ev.alt_key() {
            return;
        }
        if (ev.ctrl_key() || ev.meta_key()) && !matches!(ev.key().as_str(), "+" | "-" | "=" | "0") {
            return;
        }
        let m = Mods { shift: ev.shift_key(), alt: ev.alt_key(), ctrl: ev.ctrl_key() || ev.meta_key() };
        if ui.run(|e| e.key(&ev.key(), m)) {
            ev.prevent_default();
            ev.stop_propagation();
        }
    };

    // toolbar actions
    let zoom_in = Callback::new(move |_| ui.run(|e| e.zoom_center(1.6)));
    let zoom_out = Callback::new(move |_| ui.run(|e| e.zoom_center(1.0 / 1.6)));
    let fit = Callback::new(move |_| ui.run(|e| e.fit()));
    let zoom_sel = Callback::new(move |_| ui.run(|e| e.zoom_selection()));

    let summary = move || {
        ui.cur.with(|d| {
            d.as_ref().map(|d| {
                let s = &d.set.summary;
                (logic::fmt_clock(s.total_ms as f64 / 1000.0), (s.overlap_ms as f64 / 1000.0).round() as i64, s.problem_transitions, s.track_count)
            })
        })
    };

    let patch = Callback::new(move |(id, body): (i64, serde_json::Value)| ui.patch(id, body));
    let play_from = Callback::new(move |i: usize| ui.run(|e| e.outbox.push(Out::PlayFrom(i))));
    let remove = Callback::new(move |id: i64| ui.remove(id));

    view! {
        <div class="arr">
            <div class="arr-toolbar" role="toolbar" aria-label="Timeline controls">
                <div class="arr-tb-group">
                    <Button size=crate::ds::Size::Sm icon="minus" title="Zoom out (-)" on_click=zoom_out />
                    <span class="arr-zoom num" aria-live="off">{move || ui.zoom.get()}</span>
                    <Button size=crate::ds::Size::Sm icon="plus" title="Zoom in (+)" on_click=zoom_in />
                    <Button size=crate::ds::Size::Sm icon="expand" title="Fit the whole set (0)" on_click=fit />
                    <Button size=crate::ds::Size::Sm icon="zoom-in" title="Zoom to the selected clip (F)" on_click=zoom_sel disabled=Signal::derive(move || ui.sel.get().is_none()) />
                </div>
                <div class="arr-tb-group">
                    <Button size=crate::ds::Size::Sm variant=Variant::Outline icon="activity" title="Keep the playhead in view while playing" pressed=follow on_click=Callback::new(move |_| follow.update(|f| *f = !*f))>
                        <span class="arr-tb-label">"Follow"</span>
                    </Button>
                    <Button size=crate::ds::Size::Sm variant=Variant::Outline icon="grid" title="Snap trims to the beat grid (hold Alt to bypass)" pressed=snap on_click=Callback::new(move |_| snap.update(|f| *f = !*f))>
                        <span class="arr-tb-label">"Snap"</span>
                    </Button>
                    <SegmentedControl options=vec![("rgb", "Spectral"), ("bands", "3-band"), ("mono", "Mono")] value=style />
                </div>
                <div class="arr-summary num">
                    {move || summary().map(|(total, blends, risky, n)| view! {
                        <span>{n}" tracks"</span>
                        <span>{total}" total"</span>
                        {(blends > 0).then(|| view! { <span>{blends}"s in blends"</span> })}
                        {(risky > 0).then(|| view! { <span class="arr-risky"><Icon name="alert" />{risky}{if risky == 1 { " risky transition" } else { " risky transitions" }}</span> })}
                    })}
                </div>
            </div>

            <div class="arr-stage-wrap">
                <div
                    class="arr-stage"
                    node_ref=host
                    tabindex="0"
                    role="application"
                    aria-roledescription="timeline"
                    aria-label="Arrange timeline. Arrow keys select tracks, plus and minus zoom, Enter plays, Delete removes."
                    on:keydown=on_key
                    on:focus=move |_| ui.run(|e| e.set_focus(true))
                    on:blur=move |_| ui.run(|e| e.set_focus(false))
                    class:is-hidden=move || !has_items.get()
                >
                    <canvas class="arr-lane" node_ref=c0 aria-hidden="true"></canvas>
                    <canvas class="arr-lane" node_ref=c1 aria-hidden="true"></canvas>
                    <canvas
                        class="arr-ov"
                        node_ref=ov
                        aria-hidden="true"
                        on:pointerdown=on_down
                        on:pointermove=on_move
                        on:pointerup=on_up
                        on:pointercancel=on_cancel
                        on:pointerleave=on_leave
                        on:wheel=on_wheel
                        on:contextmenu=move |ev: web_sys::MouseEvent| ev.prevent_default()
                    ></canvas>
                </div>
                <Show when=move || !has_items.get()>
                    <EmptyState title="Nothing to arrange yet" hint="Add tracks in the Plan view, then arrange the blends here." icon="sliders" />
                </Show>
            </div>
            <div class="sr-only" aria-live="polite">{move || ui.announce.get()}</div>
            <p class="arr-help">
                <span>"Drag a clip edge to trim it"</span>
                <span>"Drag a transition chip to set its length"</span>
                <span>"Click the ruler to play from there"</span>
                <span>"Scroll to zoom, Shift+scroll or drag to pan, pinch on touch"</span>
            </p>
            <Inspector cur=ui.cur sel=ui.sel sel_band=ui.sel_band patch=patch tick=ui.tick on_play=play_from on_zoom=zoom_sel on_remove=remove />
        </div>
    }
}
