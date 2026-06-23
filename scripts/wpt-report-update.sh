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
#       Clear the store and re-run the ENTIRE css/ area, streaming results in and regenerating the
#       HTML every few seconds. Slow (tens of minutes).
#
# Uses $WEBDRIVER_BIN if set/executable, else target/release/webdriver (so it won't rebuild the
# engine every call); run-wpt.sh builds one if neither exists.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/wpt-report"
STORE="$OUT/results"
PROCS="${WPT_PROCESSES:-8}"

if [ -z "${WEBDRIVER_BIN:-}" ] && [ -x "$ROOT/target/release/webdriver" ]; then
  export WEBDRIVER_BIN="$ROOT/target/release/webdriver"
fi

regen() { python3 "$ROOT/scripts/wpt-report.py" "$STORE" "$OUT" css; }

if [ "${1:-}" = "--full" ]; then
  echo "full css run — streaming results into $STORE (this takes a while)…" >&2
  rm -rf "$STORE"; mkdir -p "$STORE"
  # Stream the structured log into the ingester; it writes one result file per test as each ends.
  ( "$ROOT/scripts/run-wpt.sh" css -- --processes "$PROCS" --log-raw=- \
      | python3 "$ROOT/scripts/wpt-ingest.py" "$STORE" ) &
  pipe=$!
  # Regenerate the HTML periodically so the report fills in live while the run continues.
  while kill -0 "$pipe" 2>/dev/null; do
    regen >/dev/null 2>&1 || true
    sleep 15
  done
  wait "$pipe"
  regen
  exit 0
fi

if [ "$#" -eq 0 ]; then
  echo "usage: $0 <test-path> [...]   |   $0 --full" >&2
  exit 2
fi
mkdir -p "$STORE"
"$ROOT/scripts/run-wpt.sh" "$@" -- --log-raw=- | python3 "$ROOT/scripts/wpt-ingest.py" "$STORE"
regen
