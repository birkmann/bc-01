<p align="center">
  <img src="packaging/icons/bc-128.png" width="96" height="96" alt="">
</p>
<h1 align="center">bc</h1>
<p align="center"><b>bandcamp library &amp; dj tool for linux</b><br>
<a href="#installing">Install</a> · <a href="#using-it">Manual</a></p>

![The bc home screen](website/assets/img/bc-home.png)

bc is a Linux app for finding and downloading music on Bandcamp, managing a
local music library, analyzing tracks, and playing and building DJ sets. It is
written in Rust and replaces an earlier Python/React version of bc.

## What it does

- **Library.** Scans your music folders (tags, cover art) into SQLite with
  full-text search. Sorting and filtering run on the server, so the track list
  has no row limit. Albums, artists, labels, tags, loved tracks,
  favourites, play history, Top 10, crate dig, metadata editing with dry run
  and undo, cleanup of junk and strays, completeness checks and moving library
  roots.
- **Bandcamp.** Explore searches Bandcamp and browses its tag pages with
  streaming previews. The Feed follows artists and labels, Fans walks the
  collections and wishlists of other fans ("shelves" keep their downloads out
  of your own library), Harvest collects artists, labels, discover feeds,
  collections and wishlists into an inbox, and Tracklists matches uploaded
  tracklists to releases. Requests are rate-limited and cached. Response
  bodies are checked for errors because Bandcamp returns them with HTTP 200.
- **Downloads.** A job queue stored in SQLite that resumes after a crash. Each
  file is written to `.part`, tagged, and then renamed, so an interrupted
  download does not leave a partial file. `bandcamp-dl` can be used instead of
  the built-in downloader. The queue pauses when disk space runs low.
- **Analysis.** One decode pass per track: tempo, beats, downbeats and
  phrases, Camelot key, energy, EBU R128 loudness with true peak, and a
  three-band waveform at about 172 points per second on an absolute dB scale.
- **Player.** Audio engine on cpal with gapless playback, EQ, filter, echo,
  limiter, key lock and five transition types. Queue, history, shuffle and
  auto-fill. MPRIS media keys and a tray icon. The same DSP code runs in the
  browser as an AudioWorklet, so a phone can play through its own speaker.
- **DJ sets.** Automix orders a pool of tracks by key and tempo compatibility
  (beam search). Sets can be edited in Plan or on the two-lane Arrange
  timeline. The live planner handles up next, wishes, tag rules, pools and a
  set clock. Sets can be rendered to WAV or MP3.
- **Recommendations.** Next up, similar tracks and taste, computed locally.
- **Desktop and phone.** `bc-desktop` runs the server and opens the UI in a
  Chromium app window. `bc-rust serve --lan` serves the UI to other devices on
  the local network after a one-time pairing.

## Performance

Compared with the previous Python/React version on the same database and
disk.

| | Previous app | bc |
|---|---|---|
| Start to ready | ≈110 s | 1.5 s |
| 200 tracks at offset 74,000 | 490–560 ms | 13–19 ms |
| Track list | 500 rows, sorted in the browser | all rows, sorted on the server |
| Search (`d`, `dub`, `dub techno`) | 280–520 ms | 25–41 ms |
| Scrolling the full list | — | 53–60 fps |
| Rescan of the whole library, nothing changed | — | 0.71 s |
| Similar tracks, typical / worst seed | 118 / 223 ms | 67 / 194 ms |
| Beatmatch phase error (PipeWire, real device) | — | −0.05 ms, 0 xruns |

## Installing

Requirements: Rust (stable) with the `wasm32-unknown-unknown` target, `trunk`,
`wasm-bindgen`, `brotli`, `clang`, `pkgconf`, and the ALSA, OpenSSL and D-Bus
development files. At runtime: a Chromium-family browser (Chromium, Google
Chrome, Brave, Edge or Vivaldi) for the app window, and optionally `ffmpeg`
(MP3 set renders, formats bc cannot decode itself) and `bandcamp-dl`.

```sh
./scripts/install.sh            # builds the UI and the app, installs into ~/.local
bc-desktop
```

On Arch Linux, `makepkg -si` in `packaging/` builds and installs a package
(`bc-desktop`, the `bc-rust` command line tool and the icons). The command
line tool is called `bc-rust` because `bc` is the GNU calculator.

## Using it

1. Open **Settings → Library** and add the folder that holds your music. It
   is scanned in the background, then analyzed.
2. For collections, wishlists and the feed, paste your Bandcamp cookie under
   **Settings → Bandcamp**. It is stored with 0600 permissions or in the
   system keyring, is never logged, and is only sent to `*.bandcamp.com`.
