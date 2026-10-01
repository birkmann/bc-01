#!/usr/bin/env bash
# Build bc-dsp as a wasm module for the AudioWorklet and put it next to the JS glue.
#   ./scripts/build.sh [outdir]      (default: crates/bc-worklet/js)
set -euo pipefail
cd "$(dirname "$0")/../../.."
OUT="${1:-crates/bc-worklet/js}"
cargo build -p bc-worklet --target wasm32-unknown-unknown --release
WASM="${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/release/bc_worklet.wasm"
mkdir -p "$OUT"
if command -v wasm-opt >/dev/null; then wasm-opt -O3 "$WASM" -o "$OUT/bc_worklet.wasm"; else cp "$WASM" "$OUT/bc_worklet.wasm"; fi
ls -l "$OUT/bc_worklet.wasm"
