#!/usr/bin/env bash
# Builds bc.app and bc-macos-<arch>.dmg into target/macos: the UI bundle, the release binaries
# (bc-desktop, and the bc-rust command line tool next to it) and the icon.
#   ./packaging/macos/build-app.sh
#   SKIP_BUILD=1 ./packaging/macos/build-app.sh   # reuse the UI and binaries already built
# Needs Rust with the wasm32-unknown-unknown target, trunk, brotli and the Xcode command line
# tools. The app is signed ad hoc (no Developer ID), so a downloaded copy has to be allowed once
# under System Settings → Privacy & Security.
#
# bc.icns is packaging/bc.svg on the macOS icon grid (an 824 px tile with a 100 px margin on a
# 1024 px canvas), converted with iconutil.
set -euo pipefail
cd "$(dirname "$0")/../.."
here=packaging/macos
out=target/macos
arch=$(uname -m)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)
target="${CARGO_TARGET_DIR:-target}/release"
export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-12.0}"

if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
    rustup target add wasm32-unknown-unknown 2>/dev/null || true
    # The UI is embedded into the server at compile time, so it is built first.
    scripts/build-ui.sh
    cargo build --release -p bc-desktop -p bc-cli
fi

app="$out/bc.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m755 "$target/bc-desktop" "$app/Contents/MacOS/bc-desktop"
# Not `bc`: that name belongs to the calculator, here as on Linux.
install -m755 "$target/bc" "$app/Contents/MacOS/bc-rust"
strip -x "$app/Contents/MacOS/bc-desktop" "$app/Contents/MacOS/bc-rust"
install -m644 "$here/bc.icns" LICENSE "$app/Contents/Resources/"
sed "s/@VERSION@/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"
# Apple silicon refuses to run unsigned code, and the signature seals Info.plist and the icon.
codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

dmg="$out/bc-macos-$arch.dmg"
stage="$out/dmg"
rm -rf "$stage" "$dmg"
mkdir -p "$stage"
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
hdiutil create -volname "bc $version" -srcfolder "$stage" -fs HFS+ -format UDZO -ov "$dmg"
rm -rf "$stage"
echo "Built $app and $dmg"
