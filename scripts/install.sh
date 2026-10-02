#!/usr/bin/env bash
# Builds and installs bc. Default prefix: ~/.local (no root needed).
#   ./scripts/install.sh                 # → ~/.local
#   sudo PREFIX=/usr/local ./scripts/install.sh
# Needs: rust (stable) with the wasm32-unknown-unknown target, trunk, wasm-bindgen, brotli; binaryen (wasm-opt) optional.
set -euo pipefail
cd "$(dirname "$0")/.."
PREFIX="${PREFIX:-$HOME/.local}"
DESTDIR="${DESTDIR:-}"
export PATH="$HOME/.cargo/bin:$PATH"
target="${CARGO_TARGET_DIR:-target}/release"

if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
    rustup target add wasm32-unknown-unknown 2>/dev/null || true
    # The UI is embedded into the server at compile time, so it is built first.
    scripts/build-ui.sh
    cargo build --release -p bc-desktop -p bc-cli
    cargo build --release -p bc-analysis --bin bc-analysis-tool
fi

root="$DESTDIR$PREFIX"

install -Dsm755 "$target/bc-desktop" "$root/bin/bc-desktop"
# Not installed as `bc`: that name belongs to the GNU calculator.
install -Dsm755 "$target/bc" "$root/bin/bc-rust"
install -Dsm755 "$target/bc-analysis-tool" "$root/bin/bc-analysis-tool"
install -Dm644 packaging/bc.desktop "$root/share/applications/bc.desktop"
# Launchers may not have ~/.local/bin on PATH: point at the binary directly.
if [[ -z "$DESTDIR" && "$PREFIX" != /usr ]]; then
    sed -i "s|^Exec=bc-desktop|Exec=$PREFIX/bin/bc-desktop|" "$root/share/applications/bc.desktop"
fi
install -Dm644 packaging/bc.svg "$root/share/icons/hicolor/scalable/apps/bc.svg"
for png in packaging/icons/bc-*.png; do
    size=${png##*-}; size=${size%.png}
    install -Dm644 "$png" "$root/share/icons/hicolor/${size}x${size}/apps/bc.png"
done
install -Dm644 LICENSE "$root/share/licenses/bc-rust/LICENSE"
if [[ -z "$DESTDIR" ]]; then
    gtk-update-icon-cache -q -t "$root/share/icons/hicolor" 2>/dev/null || true
    update-desktop-database -q "$root/share/applications" 2>/dev/null || true
fi

echo "Installed to $root. Run: bc-desktop   (or: bc-rust serve, then open http://127.0.0.1:8420)"
if ! command -v google-chrome-stable chromium brave google-chrome brave-browser microsoft-edge-stable vivaldi-stable firefox librewolf >/dev/null 2>&1; then
    echo "Note: bc-desktop opens its window in a Chromium-family browser (chromium, google-chrome, brave, …) or Firefox; none was found."
fi
if [[ -z "$DESTDIR" && ":$PATH:" != *":$PREFIX/bin:"* ]]; then
    echo "Note: $PREFIX/bin is not on your PATH."
fi
