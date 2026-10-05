<p align="center">
  <img src="packaging/icons/bc-128.png" width="96" height="96" alt="">
</p>
<h1 align="center">bc</h1>
<p align="center"><b>bandcamp library &amp; dj tool for linux and macos</b><br>
<a href="#installing">Install</a> · <a href="docs/manual.md">Manual</a></p>

![The bc home screen](website/assets/img/bc-home.png)

bc is an app for Linux and macOS for finding and downloading music on Bandcamp,
managing a local music library, analyzing tracks, and playing and building DJ
sets. It is written in Rust.

## What it does

- **Library:** music folders, tags, cover art and full-text search, with
  metadata editing, cleanup and completeness checks.
- **Bandcamp:** search, tag pages, a feed of followed artists and labels,
  other fans' collections, harvesting into an inbox, tracklist matching.
- **Downloads:** a crash-safe queue; releases you bought in FLAC, MP3 320,
  WAV and more, everything else from the public stream.
- **Analysis:** tempo, beats, phrases, Camelot key, energy, loudness and a
  three-band waveform from one pass per track.
- **Player:** gapless playback, EQ, filter, echo, key lock, transitions and
  headphone pre-listening; media keys on Linux and macOS.
- **DJ sets:** Automix by key and tempo, a two-lane Arrange timeline, a live
  planner, renders to WAV or MP3.
- **Recommendations:** next up, similar tracks and taste, computed locally.
- **Desktop and phone:** a native app window, or the UI on other devices on
  your network after a one-time pairing.

## Installing

- **Linux:** download `bc-linux-x86_64.AppImage` from the latest GitHub
  release, `chmod +x` it and run it (needs a Chromium-family browser or
  Firefox).
- **macOS:** download `bc-macos-arm64.dmg` (Apple silicon, macOS 12+) and drag
  bc to Applications; allow the first launch under System Settings → Privacy &
  Security.
- **From source:** `./scripts/install.sh`, then `bc-desktop`.

Build requirements, the Arch package and uninstalling:
[docs/install.md](docs/install.md).

## Using it

1. Add your music folder under **Settings → Library**. It is scanned, then
   analyzed in the background.
2. Sign in under **Settings → Bandcamp** for your collection, wishlist and
   feed.
3. Browse **Explore, Feed and Fans** and queue what you want under
   **Downloads**.
4. **DJ Sets → New set**: add tracks, press **Automix**, fine-tune in
   **Arrange**.

Keyboard, command line and configuration are in the
[manual](docs/manual.md).

## Documentation

- [Installing](docs/install.md): AppImage, macOS, from source, building packages, uninstalling
- [Manual](docs/manual.md): getting started, features, keyboard, command line, configuration
- [Development](docs/development.md): architecture, tests, API notes

## Limitations

- Purchase-quality downloads need you to be signed in to Bandcamp and the
  built-in downloader. A track bought on its own is only found when it is
  queued with **Tracks only**; otherwise it is widened to its album and comes
  from the stream.
- On a real library the native key matches essentia on about 93 % of tracks,
  but the tempo matches within 0.5 % on only about 82 % (93 % within 3 %). For
  essentia's tempo, see `BC_ESSENTIA_PYTHON` in the
  [manual](docs/manual.md#configuration).
- WebKitGTK is not used for the desktop window on Linux (it was not reliable
  on Wayland with NVIDIA drivers). In Firefox the window frame does not take
  the app's colours the way a Chromium app window does.
- The macOS app is signed ad hoc, not notarized, and built for Apple silicon
  only. Because the signature changes with every build, the Keychain asks
  again for access to the stored Bandcamp cookie after an update.

## License

MIT, see [LICENSE](LICENSE). Copyright (C) 2026 The bc-01 contributors.

The bundled Inter and JetBrains Mono fonts are under the SIL Open Font License
(`crates/bc-ui/assets/fonts/`).

bc is an independent project and is not affiliated with or endorsed by
Bandcamp.
