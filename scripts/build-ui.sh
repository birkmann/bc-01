#!/usr/bin/env bash
# Build the whole UI bundle: worklet wasm (play on this device) + Leptos app (trunk, release) into crates/bc-ui/dist.
# The server embeds crates/bc-ui/dist at compile time (rust-embed), so rebuild bc-server/bc-desktop afterwards.
set -euo pipefail
cd "$(dirname "$0")/.."
crates/bc-worklet/scripts/build.sh
# Size-optimised wasm without touching the workspace profiles (env overrides apply to this build only).
export CARGO_PROFILE_RELEASE_OPT_LEVEL=z CARGO_PROFILE_RELEASE_LTO=true CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_PANIC=abort
( cd crates/bc-ui && trunk build --release )
ls -l crates/bc-ui/dist | head -20
# brotli q11 siblings, served by bc-server when the client accepts br
for f in crates/bc-ui/dist/*.wasm crates/bc-ui/dist/*.js crates/bc-ui/dist/*.css; do brotli -q 11 -f -k "$f"; done