3. Browse **Explore, Feed and Fans**; queue what you want under
   **Downloads**. Finished downloads are added to the library automatically.
4. Double-click any track to play it. **DJ Sets → New set** starts a set:
   add tracks, press **Automix**, then fine-tune in **Arrange**.

Keyboard (while not typing): <kbd>Space</kbd> play/pause, <kbd>←</kbd>/<kbd>→</kbd>
seek, <kbd>Shift</kbd>+<kbd>←</kbd>/<kbd>→</kbd> previous/next, <kbd>/</kbd>
search, <kbd>Q</kbd> planner, <kbd>S</kbd> similar tracks, <kbd>V</kbd> deck
view, <kbd>X</kbd> cut to the next track now.

### Command line

```sh
bc-rust serve [--lan]      # UI and API on http://127.0.0.1:8420
bc-rust scan               # scan library roots for new, changed and missing files
bc-rust doctor             # integrity, counts, index plans, orphans
bc-rust analyze --gate     # analysis accuracy gate (runs bc-analysis-tool)
bc-rust bandcamp-replay    # re-run the Bandcamp extractors over cached pages, offline
bc-rust import --from ~/.local/share/bcapp/library.db   # import the previous app's library
```

### Configuration

Data lives in `~/.local/share/bc-rust` (`library.db`, caches, backups).
Environment variables override the defaults:

| Variable | Default | |
|---|---|---|
| `BC_DATA_DIR` | `~/.local/share/bc-rust` | database, caches, backups |
| `BC_DOWNLOAD_DIR` | `$BC_DATA_DIR/downloads` | where downloads land |
| `BC_HOST`, `BC_PORT` | `127.0.0.1`, `8420` | server address |
| `BC_AUDIO` | (system default) | `null` runs without an audio device |
| `BC_MPRIS` | `1` | `0` disables media keys |
| `BC_FFMPEG_BIN` | `ffmpeg` | used for MP3 renders and as a decode fallback |
| `BC_BANDCAMP_DL_BIN` | `bandcamp-dl` | the alternative downloader |
| `BC_ESSENTIA_PYTHON` | — | a Python with essentia, for reference BPM/key |
| `BC_HARVEST_RATE_PER_SEC` | `0.67` | Bandcamp request rate |

Tempo and key come from bc's own analyzer. If `BC_ESSENTIA_PYTHON` points at a
Python with essentia installed, new tracks use essentia's values instead and
the native ones are kept as a fallback.

## Development

A Cargo workspace of 23 crates in `crates/`:

| Layer | Crates |
|---|---|
| Shared | `bc-types` (DTOs, wasm-safe), `bc-core` (config, path safety, event bus) |
| Data & library | `bc-db`, `bc-libcore`, `bc-library`, `bc-media`, `bc-scan`, `bc-maint`, `bc-plist`, `bc-meta`, `bc-cli` |
| Bandcamp & jobs | `bc-bandcamp`, `bc-jobs` |
| Analysis & music | `bc-analysis`, `bc-waveform`, `bc-music`, `bc-recommend` |
| Audio | `bc-dsp`, `bc-engine`, `bc-worklet` |
| App | `bc-server` (axum, WebSocket hub, embedded UI), `bc-ui` (Leptos, WebAssembly), `bc-desktop` |

```sh
cargo test --workspace         # unit and integration tests
scripts/build-ui.sh            # worklet + UI into crates/bc-ui/dist (embedded by bc-server at compile time)
cargo run -p bc-cli -- serve   # then open http://127.0.0.1:8420
scripts/e2e.sh <db-copy>       # screenshots of every page and perf probes (Playwright, see e2e/)
```

`scripts/e2e.sh` runs on a copy of a library database and disables its library
roots first (`scripts/safe-db.sh`), so tests never touch real files. API notes
per area are in `docs/api/`; the `.bcw2` waveform format is described in
`docs/api/waveform-format.md`.

## Known limitations

- Downloads in owned quality (FLAC/320 from your purchases) are not built yet.
- The native BPM/key analyzer passes the synthetic accuracy gate but is less
  accurate than essentia on real libraries; see `BC_ESSENTIA_PYTHON` above.
- The desktop window needs a Chromium-family browser: WebKitGTK was not
  reliable on Wayland with NVIDIA drivers.
- Only one audio output device is used; a separate headphone cue device is
  untested.

## License

MIT, see [LICENSE](LICENSE). Copyright (C) 2026 Rafael Birkmann.

The bundled Inter and JetBrains Mono fonts are under the SIL Open Font License
(`crates/bc-ui/assets/fonts/`).

bc is an independent project and is not affiliated with or endorsed by
Bandcamp.
