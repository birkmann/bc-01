#!/usr/bin/env bash
# Render the app icon PNGs (hicolor sizes + web manifest sizes) from the one SVG source.
# Needs rsvg-convert (librsvg). Run after editing crates/bc-ui/assets/brand/bc-app-icon.svg.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
src="$root/crates/bc-ui/assets/brand/bc-app-icon.svg"
cp "$src" "$root/crates/bc-ui/assets/icon.svg"
cp "$src" "$root/packaging/bc.svg"
mkdir -p "$root/packaging/icons"
for s in 16 24 32 48 64 128 256 512; do
  rsvg-convert -w "$s" -h "$s" "$src" -o "$root/packaging/icons/bc-$s.png"
done
for s in 192 512; do
  rsvg-convert -w "$s" -h "$s" "$src" -o "$root/crates/bc-ui/assets/brand/icon$s.png"
done
