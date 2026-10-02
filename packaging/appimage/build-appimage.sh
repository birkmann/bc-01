#!/usr/bin/env bash
# Builds target/appimage/bc-linux-<arch>.AppImage with linuxdeploy (downloaded on first use into
# packaging/appimage/tools): the UI bundle, bc-desktop, bc-rust and bc-analysis-tool, the desktop
# file and the icons, as scripts/install.sh installs them.
#   ./packaging/appimage/build-appimage.sh
#   SKIP_BUILD=1 ./packaging/appimage/build-appimage.sh   # reuse the UI and binaries already built
# Needs the build requirements of scripts/install.sh. The AppImage links only glibc, ALSA and
# D-Bus from the system, so it runs on distributions at least as new as the one it was built on.
# At runtime bc-desktop still needs a Chromium-family browser or Firefox for its window.
set -euo pipefail
cd "$(dirname "$0")/../.."
here=packaging/appimage
tools="$here/tools"
out=target/appimage
arch=$(uname -m)
# linuxdeploy is itself an AppImage: extracting it avoids needing FUSE (containers, CI).
export APPIMAGE_EXTRACT_AND_RUN=1

mkdir -p "$tools" "$out"
linuxdeploy="$tools/linuxdeploy-$arch.AppImage"
if [[ ! -x "$linuxdeploy" ]]; then
    curl -fL -o "$linuxdeploy" \
        "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$arch.AppImage"
    chmod +x "$linuxdeploy"
fi

appdir="$out/AppDir"
rm -rf "$appdir"
DESTDIR="$appdir" PREFIX=/usr ./scripts/install.sh

# linuxdeploy names its output after the desktop file's Name; give it a stable name instead.
rm -f "$out"/*.AppImage
(
    cd "$out"
    "$OLDPWD/$linuxdeploy" --appdir AppDir \
        --desktop-file AppDir/usr/share/applications/bc.desktop \
        --icon-file AppDir/usr/share/icons/hicolor/scalable/apps/bc.svg \
        --output appimage
)
mv "$out"/bc*-"$arch".AppImage "$out/bc-linux-$arch.AppImage"
rm -rf "$appdir"
echo "Built $out/bc-linux-$arch.AppImage"
