# Workstream 3 API: analysis, waveforms, music logic, recommendations

Router paths are WITHOUT the `/api` prefix. DTOs: `bc_types::{analysis, sets, suggest}`.
Services: `bc_analysis::AnalysisService`, `bc_recommend::RecommendService`
(`new(deps) -> Self`, `async start(&self)`, `router(&self) -> axum::Router`).
Errors are `bc_types::Problem` (problem+json). `{T}` below = `bc_types::suggest::SuggestTrack`
(currently `TrackBrief`; becomes WS1's `TrackOut` by changing one type alias).

## Waveform API v3

Published for the UI (crate `bc-waveform`; everything here except the renderer is wasm-safe and
needs no WebGL).

* **`bc_waveform::Style`** is an alias of `view::WaveStyle`. New variant **`Style::Bars`**
  beside `RgbSpectral` / `ThreeBand` / `Mono` (an exhaustive `match` must add it).
* **`WaveTheme`** (colours, plain `[f32;4]` straight alpha) has new fields:
  `played` (default near-white `#f0f0f0`), `unplayed` (default cyan `#2ec7e6`),
  `bar_w_css: f32` (default `2.0`) and `gap_css: f32` (default `1.0`). Existing fields are kept
  (`played_dim` / `played_to` are now ignored; deck styles show the played part as the same hue
  at alpha 0.4). Because `WaveTheme` is built as `WaveTheme::default()` plus field assignments,
  nothing breaks. **The UI sets the bar geometry through the theme**: `t.bar_w_css = 1.5;
  t.gap_css = 1.0; view.set_theme(t)` (it is not on `ViewState`, so the existing `ViewState {..}`
  literals still compile). The renderer converts CSS px to device px with the DPR and snaps to
  integers (bar >= 1 px, gap 0 or >= 1 px) so bars stay crisp.
* **`ViewState`** is unchanged. With `style: Style::Bars`: the colour boundary is the playhead
  (white before, cyan after, split at the exact pixel), so **no playhead line is drawn**;
  per-track p95 normalisation is applied automatically (the `normalise` flag is ignored for
  Bars). Use `ViewMode::Overview` for the player bar. For the other styles `normalise` now means
  "p95 normalisation" (the 95th percentile of RMS / each band reaches full height).
* **`Markers.hover_s: Option<f64>`** (existing): bars between the playhead and `hover_s` are
  mixed 35 % towards `played` (hover preview). Bars after `buffered_to_s` are drawn at 45 %
  alpha. Mix-in/out and cue markers are thin 1 px lines with a small flag; the beat grid is not
  drawn in Bars mode.
* **`bc_waveform::bars`** (also re-exported at the crate root):
  ```rust
  pub struct Refs { pub rms: f32, pub peak: f32, pub low: f32, pub mid: f32, pub high: f32 } // linear p95
  pub struct BarValue { pub h: f32 }            // 0..1 of the full mirrored height, peak term included
  pub fn reference_levels(level: &Level) -> Refs;
  pub fn compute_bars(level: &Level, refs: &Refs, t0: f64, t1: f64, n_bars: usize) -> Vec<BarValue>;
  pub const BAR_MIN_PX: f32 = 1.5;              // minimum drawn height in device px
  ```
  `Level` = `bc_waveform::format::Levels` (a `Waveform.overview` / `Waveform.detail`).
  **`t0` / `t1` are fractions of the level's span (0.0 = start of the track, 1.0 = end), not
  seconds**: a whole-track mini-wave passes `0.0, 1.0`; a zoomed window passes
  `start_s / duration_s, end_s / duration_s`. Bars outside the level give `h = 0`. Typical row
  mini-wave: `let refs = reference_levels(&wf.overview); let bars = compute_bars(&wf.overview,
  &refs, 0.0, 1.0, n);` then draw each bar `max(h * height, 1.5 px)` tall, centred, `bar_w`
  wide with `gap` between. Normalisation is built in (`h` is relative to the track's RMS p95).
* **Format version 3** (`bc_waveform::format::VERSION`): older `.bcw2` files are rejected by
  `read_header` (`UnsupportedVersion`), which the store treats as a cache miss, so they are
  regenerated on demand. The waveform route's ETag is now `"{hash}-v3-{overview|detail}"`.
  **Browser caching**: the route only sends `immutable` when the URL carries both `v=<hash>` and
  `f=3`, so please append **`&f=3`** (`bc_waveform::format::VERSION`) to waveform URLs; otherwise
  an old immutable-cached v1 response would stay in the HTTP cache. Without `f` the response is
  `no-cache` + ETag (still correct, one revalidation per load).

## Analysis (`AnalysisService`)

| Method | Path | Request | Response |
| --- | --- | --- | --- |
| GET | `/analysis/status` | - | `AnalysisStatus` |
| GET | `/analysis/queue?limit=200&jobs_limit=20` | - | `AnalysisQueue` |
| POST | `/analysis/scan` | `ScanRequest` (`scope`: missing/stale/failed/all/ids/upgrade) | `ScanResponse`; **202** when a job was created, else 200 |
| POST | `/analysis/tracks/{id}` | - | **202** `Accepted{job_id}`; runs at top priority (deck-loaded track). `?wait=true` runs inline and returns `AnalysisOut` (scripts, tests) |
| GET | `/analysis/tracks/{id}` | - | `AnalysisOut` (404 if never analysed) |
| POST | `/analysis/prioritize` | `{track_ids:[..], level:"deck"\|"queue"}` | 204. Bumps pending analyze items to priority 0 / 100 (PLAN 7.3) |
| GET | `/analysis/compatible/{id}?bpm_tolerance=0.06&include_risky=false&limit=100` | - | `Page<{T}>` sorted by key then tempo score |
| GET | `/analysis/compatibility/{a}/{b}` | - | `CompatibilityOut` (400 unless both analysed) |
| GET | `/tracks/{id}/waveform?level=overview\|detail[&v=hash&f=3]` | - | binary `.bcw2` (format v3), see [waveform-format.md](waveform-format.md). `ETag` = source hash + format version; send `&v=<hash>&f=3` for the immutable cache |
| GET | `/tracks/{id}/music` | - | `TrackMusicInfo` (beat grid + cues + mix points; one fetch on deck load) |
| GET | `/tracks/{id}/cues` | - | `Vec<CuePoint>` |
| PUT | `/tracks/{id}/cues` | `Vec<CuePoint>` (user cues only; auto cues untouched) | `Vec<CuePoint>` |
| GET | `/tracks/{id}/peaks?points=200` | - | legacy JSON `{track_id, peaks:[[lo,hi]..]}` int8 pairs, derived from the overview (compat for the old mix-point code) |
| GET | `/tracks/{id}/bands?points=400` | - | legacy JSON `{track_id, bands:{low:[..],mid:[..],high:[..]}}` 0..1 floats |

### WS topics

* `analysis.batch` - `AnalysisBatchEvent` when a batch is claimed.
* `analysis.item` - `AnalysisItemEvent` per finished track (fills BPM/KEY cells without refetch).
* `analysis.progress` - `AnalysisProgressEvent` authoritative job counters.
* `invalidate` entity `track` for every analysed track id (batched).

Analysis runs as `analyze` jobs in `bc-jobs` (priority: deck 0, new downloads 50, queue/set 100,
backfill 200). Workers hold no DB handle and run at nice(10); results are written by the parent in
batches of ~200 through `Db::write`.

## Recommenders (`RecommendService`)

| Method | Path | Request | Response |
| --- | --- | --- | --- |
| POST | `/suggest/next` | `SuggestRequest` | `SuggestResponse<{T}>` |
| GET | `/suggest/loved?limit=24&seed=0&tags=a,b` | `LovedQuery` | `LovedSuggestResponse<{T}>` |
| POST | `/suggest/similar` | `SimilarRequest` | `SimilarResponse<{T}>` |
| GET | `/sets/{id}/pool?q&sort=bpm\|added\|random&order&seed&offset&limit` | `PoolQuery` | `PoolPage<{T}>` |
| POST | `/sets/{id}/suggest` | `SetSuggestRequest` | `SetSuggestResponse<{T}>` |
| POST | `/sets/{id}/automix` | `AutomixRequest` | `DjSetDetail` (400 empty pool / > 200 tracks) |
| POST | `/playlists/{id}/similar` | `SimilarPlaylistRequest` | `SimilarPlaylistOut` (== WS1 `PlaylistOut`) |

All suggest routes honour the library scope (WS1) and never fail for lack of analysis.
`nextup` is also a plain Rust function for in-process callers (WS4 auto-fill):
`bc_recommend::nextup::suggest(&Db, &Scope, &SuggestRequest) -> Result<SuggestResponse<TrackBrief>>`.

Weights (unchanged from the Python app): nextup key .35 / tempo .30 / tags .15 / energy .10;
similar tags .40 / artist .18 / label .14 / tempo .14 / key .08 / energy .06 (renormalised over
enabled+available signals); automix edge key .50 / BPM .35 (8 %) / arc .15, -.10 same artist,
beam 5x8, cap 200.

## `bc-music` (wasm-safe, no I/O)

`camelot` (to_camelot, parse_key, key/bpm compatibility, compatible_keys, transpose),
`setmath` (Slot, summarise, start_times, score_transition), `ordering` (fractional positions),
`beatgrid` (grid <-> beats, phase, snap, residual, legacy extrapolation), `mixpoints`
(heuristic + peaks, phrase snapping), `transitions` (plan_transitions), `shuffle`
(`(id*2654435761 + seed) % 2^32`). WS4/WS5 depend on this crate for the single copy of mix logic.

## `bc-waveform` renderer API (feature `webgl`; pure parts always compiled)

Cargo features: `default = ["zstd", "store"]`; `zstd` and `store` are native-only (target-gated
deps, safe to leave on for wasm but pointless); `webgl` pulls web-sys/js-sys/wasm-bindgen.
The UI depends on it with `default-features = false, features = ["webgl"]`.

Data (all targets): `bc_waveform::{format, builder, mip, legacy, scale, view}`.

```rust
// builder (analysis crate): feed the mono mix (L+R)/2 at the NATIVE rate, any chunk size
let mut b = WaveformBuilder::new(sample_rate);       // hop = round(sr*256/44100)
b.push_mono(&chunk);                                  // streaming, flat memory
let w: Waveform = b.finish(source_hash);              // overview (2048) + detail
// format
w.to_bytes(&EncodeOpts::file())   // zstd file; ::wire() raw both; ::wire_overview(); ::wire_detail()
Waveform::from_bytes(&bytes)? ; read_header(&bytes)? -> Header   // cheap, no decompress
w.meta(track_id) -> bc_types::analysis::WaveformMeta  // bytes = 0; meta_with_bytes(id, n)
w.duration_ms() / duration_s() / detail_rate_hz() / has_overview() / has_detail() / ensure_overview()
// Waveform { sample_rate, hop_samples, total_samples, source_hash:[u8;16], overview: Levels, detail: Option<Levels> }
// Levels { n, planes: [Vec<u8>;6] }  peak_pos, peak_neg, rms, low, mid, high; point(i)->[u8;6]
// detail-only container: overview.n == 0 (has_overview() == false)
// scale: db_to_u8, lin_to_u8, u8_to_db, u8_to_lin
// mip: Pyramid::{build(&Levels, dt0_s), from_waveform(&Waveform), level_for(ppp), sample_column(t0_s, t1_s)->[u8;6]}
// legacy: peaks_json(&w, points)->Vec<[i8;2]>, bands_json(&w, points)->Vec<[f32;3]>,
//         mix_envelope(&w)->(Vec<f32> level, Vec<f32> low)   // for bc_music::mixpoints::Envelope
// store (feature store, native): source_hash(&Path)->io::Result<[u8;16]>;
//   WaveformStore::new(dir, cap_bytes); put/get/get_overview_only/header/remove/evict_to_cap/stats
```

Renderer (feature `webgl`), pure types in `bc_waveform::view`:

```rust
let mut view = WaveformView::new(canvas)?;           // WebGL2, falls back to Canvas2D; view.backend()
view.resize(css_w, css_h, dpr);
view.set_data(Some(Rc::new(Waveform::from_bytes(&bytes)?)));   // uploads mip textures once
view.set_markers(Markers { grid, cues, mix_in_s, mix_out_s, loop_region, buffered_to_s, hover_s, chapter_ticks });
view.set_theme(WaveTheme { .. });                    // plain RGBA f32 colours
view.set_view(ViewState { style, playhead_s, px_per_s, offset_s, normalise, mode });
view.draw();                                         // call from rAF only when something changed
view.time_at_x(x_css) / view.x_at_time(t)            // hit-testing for scrub / marker drag
```
`WaveStyle::{RgbSpectral, ThreeBand, Mono}`; `ViewMode::{Overview (whole track), Scrolling
(playhead centred, zoom = px_per_s), Free (offset_s = left edge, zoom = px_per_s)}`. Context loss
is handled (`webglcontextlost`/`restored` re-upload on the next `draw`).

## In-process accessors for the player (WS4)

* Pure, no I/O (wasm-safe): `bc_waveform::legacy::mix_envelope(&Waveform) -> (level, low)` feeds
  `bc_music::mixpoints::plan_from_envelope(&Envelope, duration_ms, bpm, Option<&BeatGrid>, 16)` (phrase-snapped
  when the grid has a downbeat phase) and `bc_music::mixpoints::from_peaks(&[(i8,i8)], duration_ms, base)` for legacy
  parity with `mixPoints.ts`. Peaks for the old algorithm: `bc_waveform::legacy::peaks_json(&Waveform, 200)`.
* By track id (native): `bc_waveform::store::WaveformStore::get_overview_only(track_id) -> Option<Waveform>`, or the
  one-call `bc_analysis::mixplan::mix_plan_for(&Db, &WaveformStore, track_id) -> Option<MixPlan>` (stored grid, else the
  extrapolated legacy grid; heuristic when the track has no waveform).
* Over HTTP: `GET /tracks/{id}/music` returns grid, cues and `mix_points` in one fetch (auto cues `mix_in`/`mix_out`/`drop`
  are written by the analyzer into `cue_points` with `auto = 1`).

## Essentia sidecar (BPM/key for tracks without values)
`AnalysisStatus.backends["essentia-sidecar"]` reports availability. Python is auto-detected
(`web/backend/.venv/bin/python`; `BC_ESSENTIA_PYTHON` overrides). `AnalysisOut.analyzer` is
`essentia-import` | `essentia-sidecar` | `bc-rs-1`.

## Integration round additions
* **Renderer**: `WaveformView::draw_in(Region { x, y, w, h })` (CSS px, top-left origin) draws into a sub-rectangle so
  several views can share one canvas on both WebGL2 (viewport + scissor, `u_org` shader offset) and Canvas2D (clip +
  translate). `draw()` still paints the whole canvas. `time_at_x`/`x_at_time` are relative to the last drawn region.
  `resize` only assigns `canvas.width/height` when the size changed (assigning clears the GL buffer). Default beat/bar line
  alpha raised (0.38 / 0.70).
* `GET /analysis/accuracy` -> `AccuracyOut { report: Option<AccuracyReport>, native_bpm_key }`; the report is stored in
  `settings['analysis.accuracy_report']` by `bc-analysis-tool bench --store <new library.db>` (or
  `bc_analysis::bench::store_report(&Db, &Report)` for `bc analyze --gate`).
* `GET /analysis/waveform-cache` -> `WaveformCacheOut { bytes, files, cap_bytes, detail_files, overview_only_files }`.
* Browser check: `crates/bc-waveform/demo` (trunk page) + `e2e/waveform-smoke.mjs <dist>` renders three regions on one canvas
  in WebGL2 and in the forced Canvas2D fallback with system Chrome (swiftshader) and asserts both draw.
