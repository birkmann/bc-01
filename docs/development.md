# Development

- [Architecture](#architecture)
- [Building and testing](#building-and-testing)
- [API notes](#api-notes)

## Architecture

A Cargo workspace of 23 crates in `crates/`:

| Layer | Crates |
|---|---|
| Shared | `bc-types` (DTOs, wasm-safe), `bc-core` (config, path safety, event bus) |
| Data & library | `bc-db`, `bc-libcore`, `bc-library`, `bc-media`, `bc-scan`, `bc-maint`, `bc-plist`, `bc-meta`, `bc-cli` |
| Bandcamp & jobs | `bc-bandcamp`, `bc-jobs` |
| Analysis & music | `bc-analysis`, `bc-waveform`, `bc-music`, `bc-recommend` |
| Audio | `bc-dsp`, `bc-engine`, `bc-worklet` |
| App | `bc-server` (axum, WebSocket hub, embedded UI), `bc-ui` (Leptos, WebAssembly), `bc-desktop` (browser window on Linux, WebKit window on macOS) |

## Building and testing

Build requirements are listed in [install.md](install.md#from-source).

```sh
cargo test --workspace         # unit and integration tests
scripts/build-ui.sh            # worklet + UI into crates/bc-ui/dist (embedded by bc-server at compile time)
cargo run -p bc-cli -- serve   # then open http://127.0.0.1:8420
scripts/e2e.sh <db-copy>       # screenshots of every page and perf probes (Playwright, see e2e/)
```

`scripts/e2e.sh` runs on a copy of a library database and disables its library
roots first (`scripts/safe-db.sh`), so tests never touch real files.

Packaging and releases are described in
[install.md](install.md#building-packages).

## API notes

API notes per area are in [api/](api/); the `.bcw2` waveform format is
described in [api/waveform-format.md](api/waveform-format.md).
