# WS4 — Player API (`bc-engine` `PlayerService`, `bc-dsp`, `bc-worklet`)

DTOs: `bc_types::player` (serde only, wasm32-safe). Router: `PlayerService::router()`, paths
**without** the `/api` prefix (`bc-server` nests under `/api`). Errors are `bc_types::Problem`
(problem+json): 400 bad command, 404 unknown index/track, 409 nothing playing, 503 audio device unavailable.

```rust
let player = bc_engine::PlayerService::new(db, bus, &config);  // opens the audio host lazily; env BC_AUDIO=null for headless, BC_MPRIS=0 to skip MPRIS
// (or PlayerService::new_with(db, bus, &config, SessionConfig{ output, mpris, .. }) / with_ports(ports, bus, cfg) for tests)
// Bandcamp port override (Fan / Explore sources, Bandcamp streams) that KEEPS GET /sets/{id}/render:
//   PlayerService::new_with_bandcamp(db, bus, &config, cfg, Arc::new(BcPlayerPort::new(..)))
// A service built with with_ports(..) gets the render route back with `.with_render(db, &config.ffmpeg_bin)`.
player.start().await;                                          // spawns session + clock + MPRIS workers
let router = player.router();                                  // nest under /api
let reply = player.handle_command(serde_json::json!({"cmd":"next"})).await; // WS hub entry point
```

## WebSocket

Server -> client topics (normal `Event { id, topic, payload }` envelope):

| topic               | payload                       | rate |
| ------------------- | ----------------------------- | ---- |
| `player.state`      | `PlayerState` (full snapshot) | on every change that is not the clock (debounced to <= 20/s) |
| `player.clock`      | `Clock`                       | ~30 Hz while playing, 2 Hz when paused/idle |
| `player.transition` | `TransitionState` or `null`   | on start / retime / toggle / end of a blend |

Client -> server: `ClientMsg::Player { command }` where `command` is a `PlayerCommand` JSON:

```json
{"type":"player","command":{"cmd":"seek","seconds":61.5}}
{"type":"player","command":{"cmd":"play_queue","items":[{"track_id":42,"title":"x"}],"start_index":0,"source":{"kind":"release","release_id":7}}}
{"type":"player","command":{"cmd":"plan","op":{"op":"set_auto_fill","on":true}}}
```

`bc-server` forwards `command` to `PlayerService::handle_command(Value) -> Result<Value, PlayerError>`
(the reply is `{ "ok": true }` or, for `list_devices`, a `DevicesInfo`); errors are sent back as a `Problem`.

Clock extrapolation (smooth playhead at display rate):
`pos = clock.position_s + (now_ns - clock.output_timestamp_ns) / 1e9 * clock.rate` while `clock.playing`;
correct for server/client skew with `server_time_ns`. `frames_played / sample_rate` is the engine time base.

## Commands (`PlayerCommand`, tag `cmd`, snake_case)

transport: `play pause toggle stop seek{seconds} seek_relative{delta_s} next previous jump_to{index}`
start: `play_track{item,queue?} play_queue{items,start_index,source?} start_source{source,shuffle}`
queue (upcoming rows only; history is remapped, shuffle turns off): `add_to_queue insert_at{index,items} play_next{items} move_in_queue{from,to} remove_at{index} remove_range{from,to} replace_at{index,item}`
modes: `set_volume{volume} set_muted{muted} toggle_mute set_repeat{mode} cycle_repeat set_shuffle{on} toggle_shuffle`
mix: `set_mix{on} toggle_mix set_mix_settings{patch} cut_now retime{factor} set_transition_echo{on} set_transition_sync{on} nudge{delta_s} set_mix_out_override{seconds|null} mix_now`
strip (3-band EQ, kills, filter): `set_strip{patch}`
planner: `plan{op}` (see `PlanOp`), `start_mix_from{pool}`
devices: `set_output{target}`, `list_devices`
preview (cue deck): `preview_start{item,at_s?}`, `preview_stop`
misc: `mark_loved{track_id,loved}`, `clear_error`

`QueueItem` needs only `track_id` + `title` for library tracks (the session fills in path, analysis and
mix points by id). Bandcamp items set `origin:"bandcamp"` and `stream_url`/`page_url`.

## HTTP routes (all under `/player`)

| method + path | body -> response |
| --- | --- |
| `GET /player/state` | -> `PlayerState` |
| `GET /player/clock` | -> `Clock` |
| `GET /player/devices` | -> `DevicesInfo` |
| `PUT /player/output` | `OutputTarget` -> `DevicesInfo` |
| `GET /player/plan` | -> `PlanState` |
| `PUT /player/plan` | `PlanState` -> `PlanState` |
| `POST /player/plan/op` | `PlanOp` -> `PlanState` |
| `POST /player/command` | any `PlayerCommand` -> `PlayerState` |
| `POST /player/play` `pause` `toggle` `stop` `next` `previous` | - -> `PlayerState` |
| `POST /player/seek` | `{seconds}` |
| `POST /player/jump` | `{index}` |
| `POST /player/queue/play` | `{items,start_index,source?}` |
| `POST /player/queue/start-source` | `{source,shuffle}` |
| `POST /player/queue/add` `/insert` `/play-next` `/move` `/remove` `/remove-range` `/replace` | the matching command fields |
| `POST /player/volume` | `{volume}` (and optional `muted`) |
| `POST /player/repeat` | `{mode}` |
| `POST /player/shuffle` | `{on}` |
| `PUT /player/mix` | `{on}` |
| `PATCH /player/mix/settings` | `MixSettingsPatch` |
| `POST /player/mix/now` | - |
| `POST /player/transition/cut` `/retime` `/echo` `/sync` `/nudge` | `{}` `{factor}` `{on}` `{on}` `{delta_s}` |
| `PUT /player/mix-out` | `{seconds|null}` |
| `PATCH /player/strip` | `StripPatch` |
| `POST /player/preview` / `DELETE /player/preview` | `{item,at_s?}` / - |

