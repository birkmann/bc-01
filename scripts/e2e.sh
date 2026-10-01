#!/usr/bin/env bash
# e2e: start bc-server on a COPY of the real library DB, take screenshots of every page at 375/768/1440
# and run the perf probes. Needs: trunk-built UI in crates/bc-ui/dist (or BC_UI_DIR), node + playwright-core (e2e/node_modules).
# Usage: scripts/e2e.sh [db-source] [out-dir]
set -euo pipefail
cd "$(dirname "$0")/.."
SRC=${1:-${BC_DATA_DIR:-$HOME/.local/share/bc-rust}/library.db}
OUT=${2:-${TMPDIR:-/tmp}/bc-e2e}
WORK=${BC_E2E_WORK:-${TMPDIR:-/tmp}/bc-e2e/work}
BIN=${BC_SERVER_BIN:-${CARGO_TARGET_DIR:-target}/debug/bc-server}
mkdir -p "$WORK/data" "$OUT"
[ -f "$WORK/data/library.db" ] || cp "$SRC" "$WORK/data/library.db"      # never touch the original
scripts/safe-db.sh "$WORK/data/library.db"   # SAFETY: a copy still points at the real library roots
mkdir -p "$OUT/dl"
PORT=${BC_PORT:-8499}
BC_DOWNLOAD_DIR="$OUT/dl" BC_BANDCAMP_DL_BIN=/bin/false BC_AUDIO=null BC_MPRIS=0 BC_DATA_DIR="$WORK/data" BC_PORT=$PORT RUST_LOG=warn "$BIN" > "$OUT/e2e-server.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null || true' EXIT
for _ in $(seq 1 60); do curl -sf "localhost:$PORT/api/health" >/dev/null && break; sleep 0.5; done
( cd e2e && node shots.mjs "http://127.0.0.1:$PORT" "$OUT/shots" && node smoke.mjs "http://127.0.0.1:$PORT" "$OUT/smoke" && node perf.mjs "http://127.0.0.1:$PORT" $PID | tee "$OUT/perf.json" )
