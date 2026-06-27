#!/usr/bin/env bash
#
# One-off driver: run EVERY top-level WPT test area that is not yet represented in the report store,
# streaming results in and regenerating the HTML periodically (resume-safe — re-running skips tests
# already recorded). Mirrors wpt-report-update.sh's run_area, but the area list comes from a file
# (so it can cover areas outside the curated AREAS allowlist) instead of being hard-coded.
#
#   scripts/wpt-run-missing.sh <areas-file>
#
# <areas-file>: one top-level area (path under wpt/) per line.
#
# Tunables (env): WPT_PROCESSES (default 16), WPT_TIMEOUT_MULT (default 0.5).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/wpt-report"
STORE="$OUT/results"
PROCS="${WPT_PROCESSES:-16}"
TMULT="${WPT_TIMEOUT_MULT:-0.5}"

AREAS_FILE="${1:?usage: wpt-run-missing.sh <areas-file>}"
AREAS=()
while IFS= read -r __line || [ -n "$__line" ]; do
  [ -n "$__line" ] && AREAS+=("$__line")
done < "$AREAS_FILE"

if [ -z "${WEBDRIVER_BIN:-}" ] && [ -x "$ROOT/target/release/webdriver" ]; then
  export WEBDRIVER_BIN="$ROOT/target/release/webdriver"
fi

mkdir -p "$STORE"
regen() { python3 "$ROOT/scripts/wpt-report.py" "$STORE" "$OUT" all; }

# Exclude tests already recorded (reverse of wpt-ingest's percent-encoded store path -> URL).
excl="$(mktemp -t wpt-done)"
python3 - "$STORE" > "$excl" <<'PY'
import os, sys
from urllib.parse import unquote
store = sys.argv[1]
for dp, _d, names in os.walk(store):
    for n in names:
        if not n.endswith(".json"):
            continue
        rel = os.path.relpath(os.path.join(dp, n), store)[:-5]
        print("/" + "/".join(unquote(seg) for seg in rel.split(os.sep)))
PY
n=$(wc -l < "$excl" | tr -d ' ')
echo "resuming: $n tests already done; running ${#AREAS[@]} areas (procs=$PROCS, timeout x$TMULT)…" >&2
exclude_args=(); [ "$n" -gt 0 ] && exclude_args=(--exclude-file "$excl")

( "$ROOT/scripts/run-wpt.sh" "${AREAS[@]}" -- --processes "$PROCS" --timeout-multiplier "$TMULT" \
    "${exclude_args[@]}" --log-raw=- \
    | python3 "$ROOT/scripts/wpt-ingest.py" "$STORE" ) &
pipe=$!
while kill -0 "$pipe" 2>/dev/null; do
  regen >/dev/null 2>&1 || true
  sleep 30
done
wait "$pipe"
rm -f "$excl"
regen
echo "done." >&2