| `GET /sets/{id}/render?format=mp3\|wav` | -> streamed audio (`audio/mpeg` / `audio/wav`, `Content-Disposition: attachment`); 400 bad format / empty set, 404 unknown set |

All mutating `/player/*` routes return the new `PlayerState`.

### Offline set render

`GET /sets/{id}/render` performs the DJ set offline through the *same* `bc-dsp` graph as live playback (cue ranges, tempo
via vinyl rate or key lock, loudness trim toward the set median within +-6 dB, overlap from the incoming slot's beats at its
effective tempo, the incoming slot's transition type: blend / bass_swap / filter / echo_out / cut) and streams the bytes while it
renders (WAV with a streaming header, or MP3 through the `ffmpeg` binary at 320 kbps). Slots without a playable file are dropped and
their neighbours join with a cut. WS1's `/sets/{id}/export` keeps m3u8 and zip. The route exists when the service was built
with a DB (`PlayerService::new` / `new_with`). Library API: `bc_engine::render::{slots_from_set, render_stream, render_wav, render_mp3}`.

## `bc-worklet` (browser "play on this device")

See the section at the end of this file (added with the worklet crate).

## `bc-worklet` (browser "play on this device")

`bc-dsp` (the same mixer, transitions, EQ, echo, limiter, key lock and PLL as the desktop engine)
compiled to wasm32 and run inside an `AudioWorklet`. Plain C ABI (no wasm-bindgen), so the module can be
instantiated in `AudioWorkletGlobalScope`.

Build: `crates/bc-worklet/scripts/build.sh [outdir]` -> `bc_worklet.wasm` (optimised with `wasm-opt` when
present). Assets for `bc-ui`/`bc-server` to serve (same origin): `bc_worklet.wasm`, `js/processor.js`,
`js/player.js`. The server must send `Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp` to enable the `SharedArrayBuffer` PCM ring; without it PCM is
`postMessage`-transferred (`player.transport` says which).

JS API (`js/player.js`, ES module):

```js
const p = await BcPlayer.create({ wasmUrl: '/bc_worklet.wasm', processorUrl: '/processor.js', quality: 1 })
await p.load(0, '/api/stream/42', { startS: 30, rate: 1, keylock: false, trim: 1, grid: { originS, periodS } })
p.play(0)                                   // click-free cut start of a primed deck
await p.load(1, '/api/stream/43', { startS: 20, rate: 1.016, keylock: false })
p.transition({ out: 0, inc: 1, kind: 'blend'|'bass_swap'|'filter'|'echo_out'|'cut'|'eq_blend', lengthS: 15,
               echo: { send: .65, holdS: 3, upS: .3, bpm: 128 }, sync: { rate: 1.016, fromBpm: 126, toBpm: 128 },
               quantise: 'bar', phaseLock: true })
p.retime(s); p.cutNow(); p.setEcho(on); p.setSync(on); p.nudge(0.04)
p.armGapless(1); p.pause(); p.resume(); p.setVolume(0.8); p.setStrip({ low, mid, high, filter, echoSend })
p.onEvent = (name, data) => {}              // ready started ended advanced underrun transition retimed fade-done parked
p.state()                                   // { decks:[{positionS, rate, state, ready, bufferedS, lengthS} x3], active, transitioning, peak:[l,r], xruns }
```

Decoding is `decodeAudioData` by default (the context resamples to its rate); pass `decoder` (an async generator
yielding `{ pcm: Float32Array (interleaved stereo), startFrame, totalFrames }`) to stream with WebCodecs
`AudioDecoder` instead. Rust side: every function in `src/lib.rs` is `extern "C"` (`bcw_new`, `bcw_push_pcm`,
`bcw_render`, `bcw_cue`, `bcw_transition`, `bcw_poll_event`, `bcw_snapshot`, ...). `js/test-node.mjs` is a Node smoke
test of the wasm module (`node js/test-node.mjs`); `js/test-load.mjs` checks that `BcPlayer.load()` flags only the file's final chunk `last`. A custom `decoder` yields `{pcm, startFrame, totalFrames?, last?}`.

For a phone remote-controlling the desktop engine use the WebSocket commands above instead; "play on this
device" runs a *separate* local session in the browser (the UI owns the queue and calls `BcPlayer`).

## Planner fields (PlayerState) and key-lock quality
- `max_play_s`: the pace limit (mirror of `mix_settings.max_play_s`).
- `entry_points: [{uid, drop_s, out_s}]`: where the current track and the next 11 queue rows come in (`drop_s`, the mix-in / drop point; `Entry::Drop`) and start blending out (`out_s`, `null` = plays to its end), in track seconds. Empty with mix off. Set slots report their pinned cues. Use it for the planner's "starts in" offsets so they match the engine.
- `mix_settings.key_lock_quality`: `"fast" | "balanced" | "high"` (default `balanced`) = phase-vocoder frame 1024 / 2048 / 4096. It applies to tracks loaded after the change (never mid-note). Worklet: `player.setKeyLockQuality(q)` (`bcw_set_quality`).
