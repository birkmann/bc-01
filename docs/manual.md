# Manual

- [Getting started](#getting-started)
- [Features](#features)
- [Keyboard](#keyboard)
- [Desktop and phone](#desktop-and-phone)
- [Command line](#command-line)
- [Configuration](#configuration)

## Getting started

1. Open **Settings → Library** and add the folder that holds your music. It
   is scanned in the background, then analyzed.
2. For collections, wishlists and the feed, click **Sign in to Bandcamp**
   under **Settings → Bandcamp**. In the desktop app this opens Bandcamp's own
   sign-in page in a window, and bc keeps only the login cookie, never the
   password. In a browser on another device, paste the `identity` cookie
   instead. The cookie is stored with 0600 permissions or in the system
   keyring, is never logged, and is only sent to `*.bandcamp.com`. Once you
   are signed in, pick a **Download quality** there: releases you bought are
   then downloaded from your collection in that format.
3. Browse **Explore, Feed and Fans**; queue what you want under
   **Downloads**. Finished downloads are added to the library automatically.
4. Double-click any track to play it. **DJ Sets → New set** starts a set:
   add tracks, press **Automix**, then fine-tune in **Arrange**. To pre-listen
   on headphones, choose a **Cue / headphones** device under
   **Settings → Audio**.

## Features

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
  download does not leave a partial file. Releases you bought come from your
  collection in the format you pick (FLAC, MP3 320, WAV, ...); everything else
  is Bandcamp's public stream. `bandcamp-dl` can be used instead of the
  built-in downloader. The queue pauses when disk space runs low.
- **Analysis.** One decode pass per track: tempo, beats, downbeats and
  phrases, Camelot key, energy, EBU R128 loudness with true peak, and a
  three-band waveform at about 172 points per second on an absolute dB scale.
- **Player.** Audio engine on cpal with gapless playback, EQ, filter, echo,
  limiter, key lock and five transition types. An optional second output
  device pre-listens on headphones while the main output keeps playing.
  Queue, history, shuffle and auto-fill. Media keys (MPRIS on Linux, Now
  Playing on macOS) and a tray icon on Linux.
- **DJ sets.** Automix orders a pool of tracks by key and tempo compatibility
  (beam search). Sets can be edited in Plan or on the two-lane Arrange
  timeline. The live planner handles up next, wishes, tag rules, pools and a
  set clock. Sets can be rendered to WAV or MP3.
- **Recommendations.** Next up, similar tracks and taste, computed locally.

## Keyboard

While not typing:

| Key | Action |
|---|---|
| <kbd>Space</kbd> | play/pause |
| <kbd>←</kbd> / <kbd>→</kbd> | seek |
| <kbd>Shift</kbd>+<kbd>←</kbd> / <kbd>→</kbd> | previous / next track |
| <kbd>/</kbd> | search |
| <kbd>Q</kbd> | planner |
| <kbd>S</kbd> | similar tracks |
| <kbd>V</kbd> | deck view |
| <kbd>X</kbd> | cut to the next track now |

## Desktop and phone

`bc-desktop` runs the server and opens the UI in a native window on macOS, or
in a Chromium or Firefox app window on Linux. With a Chromium-family browser,
bc installs itself as a web app of its own browser profile on first start.
Click the chevron in the window's title strip once and bc's header becomes the
title bar.

`bc-rust serve --lan` serves the UI to other devices on the local network
after a one-time pairing. The player's DSP code also runs in the browser as an
AudioWorklet, so a phone can play through its own speaker.

## Command line

```sh
bc-rust serve [--lan]      # UI and API on http://127.0.0.1:8420
bc-rust scan               # scan library roots for new, changed and missing files
bc-rust doctor             # integrity, counts, index plans, orphans
bc-rust analyze --gate     # analysis accuracy gate (runs bc-analysis-tool)
bc-rust bandcamp-replay    # re-run the Bandcamp extractors over cached pages, offline
bc-rust import --from ~/.local/share/bcapp/library.db   # import the previous app's library
```

## Configuration

Data lives in `~/.local/share/bc-rust` on Linux and in
`~/Library/Application Support/bc-rust` on macOS (`library.db`, caches,
backups). Environment variables override the defaults:

| Variable | Default | |
|---|---|---|
| `BC_DATA_DIR` | see above | database, caches, backups |
| `BC_DOWNLOAD_DIR` | `$BC_DATA_DIR/downloads` | where downloads land |
| `BC_DOWNLOAD_TEMPLATE` | `%{artist}/%{album}/%{track} - %{title}` | file layout below the downloads folder (bandcamp-dl tokens; also `%{trackartist}`, `%{date}`, `%{label}`) |
| `BC_HOST`, `BC_PORT` | `127.0.0.1`, `8420` | server address |
| `BC_AUDIO` | (system default) | `null` runs without an audio device |
| `BC_MPRIS` | `1` | `0` disables media keys |
| `BC_FFMPEG_BIN` | `ffmpeg` | used for MP3 renders and as a decode fallback |
| `BC_BANDCAMP_DL_BIN` | `bandcamp-dl` | the alternative downloader |
| `BC_ESSENTIA_PYTHON` | — | a Python with essentia, for reference BPM/key |
| `BC_HARVEST_RATE_PER_SEC` | `0.67` | Bandcamp request rate |
| `BC_DESKTOP_BROWSER` | (first found) | Linux: browser for the app window, Chromium family or Firefox |

Tempo and key come from bc's own analyzer. If `BC_ESSENTIA_PYTHON` points at a
Python with essentia installed, new tracks use essentia's values instead and
the native ones are kept as a fallback.
