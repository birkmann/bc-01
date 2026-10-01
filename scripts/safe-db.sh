#!/usr/bin/env bash
# Neutralise a COPY of the library DB before any server runs on it: a copy still registers the user's REAL
# library roots, so scans/downloads/moves/deletes would touch real files.
# Usage: scripts/safe-db.sh /path/to/copy/library.db   (refuses the original data dirs)
set -euo pipefail
DB=${1:?db copy}
MODE=${2:-disable}   # disable (default) | playback: keep roots readable for streaming tests, no watchers; only run read-only scripts (smoke*.mjs) then
case "$(readlink -f "$DB")" in
  "$(readlink -f "${BC_DATA_DIR:-$HOME/.local/share/bc-rust}")"/*|"$HOME"/.local/share/*) echo "refusing: $DB is not a scratch copy" >&2; exit 1;;
esac
python3 - "$DB" "$MODE" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
mode = sys.argv[2] if len(sys.argv) > 2 else "disable"
if mode == "playback":
    c.execute("UPDATE library_roots SET watch=0")
else:
    c.execute("UPDATE library_roots SET enabled=0")
try:
    c.execute("UPDATE settings SET value=NULL WHERE key LIKE 'bandcamp.identity%'")
except sqlite3.Error:
    pass
c.commit()
print("roots disabled:", c.execute("select count(*) from library_roots where enabled=0").fetchone()[0])
PY
