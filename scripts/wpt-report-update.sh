#!/usr/bin/env bash
#
# Build/refresh the WPT CSS coverage report. All artifacts live under ONE directory, wpt-report/
# (gitignored):
#   wpt-report/results/   per-test result store (one JSON per test, written live as tests finish)
#   wpt-report/site/      generated multi-page HTML (one index.html per directory)
#   wpt-report/index.html entry point (redirects into site/) — open this in a browser
#
# Results stream into the store as the run progresses (via wpt's mozlog --log-raw), so you can watch
# them land: the HTML is regenerated periodically during a full run, and you can also rebuild it any
# time with `scripts/wpt-report.py wpt-report/results wpt-report`.
#
#   scripts/wpt-report-update.sh css/css-grid/foo.html [more tests/dirs...]
#       Re-run just those test(s)/dir(s); their result files are overwritten in the store and the
#       HTML is rebuilt. Fast — no need to re-run the whole suite.
#
#   scripts/wpt-report-update.sh --full
#       RESUME: run every css/ test NOT already in the store, streaming results in and regenerating
#       the HTML periodically. Restarting (e.g. to change parallelism) keeps prior results and only
#       runs what's left — it never wipes the store. Run repeatedly until the store is complete.
#
#   scripts/wpt-report-update.sh --fresh
#       Wipe the store and run the ENTIRE css/ area from scratch.
#
# Tunables (env): WPT_PROCESSES (parallel runners, default 16), WPT_TIMEOUT_MULT (per-test timeout
# multiplier, default 0.5 — most of our timeouts are unsupported features, so a shorter clock trims
# the tail without losing real passes).
#
# Uses $WEBDRIVER_BIN if set/executable, else target/release/webdriver (so it won't rebuild the
# engine every call); run-wpt.sh builds one if neither exists.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/wpt-report"
STORE="$OUT/results"
PROCS="${WPT_PROCESSES:-16}"
TMULT="${WPT_TIMEOUT_MULT:-0.5}"

if [ -z "${WEBDRIVER_BIN:-}" ] && [ -x "$ROOT/target/release/webdriver" ]; then
  export WEBDRIVER_BIN="$ROOT/target/release/webdriver"
fi

regen() { python3 "$ROOT/scripts/wpt-report.py" "$STORE" "$OUT" css; }

run_area() {
  # Run css/, excluding tests already recorded in the store (resume), streaming results into it.
  local excl
  excl="$(mktemp -t wpt-done)"
  # Recover each done test's URL from its store path (reverse of wpt-ingest's percent-encoding).
  python3 - "$STORE" > "$excl" <<'PY'
import os, sys
from urllib.parse import unquote
store = sys.argv[1]
for dp, _d, names in os.walk(store):
    for n in names:
        if not n.endswith(".json"):
            continue
        rel = os.path.relpath(os.path.join(dp, n), store)[:-5]   # strip .json
        print("/" + "/".join(unquote(seg) for seg in rel.split(os.sep)))
PY
  local n; n=$(wc -l < "$excl" | tr -d ' ')
  echo "resuming: $n tests already done, running the rest (procs=$PROCS, timeout x$TMULT)…" >&2
  local exclude_args=()
  [ "$n" -gt 0 ] && exclude_args=(--exclude-file "$excl")
  ( "$ROOT/scripts/run-wpt.sh" css -- --processes "$PROCS" --timeout-multiplier "$TMULT" \
      "${exclude_args[@]}" --log-raw=- \
      | python3 "$ROOT/scripts/wpt-ingest.py" "$STORE" ) &
  local pipe=$!
  while kill -0 "$pipe" 2>/dev/null; do
    regen >/dev/null 2>&1 || true
    sleep 20
  done
  wait "$pipe"
  rm -f "$excl"
  regen
}

if [ "${1:-}" = "--fresh" ]; then
  rm -rf "$STORE"; mkdir -p "$STORE"
  run_area
  exit 0
fi
if [ "${1:-}" = "--full" ]; then
  mkdir -p "$STORE"
  run_area
  exit 0
fi

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <test-path> [...]   |   $0 --full" >&2
  exit 2
fi
mkdir -p "$STORE"
"$ROOT/scripts/run-wpt.sh" "$@" -- --log-raw=- | python3 "$ROOT/scripts/wpt-ingest.py" "$STORE"
regen
