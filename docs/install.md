# Installing bc

- [Linux (AppImage)](#linux-appimage)
- [macOS](#macos)
- [From source](#from-source)
- [Building packages](#building-packages)
- [Uninstalling](#uninstalling)

## Linux (AppImage)

Download `bc-linux-x86_64.AppImage` from the latest GitHub release, make it
executable (`chmod +x`) and run it. It needs a Chromium-family browser or
Firefox for the app window, and `libfuse2` on distributions that do not ship
it.

## macOS

Download `bc-macos-arm64.dmg` from the latest GitHub release (Apple silicon,
macOS 12 or later) and drag bc to Applications. The app is not notarized:
allow the first launch under System Settings → Privacy & Security → Open
Anyway. On macOS bc shows its UI in a native window (WebKit), so no other
browser is needed. Closing the window keeps music playing; the dock icon
brings it back and ⌘Q quits. Play/pause and next track are in the
**Controls** menu.

The command line tool is inside the app bundle:
`/Applications/bc.app/Contents/MacOS/bc-rust`. `ffmpeg` and `bandcamp-dl`
from Homebrew are found without changes to `PATH`.

## From source

Build requirements: Rust (stable) with the `wasm32-unknown-unknown` target,
`trunk`, `wasm-bindgen`, `brotli`, `clang`, `pkgconf`, and the ALSA, OpenSSL
and D-Bus development files.

At runtime: a Chromium-family browser (Chromium, Google Chrome, Brave, Edge or
Vivaldi) or Firefox for the app window, and optionally `ffmpeg` (MP3 set
renders, formats bc cannot decode itself) and `bandcamp-dl`.

```sh
./scripts/install.sh            # builds the UI and the app, installs into ~/.local
bc-desktop
```

On Arch Linux, `makepkg -si` in `packaging/` builds and installs a package
(`bc-desktop`, the `bc-rust` command line tool and the icons). The command
line tool is called `bc-rust` because `bc` is the GNU calculator.

## Building packages

`./packaging/appimage/build-appimage.sh` builds the AppImage
(`target/appimage/bc-linux-x86_64.AppImage`).

The macOS app needs Rust with the `wasm32-unknown-unknown` target, `trunk`,
`brotli` and the Xcode command line tools (e.g. `brew install trunk brotli`):

```sh
./packaging/macos/build-app.sh  # → target/macos/bc.app and bc-macos-arm64.dmg
```

The release workflow (`.github/workflows/release.yml`) builds the macOS disk
image and the AppImage and attaches them to the GitHub release of every `v*`
tag; it can also be started by hand from the Actions tab.

## Uninstalling

Quit bc first. Then run only the block for the way you installed it (pacman
reports `target not found: bc-rust` when bc was not installed as a package).

AppImage:

```sh
rm ~/Downloads/bc-linux-x86_64.AppImage   # wherever you put it
```

`scripts/install.sh` (use the same `PREFIX`, and `sudo`, if you changed it):

```sh
PREFIX=~/.local
rm -f "$PREFIX"/bin/{bc-desktop,bc-rust,bc-analysis-tool} \
      "$PREFIX"/share/applications/bc.desktop \
      "$PREFIX"/share/icons/hicolor/*/apps/bc.{png,svg}
rm -rf "$PREFIX"/share/licenses/bc-rust
```

Arch package:

```sh
sudo pacman -R bc-rust
```

macOS:

```sh
rm -rf /Applications/bc.app
```

On Linux, a Chromium-family browser also adds its own **bc** launcher (and
icons) for the bc window to the application menu. Remove it after any of the
Linux methods above:

```sh
for f in $(grep -l 'bc-rust/window-profile' ~/.local/share/applications/*.desktop); do
    rm -f "$f" ~/.local/share/icons/hicolor/*/apps/"$(basename "$f" .desktop)".png
done
```

This leaves your library database, settings, caches and downloads in place,
so a later install picks up where you left off. To remove those as well (this
deletes `library.db` and everything under `BC_DATA_DIR`, including downloads
unless `BC_DOWNLOAD_DIR` points elsewhere; your own music folders are not
touched):

```sh
# Linux
rm -rf ~/.local/share/bc-rust            # data, caches, backups, browser profiles
secret-tool clear service bc-rust        # Bandcamp sign-in in the keyring

# macOS
rm -rf ~/Library/Application\ Support/bc-rust \
       ~/Library/Caches/io.github.birkmann.bc ~/Library/WebKit/io.github.birkmann.bc
security delete-generic-password -s bc-rust   # Bandcamp sign-in in the Keychain
```
